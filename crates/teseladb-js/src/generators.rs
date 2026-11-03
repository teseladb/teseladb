//! Generators. The parser rewrites a generator's body so that every `yield` is a statement of
//! its own (`yield x;` or `%tN = yield x;`), the values evaluated before it kept in temporaries,
//! and marks each statement that holds a `yield` with [`Stmt::Suspends`]. The interpreter runs
//! the body with the ordinary recursive walk; a `yield` unwinds it with [`Ctrl::Suspend`], each
//! marked statement saving where it was on the way out, and the next call to `next` walks back
//! down the same statements restoring that state until it reaches the `yield` it left.

use std::cell::RefCell;
use std::rc::Rc;

use crate::ast::*;
use crate::builtins::{arg, method};
use crate::interp::{Ctrl, EnvRef, Interp, IterRec, R};
use crate::value::*;

// Rewriting.

/// Whether `e` holds a `yield` of this function (not of a function inside it).
pub fn expr_yields(e: &Expr) -> bool {
    match e {
        Expr::Yield(..) => true,
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Regex(..)
        | Expr::TemplateObject(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::Ident(..)
        | Expr::This
        | Expr::Func(_) => false,
        Expr::Class(c) => crate::lowering::class_yields(c),
        Expr::Template(_, xs) | Expr::Seq(xs) => xs.iter().any(expr_yields),
        Expr::SuperMember(x)
        | Expr::Spread(x)
        | Expr::Unary(_, x)
        | Expr::OptChain(x)
        | Expr::Typeof(x)
        | Expr::Delete(x)
        | Expr::PrivateIn(_, x)
        | Expr::Await(x)
        | Expr::At(_, x) => expr_yields(x),
        Expr::SuperCall(xs, _) => xs.iter().any(expr_yields),
        Expr::Array(xs) => xs.iter().flatten().any(expr_yields),
        Expr::Object(props) => props.iter().any(|p| match p {
            ObjProp::Value(k, v) => key_yields(k) || expr_yields(v),
            ObjProp::Accessor(k, ..) => key_yields(k),
            ObjProp::Spread(v) => expr_yields(v),
        }),
        Expr::Update { target, .. } => expr_yields(target),
        Expr::Binary(_, a, b) | Expr::Logical(_, a, b) => expr_yields(a) || expr_yields(b),
        Expr::Assign { target, value, .. } => pattern_yields(target) || expr_yields(value),
        Expr::Cond(a, b, c) => expr_yields(a) || expr_yields(b) || expr_yields(c),
        Expr::Member { obj, prop, .. } => {
            expr_yields(obj) || matches!(&**prop, MemberProp::Computed(k) if expr_yields(k))
        }
        Expr::Call { callee, args, .. } | Expr::New { callee, args, .. } => {
            expr_yields(callee) || args.iter().any(expr_yields)
        }
    }
}

/// A name or a member, through the positions the parser wraps them in.
fn is_reference(e: &Expr) -> bool {
    match e {
        Expr::At(_, x) => is_reference(x),
        Expr::Member { .. } | Expr::Ident(..) => true,
        _ => false,
    }
}

pub(crate) fn key_yields(k: &PropName) -> bool {
    matches!(k, PropName::Computed(e) if expr_yields(e))
}

fn pattern_yields(p: &Pattern) -> bool {
    match p {
        Pattern::Name(_) => false,
        Pattern::Expr(e) => expr_yields(e),
        Pattern::Object(props, rest) => {
            props.iter().any(|(k, p, d)| {
                key_yields(k) || pattern_yields(p) || d.as_ref().is_some_and(expr_yields)
            }) || rest.as_deref().is_some_and(pattern_yields)
        }
        Pattern::Array(items, rest) => {
            items
                .iter()
                .flatten()
                .any(|(p, d)| pattern_yields(p) || d.as_ref().is_some_and(expr_yields))
                || rest.as_deref().is_some_and(pattern_yields)
        }
    }
}

fn stmt_suspends(s: &Stmt) -> bool {
    matches!(s, Stmt::Suspends(_))
}

/// Rewrites a generator's body (see the module's documentation).
pub fn lower(body: Vec<Stmt>) -> Vec<Stmt> {
    let mut l = Lower { names: Vec::new() };
    let mut out = l.stmts(body);
    if !l.names.is_empty() {
        let temps = l.names.into_iter().map(|n| (Pattern::Name(n), None)).collect();
        out.insert(0, Stmt::Decl(DeclKind::Var, temps));
    }
    out
}

/// A temporary of the rewriting is `%t<n>`, or `%t<n>|<text>` when it holds the value of an
/// expression that messages name by its source (`a.b is null`); see
/// [`crate::interp::callee_text`].
pub(crate) const TEMP_PREFIX: char = '%';

pub(crate) fn ident(n: &Name) -> Expr {
    Expr::Ident(n.clone(), Pos::default())
}

pub(crate) fn set(t: &Name, e: Expr) -> Stmt {
    let s = Stmt::Expr(Expr::Assign {
        op: "=",
        target: Box::new(Pattern::Name(t.clone())),
        value: Box::new(e),
    });
    if let Stmt::Expr(Expr::Assign { value, .. }) = &s
        && matches!(**value, Expr::Yield(..))
    {
        return Stmt::Suspends(Box::new(s));
    }
    s
}

fn not(e: Expr) -> Expr {
    Expr::Unary("!", Box::new(e))
}

/// A block of statements, marked when one of them suspends.
pub(crate) fn block(stmts: Vec<Stmt>) -> Stmt {
    mark(Stmt::Block(stmts))
}

pub(crate) fn mark(s: Stmt) -> Stmt {
    let suspends = match &s {
        Stmt::Block(b) => b.iter().any(stmt_suspends),
        Stmt::If(_, a, b) => stmt_suspends(a) || b.as_deref().is_some_and(stmt_suspends),
        Stmt::While(_, b) | Stmt::DoWhile(b, _) | Stmt::Labeled(_, b) | Stmt::With(_, b) => {
            stmt_suspends(b)
        }
        Stmt::For(_, _, _, b) => stmt_suspends(b),
        Stmt::ForIn { body, .. } => stmt_suspends(body),
        Stmt::Try(a, c, f) => {
            a.iter().any(stmt_suspends)
                || c.as_ref().is_some_and(|(_, c)| c.iter().any(stmt_suspends))
                || f.as_ref().is_some_and(|f| f.iter().any(stmt_suspends))
        }
        Stmt::Switch(_, cases) => cases.iter().flat_map(|(_, b)| b).any(stmt_suspends),
        _ => false,
    };
    if suspends { Stmt::Suspends(Box::new(s)) } else { s }
}

pub(crate) struct Lower {
    /// The temporaries made so far.
    names: Vec<Name>,
}

impl Lower {
    pub(crate) fn tmp(&mut self) -> Name {
        let n: Name = format!("{TEMP_PREFIX}t{}", self.names.len() + 1).into();
        self.names.push(n.clone());
        n
    }

    /// A temporary for the value of `e`, which keeps the text messages give `e`.
    pub(crate) fn tmp_for(&mut self, e: &Expr) -> Name {
        let text = crate::interp::callee_text(e);
        if text == crate::interp::INTERMEDIATE {
            return self.tmp();
        }
        let n: Name = format!("{TEMP_PREFIX}t{}|{text}", self.names.len() + 1).into();
        self.names.push(n.clone());
        n
    }

    /// `e` kept in a temporary, so that a later `yield` cannot change it.
    pub(crate) fn spill(&mut self, e: Expr, pre: &mut Vec<Stmt>) -> Expr {
        if matches!(
            e,
            Expr::Num(_)
                | Expr::BigInt(_)
                | Expr::Str(_)
                | Expr::Bool(_)
                | Expr::Null
                | Expr::Undefined
                | Expr::This
                | Expr::Func(_)
        ) {
            return e;
        }
        let t = self.tmp_for(&e);
        pre.push(set(&t, e));
        ident(&t)
    }

    /// Expressions evaluated in order: the ones before the last that yields are kept in
    /// temporaries.
    fn ordered(&mut self, xs: Vec<Expr>, pre: &mut Vec<Stmt>) -> Vec<Expr> {
        let last = xs.iter().rposition(expr_yields);
        xs.into_iter()
            .enumerate()
            .map(|(i, x)| match last {
                Some(l) if i < l => {
                    let x = self.expr(x, pre);
                    self.spill(x, pre)
                }
                Some(l) if i == l => self.expr(x, pre),
                _ => x,
            })
            .collect()
    }

    fn spread_ordered(&mut self, xs: Vec<Expr>, pre: &mut Vec<Stmt>) -> Vec<Expr> {
        let last = xs.iter().rposition(expr_yields);
        xs.into_iter()
            .enumerate()
            .map(|(i, x)| match (last, x) {
                (Some(l), Expr::Spread(inner)) if i <= l => {
                    let inner = self.expr(*inner, pre);
                    let inner = if i < l { self.spill(inner, pre) } else { inner };
                    Expr::Spread(Box::new(inner))
                }
                (Some(l), x) if i < l => {
                    let x = self.expr(x, pre);
                    self.spill(x, pre)
                }
                (Some(l), x) if i == l => self.expr(x, pre),
                (_, x) => x,
            })
            .collect()
    }

    pub(crate) fn key(&mut self, k: PropName, pre: &mut Vec<Stmt>) -> PropName {
        match k {
            PropName::Computed(e) => {
                let e = self.expr(*e, pre);
                PropName::Computed(Box::new(self.spill(e, pre)))
            }
            k => k,
        }
    }

    /// `e` without `yield`, the statements it needs first pushed to `pre`.
    pub(crate) fn expr(&mut self, e: Expr, pre: &mut Vec<Stmt>) -> Expr {
        if !expr_yields(&e) {
            return e;
        }
        match e {
            Expr::Yield(a, delegate) => {
                let a = a.map(|a| Box::new(self.expr(*a, pre)));
                let t = self.tmp();
                pre.push(set(&t, Expr::Yield(a, delegate)));
                ident(&t)
            }
            Expr::Template(strs, subs) => Expr::Template(strs, self.ordered(subs, pre)),
            Expr::Seq(xs) => {
                let n = xs.len();
                let mut out = Vec::new();
                for (i, x) in xs.into_iter().enumerate() {
                    let x = self.expr(x, pre);
                    if i + 1 == n {
                        out.push(x);
                    } else {
                        pre.push(Stmt::Expr(x));
                    }
                }
                Expr::Seq(out)
            }
            Expr::SuperMember(x) => Expr::SuperMember(Box::new(self.expr(*x, pre))),
            Expr::Spread(x) => Expr::Spread(Box::new(self.expr(*x, pre))),
            Expr::Unary(op, x) => Expr::Unary(op, Box::new(self.expr(*x, pre))),
            Expr::OptChain(x) => self.opt_chain(*x, pre),
            Expr::Class(c) => Expr::Class(self.class(c, pre)),
            Expr::Typeof(x) => Expr::Typeof(Box::new(self.expr(*x, pre))),
            // `delete` of a value that is not a reference is `true` once it is evaluated.
            Expr::Delete(x) if !is_reference(&x) => {
                Expr::Seq(vec![self.expr(*x, pre), Expr::Bool(true)])
            }
            Expr::Delete(x) => Expr::Delete(Box::new(self.expr(*x, pre))),
            Expr::PrivateIn(n, x) => Expr::PrivateIn(n, Box::new(self.expr(*x, pre))),
            Expr::Await(x) => Expr::Await(Box::new(self.expr(*x, pre))),
            Expr::At(p, x) => Expr::At(p, Box::new(self.expr(*x, pre))),
            Expr::SuperCall(xs, pos) => Expr::SuperCall(self.spread_ordered(xs, pre), pos),
            Expr::Array(items) => {
                let present: Vec<bool> = items.iter().map(Option::is_some).collect();
                let xs = self.spread_ordered(items.into_iter().flatten().collect(), pre);
                let mut xs = xs.into_iter();
                Expr::Array(present.into_iter().map(|p| if p { xs.next() } else { None }).collect())
            }
            Expr::Object(props) => {
                let last = props.iter().rposition(|p| match p {
                    ObjProp::Value(k, v) => key_yields(k) || expr_yields(v),
                    ObjProp::Accessor(k, ..) => key_yields(k),
                    ObjProp::Spread(v) => expr_yields(v),
                });
                let mut out = Vec::new();
                for (i, p) in props.into_iter().enumerate() {
                    let before = last.is_some_and(|l| i < l);
                    let upto = last.is_some_and(|l| i <= l);
                    out.push(match p {
                        ObjProp::Value(k, v) if upto => {
                            let k = self.key(k, pre);
                            let v = self.expr(v, pre);
                            ObjProp::Value(k, if before { self.spill(v, pre) } else { v })
                        }
                        ObjProp::Accessor(k, kind, f) if upto => {
                            ObjProp::Accessor(self.key(k, pre), kind, f)
                        }
                        ObjProp::Spread(v) if upto => {
                            let v = self.expr(v, pre);
                            ObjProp::Spread(if before { self.spill(v, pre) } else { v })
                        }
                        p => p,
                    });
                }
                Expr::Object(out)
            }
            Expr::Update { op, prefix, target } => {
                Expr::Update { op, prefix, target: Box::new(self.target_expr(*target, pre)) }
            }
            Expr::Binary(op, a, b) => {
                let a = self.expr(*a, pre);
                let a = if expr_yields(&b) { self.spill(a, pre) } else { a };
                Expr::Binary(op, Box::new(a), Box::new(self.expr(*b, pre)))
            }
            Expr::Logical(op, a, b) => {
                let a = self.expr(*a, pre);
                if !expr_yields(&b) {
                    return Expr::Logical(op, Box::new(a), b);
                }
                let t = self.tmp();
                pre.push(set(&t, a));
                let test = match op {
                    "&&" => ident(&t),
                    "||" => not(ident(&t)),
                    _ => Expr::Binary("==", Box::new(ident(&t)), Box::new(Expr::Null)),
                };
                let mut inner = Vec::new();
                let b = self.expr(*b, &mut inner);
                inner.push(set(&t, b));
                pre.push(mark(Stmt::If(test, Box::new(block(inner)), None)));
                ident(&t)
            }
            Expr::Cond(c, a, b) => {
                let c = self.expr(*c, pre);
                let t = self.tmp();
                let mut on_a = Vec::new();
                let a = self.expr(*a, &mut on_a);
                on_a.push(set(&t, a));
                let mut on_b = Vec::new();
                let b = self.expr(*b, &mut on_b);
                on_b.push(set(&t, b));
                pre.push(mark(Stmt::If(c, Box::new(block(on_a)), Some(Box::new(block(on_b))))));
                ident(&t)
            }
            Expr::Member { obj, prop, optional, pos } => {
                let prop_yields = matches!(&*prop, MemberProp::Computed(k) if expr_yields(k));
                let obj = self.expr(*obj, pre);
                let obj = if prop_yields { self.spill(obj, pre) } else { obj };
                let prop = match *prop {
                    MemberProp::Computed(k) => MemberProp::Computed(self.expr(k, pre)),
                    p => p,
                };
                Expr::Member { obj: Box::new(obj), prop: Box::new(prop), optional, pos }
            }
            Expr::Call { callee, args, optional, pos } => {
                let args_yield = args.iter().any(expr_yields);
                let callee = self.callee(*callee, args_yield, pre);
                let args = if args_yield { self.spread_ordered(args, pre) } else { args };
                Expr::Call { callee: Box::new(callee), args, optional, pos }
            }
            Expr::New { callee, args, pos } => {
                let callee = self.expr(*callee, pre);
                let callee =
                    if args.iter().any(expr_yields) { self.spill(callee, pre) } else { callee };
                Expr::New { callee: Box::new(callee), args: self.spread_ordered(args, pre), pos }
            }
            Expr::Assign { op, target, value } => self.assign(op, *target, *value, pre),
            other => other,
        }
    }

    /// A callee whose arguments yield: its object and key are kept, so that the call still
    /// gets its `this`.
    fn callee(&mut self, callee: Expr, args_yield: bool, pre: &mut Vec<Stmt>) -> Expr {
        match callee {
            Expr::Member { obj, prop, optional, pos } if args_yield || expr_yields(&obj) => {
                let obj = self.expr(*obj, pre);
                let obj = self.spill(obj, pre);
                let prop = match *prop {
                    MemberProp::Computed(k) => {
                        let k = self.expr(k, pre);
                        MemberProp::Computed(self.spill(k, pre))
                    }
                    p => p,
                };
                Expr::Member { obj: Box::new(obj), prop: Box::new(prop), optional, pos }
            }
            Expr::SuperMember(_) | Expr::Ident(..) => self.expr(callee, pre),
            c => {
                let c = self.expr(c, pre);
                if args_yield { self.spill(c, pre) } else { c }
            }
        }
    }

    /// An assignment target (a name or a member) whose object and key are fixed before what
    /// follows yields.
    fn target_expr(&mut self, e: Expr, pre: &mut Vec<Stmt>) -> Expr {
        match e {
            Expr::Member { obj, prop, optional, pos } => {
                let obj = self.expr(*obj, pre);
                let obj = self.spill(obj, pre);
                let prop = match *prop {
                    MemberProp::Computed(k) => {
                        let k = self.expr(k, pre);
                        MemberProp::Computed(self.spill(k, pre))
                    }
                    p => p,
                };
                Expr::Member { obj: Box::new(obj), prop: Box::new(prop), optional, pos }
            }
            e => e,
        }
    }

    fn assign(
        &mut self,
        op: &'static str,
        target: Pattern,
        value: Expr,
        pre: &mut Vec<Stmt>,
    ) -> Expr {
        let target = match target {
            Pattern::Expr(e) => Pattern::Expr(Box::new(self.target_expr(*e, pre))),
            p @ (Pattern::Array(..) | Pattern::Object(..)) if pattern_yields(&p) => {
                let v = self.expr(value, pre);
                let t = self.tmp();
                pre.push(set(&t, v));
                self.destructure(p, &t, None, pre);
                return ident(&t);
            }
            p => p,
        };
        let read = match &target {
            Pattern::Name(n) => Some(ident(n)),
            Pattern::Expr(e) => Some(clone_target(e)),
            _ => None,
        };
        let value_yields = expr_yields(&value);
        match (op, read) {
            ("&&=" | "||=" | "??=", Some(read)) if value_yields => {
                let test = match op {
                    "&&=" => read.clone_shallow(),
                    "||=" => not(read.clone_shallow()),
                    _ => Expr::Binary("==", Box::new(read.clone_shallow()), Box::new(Expr::Null)),
                };
                let mut inner = Vec::new();
                let v = self.expr(value, &mut inner);
                inner.push(Stmt::Expr(Expr::Assign {
                    op: "=",
                    target: Box::new(target),
                    value: Box::new(v),
                }));
                pre.push(mark(Stmt::If(test, Box::new(block(inner)), None)));
                read
            }
            (op, Some(read)) if op != "=" && value_yields => {
                let old = self.spill(read, pre);
                let v = self.expr(value, pre);
                let base = &op[..op.len() - 1];
                Expr::Assign {
                    op: "=",
                    target: Box::new(target),
                    value: Box::new(Expr::Binary(binary_op(base), Box::new(old), Box::new(v))),
                }
            }
            _ => {
                let v = self.expr(value, pre);
                Expr::Assign { op, target: Box::new(target), value: Box::new(v) }
            }
        }
    }

    fn stmts(&mut self, body: Vec<Stmt>) -> Vec<Stmt> {
        let mut out = Vec::new();
        for s in body {
            self.stmt(s, &mut out);
        }
        out
    }

    fn sub(&mut self, s: Stmt) -> Stmt {
        let mut out = Vec::new();
        self.stmt(s, &mut out);
        if out.len() == 1 { out.pop().unwrap_or(Stmt::Empty) } else { block(out) }
    }

    pub(crate) fn stmt(&mut self, s: Stmt, out: &mut Vec<Stmt>) {
        // A `yield` statement keeps no position mark: its own evaluation sets none.
        let s = match s {
            Stmt::Expr(Expr::At(_, e)) if matches!(*e, Expr::Yield(..)) => Stmt::Expr(*e),
            s => s,
        };
        match s {
            Stmt::Expr(Expr::Yield(a, d)) => {
                let a = a.map(|a| Box::new(self.expr(*a, out)));
                out.push(Stmt::Suspends(Box::new(Stmt::Expr(Expr::Yield(a, d)))));
            }
            Stmt::Expr(e) if expr_yields(&e) => {
                let e = self.expr(e, out);
                if !matches!(e, Expr::Ident(..)) {
                    out.push(Stmt::Expr(e));
                }
            }
            Stmt::Decl(kind, decls)
                if decls
                    .iter()
                    .any(|(p, d)| pattern_yields(p) || d.as_ref().is_some_and(expr_yields)) =>
            {
                for (p, d) in decls {
                    let d = d.map(|d| self.expr(d, out));
                    if pattern_yields(&p) {
                        let t = self.tmp();
                        out.push(set(&t, d.unwrap_or(Expr::Undefined)));
                        self.destructure(p, &t, Some(kind), out);
                    } else {
                        out.push(Stmt::Decl(kind, vec![(p, d)]));
                    }
                }
            }
            Stmt::Return(Some(e)) if expr_yields(&e) => {
                let e = self.expr(e, out);
                out.push(Stmt::Return(Some(e)));
            }
            Stmt::Throw(e, pos) if expr_yields(&e) => {
                let e = self.expr(e, out);
                out.push(Stmt::Throw(e, pos));
            }
            Stmt::If(c, a, b) => {
                let c = self.expr(c, out);
                let a = self.sub(*a);
                let b = b.map(|b| Box::new(self.sub(*b)));
                out.push(mark(Stmt::If(c, Box::new(a), b)));
            }
            Stmt::Block(b) => out.push(block(self.stmts(b))),
            Stmt::While(c, body) => {
                let body = self.sub(*body);
                if expr_yields(&c) {
                    // while (true) { <c>; if (!c) break; body }
                    let mut pre = Vec::new();
                    let c = self.expr(c, &mut pre);
                    pre.push(Stmt::If(not(c), Box::new(Stmt::Break(None)), None));
                    pre.push(body);
                    out.push(mark(Stmt::While(Expr::Bool(true), Box::new(block(pre)))));
                } else {
                    out.push(mark(Stmt::While(c, Box::new(body))));
                }
            }
            Stmt::DoWhile(body, c) => {
                let body = self.sub(*body);
                if expr_yields(&c) {
                    // first = true; while (true) { if (!first) { <c>; if (!c) break; }
                    // first = false; body }
                    let first = self.tmp();
                    out.push(set(&first, Expr::Bool(true)));
                    let mut check = Vec::new();
                    let c = self.expr(c, &mut check);
                    check.push(Stmt::If(not(c), Box::new(Stmt::Break(None)), None));
                    let inner = vec![
                        mark(Stmt::If(not(ident(&first)), Box::new(block(check)), None)),
                        set(&first, Expr::Bool(false)),
                        body,
                    ];
                    out.push(mark(Stmt::While(Expr::Bool(true), Box::new(block(inner)))));
                } else {
                    out.push(mark(Stmt::DoWhile(Box::new(body), c)));
                }
            }
            Stmt::For(init, test, update, body) => self.for_stmt(init, test, update, *body, out),
            Stmt::ForIn { decl, target, of, obj, body } => {
                let obj = self.expr(obj, out);
                let (target, body) = if pattern_yields(&target) {
                    // for (x of …) { <target> = x; body }
                    let t = self.tmp();
                    let mut inner = Vec::new();
                    self.destructure(target, &t, decl, &mut inner);
                    self.stmt(*body, &mut inner);
                    (Pattern::Name(t), Box::new(block(inner)))
                } else {
                    (target, Box::new(self.sub(*body)))
                };
                out.push(mark(Stmt::ForIn { decl, target, of, obj, body }));
            }
            Stmt::Try(a, c, f) => {
                let a = self.stmts(a);
                let c = c.map(|(p, b)| match p {
                    Some(p) if pattern_yields(&p) => {
                        // catch (e) { let <p> = e; … }
                        let t = self.tmp();
                        let mut inner = Vec::new();
                        self.destructure(p, &t, Some(DeclKind::Let), &mut inner);
                        inner.extend(self.stmts(b));
                        (Some(Pattern::Name(t)), inner)
                    }
                    p => (p, self.stmts(b)),
                });
                let f = f.map(|f| self.stmts(f));
                out.push(mark(Stmt::Try(a, c, f)));
            }
            Stmt::Switch(d, cases) => self.switch(d, cases, out),
            Stmt::Class(n, c) => {
                let c = self.class(c, out);
                out.push(Stmt::Class(n, c));
            }
            Stmt::Labeled(l, body) => {
                let body = self.sub(*body);
                out.push(mark(Stmt::Labeled(l, Box::new(body))));
            }
            Stmt::With(o, body) => {
                let o = self.expr(o, out);
                let body = self.sub(*body);
                out.push(mark(Stmt::With(o, Box::new(body))));
            }
            s => out.push(s),
        }
    }

    fn for_stmt(
        &mut self,
        init: Option<ForInit>,
        test: Option<Expr>,
        update: Option<Expr>,
        body: Stmt,
        out: &mut Vec<Stmt>,
    ) {
        let body = self.sub(body);
        let header_yields =
            test.as_ref().is_some_and(expr_yields) || update.as_ref().is_some_and(expr_yields);
        let init_yields = match &init {
            Some(ForInit::Expr(e)) => expr_yields(e),
            Some(ForInit::Decl(_, d)) => {
                d.iter().any(|(p, e)| pattern_yields(p) || e.as_ref().is_some_and(expr_yields))
            }
            None => false,
        };
        if !header_yields && !init_yields {
            out.push(mark(Stmt::For(init, test, update, Box::new(body))));
            return;
        }
        // { init; first = true; while (true) { if (!first) { <update> } first = false;
        //   <test>; if (!test) break; body } }
        let mut outer = Vec::new();
        match init {
            Some(ForInit::Expr(e)) => self.stmt(Stmt::Expr(e), &mut outer),
            Some(ForInit::Decl(k, d)) => self.stmt(Stmt::Decl(k, d), &mut outer),
            None => {}
        }
        let first = self.tmp();
        outer.push(set(&first, Expr::Bool(true)));
        let mut inner = Vec::new();
        if let Some(u) = update {
            let mut upd = Vec::new();
            self.stmt(Stmt::Expr(u), &mut upd);
            inner.push(mark(Stmt::If(not(ident(&first)), Box::new(block(upd)), None)));
        }
        inner.push(set(&first, Expr::Bool(false)));
        if let Some(t) = test {
            let t = self.expr(t, &mut inner);
            inner.push(Stmt::If(not(t), Box::new(Stmt::Break(None)), None));
        }
        inner.push(body);
        outer.push(mark(Stmt::While(Expr::Bool(true), Box::new(block(inner)))));
        out.push(block(outer));
    }

    fn switch(&mut self, d: Expr, cases: Vec<(Option<Expr>, Vec<Stmt>)>, out: &mut Vec<Stmt>) {
        let d = self.expr(d, out);
        let tests_yield = cases.iter().any(|(t, _)| t.as_ref().is_some_and(expr_yields));
        if !tests_yield {
            let cases = cases.into_iter().map(|(t, b)| (t, self.stmts(b))).collect();
            out.push(mark(Stmt::Switch(d, cases)));
            return;
        }
        // d' = d; m = -1; for each case i: if (m === -1) { <t>; if (d' === t) m = i }
        // switch (m) { case i: … default: … }
        let dv = self.spill(d, out);
        let m = self.tmp();
        out.push(set(&m, Expr::Num(-1.0)));
        let mut new_cases = Vec::new();
        for (i, (t, b)) in cases.into_iter().enumerate() {
            let idx = Expr::Num(i as f64);
            if let Some(t) = t {
                let mut check = Vec::new();
                let t = self.expr(t, &mut check);
                let eq = Expr::Binary("===", Box::new(dv.clone_shallow()), Box::new(t));
                check.push(Stmt::If(eq, Box::new(set(&m, idx.clone_shallow())), None));
                let unset = Expr::Binary("===", Box::new(ident(&m)), Box::new(Expr::Num(-1.0)));
                out.push(mark(Stmt::If(unset, Box::new(block(check)), None)));
                new_cases.push((Some(idx), self.stmts(b)));
            } else {
                new_cases.push((None, self.stmts(b)));
            }
        }
        out.push(mark(Stmt::Switch(ident(&m), new_cases)));
    }
}

/// `+=` without its `=`, as the operator the parser uses.
fn binary_op(base: &str) -> &'static str {
    const OPS: [&str; 12] = ["+", "-", "*", "/", "%", "**", "<<", ">>", ">>>", "&", "|", "^"];
    OPS.iter().find(|o| **o == base).copied().unwrap_or("+")
}

