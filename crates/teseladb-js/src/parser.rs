//! The parser of ECMAScript scripts and functions (ECMA-262 §13 to §15) into [`crate::ast`], with
//! automatic semicolon insertion and the messages of SpiderMonkey for the common mistakes.

use std::rc::Rc;

use crate::ast::*;
use crate::early::{Decl, ScopeKind, Scopes};
use crate::lexer::{SyntaxError, Tok, Token, tokenize};

/// Parses a whole program as RefDb gives it: an anonymous function may stand as a whole
/// statement (`function(){ … }`), as RefDb reads a function's source.
pub fn parse_script(src: &str) -> Result<Vec<Stmt>, SyntaxError> {
    parse(src, &[], (false, false), true, Room::top())
}

/// Parses the code of an indirect `eval` or of `Function`, with the `room` left: a script.
pub fn parse_eval(src: &str, room: Room) -> Result<Vec<Stmt>, SyntaxError> {
    parse(src, &[], (false, false), false, room)
}

/// Parses a whole script that may use the private names `privates` (the code of a direct
/// `eval` inside a class); `supers` says whether it may use `super.x` and `super()` (the code
/// of a direct `eval` inside a method or a derived constructor).
pub fn parse_script_in(
    src: &str,
    privates: &[Name],
    supers: (bool, bool),
    room: Room,
) -> Result<Vec<Stmt>, SyntaxError> {
    parse(src, privates, supers, false, room)
}

/// What a parse may take before the engine of a `refrouter` 9.0.4 stops it
/// ([`crate::recursion`]): its native stack, the arrow functions nested in one another, and ours.
#[derive(Clone, Copy, Debug)]
pub struct Room {
    /// The engine's native stack, in the units of [`crate::recursion`].
    pub units: usize,
    /// The arrow functions nested in one another: the engine does not check them, and its
    /// stack runs out after this many.
    pub arrows: usize,
    /// Bytes of our native stack the parse may take.
    pub stack: usize,
}

impl Room {
    /// The room of a program's parse.
    pub fn top() -> Room {
        Room {
            units: crate::recursion::TOP_PARSE,
            arrows: crate::recursion::TOP_ARROWS,
            stack: crate::DEFAULT_STACK / 2,
        }
    }
}

/// The message of a parse the engine stops for its depth.
pub const TOO_MUCH_RECURSION: &str = "too much recursion";
/// The message of a parse whose arrow functions exhaust the engine's stack.
pub const STACK_EXHAUSTED: &str = "wasm trap: call stack exhausted";

fn parse(
    src: &str,
    privates: &[Name],
    supers: (bool, bool),
    anonymous: bool,
    room: Room,
) -> Result<Vec<Stmt>, SyntaxError> {
    let mut p = Parser {
        toks: tokenize(src)?,
        i: 0,
        src: src.chars().collect(),
        depth: 0,
        units: 0,
        arrows: 0,
        room,
        stack_base: crate::interp::stack_position(),
        strict: false,
        in_gen: false,
        in_async: false,
        origin: Pos { line: 1, col: 1 },
        scopes: Scopes::default(),
        binding: None,
        in_params: false,
        in_static: false,
        namer: crate::names::Namer::default(),
        privates: Vec::new(),
        loose_privates: false,
        cover: Vec::new(),
        super_prop: supers.0,
        super_call: supers.1,
    };
    if !privates.is_empty() {
        let declared = privates.iter().map(|n| (n.clone(), PrivateKind::Field, false)).collect();
        p.privates.push(PrivateScope { declared, used: Vec::new() });
    }
    p.scopes.push(ScopeKind::Function);
    let mut out = Vec::new();
    while !p.at_eof() {
        let function =
            anonymous && p.is_word("function") && matches!(p.peek_at(1), Tok::Punct("("));
        if function && out.is_empty() {
            // The program's own function: the engine's room is measured inside it.
            p.room.units += crate::recursion::FUNCTION;
        }
        out.push(if function { p.expression_statement()? } else { p.statement()? });
        if out.len() == 1 && has_use_strict(&out) {
            p.strict = true;
        }
    }
    if p.strict {
        p.check_legacy(0)?;
    }
    p.namer.finish();
    Ok(out)
}

/// Parses one expression (a template's substitution).
fn parse_expression(
    src: &str,
    line: u32,
    col: u32,
    in_gen: bool,
    in_async: bool,
    supers: (bool, bool),
    room: Room,
) -> Result<Expr, SyntaxError> {
    let mut p = Parser {
        toks: crate::lexer::tokenize_at(src, line, col)?,
        i: 0,
        src: src.chars().collect(),
        depth: 0,
        units: 0,
        arrows: 0,
        room,
        stack_base: crate::interp::stack_position(),
        strict: false,
        in_gen,
        in_async,
        origin: Pos { line, col },
        scopes: Scopes::default(),
        binding: None,
        in_params: false,
        in_static: false,
        namer: crate::names::Namer::default(),
        privates: Vec::new(),
        loose_privates: true,
        cover: Vec::new(),
        super_prop: supers.0,
        super_call: supers.1,
    };
    p.scopes.push(ScopeKind::Function);
    let start = p.pos();
    // The substitution ends at its `}`.
    if p.at_eof() {
        return p.fail("expected expression, got '}'".into());
    }
    let e = p.expression()?;
    if !p.at_eof() {
        return p.fail("missing } in template string".into());
    }
    if let Some(at) = p.cover.first() {
        return p.fail_at(*at, "missing : after property id".into());
    }
    p.namer.finish();
    Ok(at(start, e))
}

/// `e` marked with the place where it starts, which becomes the place of an error inside it as
/// SpiderMonkey reports it (016 T021); literals are not marked.
fn at(pos: Pos, e: Expr) -> Expr {
    match e {
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Regex(..)
        | Expr::Func(_)
        | Expr::Class(_)
        | Expr::At(..) => e,
        e => Expr::At(pos, Box::new(e)),
    }
}

/// Where a call of `callee` is reported: the start of a name, the name of a property, the key of
/// a computed member; `paren` for other callees.
fn call_pos(callee: &Expr, paren: Pos) -> Pos {
    match callee {
        // A key that is not a constant string is an element: SpiderMonkey reports its `(`.
        Expr::Member { prop, .. } if matches!(&**prop, MemberProp::Computed(k) if !constant_string(k)) => {
            paren
        }
        Expr::Ident(_, p) | Expr::Member { pos: p, .. } => *p,
        _ => paren,
    }
}

/// Whether a computed key folds to a string (`'k'`, `'k' + 1`).
fn constant_string(e: &Expr) -> bool {
    match e {
        Expr::Str(_) => true,
        Expr::At(_, x) => constant_string(x),
        Expr::Binary("+", a, b) => {
            let side = |x: &Expr| matches!(x, Expr::Num(_)) || constant_string(x);
            (constant_string(a) && side(b)) || (side(a) && constant_string(b))
        }
        _ => false,
    }
}

/// `e` marked with where it starts, literals too (a declaration's initializer, what `for…of`
/// iterates: SpiderMonkey reports an error of `let [a] = null` at the `null`).
fn at_always(pos: Pos, e: Expr) -> Expr {
    match e {
        Expr::Func(_) | Expr::Class(_) | Expr::At(..) => e,
        e => Expr::At(pos, Box::new(e)),
    }
}

/// The precedence of the relational operators (`<`, `in`…).
const RELATIONAL: u8 = 8;

/// Whether `e` is a private member (`delete this.#x` is an early error).
fn deletes_private(e: &Expr) -> bool {
    match e {
        Expr::Member { prop, .. } => matches!(**prop, MemberProp::Private(_)),
        Expr::At(_, x) | Expr::OptChain(x) => deletes_private(x),
        _ => false,
    }
}

/// The error of a parse the engine stops for its depth: no place, as RefDb shows it.
fn recursion() -> SyntaxError {
    SyntaxError { message: TOO_MUCH_RECURSION.into(), line: 0, col: 1 }
}

/// A function's parameters with their defaults, and its rest parameter.
type Params = (Vec<(Pattern, Option<Expr>)>, Option<Pattern>);

/// A word that goes between two operands (`a in b`) or ends an expression.
fn is_operator_word(w: &str) -> bool {
    matches!(w, "in" | "instanceof" | "of")
}

/// The names `p` binds.
fn bound_names(p: &Pattern) -> Vec<Name> {
    let mut out = Vec::new();
    crate::interp::pattern_names(p, &mut out);
    out
}

/// The first name of `seen` that an earlier one already has, and where it is.
fn first_duplicate(seen: &[(Name, Pos)]) -> Option<(Name, Pos)> {
    seen.iter()
        .enumerate()
        .find(|(i, (n, _))| seen[..*i].iter().any(|(m, _)| m == n))
        .map(|(_, x)| x.clone())
}

/// What kind of function a body belongs to.
#[derive(Clone, Copy, Default)]
struct FnKind {
    generator: bool,
    is_async: bool,
    method: bool,
    /// A class's constructor.
    ctor: bool,
}

struct Parser {
    toks: Vec<Token>,
    i: usize,
    src: Vec<char>,
    depth: usize,
    /// The engine's native stack the constructs open here take ([`Room::units`]).
    units: usize,
    /// The arrow functions open here.
    arrows: usize,
    /// What the parse may take.
    room: Room,
    /// Where our native stack was when the parse began.
    stack_base: usize,
    /// Whether the code being read is strict.
    strict: bool,
    /// Whether the code being read is the body of a generator (`yield` is an operator).
    in_gen: bool,
    /// Where the source starts in the program (a template's substitution starts inside it).
    origin: Pos,
    /// Whether the code being read is the body of an async function (`await` is an operator).
    in_async: bool,
    /// The declarations of the scopes open here, for the early errors of redeclarations.
    scopes: Scopes,
    /// What the names a binding pattern reads declare, while it reads a declaration.
    binding: Option<Decl>,
    /// Whether the code being read is a function's parameter list (no `yield` there).
    in_params: bool,
    /// Whether the code being read is a class's `static { … }` block (no `await` binding).
    in_static: bool,
    /// Whether `super.x` is allowed here: in a method, a constructor, a field's initializer or
    /// a static block, and in the arrows inside them.
    super_prop: bool,
    /// Whether `super()` is allowed here: in a derived class's constructor and its arrows.
    super_call: bool,
    /// The functions read and where they are written, for their guessed names.
    namer: crate::names::Namer,
    /// The private names of the classes open here, innermost last.
    privates: Vec<PrivateScope>,
    /// Whether a private name no class here declares is accepted (a template's substitution,
    /// whose names the code around it checks).
    loose_privates: bool,
    /// Where each `{a = 1}` of an object literal is that no assignment has made a pattern yet:
    /// one left when its statement ends is an error.
    cover: Vec<Pos>,
}

