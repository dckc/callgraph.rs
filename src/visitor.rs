// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use graphviz::{self, Labeller, GraphWalk};

use rustc::middle::ty;
use rustc_trans::save::{self, SaveContext};

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::iter::FromIterator;

use syntax::ast::NodeId;
use syntax::{ast, visit};


// Records functions and function calls, processes and outputs this data.
pub struct RecordVisitor<'l, 'tcx: 'l> {
    // Used by the save-analysis API.
    save_cx: SaveContext<'l, 'tcx>,

    // Track statically dispatched function calls.
    static_calls: HashSet<(NodeId, NodeId)>,
    // During the collection phase, the tuples are (caller def, callee decl).
    // post_process converts these to (caller def, callee def).
    dynamic_calls: HashSet<(NodeId, NodeId)>,
    // Track function definitions.
    functions: HashMap<NodeId, String>,
    // Track method declarations.
    method_decls: HashMap<NodeId, String>,
    // Maps a method decl to its implementing methods.
    method_impls: HashMap<NodeId, Vec<NodeId>>,

    // Which function we're calling from, we'll update this as we walk the AST.
    cur_fn: Option<NodeId>,
}

// `this.cur_fn.is_some()` or returns.
macro_rules! ensure_cur_fn {($this: expr, $span: expr) => {
    if $this.cur_fn.is_none() {
        println!("WARNING: call at {:?} without known current function",
                 $span);
        return;
    }
}}

// Backup self.cur_fn, set cur_fn to id, continue to walk the AST by executing
// $walk, then restore self.cur_fn.
macro_rules! push_walk_pop {($this: expr, $id: expr, $walk: expr) => {{
    let prev_fn = $this.cur_fn;
    $this.cur_fn = Some($id);
    $walk;
    $this.cur_fn = prev_fn;
}}}

// Return if we're in generated code.
macro_rules! skip_generated_code {($span: expr) => {
    if save::generated_code($span) {
        return;
    }
}}

// True if the def_id refers to an item in the current crate.
fn is_local(id: ast::DefId) -> bool {
    id.krate == ast::LOCAL_CRATE
}


impl<'l, 'tcx: 'l> RecordVisitor<'l, 'tcx> {
    pub fn new(tcx: &'l ty::ctxt<'tcx>) -> RecordVisitor<'l, 'tcx> {
        RecordVisitor {
            save_cx: SaveContext::new(tcx),

            static_calls: HashSet::new(),
            dynamic_calls: HashSet::new(),
            functions: HashMap::new(),
            method_decls: HashMap::new(),
            method_impls: HashMap::new(),

            cur_fn: None,
        }
    }

    // Dump collected and processed information to stdout.
    // Must be called after post_process.
    pub fn dump(&self) {
        println!("Found fns:");
        for (k, d) in self.functions.iter() {
            println!("{}: {}", k, d);
        }

        println!("\nFound method decls:");
        for (k, d) in self.method_decls.iter() {
            println!("{}: {}", k, d);
        }

        println!("\nFound calls:");
        for &(ref from, ref to) in self.static_calls.iter() {
            let from = &self.functions[from];
            let to = &self.functions[to];
            println!("{} -> {}", from, to);
        }

        println!("\nFound potential calls:");
        for &(ref from, ref to) in self.dynamic_calls.iter() {
            let from = &self.functions[from];
            let to = &self.functions[to];
            println!("{} -> {}", from, to);
        }
    }

    // Make a graphviz dot file.
    // Must be called after post_process.
    pub fn dot(&self) {
        // TODO use crate name 
        let mut file = File::create("out.dot").unwrap();
        graphviz::render(self, &mut file).unwrap();
    }

    // Processes dynamically dispatched method calls. Converts calls to the decl
    // to a call to every method implementing the decl.
    pub fn post_process(&mut self) {
        let mut processed_calls = HashSet::new();

        for &(ref from, ref to) in self.dynamic_calls.iter() {
            for to in self.method_impls[to].iter() {
                processed_calls.insert((*from, *to));
            }
        }

        self.dynamic_calls = processed_calls;
    }

    // Helper function. Record a method call.
    fn record_method_call(&mut self, mrd: &save::MethodCallData) {
        ensure_cur_fn!(self, mrd.span);

        if let Some(ref_id) = mrd.ref_id {
            if is_local(ref_id) {
                self.static_calls.insert((self.cur_fn.unwrap(), ref_id.node));
            }
            return;
        }

        if let Some(decl_id) = mrd.decl_id {
            if is_local(decl_id) {
                self.dynamic_calls.insert((self.cur_fn.unwrap(), decl_id.node));
            }
        }
    }

    // Record that def implements decl.
    fn append_method_impl(&mut self, decl: NodeId, def: NodeId) {
        if !self.method_impls.contains_key(&decl) {
            self.method_impls.insert(decl, vec![]);
        }

        self.method_impls.get_mut(&decl).unwrap().push(def);
    }
}