/// A read of an assignment target whose parts are already temporaries or names.
fn clone_target(e: &Expr) -> Expr {
    e.clone_shallow()
}

impl Expr {
    /// A copy of a simple expression: a literal, a name, `this` or a member of those. The
    /// rewriting only copies these.
    pub fn clone_shallow(&self) -> Expr {
        match self {
            Expr::Num(n) => Expr::Num(*n),
            Expr::BigInt(b) => Expr::BigInt(b.clone()),
            Expr::Str(s) => Expr::Str(s.clone()),
            Expr::Bool(b) => Expr::Bool(*b),
            Expr::Null => Expr::Null,
            Expr::Ident(n, p) => Expr::Ident(n.clone(), *p),
            Expr::This => Expr::This,
            Expr::Member { obj, prop, optional, pos } => Expr::Member {
                obj: Box::new(obj.clone_shallow()),
                prop: Box::new(match &**prop {
                    MemberProp::Name(n) => MemberProp::Name(n.clone()),
                    MemberProp::Computed(k) => MemberProp::Computed(k.clone_shallow()),
                    MemberProp::Private(n) => MemberProp::Private(n.clone()),
                }),
                optional: *optional,
                pos: *pos,
            },
            Expr::Func(f) => Expr::Func(f.clone()),
            Expr::At(p, x) => Expr::At(*p, Box::new(x.clone_shallow())),
            _ => Expr::Undefined,
        }
    }
}