/// What a private name of a class declares.
#[derive(Clone, Copy, PartialEq)]
enum PrivateKind {
    Field,
    Method,
    Getter,
    Setter,
}

/// The private names a class declares (with their kind and whether they are static) and those
/// its code uses, with where.
#[derive(Default)]
struct PrivateScope {
    declared: Vec<(Name, PrivateKind, bool)>,
    used: Vec<(Name, Pos)>,
}

/// The name of a method or accessor: `m`, `get g`, `set s`.
fn accessor_name(kind: PropKind, key: &str) -> String {
    match kind {
        PropKind::Get => format!("get {key}"),
        PropKind::Set => format!("set {key}"),
        _ => key.to_string(),
    }
}

/// Whether `body` starts with the directive `"use strict"`.
/// A function declared as the branch of an `if` is in a block of its own (Annex B.3.4).
fn in_block(s: Stmt) -> Stmt {
    if matches!(s, Stmt::Func(..)) { Stmt::Block(vec![s]) } else { s }
}

pub fn has_use_strict(body: &[Stmt]) -> bool {
    matches!(body.first(), Some(Stmt::Expr(Expr::Str(s))) if &**s == "use strict")
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Ident(w) => {
            if is_reserved(w) {
                format!("keyword '{w}'")
            } else {
                "identifier".to_owned()
            }
        }
        Tok::Num(_) | Tok::BigInt(_) => "numeric literal".to_owned(),
        Tok::Str(_) => "string literal".to_owned(),
        Tok::Template(..) => "template literal".to_owned(),
        Tok::Regex(..) => "regular expression literal".to_owned(),
        Tok::Private(n) => format!("private name {n}"),
        Tok::Punct(p) => format!("'{p}'"),
        Tok::Eof => "end of script".to_owned(),
    }
}

fn is_reserved(w: &str) -> bool {
    matches!(
        w,
        "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "export"
            | "extends"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "null"
            | "true"
            | "false"
            | "enum"
    )
}

impl Parser {
    fn tok(&self) -> &Tok {
        &self.toks[self.i].tok
    }

    fn pos(&self) -> Pos {
        let t = &self.toks[self.i];
        Pos { line: t.line, col: t.col }
    }

    fn at_eof(&self) -> bool {
        matches!(self.tok(), Tok::Eof)
    }

    fn peek_at(&self, k: usize) -> &Tok {
        &self.toks[(self.i + k).min(self.toks.len() - 1)].tok
    }

    fn is(&self, p: &str) -> bool {
        matches!(self.tok(), Tok::Punct(q) if *q == p)
    }

    fn is_word(&self, w: &str) -> bool {
        matches!(self.tok(), Tok::Ident(x) if &**x == w)
    }

