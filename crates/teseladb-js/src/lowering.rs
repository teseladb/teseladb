//! The parts of a generator's rewriting ([`crate::generators`]) where a `yield` sits inside
//! something the interpreter evaluates in one go: a destructuring pattern whose defaults or
//! computed keys yield, a class whose `extends` or computed keys yield, and an optional chain
//! whose keys or arguments after a `?.` yield (measured on a `refrouter` 9.0.4 on 02-11).

use std::rc::Rc;

use crate::ast::*;
use crate::generators::{Lower, block, ident, mark, set};

/// One link of an optional chain, from its base outwards.
enum Link {
    Member(MemberProp, bool, Pos),
    Call(Vec<Expr>, bool, Pos),
}

impl Link {
    fn optional(&self) -> bool {
        match self {
            Link::Member(_, o, _) | Link::Call(_, o, _) => *o,
        }
    }

    fn yields(&self) -> bool {
        match self {
            Link::Member(MemberProp::Computed(k), ..) => crate::generators::expr_yields(k),
            Link::Member(..) => false,
            Link::Call(args, ..) => args.iter().any(crate::generators::expr_yields),
        }
    }

    fn without_check(self) -> Link {
        match self {
            Link::Member(p, _, pos) => Link::Member(p, false, pos),
            Link::Call(a, _, pos) => Link::Call(a, false, pos),
        }
    }
}

/// The base of a chain and its links, base first.
fn split_chain(e: Expr) -> (Expr, Vec<Link>) {
    let mut links = Vec::new();
    let mut cur = e;
    loop {
        cur = match cur {
            Expr::Member { obj, prop, optional, pos } => {
                links.push(Link::Member(*prop, optional, pos));
                *obj
            }
            Expr::Call { callee, args, optional, pos } => {
                links.push(Link::Call(args, optional, pos));
                *callee
            }
            base => {
                links.reverse();
                return (base, links);
            }
        };
    }
}

fn rebuild(base: Expr, links: Vec<Link>) -> Expr {
    links.into_iter().fold(base, |e, l| match l {
        Link::Member(prop, optional, pos) => {
            Expr::Member { obj: Box::new(e), prop: Box::new(prop), optional, pos }
        }
        Link::Call(args, optional, pos) => Expr::Call { callee: Box::new(e), args, optional, pos },
    })
}

/// The last link when it is a member: the callee of a call that follows it.
fn pop_member(links: &mut Vec<Link>) -> Option<(MemberProp, bool, Pos)> {
    match links.pop() {
        Some(Link::Member(p, o, pos)) => Some((p, o, pos)),
        other => {
            links.extend(other);
            None
        }
    }
}

fn not_nullish(e: Expr) -> Expr {
    Expr::Binary("!=", Box::new(e), Box::new(Expr::Null))
}

fn assign(target: Pattern, value: Expr) -> Stmt {
    Stmt::Expr(Expr::Assign { op: "=", target: Box::new(target), value: Box::new(value) })
}

/// `{ n: e }.n`: an anonymous function or class that keeps the name `n` it would take from
/// its binding when it goes through a temporary.
fn named(n: &Name, e: Expr) -> Expr {
    let obj = Expr::Object(vec![ObjProp::Value(PropName::Fixed(n.clone()), e)]);
    Expr::Member {
        obj: Box::new(obj),
        prop: Box::new(MemberProp::Name(n.clone())),
        optional: false,
        pos: Pos::default(),
    }
}

/// Whether evaluating the class `c` yields: in its `extends` or in a computed key.
pub(crate) fn class_yields(c: &ClassDef) -> bool {
    c.parent.as_deref().is_some_and(crate::generators::expr_yields)
        || c.members.iter().any(|m| crate::generators::key_yields(&m.key))
}

impl Lower {
    /// `p` bound to the temporary `t`: a declaration of kind `kind`, or an assignment.
    pub(crate) fn bind_to(
        &mut self,
        kind: Option<DeclKind>,
        p: Pattern,
        t: &Name,
        out: &mut Vec<Stmt>,
    ) {
        match kind {
            Some(k) => self.stmt(Stmt::Decl(k, vec![(p, Some(ident(t)))]), out),
            None => self.stmt(assign(p, ident(t)), out),
        }
    }

    /// A destructuring pattern that yields, bound to the temporary `v`: the values are read
    /// into temporaries by a pattern that does not yield, as the interpreter reads them (an
    /// array pattern reads its iterator first and evaluates its defaults after), and then
    /// each default and each target is a statement of its own.
    pub(crate) fn destructure(
        &mut self,
        p: Pattern,
        v: &Name,
        kind: Option<DeclKind>,
        out: &mut Vec<Stmt>,
    ) {
        match p {
            Pattern::Array(items, rest) => {
                let mut plain = Vec::new();
                let mut parts = Vec::new();
                for item in items {
                    plain.push(item.map(|(target, d)| {
                        let t = self.tmp();
                        parts.push((target, d, t.clone()));
                        (Pattern::Name(t), None)
                    }));
                }
                let rest = rest.map(|r| (*r, self.tmp()));
                let plain_rest = rest.as_ref().map(|(_, t)| Box::new(Pattern::Name(t.clone())));
                out.push(assign(Pattern::Array(plain, plain_rest), ident(v)));
                for (target, d, t) in parts {
                    self.element(target, d, &t, kind, out);
                }
                if let Some((r, t)) = rest {
                    self.bind_to(kind, r, &t, out);
                }
            }
            Pattern::Object(props, None) => {
                // `null` and `undefined` fail before any key is evaluated.
                out.push(assign(Pattern::Object(Vec::new(), None), ident(v)));
                for (k, target, d) in props {
                    let k = self.key(k, out);
                    let t = self.tmp();
                    let one = Pattern::Object(vec![(k, Pattern::Name(t.clone()), None)], None);
                    out.push(assign(one, ident(v)));
                    self.element(target, d, &t, kind, out);
                }
            }
            Pattern::Object(props, Some(rest)) => {
                // With a rest the keys are read by one pattern, which leaves the others to it.
                let mut plain = Vec::new();
                let mut parts = Vec::new();
                for (k, target, d) in props {
                    let k = self.key(k, out);
                    let t = self.tmp();
                    plain.push((k, Pattern::Name(t.clone()), None));
                    parts.push((target, d, t));
                }
                let r = self.tmp();
                let plain_rest = Some(Box::new(Pattern::Name(r.clone())));
                out.push(assign(Pattern::Object(plain, plain_rest), ident(v)));
                for (target, d, t) in parts {
                    self.element(target, d, &t, kind, out);
                }
                self.bind_to(kind, *rest, &r, out);
            }
            p => self.bind_to(kind, p, v, out),
        }
    }