// Running.

/// What `next`, `throw` or `return` sends into a suspended generator.
pub enum Resume {
    Next(Val),
    Throw(Val),
    Return(Val),
}

/// What a marked statement keeps when a `yield` inside it suspends the generator.
enum Saved {
    /// A list of statements: the one running and the scope.
    List { idx: usize, env: EnvRef },
    /// The branch of an `if`.
    Branch(bool),
    /// A loop: its scope (a `for`'s scope of this iteration).
    Loop { env: EnvRef, per_iteration: bool },
    /// A `for…in` or `for…of`: the iteration and the scope of the iteration.
    ForIn { rec: IterRec, env: EnvRef },
    /// A `try`: which part runs, the scope of the `catch` and the completion `finally` keeps.
    Try { phase: u8, env: Option<EnvRef>, pending: Option<Result<(), Ctrl>> },
    /// A `switch`: the case whose statements run, the statement and the scope.
    Switch { case: usize, idx: usize, env: EnvRef },
    /// `yield*`: the inner iterator.
    Delegate { iter: Val },
}

/// The state of one run of a generator's body.
struct Run {
    saved: Vec<Saved>,
    resuming: bool,
    sent: Option<Resume>,
    /// The value of the `yield` that suspended, or the inner result of a `yield*`.
    yielded: Option<(Val, bool)>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Status {
    Start,
    Suspended,
    Running,
    Done,
}

/// A generator object's state.
pub struct GenState {
    def: Rc<FuncDef>,
    env: EnvRef,
    status: Status,
    saved: Vec<Saved>,
}

impl std::fmt::Debug for GenState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GenState").field("status", &self.status).finish_non_exhaustive()
    }
}

