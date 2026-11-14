//! Function declarations in blocks of non-strict code (ECMA-262 Annex B.3.3): besides the
//! block's own binding, each one whose name a `var` could take without an early error gets a
//! `var` of the function, or of the program, that holds it, which takes the function's value
//! when the declaration is evaluated (measured on a reference router 9.0.4 on 02-11:
//! `{ function f() {} } typeof f` gives `function`).

use std::rc::Rc;

use crate::ast::{DeclKind, ForInit, FuncDef, Name, Stmt};
use crate::interp::pattern_names;

/// The declarations of blocks of `body` that Annex B.3.3 applies to, with their names; `params`
/// are the parameter names of the function, which such a `var` never takes.
pub(crate) fn block_functions(body: &[Stmt], params: &[Name]) -> Vec<(Name, Rc<FuncDef>)> {
    let mut walk = Walk { lexical: params.to_vec(), found: Vec::new() };
    for s in body {
        lexical_names(s, &mut walk.lexical);
    }
    for s in body {
        walk.nested(s);
    }
    walk.found
}

struct Walk {
    /// The names a `var` would clash with at the current point: the parameters and the lexical
    /// declarations of the enclosing scopes.
    lexical: Vec<Name>,
    found: Vec<(Name, Rc<FuncDef>)>,
}

impl Walk {
    /// A statement list that is a block scope of its own.
    fn block(&mut self, body: &[Stmt]) {
        self.statements(&body.iter().collect::<Vec<_>>());
    }

    /// The blocks inside a statement, outside nested functions and classes.
    fn nested(&mut self, s: &Stmt) {
        match s {
            Stmt::Block(b) => self.block(b),
            Stmt::If(_, a, b) => {
                self.body(a);
                if let Some(b) = b {
                    self.body(b);
                }
            }
            Stmt::While(_, b) | Stmt::DoWhile(b, _) | Stmt::Labeled(_, b) | Stmt::With(_, b) => {
                self.body(b)
            }
            Stmt::For(init, _, _, b) => {
                let mark = self.lexical.len();
                if let Some(ForInit::Decl(DeclKind::Let | DeclKind::Const, d)) = init {
                    d.iter().for_each(|(p, _)| pattern_names(p, &mut self.lexical));
                }
                self.body(b);
                self.lexical.truncate(mark);
            }
            Stmt::ForIn { decl, target, body, .. } => {
                let mark = self.lexical.len();
                if matches!(decl, Some(DeclKind::Let | DeclKind::Const)) {
                    pattern_names(target, &mut self.lexical);
                }
                self.body(body);
                self.lexical.truncate(mark);
            }
            Stmt::Try(a, c, f) => {
                self.block(a);
                if let Some((_, c)) = c {
                    self.block(c);
                }
                if let Some(f) = f {
                    self.block(f);
                }
            }
            Stmt::Switch(_, cases) => {
                let all: Vec<&Stmt> = cases.iter().flat_map(|(_, b)| b).collect();
                self.statements(&all);
            }
            Stmt::Suspends(s) => self.nested(s),
            _ => {}
        }
    }

    /// The body of an `if`, a loop, a label or a `with`.
    fn body(&mut self, s: &Stmt) {
        self.block(std::slice::from_ref(s));
    }

    /// The statements of one block scope (a block, or all the cases of a `switch`).
    fn statements(&mut self, body: &[&Stmt]) {
        let mark = self.lexical.len();
        let mut functions = Vec::new();
        for s in body {
            match s {
                // Only plain functions: not generators nor async functions.
                Stmt::Func(n, def) if !def.generator && !def.is_async => {
                    functions.push((n.clone(), def.clone()))
                }
                Stmt::Func(..) => {}
                other => lexical_names(other, &mut self.lexical),
            }
        }
        for (n, def) in &functions {
            if !self.lexical.contains(n) {
                self.found.push((n.clone(), def.clone()));
            }
        }
        for s in body {
            self.nested(s);
        }
        self.lexical.truncate(mark);
    }
}

/// The `let`, `const` and `class` names a statement declares in its own scope.
fn lexical_names(s: &Stmt, out: &mut Vec<Name>) {
    match s {
        Stmt::Decl(DeclKind::Let | DeclKind::Const, d) => {
            d.iter().for_each(|(p, _)| pattern_names(p, out));
        }
        Stmt::Class(n, _) => out.push(n.clone()),
        Stmt::Suspends(s) => lexical_names(s, out),
        _ => {}
    }
}
