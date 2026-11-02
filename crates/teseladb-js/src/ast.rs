//! The syntax tree the parser builds and the interpreter walks.

use std::rc::Rc;

pub type Name = Rc<str>;

/// A position in the source, for error messages.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DeclKind {
    Var,
    Let,
    Const,
}

/// A binding target: a name or a destructuring pattern.
#[derive(Debug)]
pub enum Pattern {
    Name(Name),
    /// `{a, b: c = 1, ...rest}`.
    Object(Vec<(PropName, Pattern, Option<Expr>)>, Option<Box<Pattern>>),
    /// `[a, , b = 1, ...rest]`.
    Array(Vec<Option<(Pattern, Option<Expr>)>>, Option<Box<Pattern>>),
    /// An assignment target that is not a name (`a.b`, `a[0]`), only in assignments.
    Expr(Box<Expr>),
}

#[derive(Debug)]
pub enum PropName {
    Fixed(Name),
    Computed(Box<Expr>),
}

/// A function's code, shared by every closure made from it.
#[derive(Debug)]
pub struct FuncDef {
    pub name: Option<Name>,
    pub params: Vec<(Pattern, Option<Expr>)>,
    pub rest: Option<Pattern>,
    pub body: Vec<Stmt>,
    pub arrow: bool,
    pub is_ctor_of_derived: bool,
    /// Strict mode code (a class, or `"use strict"` in it or around it).
    pub strict: bool,
    /// `function*`: a call makes a generator.
    pub generator: bool,
    /// `async function`: a call gives a promise.
    pub is_async: bool,
    /// A method, getter or setter of an object literal or a class: `new` refuses it.
    pub method: bool,
    pub source: Rc<str>,
    /// The name SpiderMonkey guesses for it when it has none ([`crate::names`]).
    pub guessed: std::cell::OnceCell<Name>,
}

#[derive(Debug)]
pub struct ClassDef {
    /// Where `class` is: the place of what its implicit constructor does.
    pub pos: Pos,
    pub name: Option<Name>,
    pub parent: Option<Box<Expr>>,
    pub ctor: Option<Rc<FuncDef>>,
    pub members: Vec<ClassMember>,
}

#[derive(Debug)]
pub struct ClassMember {
    pub is_static: bool,
    pub key: PropName,
    pub kind: PropKind,
    /// A field's initializer (`x = 1;`), `None` for methods.
    pub field: Option<Expr>,
    pub func: Option<Rc<FuncDef>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PropKind {
    Value,
    Get,
    Set,
}

#[derive(Debug)]
pub enum ObjProp {
    Value(PropName, Expr),
    Accessor(PropName, PropKind, Rc<FuncDef>),
    Spread(Expr),
}

#[derive(Debug)]
pub enum Expr {
    Num(f64),
    BigInt(Rc<crate::bigint::BigInt>),
    Str(Name),
    Template(Vec<Name>, Vec<Expr>),
    /// The strings a tagged template gives its tag: the cooked pieces with their `raw`.
    TemplateObject(Rc<crate::lexer::TemplateParts>),
    Regex(Name, Name),
    Bool(bool),
    Null,
    Undefined,
    Ident(Name, Pos),
    This,
    SuperMember(Box<Expr>),
    SuperCall(Vec<Expr>, Pos),
    Array(Vec<Option<Expr>>),
    Object(Vec<ObjProp>),
    Func(Rc<FuncDef>),
    Class(Rc<ClassDef>),
    Spread(Box<Expr>),
    Unary(&'static str, Box<Expr>),
    Update {
        op: &'static str,
        prefix: bool,
        target: Box<Expr>,
    },
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Logical(&'static str, Box<Expr>, Box<Expr>),
    Assign {
        op: &'static str,
        target: Box<Pattern>,
        value: Box<Expr>,
    },
    Cond(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `obj.name` or `obj[expr]`; `optional` for `?.`.
    Member {
        obj: Box<Expr>,
        prop: Box<MemberProp>,
        optional: bool,
        pos: Pos,
    },
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        optional: bool,
        pos: Pos,
    },
    New {
        callee: Box<Expr>,
        args: Vec<Expr>,
        pos: Pos,
    },
    Seq(Vec<Expr>),
    /// The end of an optional chain: a short circuit inside gives `undefined` here.
    OptChain(Box<Expr>),
    Typeof(Box<Expr>),
    /// `yield x` (`false`) or `yield* x` (`true`) in a generator.
    Yield(Option<Box<Expr>>, bool),
    /// `await x` in an async function.
    Await(Box<Expr>),
    /// An expression and where it starts: evaluating it moves the place errors are reported at,
    /// as SpiderMonkey does (016 T021).
    At(Pos, Box<Expr>),
    Delete(Box<Expr>),
}

#[derive(Debug)]
pub enum MemberProp {
    Name(Name),
    Computed(Expr),
}

#[derive(Debug)]
pub enum ForInit {
    Decl(DeclKind, Vec<(Pattern, Option<Expr>)>),
    Expr(Expr),
}

#[derive(Debug)]
pub enum Stmt {
    Expr(Expr),
    Decl(DeclKind, Vec<(Pattern, Option<Expr>)>),
    Func(Name, Rc<FuncDef>),
    Class(Name, Rc<ClassDef>),
    Return(Option<Expr>),
    If(Expr, Box<Stmt>, Option<Box<Stmt>>),
    Block(Vec<Stmt>),
    While(Expr, Box<Stmt>),
    DoWhile(Box<Stmt>, Expr),
    For(Option<ForInit>, Option<Expr>, Option<Expr>, Box<Stmt>),
    /// `for (x in obj)` (`of` false) and `for (x of it)`.
    ForIn {
        decl: Option<DeclKind>,
        target: Pattern,
        of: bool,
        obj: Expr,
        body: Box<Stmt>,
    },
    Break(Option<Name>),
    Continue(Option<Name>),
    Throw(Expr, Pos),
    Try(Vec<Stmt>, Option<(Option<Pattern>, Vec<Stmt>)>, Option<Vec<Stmt>>),
    Switch(Expr, Vec<(Option<Expr>, Vec<Stmt>)>),
    Labeled(Name, Box<Stmt>),
    /// `with (obj) body`: the object's properties are names in the body.
    With(Expr, Box<Stmt>),
    Empty,
    /// A statement of a generator that holds a `yield` (see `generators.rs`).
    Suspends(Box<Stmt>),
}