/// A generator for a call of `def` whose scope `env` has its parameters bound.
pub fn make(it: &mut Interp, fo: &ObjRef, def: &Rc<FuncDef>, env: EnvRef) -> R {
    let fallback = if def.is_async { "AsyncGenerator" } else { "Generator" };
    let proto = match it.get(&Val::Obj(fo.clone()), "prototype")? {
        Val::Obj(p) => p,
        _ => it.realm.proto(fallback),
    };
    if def.is_async {
        return Ok(Val::Obj(it.alloc(Some(proto), Kind::AsyncGenerator)?));
    }
    let st = GenState { def: def.clone(), env, status: Status::Start, saved: Vec::new() };
    Ok(Val::Obj(it.alloc(Some(proto), Kind::Generator(Rc::new(RefCell::new(st))))?))
}

fn result(it: &mut Interp, value: Val, done: bool) -> R {
    let o = it.object()?;
    it.grow(&o, 2)?;
    {
        let mut b = o.borrow_mut();
        b.props.push(("value".into(), Prop::data(value)));
        b.props.push(("done".into(), Prop::data(Val::Bool(done))));
    }
    Ok(Val::Obj(o))
}

fn state(it: &mut Interp, this: &Val, name: &str) -> R<Rc<RefCell<GenState>>> {
    if let Val::Obj(o) = this
        && let Kind::Generator(st) = &o.borrow().kind
    {
        return Ok(st.clone());
    }
    it.type_error(&format!(
        "{name} method called on incompatible {}",
        crate::builtins::class_name(this)
    ))
}