// A visitor pattern implementation for visiting nodes in the AST. We only
// implement the methods for the nodes we are interested in visiting. Here,
// functions and methods, and references to functions and methods.
//
// Note that a function call (which applies to UFCS methods), `foo()` is just
// an expression involving `foo`, which can be anything with function type.
// E.g., `let x = foo; x();` is legal if `foo` is a function. Since in this
// case we would be interested in `foo`, but not `x`, we don't actually track
// call expressions, but rather path expressions which refer to functions. This
// will give us some false positives (e.g., if a function has `let x = foo;`,
// but `x` is never used).
impl<'v, 'l, 'tcx: 'l> visit::Visitor<'v> for RecordVisitor<'l, 'tcx> {
    // Visit a path - the path could point to a function or method.
    fn visit_path(&mut self, path: &'v ast::Path, id: NodeId) {
        skip_generated_code!(path.span);

        let data = self.save_cx.get_path_data(id, path);
        if let save::Data::FunctionCallData(ref fcd) = data {
            if is_local(fcd.ref_id) {
                let to = fcd.ref_id.node;
                ensure_cur_fn!(self, fcd.span);
                self.static_calls.insert((self.cur_fn.unwrap(), to));
            }
        }
        if let save::Data::MethodCallData(ref mrd) = data {
            self.record_method_call(mrd);
        }

        // Continue walking the AST.
        visit::walk_path(self, path)
    }

    // Visit an expression
    fn visit_expr(&mut self, ex: &'v ast::Expr) {
        skip_generated_code!(ex.span);

        visit::walk_expr(self, ex);

        // Skip everything except method calls. (We shouldn't have to do this, but
        // calling get_expr_data on an expression it doesn't know about will panic).
        if let ast::Expr_::ExprMethodCall(..) = ex.node {} else
            return;
        }

        let data = self.save_cx.get_expr_data(ex);
        if let Some(save::Data::MethodCallData(ref mrd)) = data {
            self.record_method_call(mrd);
        }
    }

    fn visit_item(&mut self, item: &'v ast::Item) {
        skip_generated_code!(item.span);

        if let ast::Item_::ItemFn(..) = item.node {
            let data = self.save_cx.get_item_data(item);
            if let save::Data::FunctionData(fd) = data {
                self.functions.insert(fd.id, fd.qualname);

                push_walk_pop!(self, fd.id, visit::walk_item(self, item));

                return;
            }
        }

        visit::walk_item(self, item)
    }

    fn visit_trait_item(&mut self, ti: &'v ast::TraitItem) {
        skip_generated_code!(ti.span);

        // Note to self: it is kinda sucky we have to examine the AST before
        // asking for data here.
        match ti.node {
            // A method declaration.
            ast::TraitItem_::MethodTraitItem(_, None) => {
                let fd = self.save_cx.get_method_data(ti.id, ti.ident.name, ti.span);
                self.method_decls.insert(fd.id, fd.qualname);
                self.method_impls.insert(fd.id, vec![]);
            }
            // A default method. This declares a trait method and provides an
            // implementation.
            ast::TraitItem_::MethodTraitItem(_, Some(_)) => {
                let fd = self.save_cx.get_method_data(ti.id, ti.ident.name, ti.span);
                // Record, a declaration, a definintion, and a reflexive implementation.
                self.method_decls.insert(fd.id, fd.qualname.clone());
                self.functions.insert(fd.id, fd.qualname);
                self.append_method_impl(fd.id, fd.id);
                
                push_walk_pop!(self, fd.id, visit::walk_trait_item(self, ti));

                return;
            }
            _ => {}
        }

        visit::walk_trait_item(self, ti)
    }

    fn visit_impl_item(&mut self, ii: &'v ast::ImplItem) {
        skip_generated_code!(ii.span);

        if let ast::ImplItem_::MethodImplItem(..) = ii.node {
            let fd = self.save_cx.get_method_data(ii.id, ii.ident.name, ii.span);
            // Record the method's existence.
            self.functions.insert(fd.id, fd.qualname);
            if let Some(decl) = fd.declaration {
                if is_local(decl) {
                    // If we're implementing a method in the local crate, record
                    // the implementation of the decl.
                    self.append_method_impl(decl.node, fd.id);
                }
            }

            push_walk_pop!(self, fd.id, visit::walk_impl_item(self, ii));

            return;
        }

        visit::walk_impl_item(self, ii)
    }
}

