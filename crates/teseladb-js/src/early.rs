//! The early errors of declarations (ECMA-262 §8.2, §14.2.1, §14.12.1, §14.15.1, §15.2.1): a
//! name declared twice in a scope where that is not allowed, with SpiderMonkey's message
//! («redeclaration of let x», naming what was declared first).

use crate::ast::Name;

/// What declared a name.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Decl {
    Var,
    Let,
    Const,
    Class,
    /// A function declaration; `plain` when it is neither a generator nor async (two of those
    /// may share a name in a block of sloppy code, ECMA-262 §B.3.2.4).
    Function {
        plain: bool,
    },
    Param,
    /// A catch parameter; `simple` when it is a name, which a `var` may redeclare (§B.3.4).
    Catch {
        simple: bool,
    },
}

impl Decl {
    fn word(self) -> &'static str {
        match self {
            Decl::Var => "var",
            Decl::Let => "let",
            Decl::Const => "const",
            Decl::Class => "class",
            Decl::Function { .. } => "function",
            Decl::Param => "formal parameter",
            Decl::Catch { .. } => "catch parameter",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ScopeKind {
    /// A function's parameters and body, or a whole script.
    Function,
    /// A block, a `switch`'s cases or a `for` head.
    Block,
    /// A catch clause: its parameter and its block.
    Catch,
}

#[derive(Clone, Debug)]
struct Scope {
    kind: ScopeKind,
    names: Vec<(Name, Decl)>,
}

/// The scopes open where the parser is.
#[derive(Clone, Debug, Default)]
pub struct Scopes(Vec<Scope>);

impl Scopes {
    pub fn push(&mut self, kind: ScopeKind) {
        self.0.push(Scope { kind, names: Vec::new() });
    }

    pub fn pop(&mut self) {
        self.0.pop();
    }

    /// Declares `name`; the error is the message when that is a redeclaration.
    pub fn declare(&mut self, name: &Name, decl: Decl, strict: bool) -> Result<(), String> {
        let Some(top) = self.0.last() else { return Ok(()) };
        let var_like = decl == Decl::Var
            || (matches!(decl, Decl::Function { .. }) && top.kind == ScopeKind::Function);
        if decl == Decl::Var {
            return self.declare_var(name);
        }
        let top = self.0.last_mut().expect("a scope");
        if let Some((_, old)) = top.names.iter().find(|(n, _)| n == name) {
            let allowed = match (*old, decl) {
                (Decl::Param, Decl::Param) => true,
                (Decl::Var | Decl::Param | Decl::Function { .. }, _) if var_like => {
                    !matches!(old, Decl::Let | Decl::Const | Decl::Class)
                }
                (Decl::Function { plain: true }, Decl::Function { plain: true }) => !strict,
                _ => false,
            };
            if !allowed {
                return Err(format!("redeclaration of {} {name}", old.word()));
            }
        }
        top.names.push((name.clone(), decl));
        Ok(())
    }

    /// `var name`: it may not meet a lexical declaration of the same name in any scope up to its
    /// function's, and each of those scopes remembers it.
    fn declare_var(&mut self, name: &Name) -> Result<(), String> {
        for scope in self.0.iter_mut().rev() {
            let clash = scope.names.iter().find(|(n, d)| {
                n == name
                    && match d {
                        Decl::Let | Decl::Const | Decl::Class | Decl::Catch { simple: false } => {
                            true
                        }
                        Decl::Function { .. } => scope.kind != ScopeKind::Function,
                        _ => false,
                    }
            });
            if let Some((_, old)) = clash {
                return Err(format!("redeclaration of {} {name}", old.word()));
            }
            scope.names.push((name.clone(), Decl::Var));
            if scope.kind == ScopeKind::Function {
                break;
            }
        }
        Ok(())
    }
}