fn resume(it: &mut Interp, this: &Val, how: Resume, name: &str) -> R {
    let st = state(it, this, name)?;
    let status = st.borrow().status;
    match (status, how) {
        (Status::Running, _) => it.type_error("already executing generator"),
        (Status::Done, Resume::Throw(v)) => Err(Ctrl::Throw(v)),
        (Status::Done, Resume::Return(v)) => result(it, v, true),
        (Status::Done, Resume::Next(_)) => result(it, Val::Undef, true),
        (Status::Start, Resume::Throw(v)) => {
            st.borrow_mut().status = Status::Done;
            Err(Ctrl::Throw(v))
        }
        (Status::Start, Resume::Return(v)) => {
            st.borrow_mut().status = Status::Done;
            result(it, v, true)
        }
        (_, how) => {
            let (def, env, saved) = {
                let mut b = st.borrow_mut();
                b.status = Status::Running;
                (b.def.clone(), b.env.clone(), std::mem::take(&mut b.saved))
            };
            let mut run = Run {
                saved,
                resuming: status == Status::Suspended,
                sent: Some(how),
                yielded: None,
            };
            let outer = std::mem::replace(&mut it.strict, def.strict);
            // The body runs as a frame of its own.
            let r = it.nested(1, 0, false, |it| it.exec_list(&def.body, &env, &mut run, false));
            it.strict = outer;
            let mut b = st.borrow_mut();
            b.status = Status::Done;
            match r {
                Err(Ctrl::Suspend) => {
                    b.status = Status::Suspended;
                    b.saved = run.saved;
                    drop(b);
                    match run.yielded.take() {
                        Some((v, false)) => result(it, v, false),
                        Some((raw, true)) => Ok(raw),
                        None => result(it, Val::Undef, false),
                    }
                }
                Ok(()) | Err(Ctrl::Break(_) | Ctrl::Continue(_)) => {
                    drop(b);
                    result(it, Val::Undef, true)
                }
                Err(Ctrl::Return(v)) => {
                    drop(b);
                    result(it, v, true)
                }
                Err(e) => Err(e),
            }
        }
    }
}