    /// One element of a pattern read into `t`: its default when `t` is `undefined`, then its
    /// target.
    fn element(
        &mut self,
        target: Pattern,
        d: Option<Expr>,
        t: &Name,
        kind: Option<DeclKind>,
        out: &mut Vec<Stmt>,
    ) {
        if let Some(d) = d {
            let d = match &target {
                Pattern::Name(n) if crate::interp::anonymous_definition(&d) => named(n, d),
                _ => d,
            };
            let mut inner = Vec::new();
            let d = self.expr(d, &mut inner);
            inner.push(set(t, d));
            let unset = Expr::Binary("===", Box::new(ident(t)), Box::new(Expr::Undefined));
            out.push(mark(Stmt::If(unset, Box::new(block(inner)), None)));
        }
        self.bind_to(kind, target, t, out);
    }

    /// A class whose `extends` or computed keys yield: they are evaluated in order into
    /// temporaries before the class.
    pub(crate) fn class(&mut self, c: Rc<ClassDef>, pre: &mut Vec<Stmt>) -> Rc<ClassDef> {
        if !class_yields(&c) {
            return c;
        }
        // The parser's tree is not shared yet; a shared one is left as it is.
        let c = match Rc::try_unwrap(c) {
            Ok(c) => c,
            Err(c) => return c,
        };
        let last = c.members.iter().rposition(|m| crate::generators::key_yields(&m.key));
        let parent = c.parent.map(|p| {
            let p = self.expr(*p, pre);
            Box::new(if last.is_some() { self.spill(p, pre) } else { p })
        });
        let members = c
            .members
            .into_iter()
            .enumerate()
            .map(|(i, m)| {
                if last.is_some_and(|l| i <= l) {
                    ClassMember { key: self.key(m.key, pre), ..m }
                } else {
                    m
                }
            })
            .collect();
        Rc::new(ClassDef { parent, members, ..c })
    }

    /// An optional chain: when a key or the arguments after a `?.` yield, the `yield` only
    /// runs if the value before the `?.` is neither `null` nor `undefined`.
    pub(crate) fn opt_chain(&mut self, x: Expr, pre: &mut Vec<Stmt>) -> Expr {
        let (base, mut links) = split_chain(x);
        let gate =
            (0..links.len()).find(|&j| links[j].optional() && links[j..].iter().any(Link::yields));
        let Some(j) = gate else {
            return Expr::OptChain(Box::new(self.expr(rebuild(base, links), pre)));
        };
        let mut after = links.split_off(j);
        let first = after.remove(0).without_check();
        let r = self.tmp();
        pre.push(set(&r, Expr::Undefined));
        // `a.f?.(…)`: the call keeps `a` as its `this`, so `a` and the key are fixed and the
        // callee is read again inside.
        let callee = if matches!(first, Link::Call(..)) { pop_member(&mut links) } else { None };
        let (head, checked) = match callee {
            Some((prop, optional, pos)) => {
                let obj = self.temp_of(Expr::OptChain(Box::new(rebuild(base, links))), pre);
                let prop = match prop {
                    MemberProp::Computed(k) => {
                        let k = self.expr(k, pre);
                        MemberProp::Computed(ident(&self.temp_of(k, pre)))
                    }
                    p => p,
                };
                let callee = Expr::Member {
                    obj: Box::new(ident(&obj)),
                    prop: Box::new(prop),
                    optional,
                    pos,
                };
                let f = self.temp_of(callee.clone_shallow(), pre);
                (callee, ident(&f))
            }
            None => {
                let head = match rebuild(base, links) {
                    // A name stays a name, so that an error still names it.
                    Expr::Ident(n, p) => Expr::Ident(n, p),
                    e => {
                        let e = self.expr(Expr::OptChain(Box::new(e)), pre);
                        ident(&self.temp_of(e, pre))
                    }
                };
                (head.clone_shallow(), head)
            }
        };
        let mut rest = vec![first];
        rest.extend(after);
        let mut inner = Vec::new();
        let v = self.expr(Expr::OptChain(Box::new(rebuild(head, rest))), &mut inner);
        inner.push(set(&r, v));
        pre.push(mark(Stmt::If(not_nullish(checked), Box::new(block(inner)), None)));
        ident(&r)
    }

    /// `e` in a new temporary.
    fn temp_of(&mut self, e: Expr, pre: &mut Vec<Stmt>) -> Name {
        let t = self.tmp_for(&e);
        pre.push(set(&t, e));
        t
    }
}