// Graphviz interaction.
//
// We use NodeIds to identify nodes in the graph to Graphviz. We label them by
// looking up the name for the id in self.functions. Edges are the union of
// static and dynamic calls. We don't label edges, but potential calls due to
// dynamic dispatch get dotted edges.
//
// Invariants: all edges must be beween nodes which are in self.functions.
//             post_process must have been called (i.e., no decls left in the graph)

// Whether a call certainly happens (e.g., static dispatch) or only might happen
// (e.g., all possible receiving methods of dynamic dispatch).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CallKind {
    Definite,
    Potential,
}

// An edge in the callgraph, only used with graphviz.
pub type Edge = (NodeId, NodeId, CallKind);

// Issues ids, labels, and styles for graphviz.
impl<'a, 'l, 'tcx: 'l> Labeller<'a, NodeId, Edge> for RecordVisitor<'l, 'tcx> {
    fn graph_id(&'a self) -> graphviz::Id<'a> {
        graphviz::Id::new("Callgraph_for_TODO").unwrap()
    }

    fn node_id(&'a self, n: &NodeId) -> graphviz::Id<'a> {
        graphviz::Id::new(format!("n_{}", n)).unwrap()
    }

    fn node_label(&'a self, n: &NodeId) -> graphviz::LabelText<'a> {
        // To find the label, we just lookup the function name.
        graphviz::LabelText::label(&*self.functions[n])
    }

    // TODO styles
}

// Drives the graphviz visualisation.
impl<'a, 'l, 'tcx: 'l> GraphWalk<'a, NodeId, Edge> for RecordVisitor<'l, 'tcx> {
    fn nodes(&'a self) -> graphviz::Nodes<'a, NodeId> {
        graphviz::Nodes::from_iter(self.functions.keys().cloned())
    }

    fn edges(&'a self) -> graphviz::Edges<'a, Edge> {
        let static_iter = self.static_calls.iter().map(|&(ref f, ref t)| (f.clone(),
                                                                          t.clone(),
                                                                          CallKind::Definite));
        let dyn_iter = self.dynamic_calls.iter().map(|&(ref f, ref t)| (f.clone(),
                                                                        t.clone(),
                                                                        CallKind::Potential));
        graphviz::Edges::from_iter(static_iter.chain(dyn_iter))
    }

    fn source(&'a self, &(from, _, _): &Edge) -> NodeId {
        from
    }

    fn target(&'a self, &(_, to, _): &Edge) -> NodeId {
        to
    }
}