fn gen_next(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    resume(it, this, Resume::Next(arg(a, 0)), "next")
}

fn gen_return(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    resume(it, this, Resume::Return(arg(a, 0)), "return")
}

fn gen_throw(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    resume(it, this, Resume::Throw(arg(a, 0)), "throw")
}

fn async_gen_step(_: &mut Interp, _: &Val, _: &[Val]) -> R {
    Err(Ctrl::Fatal(crate::interp::Fatal::Trap))
}

fn iter_self(_: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(this.clone())
}

/// `%GeneratorFunction.prototype%`, `%GeneratorPrototype%` and their async forms.
pub fn install(it: &mut Interp) -> R<()> {
    let fproto = it.realm.function.clone();
    for (fname, pname, ctor_name, step) in [
        ("GeneratorFunction", "Generator", "GeneratorFunction", false),
        ("AsyncGeneratorFunction", "AsyncGenerator", "AsyncGeneratorFunction", true),
    ] {
        let gf = it.alloc(Some(fproto.clone()), Kind::Ordinary)?;
        // %AsyncIteratorPrototype%, with only `Symbol.asyncIterator`, under the async one.
        let above = if step {
            let aip = it.alloc(Some(it.realm.object.clone()), Kind::Ordinary)?;
            let f = it.native("[Symbol.asyncIterator]", iter_self, 0)?;
            let key = crate::builtins::well_known("asyncIterator");
            it.define(&aip, &key, Prop::hidden(Val::Obj(f)))?;
            aip
        } else {
            it.realm.iter.clone()
        };
        let gp = it.alloc(Some(above), Kind::Ordinary)?;
        let c = it.native(ctor_name, crate::builtins::function_ctor, 1)?;
        it.define(&c, "prototype", Prop { writable: false, ..Prop::hidden(Val::Obj(gf.clone())) })?;
        // Read-only and configurable (ECMA-262 §27.3.3, §27.5.1).
        let fixed = |v: ObjRef| Prop { writable: false, ..Prop::hidden(Val::Obj(v)) };
        it.define(&gf, "constructor", fixed(c))?;
        it.define(&gf, "prototype", fixed(gp.clone()))?;
        it.define(&gp, "constructor", fixed(gf.clone()))?;
        if step {
            for m in ["next", "return", "throw"] {
                method(it, &gp, m, async_gen_step, 1)?;
            }
        } else {
            method(it, &gp, "next", gen_next, 1)?;
            method(it, &gp, "return", gen_return, 1)?;
            method(it, &gp, "throw", gen_throw, 1)?;
        }
        crate::builtins::to_string_tag(it, &gp, pname)?;
        crate::builtins::to_string_tag(it, &gf, fname)?;
        it.realm.protos.push((fname_static(fname), gf));
        it.realm.protos.push((fname_static(pname), gp));
    }
    Ok(())
}

fn fname_static(n: &str) -> &'static str {
    match n {
        "GeneratorFunction" => "GeneratorFunction",
        "AsyncGeneratorFunction" => "AsyncGeneratorFunction",
        "Generator" => "Generator",
        _ => "AsyncGenerator",
    }
}