    fn eat(&mut self, p: &str) -> bool {
        if self.is(p) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.is_word(w) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn fail<T>(&self, message: String) -> Result<T, SyntaxError> {
        let p = self.pos();
        Err(SyntaxError { message, line: p.line, col: p.col })
    }

    /// Strict mode code from the token `from` up to here: a legacy octal number or escape
    /// fails ([`crate::lexer::Token::legacy`]).
    fn check_legacy(&self, from: usize) -> Result<(), SyntaxError> {
        match self.toks[from..self.i].iter().find_map(|t| t.legacy.map(|m| (t, m))) {
            Some((t, m)) => Err(SyntaxError { message: m.into(), line: t.line, col: t.col }),
            None => Ok(()),
        }
    }

    fn fail_at<T>(&self, p: Pos, message: String) -> Result<T, SyntaxError> {
        Err(SyntaxError { message, line: p.line, col: p.col })
    }

    fn pos_at(&self, k: usize) -> Pos {
        let t = &self.toks[(self.i + k).min(self.toks.len() - 1)];
        Pos { line: t.line, col: t.col }
    }

    /// Declares `name`, read at `p`, as `decl` in the scope open here.
    fn declare(&mut self, name: &Name, decl: Decl, p: Pos) -> Result<(), SyntaxError> {
        if decl != Decl::Param && self.strict && matches!(&**name, "eval" | "arguments") {
            return self.fail_at(
                p,
                format!("'{name}' can't be defined or assigned to in strict mode code"),
            );
        }
        if matches!(decl, Decl::Let | Decl::Const) && &**name == "let" {
            return self.fail_at(p, "a lexical declaration can't define a 'let' binding".into());
        }
        let strict = self.strict;
        self.scopes.declare(name, decl, strict).map_or_else(|m| self.fail_at(p, m), Ok)
    }

    /// A name a binding pattern binds, declared when the pattern is a declaration's.
    fn binding_name(&mut self) -> Result<Name, SyntaxError> {
        let p = self.pos();
        let name = self.ident()?;
        if let Some(d) = self.binding {
            self.declare(&name, d, p)?;
        }
        Ok(name)
    }

    /// `statements` inside a new scope of `kind`.
    fn scoped<T>(
        &mut self,
        kind: ScopeKind,
        f: impl FnOnce(&mut Self) -> Result<T, SyntaxError>,
    ) -> Result<T, SyntaxError> {
        self.scopes.push(kind);
        let r = f(self);
        self.scopes.pop();
        r
    }

    fn unexpected<T>(&self) -> Result<T, SyntaxError> {
        self.fail(format!("unexpected token: {}", describe(self.tok())))
    }

    fn expect(&mut self, p: &str) -> Result<(), SyntaxError> {
        if self.eat(p) {
            return Ok(());
        }
        let got = describe(self.tok());
        let msg = match p {
            ")" => format!("missing ) after argument list, got {got}"),
            "]" => format!("missing ] after element list, got {got}"),
            "}" => format!("missing }} after property list, got {got}"),
            _ => format!("expected '{p}', got {got}"),
        };
        self.fail(msg)
    }

    fn ident(&mut self) -> Result<Name, SyntaxError> {
        match self.tok().clone() {
            Tok::Ident(w) if &*w == "yield" && (self.in_gen || self.strict) => {
                self.fail("yield is a reserved identifier".into())
            }
            Tok::Ident(w) if &*w == "await" && self.in_async => {
                self.fail("await is a reserved identifier".into())
            }
            Tok::Ident(w) if &*w == "await" && self.in_static => {
                self.fail("await is a reserved identifier".into())
            }
            Tok::Ident(w) if !is_reserved(&w) => {
                self.i += 1;
                Ok(w)
            }
            _ => self.fail(format!("missing name, got {}", describe(self.tok()))),
        }
    }

    /// A property name after `.`: any identifier, reserved words included.
    fn prop_ident(&mut self) -> Result<Name, SyntaxError> {
        match self.tok().clone() {
            Tok::Ident(w) => {
                self.i += 1;
                Ok(w)
            }
            _ => self.fail(format!("missing name after . operator, got {}", describe(self.tok()))),
        }
    }

    /// Automatic semicolon insertion (§12.10).
    fn semicolon(&mut self) -> Result<(), SyntaxError> {
        if self.eat(";") || self.is("}") || self.at_eof() || self.toks[self.i].newline_before {
            return Ok(());
        }
        let got = describe(self.tok());
        self.fail(format!("unexpected token: {got}"))
    }

    fn enter(&mut self) -> Result<(), SyntaxError> {
        self.depth += 1;
        let used = self.stack_base.saturating_sub(crate::interp::stack_position());
        if used > self.room.stack {
            return Err(recursion());
        }
        Ok(())
    }

    /// Reads with `run` a construct that takes `cost` of the engine's stack; a `checked` one
    /// stops the parse when the constructs open take more than the room.
    fn nested<T>(
        &mut self,
        cost: usize,
        checked: bool,
        run: impl FnOnce(&mut Self) -> Result<T, SyntaxError>,
    ) -> Result<T, SyntaxError> {
        self.units += cost;
        let r = if checked && self.units > self.room.units { Err(recursion()) } else { run(self) };
        self.units -= cost;
        r
    }

    // Statements.

    fn statement(&mut self) -> Result<Stmt, SyntaxError> {
        self.enter()?;
        let mark = self.cover.len();
        let cost = match self.tok() {
            Tok::Punct("{") => crate::recursion::BLOCK,
            Tok::Ident(w)
                if matches!(&**w, "if" | "while" | "do" | "for" | "with" | "switch" | "try") =>
            {
                crate::recursion::COMPOUND_STATEMENT
            }
            _ => crate::recursion::STATEMENT,
        };
        let s = self.nested(cost, true, Self::statement_inner);
        self.depth -= 1;
        match self.cover.get(mark) {
            Some(p) if s.is_ok() => self.fail_at(*p, "missing : after property id".into()),
            _ => s,
        }
    }

    fn statement_inner(&mut self) -> Result<Stmt, SyntaxError> {
        let pos = self.pos();
        if self.eat("{") {
            return Ok(Stmt::Block(self.scoped(ScopeKind::Block, Self::block_rest)?));
        }
        if self.eat(";") {
            return Ok(Stmt::Empty);
        }
        let word = match self.tok() {
            Tok::Ident(w) => w.clone(),
            _ => return self.expression_statement(),
        };
        if matches!(self.peek_at(1), Tok::Punct(":")) && !is_reserved(&word) {
            if &*word == "yield" && (self.in_gen || self.strict) {
                return self.fail("yield is a reserved identifier".into());
            }
            self.i += 2;
            return Ok(Stmt::Labeled(word, Box::new(self.statement()?)));
        }
        match &*word {
            "var" | "const" => {
                self.i += 1;
                let kind = if &*word == "var" { DeclKind::Var } else { DeclKind::Const };
                let d = self.declarations(kind, true)?;
                self.semicolon()?;
                Ok(Stmt::Decl(kind, d))
            }
            "let" if matches!(self.peek_at(1), Tok::Ident(_) | Tok::Punct("[" | "{")) => {
                self.i += 1;
                let d = self.declarations(DeclKind::Let, true)?;
                self.semicolon()?;
                Ok(Stmt::Decl(DeclKind::Let, d))
            }
            "async"
                if matches!(self.peek_at(1), Tok::Ident(w) if &**w == "function")
                    && !self.toks[self.i + 1].newline_before =>
            {
                self.i += 2;
                let name_pos = self.pos_at(usize::from(self.is("*")));
                let f = self.function_rest(pos, false, true, false)?;
                let name = self.statement_name(f.name.clone())?;
                self.declare(&name, Decl::Function { plain: false }, name_pos)?;
                Ok(Stmt::Func(name, f))
            }
            "function" => {
                self.i += 1;
                let name_pos = self.pos_at(usize::from(self.is("*")));
                let f = self.function_rest(pos, false, false, false)?;
                let name = self.statement_name(f.name.clone())?;
                self.declare(&name, Decl::Function { plain: !f.generator }, name_pos)?;
                Ok(Stmt::Func(name, f))
            }
            "class" => {
                self.i += 1;
                let name_pos = self.pos();
                let c = self.class_rest()?;
                let name = c
                    .name
                    .clone()
                    .map_or_else(|| self.fail("class statement requires a name".into()), Ok)?;
                self.declare(&name, Decl::Class, name_pos)?;
                Ok(Stmt::Class(name, c))
            }
            "return" => {
                self.i += 1;
                let e = if self.is(";")
                    || self.is("}")
                    || self.at_eof()
                    || self.toks[self.i].newline_before
                {
                    None
                } else {
                    Some(at(pos, self.expression()?))
                };
                self.semicolon()?;
                Ok(Stmt::Return(e))
            }
            "if" => {
                self.i += 1;
                let c = self.condition()?;
                let then = Box::new(in_block(self.statement()?));
                let other = if self.eat_word("else") {
                    Some(Box::new(in_block(self.statement()?)))
                } else {
                    None
                };
                Ok(Stmt::If(c, then, other))
            }
            "while" => {
                self.i += 1;
                let c = self.condition()?;
                Ok(Stmt::While(c, Box::new(self.statement()?)))
            }
            "do" => {
                self.i += 1;
                let body = Box::new(self.statement()?);
                if !self.eat_word("while") {
                    return self.unexpected();
                }
                let c = self.condition()?;
                self.eat(";");
                Ok(Stmt::DoWhile(body, c))
            }
            "for" => {
                self.i += 1;
                if self.in_async && self.eat_word("await") {
                    // `for await` reads its first value through a promise job: the engine of
                    // RefDb 9.0 stops there (`await`), after evaluating what it iterates.
                    return match self.for_rest()? {
                        Stmt::ForIn { obj, .. } => Ok(Stmt::Block(vec![
                            Stmt::Expr(obj),
                            Stmt::Expr(Expr::Await(Box::new(Expr::Undefined))),
                        ])),
                        _ => self.fail("for await needs of".into()),
                    };
                }
                if self.is_word("await") {
                    return self.fail(
                        "for await (... of ...) is only valid in async functions and async \
                         generators"
                            .into(),
                    );
                }
                self.for_rest()
            }
            "break" | "continue" => {
                self.i += 1;
                let label = match self.tok() {
                    Tok::Ident(w) if !self.toks[self.i].newline_before && !is_reserved(w) => {
                        let w = w.clone();
                        self.i += 1;
                        Some(w)
                    }
                    _ => None,
                };
                self.semicolon()?;
                Ok(if &*word == "break" { Stmt::Break(label) } else { Stmt::Continue(label) })
            }
            "throw" => {
                self.i += 1;
                if self.toks[self.i].newline_before {
                    return self.fail(
                        "no line break is allowed between 'throw' and its expression".into(),
                    );
                }
                let e = self.expression()?;
                self.semicolon()?;
                Ok(Stmt::Throw(e, pos))
            }
            "try" => {
                self.i += 1;
                self.try_rest()
            }
            "switch" => {
                self.i += 1;
                self.switch_rest()
            }
            "debugger" => {
                self.i += 1;
                self.semicolon()?;
                Ok(Stmt::Empty)
            }
            "with" => {
                if self.strict {
                    return self.fail("strict mode code may not contain 'with' statements".into());
                }
                self.i += 1;
                self.expect("(")?;
                let o = self.expr_at()?;
                self.expect(")")?;
                Ok(Stmt::With(o, Box::new(self.statement()?)))
            }
            "import" | "export" => self.fail(format!("{word} is not supported")),
            _ => self.expression_statement(),
        }
    }

    /// The `(c)` of `if`, `while` and `do … while`.
    fn condition(&mut self) -> Result<Expr, SyntaxError> {
        self.expect_or("(", "missing ( before condition")?;
        let c = self.expr_at()?;
        self.expect_or(")", "missing ) after condition")?;
        Ok(c)
    }

    fn statement_name(&self, name: Option<Name>) -> Result<Name, SyntaxError> {
        name.map_or_else(|| self.fail("function statement requires a name".into()), Ok)
    }

    fn expression_statement(&mut self) -> Result<Stmt, SyntaxError> {
        let p = self.pos();
        let e = self.expression()?;
        let e = at(p, self.paren_call(e));
        self.semicolon()?;
        Ok(Stmt::Expr(e))
    }

    /// A call of a name with arguments that is a whole statement or a whole initializer is
    /// reported at its `(`, as SpiderMonkey does (`f(1);` and `let e = Error('m')`: the `(`;
    /// `var q = f()`, `x = Error('m')` and `throw Error('m')`: the name; measured on a `refrouter`
    /// 9.0.4 on 02-11).
    fn paren_call(&self, e: Expr) -> Expr {
        match e {
            Expr::Call { callee, args, optional, pos }
                if !args.is_empty() && matches!(*callee, Expr::Ident(..)) =>
            {
                let len = match &*callee {
                    Expr::Ident(n, _) => n.chars().count() as u32,
                    _ => 0,
                };
                let pos = self.paren_after(Pos { line: pos.line, col: pos.col + len });
                Expr::Call { callee, args, optional, pos }
            }
            e => e,
        }
    }

    /// Where the first `(` at or after `from` is in the source.
    fn paren_after(&self, from: Pos) -> Pos {
        let (mut line, mut col) = (self.origin.line, self.origin.col);
        let mut seen = false;
        for &c in &self.src {
            seen |= line > from.line || (line == from.line && col >= from.col);
            if seen && c == '(' {
                return Pos { line, col };
            }
            if c == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
        }
        from
    }

    /// An expression marked with where it starts.
    fn expr_at(&mut self) -> Result<Expr, SyntaxError> {
        let p = self.pos();
        Ok(at(p, self.expression()?))
    }

    fn assign_at(&mut self) -> Result<Expr, SyntaxError> {
        let p = self.pos();
        Ok(at(p, self.assign()?))
    }

    fn block_rest(&mut self) -> Result<Vec<Stmt>, SyntaxError> {
        let mut out = Vec::new();
        while !self.eat("}") {
            if self.at_eof() {
                return self.fail("missing } in compound statement".into());
            }
            out.push(self.statement()?);
        }
        Ok(out)
    }

    fn declarations(
        &mut self,
        kind: DeclKind,
        allow_in: bool,
    ) -> Result<Vec<(Pattern, Option<Expr>)>, SyntaxError> {
        let mut out = Vec::new();
        let decl = match kind {
            DeclKind::Var => Decl::Var,
            DeclKind::Let => Decl::Let,
            DeclKind::Const => Decl::Const,
        };
        loop {
            let outer = self.binding.replace(decl);
            let target = self.binding_pattern();
            self.binding = outer;
            let target = target?;
            let init = if self.eat("=") {
                let p = self.pos();
                let mark = self.namer.mark();
                let e = self.assign_in(allow_in)?;
                let e = self.paren_call(e);
                if let Pattern::Name(n) = &target {
                    self.namer.place(mark, n.to_string(), true, &e);
                }
                Some(at_always(p, e))
            } else {
                None
            };
            // The head of a `for (… in/of …)` was read before: here every declaration ends.
            if init.is_none() && !matches!(target, Pattern::Name(_)) {
                return self.fail("missing = in destructuring declaration".into());
            }
            if init.is_none() && kind == DeclKind::Const {
                return self.fail("missing = in const declaration".into());
            }
            out.push((target, init));
            if !self.eat(",") {
                return Ok(out);
            }
        }
    }

    fn binding_pattern(&mut self) -> Result<Pattern, SyntaxError> {
        if self.eat("[") {
            let mut items = Vec::new();
            let mut rest = None;
            while !self.eat("]") {
                if self.eat(",") {
                    items.push(None);
                    continue;
                }
                if self.eat("...") {
                    rest = Some(Box::new(self.binding_pattern()?));
                    self.expect("]")?;
                    break;
                }
                let p = self.binding_pattern()?;
                let d = if self.eat("=") { Some(self.default_value()?) } else { None };
                items.push(Some((p, d)));
                if !self.is("]") {
                    self.expect(",")?;
                }
            }
            return Ok(Pattern::Array(items, rest));
        }
        if self.eat("{") {
            let mut props = Vec::new();
            let mut rest = None;
            while !self.eat("}") {
                if self.eat("...") {
                    rest = Some(Box::new(Pattern::Name(self.binding_name()?)));
                    self.expect("}")?;
                    break;
                }
                let key_pos = self.pos();
                let key = self.prop_name()?;
                let target = if self.eat(":") {
                    self.binding_pattern()?
                } else {
                    match &key {
                        PropName::Fixed(n) => {
                            if let Some(d) = self.binding {
                                self.declare(n, d, key_pos)?;
                            }
                            Pattern::Name(n.clone())
                        }
                        PropName::Computed(_) | PropName::Private(_) => return self.unexpected(),
                    }
                };
                let d = if self.eat("=") { Some(self.default_value()?) } else { None };
                props.push((key, target, d));
                if !self.is("}") {
                    self.expect(",")?;
                }
            }
            return Ok(Pattern::Object(props, rest));
        }
        Ok(Pattern::Name(self.binding_name()?))
    }

    /// The default of a pattern's element: an expression, which binds nothing.
    fn default_value(&mut self) -> Result<Expr, SyntaxError> {
        let outer = self.binding.take();
        let e = self.assign();
        self.binding = outer;
        e
    }

    fn for_rest(&mut self) -> Result<Stmt, SyntaxError> {
        self.scoped(ScopeKind::Block, Self::for_rest_in)
    }

    fn for_rest_in(&mut self) -> Result<Stmt, SyntaxError> {
        self.expect("(")?;
        let decl = match self.tok() {
            Tok::Ident(w) if &**w == "var" => Some(DeclKind::Var),
            Tok::Ident(w) if &**w == "const" => Some(DeclKind::Const),
            Tok::Ident(w)
                if &**w == "let"
                    && matches!(self.peek_at(1), Tok::Ident(_) | Tok::Punct("[" | "{")) =>
            {
                Some(DeclKind::Let)
            }
            _ => None,
        };
        let init = if let Some(kind) = decl {
            self.i += 1;
            let start = self.i;
            let saved = self.scopes.clone();
            let decl = match kind {
                DeclKind::Var => Decl::Var,
                DeclKind::Let => Decl::Let,
                DeclKind::Const => Decl::Const,
            };
            let outer = self.binding.replace(decl);
            let target = self.binding_pattern();
            self.binding = outer;
            let target = target?;
            if self.is_word("of") || self.is_word("in") {
                let of = self.is_word("of");
                self.i += 1;
                let p = self.pos();
                let obj = if of { self.assign()? } else { self.expression()? };
                let obj = at_always(p, obj);
                self.expect(")")?;
                let body = Box::new(self.statement()?);
                return Ok(Stmt::ForIn { decl: Some(kind), target, of, obj, body });
            }
            self.i = start;
            self.scopes = saved;
            let d = self.declarations(kind, false)?;
            if self.is_word("in") || self.is_word("of") {
                return self.for_in_initializer(kind, d);
            }
            Some(ForInit::Decl(kind, d))
        } else if self.is(";") {
            None
        } else {
            let start = self.i;
            let cover = self.cover.len();
            let e = self.lhs()?;
            if self.is_word("of") || self.is_word("in") {
                let of = self.is_word("of");
                self.cover.truncate(cover);
                self.i += 1;
                let target = to_pattern(e).map_or_else(|m| self.fail(m), Ok)?;
                let p = self.pos();
                let obj = if of { self.assign()? } else { self.expression()? };
                let obj = at_always(p, obj);
                self.expect(")")?;
                let body = Box::new(self.statement()?);
                return Ok(Stmt::ForIn { decl: None, target, of, obj, body });
            }
            self.i = start;
            self.cover.truncate(cover);
            let e = self.expression_in(false)?;
            if self.is_word("in") || self.is_word("of") {
                return self.fail("invalid for-in/of left-hand side".into());
            }
            Some(ForInit::Expr(e))
        };
        self.expect_or(";", "missing ; after for-loop initializer")?;
        let test = if self.is(";") { None } else { Some(self.expr_at()?) };
        self.expect_or(";", "missing ; after for-loop condition")?;
        let update = if self.is(")") { None } else { Some(self.expr_at()?) };
        self.expect_or(")", "missing ) after for-loop control")?;
        Ok(Stmt::For(init, test, update, Box::new(self.statement()?)))
    }

    fn expect_or(&mut self, p: &str, message: &str) -> Result<(), SyntaxError> {
        if self.eat(p) { Ok(()) } else { self.fail(message.into()) }
    }

    /// `for (var x = 1 in o)`: Annex B.3.5 lets sloppy code give a `var` of a name an
    /// initializer, evaluated before the object; every other declaration with one is an error.
    fn for_in_initializer(
        &mut self,
        kind: DeclKind,
        mut d: Vec<(Pattern, Option<Expr>)>,
    ) -> Result<Stmt, SyntaxError> {
        if self.is_word("of") {
            return self.fail(
                "a declaration in the head of a for-of loop can't have an initializer".into(),
            );
        }
        if kind != DeclKind::Var {
            return self.fail(
                "a lexical declaration in the head of a for-in loop can't have an initializer"
                    .into(),
            );
        }
        if self.strict {
            return self.fail("for-in loop head declarations may not have initializers".into());
        }
        let name = match d.pop() {
            Some((Pattern::Name(n), init)) if d.is_empty() => {
                d.push((Pattern::Name(n.clone()), init));
                n
            }
            _ => return self.fail("missing ; after for-loop initializer".into()),
        };
        self.i += 1;
        let p = self.pos();
        let obj = at_always(p, self.expression()?);
        self.expect(")")?;
        let body = Box::new(self.statement()?);
        let target = Pattern::Name(name);
        Ok(Stmt::Block(vec![
            Stmt::Decl(DeclKind::Var, d),
            Stmt::ForIn { decl: Some(DeclKind::Var), target, of: false, obj, body },
        ]))
    }

    fn try_rest(&mut self) -> Result<Stmt, SyntaxError> {
        self.expect("{")?;
        let body = self.scoped(ScopeKind::Block, Self::block_rest)?;
        let catch = if self.eat_word("catch") {
            Some(self.scoped(ScopeKind::Catch, Self::catch_rest)?)
        } else {
            None
        };
        let finally = if self.eat_word("finally") {
            self.expect("{")?;
            Some(self.scoped(ScopeKind::Block, Self::block_rest)?)
        } else {
            None
        };
        if catch.is_none() && finally.is_none() {
            return self.fail("missing catch or finally after try".into());
        }
        Ok(Stmt::Try(body, catch, finally))
    }

    /// A catch clause after `catch`: its parameter and block share a scope.
    fn catch_rest(&mut self) -> Result<(Option<Pattern>, Vec<Stmt>), SyntaxError> {
        let param = if self.eat("(") {
            let simple = matches!(self.tok(), Tok::Ident(_));
            let outer = self.binding.replace(Decl::Catch { simple });
            let p = self.binding_pattern();
            self.binding = outer;
            let p = p?;
            self.expect(")")?;
            Some(p)
        } else {
            None
        };
        self.expect("{")?;
        Ok((param, self.block_rest()?))
    }

    fn switch_rest(&mut self) -> Result<Stmt, SyntaxError> {
        self.expect("(")?;
        let d = self.expr_at()?;
        self.expect(")")?;
        self.expect("{")?;
        self.scoped(ScopeKind::Block, |p| p.cases(d))
    }

    /// A `switch`'s cases after `{`.
    fn cases(&mut self, d: Expr) -> Result<Stmt, SyntaxError> {
        let mut cases = Vec::new();
        let mut has_default = false;
        while !self.eat("}") {
            let test = if self.eat_word("case") {
                Some(self.expression()?)
            } else if self.is_word("default") {
                if has_default {
                    return self.fail("more than one switch default".into());
                }
                has_default = true;
                self.i += 1;
                None
            } else {
                return self.fail("invalid switch statement".into());
            };
            self.expect(":")?;
            let mut body = Vec::new();
            while !self.is("}") && !self.is_word("case") && !self.is_word("default") {
                if self.at_eof() {
                    return self.fail("expected expression, got end of script".into());
                }
                body.push(self.statement()?);
            }
            cases.push((test, body));
        }
        Ok(Stmt::Switch(d, cases))
    }

    // Functions and classes.

    fn source_from(&self, start: Pos) -> Rc<str> {
        // The source text from `start` to the end of the previous token.
        let end = &self.toks[self.i.saturating_sub(1)];
        let (mut line, mut col, mut idx) = (self.origin.line, self.origin.col, 0usize);
        let mut from = None;
        while idx < self.src.len() {
            if from.is_none() && line == start.line && col == start.col {
                from = Some(idx);
            }
            if line == end.line && col == end.col {
                break;
            }
            if self.src[idx] == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
            idx += 1;
        }
        let from = from.unwrap_or(0);
        // The last token's length: through the closing brace or the expression's end.
        let mut stop = idx + 1;
        if let Tok::Punct(p) = &end.tok {
            stop = idx + p.chars().count();
        } else {
            while stop < self.src.len()
                && !self.src[stop].is_whitespace()
                && !";,)]}".contains(self.src[stop])
            {
                stop += 1;
            }
        }
        self.src[from..stop.min(self.src.len())].iter().collect::<String>().into()
    }

    /// The parameters and the names they bind, where each is. Duplicate names: only a plain
    /// list of a function that is not an arrow or a method may have them, in sloppy code.
    fn params(
        &mut self,
        unique: bool,
        unclosed: &str,
    ) -> Result<(Params, Vec<(Name, Pos)>), SyntaxError> {
        let outer =
            (self.binding.replace(Decl::Param), std::mem::replace(&mut self.in_params, true));
        let r = self.params_in(unclosed);
        (self.binding, self.in_params) = outer;
        let ((params, rest), seen) = r?;
        let simple = rest.is_none()
            && params.iter().all(|(p, d)| d.is_none() && matches!(p, Pattern::Name(_)));
        if let Some((_, p)) = first_duplicate(&seen).filter(|_| unique || !simple) {
            return self.fail_at(p, "duplicate argument names not allowed in this context".into());
        }
        self.strict_params(&seen)?;
        Ok(((params, rest), seen))
    }

    /// In strict code no two parameters have the same name.
    fn strict_params(&self, seen: &[(Name, Pos)]) -> Result<(), SyntaxError> {
        match first_duplicate(seen).filter(|_| self.strict) {
            Some((n, p)) => self.fail_at(p, format!("duplicate formal argument {n}")),
            None => Ok(()),
        }
    }

    /// The parameter list and the names it binds, where each is.
    fn params_in(&mut self, unclosed: &str) -> Result<(Params, Vec<(Name, Pos)>), SyntaxError> {
        self.expect_or("(", "missing ( before formal parameters")?;
        let mut params = Vec::new();
        let mut rest = None;
        let mut seen = Vec::new();
        while !self.eat(")") {
            if self.eat("...") {
                let at = self.pos();
                let p = self.binding_pattern()?;
                seen.extend(bound_names(&p).into_iter().map(|n| (n, at)));
                rest = Some(p);
                if self.is(",") {
                    return self.fail("parameter after rest parameter".into());
                }
                self.expect_or(")", unclosed)?;
                break;
            }
            let at = self.pos();
            let p = self.binding_pattern()?;
            seen.extend(bound_names(&p).into_iter().map(|n| (n, at)));
            let d = if self.eat("=") { Some(self.default_value()?) } else { None };
            params.push((p, d));
            if !self.is(")") {
                self.expect_or(",", unclosed)?;
            }
        }
        Ok(((params, rest), seen))
    }

    /// `function` (and `async` before it) already read: an optional `*`, the name, parameters
    /// and body.
    fn function_rest(
        &mut self,
        start: Pos,
        method: bool,
        is_async: bool,
        expression: bool,
    ) -> Result<Rc<FuncDef>, SyntaxError> {
        let generator = self.eat("*");
        // A function expression's name belongs to the function: `yield` is a name unless it is
        // a generator's (ECMA-262 §15.2, §15.5).
        let outer = self.in_gen;
        if expression {
            self.in_gen = generator;
        }
        let name = if !method && matches!(self.tok(), Tok::Ident(_)) && !self.is("(") {
            self.ident()
        } else {
            Ok("".into())
        };
        self.in_gen = outer;
        let name = name?;
        let name = if name.is_empty() { None } else { Some(name) };
        self.function_body(start, name, false, FnKind { generator, is_async, ..FnKind::default() })
    }

    fn function_body(
        &mut self,
        start: Pos,
        name: Option<Name>,
        derived_ctor: bool,
        kind: FnKind,
    ) -> Result<Rc<FuncDef>, SyntaxError> {
        let outer = (self.strict, self.in_gen, self.in_async, self.in_static);
        let supers = (self.super_prop, self.super_call);
        self.in_gen = kind.generator;
        self.in_async = kind.is_async;
        self.in_static = false;
        (self.super_prop, self.super_call) = (kind.method || kind.ctor, derived_ctor);
        let entry = self.namer.enter();
        let r = self.nested(crate::recursion::FUNCTION, true, |p| {
            p.scoped(ScopeKind::Function, |p| p.function_body_in(start, name, derived_ctor, kind))
        });
        self.namer.leave(entry, r.as_ref().ok());
        (self.strict, self.in_gen, self.in_async, self.in_static) = outer;
        (self.super_prop, self.super_call) = supers;
        r
    }

    fn function_body_in(
        &mut self,
        start: Pos,
        name: Option<Name>,
        derived_ctor: bool,
        kind: FnKind,
    ) -> Result<Rc<FuncDef>, SyntaxError> {
        let outer_strict = self.strict;
        let from = self.i;
        let ((params, rest), seen) =
            self.params(kind.method || kind.ctor, "missing ) after formal parameters")?;
        self.expect("{")?;
        // A directive is the first statement: it is read before the rest of the body.
        if matches!(self.tok(), Tok::Str(s) if &**s == "use strict") {
            let what = if rest.is_some() {
                Some("rest")
            } else if params.iter().any(|(p, _)| !matches!(p, Pattern::Name(_))) {
                Some("destructuring")
            } else if params.iter().any(|(_, d)| d.is_some()) {
                Some("default")
            } else {
                None
            };
            if let Some(what) = what {
                return self
                    .fail(format!("\"use strict\" not allowed in function with {what} parameter"));
            }
            self.strict = true;
            self.strict_params(&seen)?;
        }
        let body = self.block_rest();
        let strict = self.strict || body.as_deref().is_ok_and(has_use_strict);
        let body = body?;
        if strict && !outer_strict {
            self.check_legacy(from)?;
        }
        let body =
            if kind.generator && !kind.is_async { crate::generators::lower(body) } else { body };
        Ok(Rc::new(FuncDef {
            name,
            params,
            rest,
            body,
            arrow: false,
            is_ctor_of_derived: derived_ctor,
            strict,
            generator: kind.generator,
            is_async: kind.is_async,
            method: kind.method,
            source: self.source_from(start),
            guessed: std::cell::OnceCell::new(),
            annex: std::cell::Cell::new(false),
        }))
    }

    /// A class: its code is strict.
    fn class_rest(&mut self) -> Result<Rc<ClassDef>, SyntaxError> {
        let outer = self.strict;
        let from = self.i;
        self.strict = true;
        let c = self.class_body();
        self.strict = outer;
        let c = c?;
        if !outer {
            self.check_legacy(from)?;
        }
        Ok(c)
    }

    fn class_body(&mut self) -> Result<Rc<ClassDef>, SyntaxError> {
        let pos = {
            let t = &self.toks[self.i.saturating_sub(1)];
            Pos { line: t.line, col: t.col }
        };
        let name = if matches!(self.tok(), Tok::Ident(w) if &**w != "extends") {
            Some(self.ident()?)
        } else {
            None
        };
        let parent = if self.eat_word("extends") { Some(Box::new(self.lhs()?)) } else { None };
        self.expect("{")?;
        self.privates.push(PrivateScope::default());
        let mut ctor = None;
        let mut members = Vec::new();
        while !self.eat("}") {
            if self.eat(";") {
                continue;
            }
            let start = self.pos();
            let is_static =
                self.is_word("static") && !matches!(self.peek_at(1), Tok::Punct("(" | "=")) && {
                    self.i += 1;
                    true
                };
            if is_static && self.is("{") {
                self.i += 1;
                let f = self.static_block(start)?;
                let key = PropName::Fixed(crate::interp::STATIC_BLOCK.into());
                members.push(ClassMember {
                    is_static,
                    key,
                    kind: PropKind::Value,
                    field: None,
                    func: Some(f),
                });
                continue;
            }
            let mut kind = PropKind::Value;
            if (self.is_word("get") || self.is_word("set"))
                && !matches!(self.peek_at(1), Tok::Punct("(" | "=" | ";" | "}"))
            {
                kind = if self.is_word("get") { PropKind::Get } else { PropKind::Set };
                self.i += 1;
            }
            let fk = self.method_prefix(kind);
            let name_pos = self.pos();
            let key = match self.tok().clone() {
                Tok::Private(n) => {
                    self.i += 1;
                    let what = match kind {
                        _ if !self.is("(") => PrivateKind::Field,
                        PropKind::Get => PrivateKind::Getter,
                        PropKind::Set => PrivateKind::Setter,
                        PropKind::Value => PrivateKind::Method,
                    };
                    self.declare_private(&n, what, is_static, name_pos)?;
                    PropName::Private(n)
                }
                _ => self.prop_name()?,
            };
            self.class_member_rules(&key, is_static, kind, fk, start, name_pos, ctor.is_some())?;
            if self.is("(") {
                let is_ctor =
                    !is_static && matches!(&key, PropName::Fixed(n) if &**n == "constructor");
                let fk = FnKind { method: !is_ctor, ctor: is_ctor, ..fk };
                let f = self.function_body(start, None, is_ctor && parent.is_some(), fk)?;
                if let PropName::Fixed(n) | PropName::Private(n) = &key
                    && !is_ctor
                {
                    self.namer.method(&f, accessor_name(kind, n));
                }
                if is_ctor {
                    ctor = Some(f);
                } else {
                    members.push(ClassMember { is_static, key, kind, field: None, func: Some(f) });
                }
            } else {
                let supers = (self.super_prop, self.super_call);
                (self.super_prop, self.super_call) = (true, false);
                let init = if self.eat("=") { self.assign() } else { Ok(Expr::Undefined) };
                (self.super_prop, self.super_call) = supers;
                let init = Some(init?);
                self.semicolon()?;
                members.push(ClassMember { is_static, key, kind, field: init, func: None });
            }
        }
        self.end_private_scope()?;
        let source = self.source_from(pos);
        Ok(Rc::new(ClassDef { pos, name, parent, ctor, members, source }))
    }

    /// A use of the private name `n` at `pos`, checked when its class ends (measured on a
    /// `refrouter` 9.0.4 on 02-11: a name no class around declares is a `SyntaxError`).
    fn private_ref(&mut self, n: Name, pos: Pos) -> Result<(), SyntaxError> {
        match self.privates.last_mut() {
            Some(scope) => {
                scope.used.push((n, pos));
                Ok(())
            }
            None if self.loose_privates => Ok(()),
            None => {
                self.fail_at(pos, format!("reference to undeclared private field or method {n}"))
            }
        }
    }

    /// `#x` after `.` or `?.`: a private member.
    fn private_member(&mut self) -> Result<Option<MemberProp>, SyntaxError> {
        let Tok::Private(n) = self.tok().clone() else { return Ok(None) };
        let pos = self.pos();
        self.i += 1;
        self.private_ref(n.clone(), pos)?;
        Ok(Some(MemberProp::Private(n)))
    }

    /// A private name a class declares: a name once, or a getter and a setter both static or
    /// both not (measured on a `refrouter` 9.0.4 on 02-11, with its message for any other repeat).
    fn declare_private(
        &mut self,
        n: &Name,
        kind: PrivateKind,
        is_static: bool,
        pos: Pos,
    ) -> Result<(), SyntaxError> {
        let declared = self.privates.last().map_or(&[][..], |s| &s.declared[..]);
        if let Some(&(_, k, was_static)) = declared.iter().find(|(m, ..)| m == n) {
            let pair = matches!(
                (k, kind),
                (PrivateKind::Getter, PrivateKind::Setter)
                    | (PrivateKind::Setter, PrivateKind::Getter)
            ) && was_static == is_static;
            if !pair {
                let what = if (k, kind) == (PrivateKind::Method, PrivateKind::Method) {
                    "method"
                } else {
                    "name"
                };
                return self.fail_at(
                    pos,
                    format!("getter and setter for private {what} {n} should either be both static or non-static"),
                );
            }
        }
        if let Some(scope) = self.privates.last_mut() {
            scope.declared.push((n.clone(), kind, is_static));
        }
        Ok(())
    }

    /// Ends a class's private names: the uses it does not declare go to the class around it.
    fn end_private_scope(&mut self) -> Result<(), SyntaxError> {
        let scope = self.privates.pop().unwrap_or_default();
        for (n, pos) in scope.used {
            if !scope.declared.iter().any(|(m, ..)| *m == n) {
                self.private_ref(n, pos)?;
            }
        }
        Ok(())
    }

    /// The early errors of a class member (measured on a `refrouter` 9.0.4 on 02-11): a
    /// `constructor` that is not a plain method or comes twice, and a static `prototype`.
    #[allow(clippy::too_many_arguments)]
    fn class_member_rules(
        &self,
        key: &PropName,
        is_static: bool,
        kind: PropKind,
        fk: FnKind,
        start: Pos,
        name_pos: Pos,
        has_ctor: bool,
    ) -> Result<(), SyntaxError> {
        if matches!(key, PropName::Private(n) if &**n == "#constructor") {
            return self.fail_at(
                start,
                "a class constructor definition must be a plain public method".into(),
            );
        }
        let PropName::Fixed(name) = key else { return Ok(()) };
        let method = self.is("(");
        if &**name == "constructor" && (!method || !is_static) {
            if !method || kind != PropKind::Value || fk.generator || fk.is_async {
                return self.fail_at(
                    start,
                    "a class constructor definition must be a plain public method".into(),
                );
            }
            if has_ctor {
                return self.fail_at(
                    start,
                    "a class cannot have more than one constructor definition".into(),
                );
            }
        }
        if is_static && &**name == "prototype" {
            return self.fail_at(
                name_pos,
                "a class may not have a static property named 'prototype'".into(),
            );
        }
        Ok(())
    }

    /// A class's `static { … }` after `{`: a method of the class with no parameters.
    fn static_block(&mut self, start: Pos) -> Result<Rc<FuncDef>, SyntaxError> {
        let outer = (self.in_gen, self.in_async, self.in_static, self.super_prop, self.super_call);
        (self.in_gen, self.in_async, self.in_static, self.super_prop, self.super_call) =
            (false, false, true, true, false);
        let body = self.scoped(ScopeKind::Function, Self::block_rest);
        (self.in_gen, self.in_async, self.in_static, self.super_prop, self.super_call) = outer;
        Ok(Rc::new(FuncDef {
            name: None,
            params: Vec::new(),
            rest: None,
            body: body?,
            arrow: false,
            is_ctor_of_derived: false,
            strict: true,
            generator: false,
            is_async: false,
            method: true,
            source: self.source_from(start),
            guessed: std::cell::OnceCell::new(),
            annex: std::cell::Cell::new(false),
        }))
    }

    fn prop_name(&mut self) -> Result<PropName, SyntaxError> {
        match self.tok().clone() {
            Tok::Ident(w) => {
                self.i += 1;
                Ok(PropName::Fixed(w))
            }
            Tok::Str(s) => {
                self.i += 1;
                Ok(PropName::Fixed(s))
            }
            Tok::Num(n) => {
                self.i += 1;
                Ok(PropName::Fixed(crate::number::to_string(n).into()))
            }
            Tok::BigInt(b) => {
                self.i += 1;
                Ok(PropName::Fixed(b.to_string().into()))
            }
            Tok::Punct("[") => {
                self.i += 1;
                let e = self.assign()?;
                self.expect("]")?;
                Ok(PropName::Computed(Box::new(e)))
            }
            Tok::Private(_) => self.fail("private fields aren't valid in this context".into()),
            _ => self.fail(format!("invalid property name, got {}", describe(self.tok()))),
        }
    }

    // Expressions.

    fn expression(&mut self) -> Result<Expr, SyntaxError> {
        self.expression_in(true)
    }

    fn expression_in(&mut self, allow_in: bool) -> Result<Expr, SyntaxError> {
        let first = self.assign_in(allow_in)?;
        if !self.is(",") {
            return Ok(first);
        }
        let mut all = vec![first];
        while self.eat(",") {
            let p = self.pos();
            all.push(at(p, self.assign_in(allow_in)?));
        }
        Ok(Expr::Seq(all))
    }

    fn assign(&mut self) -> Result<Expr, SyntaxError> {
        self.assign_in(true)
    }

    fn is_arrow_ahead(&self) -> bool {
        match self.tok() {
            Tok::Ident(w) if !is_reserved(w) => matches!(self.peek_at(1), Tok::Punct("=>")),
            Tok::Punct("(") => {
                let mut depth = 0usize;
                let mut k = self.i;
                while k < self.toks.len() {
                    match &self.toks[k].tok {
                        Tok::Punct("(" | "[" | "{") => depth += 1,
                        Tok::Punct(")" | "]" | "}") => {
                            depth -= 1;
                            if depth == 0 {
                                return matches!(
                                    self.toks.get(k + 1).map(|t| &t.tok),
                                    Some(Tok::Punct("=>"))
                                );
                            }
                        }
                        Tok::Eof => return false,
                        _ => {}
                    }
                    k += 1;
                }
                false
            }
            _ => false,
        }
    }

    fn arrow(&mut self, start: Pos, is_async: bool) -> Result<Expr, SyntaxError> {
        let outer = (self.strict, self.in_gen, self.in_async, self.in_static);
        self.in_gen = false;
        self.in_async = is_async;
        self.in_static = false;
        let entry = self.namer.enter();
        // The engine does not check the depth of arrow functions: its stack runs out.
        self.arrows += 1;
        let r = if self.arrows > self.room.arrows {
            Err(SyntaxError { message: STACK_EXHAUSTED.into(), line: 0, col: 0 })
        } else {
            self.scoped(ScopeKind::Function, |p| p.arrow_in(start, is_async))
        };
        self.arrows -= 1;
        let def = match &r {
            Ok(Expr::Func(d)) => Some(d),
            _ => None,
        };
        self.namer.leave(entry, def);
        (self.strict, self.in_gen, self.in_async, self.in_static) = outer;
        r
    }

    fn arrow_in(&mut self, start: Pos, is_async: bool) -> Result<Expr, SyntaxError> {
        let (outer_strict, from) = (self.strict, self.i);
        let (params, rest) = if let Tok::Ident(_) = self.tok() {
            (vec![(Pattern::Name(self.ident()?), None)], None)
        } else {
            self.params(true, "missing ) in parenthetical")?.0
        };
        self.expect("=>")?;
        let body = if self.eat("{") {
            if matches!(self.tok(), Tok::Str(s) if &**s == "use strict") {
                self.strict = true;
            }
            self.block_rest()
        } else {
            self.assign().map(|e| vec![Stmt::Return(Some(e))])
        };
        let strict = self.strict;
        let body = body?;
        if strict && !outer_strict {
            self.check_legacy(from)?;
        }
        Ok(Expr::Func(Rc::new(FuncDef {
            name: None,
            params,
            rest,
            body,
            arrow: true,
            is_ctor_of_derived: false,
            strict,
            generator: false,
            is_async,
            method: false,
            source: self.source_from(start),
            guessed: std::cell::OnceCell::new(),
            annex: std::cell::Cell::new(false),
        })))
    }

    fn assign_in(&mut self, allow_in: bool) -> Result<Expr, SyntaxError> {
        self.enter()?;
        let r = self.assign_inner(allow_in);
        self.depth -= 1;
        r
    }

    fn assign_inner(&mut self, allow_in: bool) -> Result<Expr, SyntaxError> {
        let start = self.pos();
        if self.is_word("async")
            && self.toks.get(self.i + 1).is_some_and(|t| !t.newline_before)
            && matches!(self.peek_at(1), Tok::Ident(_) | Tok::Punct("("))
        {
            self.i += 1;
            if self.is_arrow_ahead() {
                return self.arrow(start, true);
            }
            self.i -= 1;
        }
        if self.in_gen && self.is_word("yield") {
            return self.yield_expr(allow_in);
        }
        if self.is_arrow_ahead() {
            return self.arrow(start, false);
        }
        let cover = self.cover.len();
        let left = self.conditional(allow_in)?;
        let op = match self.tok() {
            Tok::Punct(p)
                if matches!(
                    *p,
                    "=" | "+="
                        | "-="
                        | "*="
                        | "/="
                        | "%="
                        | "**="
                        | "<<="
                        | ">>="
                        | ">>>="
                        | "&="
                        | "|="
                        | "^="
                        | "&&="
                        | "||="
                        | "??="
                ) =>
            {
                *p
            }
            _ => return Ok(left),
        };
        self.i += 1;
        let target = if op == "=" {
            self.cover.truncate(cover);
            to_pattern(left)
        } else {
            match left {
                e @ (Expr::Ident(..) | Expr::Member { .. }) => Ok(Pattern::Expr(Box::new(e))),
                _ => Err("invalid assignment left-hand side".to_owned()),
            }
        }
        .map_or_else(|m| self.fail(m), Ok)?;
        let mark = self.namer.mark();
        let value = self.nested(crate::recursion::UNARY, true, |p| p.assign_in(allow_in))?;
        if op == "=" {
            match &target {
                Pattern::Name(n) => self.namer.place(mark, n.to_string(), true, &value),
                Pattern::Expr(e) if matches!(**e, Expr::Member { .. }) => {
                    self.namer.place(mark, crate::interp::callee_text(e), false, &value)
                }
                _ => {}
            }
        }
        Ok(Expr::Assign { op, target: Box::new(target), value: Box::new(value) })
    }

    fn conditional(&mut self, allow_in: bool) -> Result<Expr, SyntaxError> {
        let c = self.binary(0, allow_in)?;
        if !self.eat("?") {
            return Ok(c);
        }
        let cost = crate::recursion::CONDITIONAL;
        let a = self.nested(cost, true, |p| p.assign_in(true))?;
        self.expect(":")?;
        let b = self.nested(cost, true, |p| p.assign_in(allow_in))?;
        Ok(Expr::Cond(Box::new(c), Box::new(a), Box::new(b)))
    }

    fn binary_op(&self, allow_in: bool) -> Option<(&'static str, u8)> {
        let op: &'static str = match self.tok() {
            Tok::Punct(p) => p,
            Tok::Ident(w) if &**w == "instanceof" => "instanceof",
            Tok::Ident(w) if &**w == "in" && allow_in => "in",
            _ => return None,
        };
        let prec = match op {
            "??" => 1,
            "||" => 2,
            "&&" => 3,
            "|" => 4,
            "^" => 5,
            "&" => 6,
            "==" | "!=" | "===" | "!==" => 7,
            "<" | ">" | "<=" | ">=" | "instanceof" | "in" => RELATIONAL,
            "<<" | ">>" | ">>>" => 9,
            "+" | "-" => 10,
            "*" | "/" | "%" => 11,
            "**" => 12,
            _ => return None,
        };
        Some((op, prec))
    }