impl Interp<'_> {
    /// Runs a list of statements of a generator, from where it suspended when resuming.
    fn exec_list(&mut self, body: &[Stmt], env: &EnvRef, run: &mut Run, scope: bool) -> R<()> {
        let (start, env) = if run.resuming {
            match run.saved.pop() {
                Some(Saved::List { idx, env }) => (idx, env),
                _ => return self.type_error("generator state lost"),
            }
        } else if scope && needs_scope(body) {
            let s = self.new_env(Some(env.clone()));
            self.hoist(body, &s, false)?;
            (0, s)
        } else {
            (0, env.clone())
        };
        for (idx, s) in body.iter().enumerate().skip(start) {
            match self.exec_gen(s, &env, run, &[]) {
                Err(Ctrl::Suspend) => {
                    run.saved.push(Saved::List { idx, env });
                    return Err(Ctrl::Suspend);
                }
                r => r?,
            }
        }
        Ok(())
    }

    fn exec_gen(&mut self, s: &Stmt, env: &EnvRef, run: &mut Run, labels: &[Name]) -> R<()> {
        let Stmt::Suspends(inner) = s else { return self.exec(s, env) };
        if !run.resuming {
            self.step()?;
        }
        match &**inner {
            Stmt::Expr(Expr::Yield(a, d)) => self.yield_stmt(a.as_deref(), *d, None, env, run),
            Stmt::Expr(Expr::Assign { target, value, .. }) => {
                let (Pattern::Name(t), Expr::Yield(a, d)) = (&**target, &**value) else {
                    return self.exec(inner, env);
                };
                self.yield_stmt(a.as_deref(), *d, Some(t), env, run)
            }
            Stmt::Block(body) => self.exec_list(body, env, run, true),
            Stmt::If(c, a, b) => {
                let branch = if run.resuming {
                    match run.saved.pop() {
                        Some(Saved::Branch(b)) => b,
                        _ => return self.type_error("generator state lost"),
                    }
                } else {
                    self.eval(c, env)?.truthy()
                };
                let r = match (branch, b) {
                    (true, _) => self.exec_gen(a, env, run, &[]),
                    (false, Some(b)) => self.exec_gen(b, env, run, &[]),
                    (false, None) => Ok(()),
                };
                if matches!(r, Err(Ctrl::Suspend)) {
                    run.saved.push(Saved::Branch(branch));
                }
                r
            }
            Stmt::While(c, body) => self.gen_loop(Some(c), None, body, env, run, labels, false),
            Stmt::DoWhile(body, c) => self.gen_loop(Some(c), None, body, env, run, labels, true),
            Stmt::For(init, test, update, body) => {
                let (scope, per_iteration) = if run.resuming {
                    match run.saved.pop() {
                        Some(Saved::Loop { env, per_iteration }) => (env, per_iteration),
                        _ => return self.type_error("generator state lost"),
                    }
                } else {
                    self.for_init(init.as_ref(), env)?
                };
                run.saved.push(Saved::Loop { env: scope, per_iteration });
                self.gen_for(test.as_ref(), update.as_ref(), body, env, run, labels)
            }
            Stmt::ForIn { decl, target, of, obj, body } => {
                self.gen_for_in(*decl, target, *of, obj, body, env, run, labels)
            }
            Stmt::Try(body, catch, finally) => {
                self.gen_try(body, catch.as_ref(), finally.as_ref(), env, run)
            }
            Stmt::Switch(d, cases) => self.gen_switch(d, cases, env, run),
            Stmt::Labeled(l, body) => {
                // `a: b: while …`: a `continue` to any of the labels goes on with the loop.
                let mut ls = labels.to_vec();
                ls.push(l.clone());
                match self.exec_gen(body, env, run, &ls) {
                    Err(Ctrl::Break(Some(x))) if x == *l => Ok(()),
                    r => r,
                }
            }
            Stmt::With(o, body) => {
                let scope = if run.resuming {
                    match run.saved.pop() {
                        Some(Saved::Loop { env, .. }) => env,
                        _ => return self.type_error("generator state lost"),
                    }
                } else {
                    let o = self.eval(o, env)?;
                    self.with_scope(&o, env)?
                };
                let r = self.exec_gen(body, &scope, run, &[]);
                if matches!(r, Err(Ctrl::Suspend)) {
                    run.saved.push(Saved::Loop { env: scope, per_iteration: false });
                }
                r
            }
            other => self.exec(other, env),
        }
    }

    /// `yield a` (or `yield* a`), its value given to `target`.
    fn yield_stmt(
        &mut self,
        a: Option<&Expr>,
        delegate: bool,
        target: Option<&Name>,
        env: &EnvRef,
        run: &mut Run,
    ) -> R<()> {
        if delegate {
            return self.yield_star(a, target, env, run);
        }
        if run.resuming {
            run.resuming = false;
            let v = match run.sent.take() {
                Some(Resume::Next(v)) => v,
                Some(Resume::Throw(v)) => return Err(Ctrl::Throw(v)),
                Some(Resume::Return(v)) => return Err(Ctrl::Return(v)),
                None => Val::Undef,
            };
            if let Some(t) = target {
                self.bind(&Pattern::Name(t.clone()), v, env, None)?;
            }
            return Ok(());
        }
        let v = match a {
            Some(e) => self.eval(e, env)?,
            None => Val::Undef,
        };
        run.yielded = Some((v, false));
        Err(Ctrl::Suspend)
    }

    fn yield_star(
        &mut self,
        a: Option<&Expr>,
        target: Option<&Name>,
        env: &EnvRef,
        run: &mut Run,
    ) -> R<()> {
        let (iter, received) = if run.resuming {
            run.resuming = false;
            let iter = match run.saved.pop() {
                Some(Saved::Delegate { iter }) => iter,
                _ => return self.type_error("generator state lost"),
            };
            (iter, run.sent.take().unwrap_or(Resume::Next(Val::Undef)))
        } else {
            let v = match a {
                Some(e) => self.eval(e, env)?,
                None => Val::Undef,
            };
            let f = self.get(&v, "@@Symbol.iterator:0")?;
            if !f.is_callable() {
                return self.type_error(&format!("{} is not iterable", self.show(&v)));
            }
            (self.call(&f, &v, &[])?, Resume::Next(Val::Undef))
        };
        let (method, v, is_return) = match received {
            Resume::Next(v) => ("next", v, false),
            Resume::Throw(v) => ("throw", v, false),
            Resume::Return(v) => ("return", v, true),
        };
        let f = self.get(&iter, method)?;
        if !f.is_callable() {
            if is_return {
                return Err(Ctrl::Return(v));
            }
            self.iter_close(&IterRec::Proto { it: iter.clone(), next: Val::Undef })?;
            return self.type_error("iterator does not have a 'throw' method");
        }
        let r = self.call(&f, &iter, &[v])?;
        if !matches!(r, Val::Obj(_)) {
            return self.type_error(&format!("iterator.{method}() returned a non-object value"));
        }
        if self.get(&r, "done")?.truthy() {
            let value = self.get(&r, "value")?;
            if is_return {
                return Err(Ctrl::Return(value));
            }
            if let Some(t) = target {
                self.bind(&Pattern::Name(t.clone()), value, env, None)?;
            }
            return Ok(());
        }
        run.yielded = Some((r, true));
        run.saved.push(Saved::Delegate { iter });
        Err(Ctrl::Suspend)
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_loop(
        &mut self,
        test: Option<&Expr>,
        _update: Option<&Expr>,
        body: &Stmt,
        env: &EnvRef,
        run: &mut Run,
        labels: &[Name],
        do_while: bool,
    ) -> R<()> {
        let mut first = do_while;
        if run.resuming {
            match run.saved.pop() {
                Some(Saved::Loop { .. }) => first = true,
                _ => return self.type_error("generator state lost"),
            }
        }
        loop {
            if !first
                && let Some(t) = test
                && !self.eval(t, env)?.truthy()
            {
                return Ok(());
            }
            match loop_result(self.exec_gen(body, env, run, &[]), labels) {
                Err(Ctrl::Suspend) => {
                    run.saved.push(Saved::Loop { env: env.clone(), per_iteration: false });
                    return Err(Ctrl::Suspend);
                }
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => return Err(e),
            }
            if do_while
                && let Some(t) = test
                && !self.eval(t, env)?.truthy()
            {
                return Ok(());
            }
            first = do_while;
        }
    }

    /// A `for` whose scope is the last saved [`Saved::Loop`].
    fn gen_for(
        &mut self,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
        env: &EnvRef,
        run: &mut Run,
        labels: &[Name],
    ) -> R<()> {
        // A loop that suspended saved its state last.
        let (mut scope, per_iteration) = match run.saved.pop() {
            Some(Saved::Loop { env, per_iteration }) => (env, per_iteration),
            _ => unreachable!("a loop resumed without its state"),
        };
        let mut skip_test = run.resuming;
        loop {
            if !skip_test
                && let Some(t) = test
                && !self.eval(t, &scope)?.truthy()
            {
                return Ok(());
            }
            skip_test = false;
            match loop_result(self.exec_gen(body, &scope, run, &[]), labels) {
                Err(Ctrl::Suspend) => {
                    run.saved.push(Saved::Loop { env: scope, per_iteration });
                    return Err(Ctrl::Suspend);
                }
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(e) => return Err(e),
            }
            if per_iteration {
                let copy = self.new_env(Some(env.clone()));
                copy.borrow_mut().vars = scope.borrow().vars.clone();
                scope = copy;
            }
            if let Some(u) = update {
                self.eval(u, &scope)?;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn gen_for_in(
        &mut self,
        decl: Option<DeclKind>,
        target: &Pattern,
        of: bool,
        obj: &Expr,
        body: &Stmt,
        env: &EnvRef,
        run: &mut Run,
        labels: &[Name],
    ) -> R<()> {
        let mut resumed = None;
        let mut rec = if run.resuming {
            match run.saved.pop() {
                Some(Saved::ForIn { rec, env }) => {
                    resumed = Some(env);
                    rec
                }
                _ => return self.type_error("generator state lost"),
            }
        } else {
            let v = self.eval(obj, env)?;
            if of {
                let r = self.open_iter(&v);
                self.name_operands(r, &[(obj, &v)])?
            } else {
                IterRec::Items(self.for_in_keys(&v)?, 0)
            }
        };
        loop {
            let scope = match resumed.take() {
                Some(s) => s,
                None => {
                    let Some(x) = self.iter_step(&mut rec)? else { return Ok(()) };
                    let scope = self.new_env(Some(env.clone()));
                    match decl {
                        Some(DeclKind::Var) => self.bind(target, x, env, None)?,
                        Some(k) => self.bind(target, x, &scope, Some(k == DeclKind::Let))?,
                        None => self.bind(target, x, &scope, None)?,
                    }
                    scope
                }
            };
            match loop_result(self.exec_gen(body, &scope, run, &[]), labels) {
                Err(Ctrl::Suspend) => {
                    run.saved.push(Saved::ForIn { rec, env: scope });
                    return Err(Ctrl::Suspend);
                }
                Ok(false) => {}
                Ok(true) => return self.iter_close(&rec),
                Err(e) => {
                    let closed = self.iter_close(&rec);
                    if !matches!(e, Ctrl::Throw(_) | Ctrl::Fatal(_)) {
                        closed?;
                    }
                    return Err(e);
                }
            }
        }
    }

    fn gen_try(
        &mut self,
        body: &[Stmt],
        catch: Option<&(Option<Pattern>, Vec<Stmt>)>,
        finally: Option<&Vec<Stmt>>,
        env: &EnvRef,
        run: &mut Run,
    ) -> R<()> {
        let (mut phase, mut catch_env, mut pending) = if run.resuming {
            match run.saved.pop() {
                Some(Saved::Try { phase, env, pending }) => (phase, env, pending),
                _ => return self.type_error("generator state lost"),
            }
        } else {
            (0, None, None)
        };
        let mut r: Result<(), Ctrl> = pending.take().unwrap_or(Ok(()));
        if phase == 0 {
            r = self.exec_list(body, env, run, true);
            if matches!(r, Err(Ctrl::Suspend)) {
                run.saved.push(Saved::Try { phase, env: None, pending: None });
                return r;
            }
            let caught = match (catch, std::mem::replace(&mut r, Ok(()))) {
                (Some((param, _)), Err(Ctrl::Throw(x))) => Some((param, x)),
                (_, other) => {
                    r = other;
                    None
                }
            };
            if let Some((param, x)) = caught {
                self.out_of_memory.set(false);
                let scope = self.new_env(Some(env.clone()));
                if let Some(p) = param {
                    self.bind(p, x, &scope, Some(true))?;
                }
                catch_env = Some(scope);
                phase = 1;
            } else {
                phase = 2;
            }
        }
        if phase == 1 {
            // Phase 1 is only reached through a catch, which made its scope.
            let (handler, scope) = match (catch, catch_env.clone()) {
                (Some((_, handler)), Some(scope)) => (handler, scope),
                _ => unreachable!("a catch phase without its catch"),
            };
            r = self.exec_list(handler, &scope, run, true);
            if matches!(r, Err(Ctrl::Suspend)) {
                run.saved.push(Saved::Try { phase, env: catch_env, pending: None });
                return r;
            }
            phase = 2;
        }
        if phase == 2
            && let Some(f) = finally
        {
            let fr = self.exec_list(f, env, run, true);
            if matches!(fr, Err(Ctrl::Suspend)) {
                run.saved.push(Saved::Try { phase, env: None, pending: Some(r) });
                return fr;
            }
            fr?;
        }
        r
    }

    fn gen_switch(
        &mut self,
        d: &Expr,
        cases: &[(Option<Expr>, Vec<Stmt>)],
        env: &EnvRef,
        run: &mut Run,
    ) -> R<()> {
        let (start, mut first_idx, scope) = if run.resuming {
            match run.saved.pop() {
                Some(Saved::Switch { case, idx, env }) => (Some(case), idx, env),
                _ => return self.type_error("generator state lost"),
            }
        } else {
            let v = self.eval(d, env)?;
            let scope = self.new_env(Some(env.clone()));
            for (_, b) in cases {
                self.hoist(b, &scope, false)?;
            }
            let mut start = None;
            for (i, (t, _)) in cases.iter().enumerate() {
                if let Some(t) = t {
                    let x = self.eval(t, &scope)?;
                    if strict_eq(&v, &x) {
                        start = Some(i);
                        break;
                    }
                }
            }
            (start.or_else(|| cases.iter().position(|(t, _)| t.is_none())), 0, scope)
        };
        let Some(start) = start else { return Ok(()) };
        for (case, (_, body)) in cases.iter().enumerate().skip(start) {
            let from = std::mem::take(&mut first_idx);
            for (idx, s) in body.iter().enumerate().skip(from) {
                match self.exec_gen(s, &scope, run, &[]) {
                    Err(Ctrl::Suspend) => {
                        run.saved.push(Saved::Switch { case, idx, env: scope });
                        return Err(Ctrl::Suspend);
                    }
                    Err(Ctrl::Break(None)) => return Ok(()),
                    r => r?,
                }
            }
        }
        Ok(())
    }
}

fn needs_scope(body: &[Stmt]) -> bool {
    body.iter().any(|s| {
        matches!(
            s,
            Stmt::Decl(DeclKind::Let | DeclKind::Const, _) | Stmt::Class(..) | Stmt::Func(..)
        )
    })
}

/// A loop body's completion: `Ok(true)` to leave the loop.
fn loop_result(r: R<()>, labels: &[Name]) -> R<bool> {
    match r {
        Ok(()) => Ok(false),
        Err(Ctrl::Break(None)) => Ok(true),
        Err(Ctrl::Continue(None)) => Ok(false),
        Err(Ctrl::Continue(Some(l))) if labels.contains(&l) => Ok(false),
        Err(Ctrl::Break(Some(l))) if labels.contains(&l) => Ok(true),
        Err(e) => Err(e),
    }
}