    fn binary(&mut self, min: u8, allow_in: bool) -> Result<Expr, SyntaxError> {
        let mut left = match self.tok().clone() {
            Tok::Private(n) if matches!(self.peek_at(1), Tok::Ident(w) if &**w == "in") => {
                // `#x in o` (ECMA-262 §13.10): a relational expression whose left is a name.
                if min > RELATIONAL {
                    return self
                        .fail("invalid use of private name due to operator precedence".into());
                }
                let pos = self.pos();
                self.i += 2;
                self.private_ref(n.clone(), pos)?;
                let right = self.binary(RELATIONAL + 1, allow_in)?;
                Expr::PrivateIn(n, Box::new(right))
            }
            _ => self.unary()?,
        };
        while let Some((op, prec)) = self.binary_op(allow_in) {
            if prec < min {
                break;
            }
            self.i += 1;
            // `**` is right-associative.
            let next = if op == "**" { prec } else { prec + 1 };
            let rp = self.pos();
            let right = self.binary(next, allow_in)?;
            left = match op {
                "&&" | "||" | "??" => Expr::Logical(op, Box::new(left), Box::new(right)),
                _ => Expr::Binary(op, Box::new(left), Box::new(at(rp, right))),
            };
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, SyntaxError> {
        self.enter()?;
        // An operator's operand takes the engine's stack; a plain operand does not.
        let operator = match self.tok() {
            Tok::Punct(op) => matches!(*op, "!" | "-" | "+" | "~" | "++" | "--"),
            Tok::Ident(w) => matches!(&**w, "typeof" | "void" | "delete" | "await"),
            _ => false,
        };
        let cost = if operator { crate::recursion::UNARY } else { 0 };
        let r = self.nested(cost, true, Self::unary_inner);
        self.depth -= 1;
        r
    }

    fn unary_inner(&mut self) -> Result<Expr, SyntaxError> {
        match self.tok().clone() {
            Tok::Punct(op @ ("!" | "-" | "+" | "~")) => {
                let p = self.pos();
                self.i += 1;
                let e = self.unary()?;
                if self.is("**") {
                    return self.fail("unparenthesized unary expression can't appear on the left-hand side of '**'".into());
                }
                Ok(at(p, Expr::Unary(op, Box::new(e))))
            }
            Tok::Punct(op @ ("++" | "--")) => {
                self.i += 1;
                let t = self.unary()?;
                if !matches!(t, Expr::Ident(..) | Expr::Member { .. }) {
                    return self.fail("invalid increment/decrement operand".into());
                }
                Ok(Expr::Update { op, prefix: true, target: Box::new(t) })
            }
            Tok::Ident(w) if &*w == "typeof" => {
                let p = self.pos();
                self.i += 1;
                Ok(at(p, Expr::Typeof(Box::new(self.unary()?))))
            }
            Tok::Ident(w) if &*w == "void" => {
                let p = self.pos();
                self.i += 1;
                Ok(at(p, Expr::Unary("void", Box::new(self.unary()?))))
            }
            Tok::Ident(w) if &*w == "delete" => {
                let p = self.pos();
                self.i += 1;
                let target = self.unary()?;
                if deletes_private(&target) {
                    return self.fail_at(p, "private fields can't be deleted".into());
                }
                Ok(at(p, Expr::Delete(Box::new(target))))
            }
            Tok::Ident(w) if &*w == "await" && self.in_async => {
                self.i += 1;
                Ok(Expr::Await(Box::new(self.unary()?)))
            }
            // Outside async functions `await` is a name; one followed on its line by an operand
            // was meant as an operator.
            Tok::Ident(w)
                if &*w == "await"
                    && !self.toks[self.i + 1].newline_before
                    && matches!(self.peek_at(1), Tok::Num(_) | Tok::Str(_) | Tok::BigInt(_))
                        | matches!(self.peek_at(1), Tok::Ident(n) if !is_operator_word(n)) =>
            {
                self.fail(
                    "await is only valid in async functions, async generators and modules".into(),
                )
            }
            _ => {
                let e = self.lhs()?;
                if let Tok::Punct(op @ ("++" | "--")) = self.tok().clone()
                    && !self.toks[self.i].newline_before
                {
                    if !matches!(e, Expr::Ident(..) | Expr::Member { .. }) {
                        return self.fail("invalid increment/decrement operand".into());
                    }
                    self.i += 1;
                    return Ok(Expr::Update { op, prefix: false, target: Box::new(e) });
                }
                Ok(e)
            }
        }
    }

    fn arguments(&mut self) -> Result<Vec<Expr>, SyntaxError> {
        self.nested(crate::recursion::ARGUMENTS, true, Self::arguments_in)
    }

    fn arguments_in(&mut self) -> Result<Vec<Expr>, SyntaxError> {
        let mut args = Vec::new();
        while !self.eat(")") {
            if self.eat("...") {
                args.push(Expr::Spread(Box::new(self.assign_at()?)));
            } else {
                args.push(self.assign_at()?);
            }
            if !self.is(")") && !self.eat(",") {
                return self.fail("missing ) after argument list".into());
            }
        }
        Ok(args)
    }

    /// Member access, calls and `new` (§13.3).
    fn lhs(&mut self) -> Result<Expr, SyntaxError> {
        let pos = self.pos();
        let mut e = if self.eat_word("new") {
            if self.eat(".") {
                let _ = self.prop_ident()?;
                Expr::Undefined
            } else {
                let callee = self.member_only()?;
                let args = if self.eat("(") { self.arguments()? } else { Vec::new() };
                Expr::New { callee: Box::new(callee), args, pos }
            }
        } else {
            self.primary()?
        };
        let mut chained = false;
        loop {
            let pos = self.pos();
            if self.eat(".") {
                let pos = self.pos();
                let prop = match self.private_member()? {
                    Some(p) => p,
                    None => MemberProp::Name(self.prop_ident()?),
                };
                e = Expr::Member { obj: Box::new(e), prop: Box::new(prop), optional: false, pos };
            } else if self.eat("?.") {
                chained = true;
                if self.eat("(") {
                    let args = self.arguments()?;
                    let pos = call_pos(&e, pos);
                    e = Expr::Call { callee: Box::new(e), args, optional: true, pos };
                } else if self.eat("[") {
                    let p = self.expression()?;
                    self.expect("]")?;
                    e = Expr::Member {
                        obj: Box::new(e),
                        prop: Box::new(MemberProp::Computed(p)),
                        optional: true,
                        pos,
                    };
                } else {
                    let prop = match self.private_member()? {
                        Some(p) => p,
                        None => MemberProp::Name(self.prop_ident()?),
                    };
                    e = Expr::Member {
                        obj: Box::new(e),
                        prop: Box::new(prop),
                        optional: true,
                        pos,
                    };
                }
            } else if self.eat("[") {
                let pos = self.pos();
                let p = self.expression()?;
                self.expect("]")?;
                e = Expr::Member {
                    obj: Box::new(e),
                    prop: Box::new(MemberProp::Computed(p)),
                    optional: false,
                    pos,
                };
            } else if self.eat("(") {
                let args = self.arguments()?;
                let pos = call_pos(&e, pos);
                e = Expr::Call { callee: Box::new(e), args, optional: false, pos };
            } else if let Tok::Template(parts, subs) = self.tok().clone() {
                // A tagged template: the tag gets the strings and the values.
                self.i += 1;
                let mut args = vec![Expr::TemplateObject(parts)];
                for s in &subs {
                    args.push(self.substitution(s)?);
                }
                e = Expr::Call { callee: Box::new(e), args, optional: false, pos };
            } else {
                break;
            }
        }
        Ok(if chained { Expr::OptChain(Box::new(e)) } else { e })
    }

    /// The callee of `new`: member accesses without calls.
    fn member_only(&mut self) -> Result<Expr, SyntaxError> {
        let pos = self.pos();
        let mut e = if self.eat_word("new") {
            let callee = self.member_only()?;
            let args = if self.eat("(") { self.arguments()? } else { Vec::new() };
            Expr::New { callee: Box::new(callee), args, pos }
        } else {
            self.primary()?
        };
        loop {
            let pos = self.pos();
            if self.eat(".") {
                let prop = match self.private_member()? {
                    Some(p) => p,
                    None => MemberProp::Name(self.prop_ident()?),
                };
                e = Expr::Member { obj: Box::new(e), prop: Box::new(prop), optional: false, pos };
            } else if self.eat("[") {
                let p = self.expression()?;
                self.expect("]")?;
                e = Expr::Member {
                    obj: Box::new(e),
                    prop: Box::new(MemberProp::Computed(p)),
                    optional: false,
                    pos,
                };
            } else {
                return Ok(e);
            }
        }
    }

    /// A template's substitution, positioned where it starts in the source.
    fn substitution(&self, sub: &(String, u32, u32)) -> Result<Expr, SyntaxError> {
        let (src, line, col) = sub;
        let supers = (self.super_prop, self.super_call);
        let room = Room {
            units: self.room.units.saturating_sub(self.units),
            arrows: self.room.arrows.saturating_sub(self.arrows),
            ..self.room
        };
        parse_expression(src, *line, *col, self.in_gen, self.in_async, supers, room)
    }

    /// An array literal after its `[`.
    fn array_literal(&mut self) -> Result<Expr, SyntaxError> {
        let mut items = Vec::new();
        while !self.eat("]") {
            if self.eat(",") {
                items.push(None);
                continue;
            }
            let item = if self.eat("...") {
                Expr::Spread(Box::new(self.assign_at()?))
            } else {
                self.assign_at()?
            };
            items.push(Some(item));
            if !self.is("]") {
                self.expect_or(",", "missing ] after element list")?;
            }
        }
        Ok(Expr::Array(items))
    }

    fn primary(&mut self) -> Result<Expr, SyntaxError> {
        let pos = self.pos();
        let t = self.tok().clone();
        self.i += 1;
        Ok(match t {
            Tok::Private(_) => {
                self.i -= 1;
                return self.fail("private names aren't valid in this context".into());
            }
            Tok::Num(n) => Expr::Num(n),
            Tok::BigInt(b) => Expr::BigInt(b),
            Tok::Str(s) => Expr::Str(s),
            Tok::Template(parts, subs) => {
                if let Some(e) = &parts.invalid {
                    let p = Pos { line: e.line, col: e.col };
                    return self.fail_at(p, e.message.clone());
                }
                let subs = self.nested(crate::recursion::PAREN, true, |p| {
                    subs.iter().map(|s| p.substitution(s)).collect::<Result<_, _>>()
                })?;
                let strs = parts.cooked.iter().map(|c| c.clone().unwrap_or_default()).collect();
                Expr::Template(strs, subs)
            }
            Tok::Regex(b, f) => {
                if let Err(m) = crate::regex::syntax(&b, &f) {
                    self.i -= 1;
                    return self.fail(m);
                }
                Expr::Regex(b, f)
            }
            Tok::Punct("(") => self.nested(crate::recursion::PAREN, true, |p| {
                let e = p.expression()?;
                p.expect_or(")", "missing ) in parenthetical")?;
                Ok(e)
            })?,
            Tok::Punct("[") => self.nested(crate::recursion::PAREN, true, Self::array_literal)?,
            Tok::Punct("{") => self.nested(crate::recursion::OBJECT, true, Self::object_literal)?,
            Tok::Ident(w) => match &*w {
                "function" => Expr::Func(self.function_rest(pos, false, false, true)?),
                "async"
                    if matches!(self.tok(), Tok::Ident(w) if &**w == "function")
                        && !self.toks[self.i].newline_before =>
                {
                    self.i += 1;
                    Expr::Func(self.function_rest(pos, false, true, true)?)
                }
                "class" => Expr::Class(self.class_rest()?),
                "this" => Expr::This,
                "null" => Expr::Null,
                "true" => Expr::Bool(true),
                "false" => Expr::Bool(false),
                "super" => {
                    const PROP: &str = "use of super property accesses only valid within methods or eval code within methods";
                    let call = self.is("(");
                    if call && !self.super_call {
                        return self
                            .fail("super() is only valid in derived class constructors".into());
                    }
                    if !call && (self.is(".") || self.is("[")) && !self.super_prop {
                        return self.fail(PROP.into());
                    }
                    if self.eat("(") {
                        Expr::SuperCall(self.arguments()?, pos)
                    } else if self.eat(".") {
                        if matches!(self.tok(), Tok::Private(_)) {
                            return self.fail("invalid access of private field on 'super'".into());
                        }
                        Expr::SuperMember(Box::new(Expr::Str(self.prop_ident()?)))
                    } else if self.eat("[") {
                        let e = self.expression()?;
                        self.expect("]")?;
                        Expr::SuperMember(Box::new(e))
                    } else {
                        return self.fail("invalid use of keyword 'super'".into());
                    }
                }
                _ if is_reserved(&w) => {
                    self.i -= 1;
                    let what = describe(self.tok());
                    return self.fail(format!("expected expression, got {what}"));
                }
                "yield" if self.in_gen || self.strict => {
                    return self.fail_at(pos, "yield is a reserved identifier".into());
                }
                _ => Expr::Ident(w, pos),
            },
            _ => {
                self.i -= 1;
                let what = describe(self.tok());
                return self.fail(format!("expected expression, got {what}"));
            }
        })
    }

    /// The `async` and `*` before a method's name, when they are not the name itself.
    fn method_prefix(&mut self, kind: PropKind) -> FnKind {
        let mut fk = FnKind::default();
        if kind != PropKind::Value {
            return fk;
        }
        if self.is_word("async")
            && !matches!(self.peek_at(1), Tok::Punct("(" | ":" | "," | "}" | "=" | ";"))
            && !self.toks[self.i + 1].newline_before
        {
            self.i += 1;
            fk.is_async = true;
        }
        fk.generator = self.eat("*");
        fk
    }

    /// `yield`, `yield expr` or `yield* expr` inside a generator.
    fn yield_expr(&mut self, allow_in: bool) -> Result<Expr, SyntaxError> {
        if self.in_params {
            return self.fail("yield expression can't be used in parameter".into());
        }
        self.i += 1;
        let delegate = !self.toks[self.i].newline_before && self.eat("*");
        let ends = self.toks[self.i].newline_before && !delegate
            || matches!(self.tok(), Tok::Punct(")" | "]" | "}" | "," | ";" | ":") | Tok::Eof);
        let arg = if ends && !delegate { None } else { Some(Box::new(self.assign_in(allow_in)?)) };
        Ok(Expr::Yield(arg, delegate))
    }

    fn object_literal(&mut self) -> Result<Expr, SyntaxError> {
        let mut props = Vec::new();
        while !self.eat("}") {
            let start = self.pos();
            if self.eat("...") {
                props.push(ObjProp::Spread(self.assign_at()?));
            } else {
                let mut kind = PropKind::Value;
                if (self.is_word("get") || self.is_word("set"))
                    && !matches!(self.peek_at(1), Tok::Punct("(" | ":" | "," | "}"))
                {
                    kind = if self.is_word("get") { PropKind::Get } else { PropKind::Set };
                    self.i += 1;
                }
                let fk = self.method_prefix(kind);
                let shorthand = match self.tok() {
                    Tok::Ident(w) if !is_reserved(w) => Some(w.clone()),
                    _ => None,
                };
                let key = self.prop_name()?;
                if (kind != PropKind::Value || fk.is_async || fk.generator) && !self.is("(") {
                    return self.fail("invalid property id".into());
                }
                if kind != PropKind::Value {
                    let fk = FnKind { method: true, ..FnKind::default() };
                    let f = self.function_body(start, None, false, fk)?;
                    if let PropName::Fixed(n) = &key {
                        self.namer.method(&f, accessor_name(kind, n));
                    }
                    props.push(ObjProp::Accessor(key, kind, f));
                } else if self.eat(":") {
                    // Each property moves the position to its key, constants too (measured).
                    let mark = self.namer.mark();
                    let v = self.assign()?;
                    if let PropName::Fixed(n) = &key {
                        self.namer.place(mark, n.to_string(), true, &v);
                    }
                    let v = match v {
                        v @ (Expr::Func(_) | Expr::Class(_)) => v,
                        v => Expr::At(start, Box::new(v)),
                    };
                    props.push(ObjProp::Value(key, v));
                } else if self.is("(") {
                    let name = match &key {
                        PropName::Fixed(n) => Some(n.clone()),
                        PropName::Computed(_) | PropName::Private(_) => None,
                    };
                    let f =
                        self.function_body(start, name, false, FnKind { method: true, ..fk })?;
                    props.push(ObjProp::Value(key, Expr::Func(f)));
                } else if let Some(name) = shorthand
                    && matches!(self.tok(), Tok::Punct("=" | "," | "}"))
                {
                    // `{a}` and, inside a pattern, `{a = 1}`.
                    let value = Expr::Ident(name, start);
                    if self.is("=") {
                        self.cover.push(self.pos());
                        self.i += 1;
                        let d = self.assign()?;
                        props.push(ObjProp::Value(
                            key,
                            Expr::Assign {
                                op: "=",
                                target: Box::new(Pattern::Expr(Box::new(value))),
                                value: Box::new(d),
                            },
                        ));
                    } else {
                        props.push(ObjProp::Value(key, value));
                    }
                } else {
                    return self.fail("missing : after property id".into());
                }
            }
            if !self.is("}") {
                self.expect_or(",", "missing } after property list")?;
            }
        }
        Ok(Expr::Object(props))
    }
}

/// `e` without its position marks.
fn unat(e: Expr) -> Expr {
    match e {
        Expr::At(_, e) => unat(*e),
        e => e,
    }
}

/// The assignment target an expression stands for (`[a, b] = …`, `({a} = …)`).
fn to_pattern(e: Expr) -> Result<Pattern, String> {
    match e {
        Expr::At(_, e) => to_pattern(*e),
        Expr::Ident(n, _) => Ok(Pattern::Name(n)),
        e @ Expr::Member { .. } => Ok(Pattern::Expr(Box::new(e))),
        Expr::Array(items) => {
            let mut out = Vec::new();
            let mut rest = None;
            for item in items {
                match item.map(unat) {
                    None => out.push(None),
                    Some(Expr::Spread(e)) => rest = Some(Box::new(to_pattern(*e)?)),
                    Some(Expr::Assign { op: "=", target, value }) => {
                        out.push(Some((*target, Some(*value))))
                    }
                    Some(e) => out.push(Some((to_pattern(e)?, None))),
                }
            }
            Ok(Pattern::Array(out, rest))
        }
        Expr::Object(props) => {
            let mut out = Vec::new();
            let mut rest = None;
            for p in props {
                let p = match p {
                    ObjProp::Value(k, v) => ObjProp::Value(k, unat(v)),
                    p => p,
                };
                match p {
                    ObjProp::Value(k, Expr::Assign { op: "=", target, value }) => {
                        let target = match *target {
                            Pattern::Expr(e) => to_pattern(*e)?,
                            other => other,
                        };
                        out.push((k, target, Some(*value)));
                    }
                    ObjProp::Value(k, v) => out.push((k, to_pattern(v)?, None)),
                    ObjProp::Spread(e) => rest = Some(Box::new(to_pattern(e)?)),
                    ObjProp::Accessor(..) => return Err("invalid destructuring target".into()),
                }
            }
            Ok(Pattern::Object(out, rest))
        }
        _ => Err("invalid assignment left-hand side".into()),
    }
}
