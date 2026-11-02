//! The parser of ECMAScript scripts and functions (ECMA-262 §13 to §15) into [`crate::ast`], with
//! automatic semicolon insertion and the messages of SpiderMonkey for the common mistakes.

use std::rc::Rc;

use crate::ast::*;
use crate::lexer::{SyntaxError, Tok, Token, tokenize};

/// Parses a whole script.
pub fn parse_script(src: &str) -> Result<Vec<Stmt>, SyntaxError> {
    let mut p = Parser { toks: tokenize(src)?, i: 0, src: src.chars().collect(), depth: 0 };
    let mut out = Vec::new();
    while !p.at_eof() {
        out.push(p.statement()?);
    }
    Ok(out)
}

/// Parses one expression (a template's substitution).
fn parse_expression(src: &str) -> Result<Expr, SyntaxError> {
    let mut p = Parser { toks: tokenize(src)?, i: 0, src: src.chars().collect(), depth: 0 };
    let e = p.expression()?;
    if !p.at_eof() {
        return p.unexpected();
    }
    Ok(e)
}

/// The deepest nesting the parser accepts before it reports too much recursion.
const MAX_DEPTH: usize = 400;

/// A function's parameters with their defaults, and its rest parameter.
type Params = (Vec<(Pattern, Option<Expr>)>, Option<Pattern>);

struct Parser {
    toks: Vec<Token>,
    i: usize,
    src: Vec<char>,
    depth: usize,
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
        Tok::Num(_) => "numeric literal".to_owned(),
        Tok::Str(_) => "string literal".to_owned(),
        Tok::Template(..) => "template literal".to_owned(),
        Tok::Regex(..) => "regular expression literal".to_owned(),
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
            Tok::Punct("#") => self.fail("private fields are not supported".to_owned()),
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
        if self.depth > MAX_DEPTH {
            return self.fail("too much recursion".to_owned());
        }
        Ok(())
    }

    // Statements.

    fn statement(&mut self) -> Result<Stmt, SyntaxError> {
        self.enter()?;
        let s = self.statement_inner();
        self.depth -= 1;
        s
    }

    fn statement_inner(&mut self) -> Result<Stmt, SyntaxError> {
        let pos = self.pos();
        if self.eat("{") {
            return Ok(Stmt::Block(self.block_rest()?));
        }
        if self.eat(";") {
            return Ok(Stmt::Empty);
        }
        let word = match self.tok() {
            Tok::Ident(w) => w.clone(),
            _ => return self.expression_statement(),
        };
        if matches!(self.peek_at(1), Tok::Punct(":")) && !is_reserved(&word) {
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
            // An anonymous function at the start of a statement is an expression, as RefDb
            // reads a function's source.
            "function" if matches!(self.peek_at(1), Tok::Punct("(")) => self.expression_statement(),
            "function" => {
                self.i += 1;
                let f = self.function_rest(pos, false)?;
                let name = f.name.clone().map_or_else(|| self.fail("missing name".into()), Ok)?;
                Ok(Stmt::Func(name, f))
            }
            "class" => {
                self.i += 1;
                let c = self.class_rest()?;
                let name = c.name.clone().map_or_else(|| self.fail("missing name".into()), Ok)?;
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
                    Some(self.expression()?)
                };
                self.semicolon()?;
                Ok(Stmt::Return(e))
            }
            "if" => {
                self.i += 1;
                self.expect("(")?;
                let c = self.expression()?;
                self.expect(")")?;
                let then = Box::new(self.statement()?);
                let other =
                    if self.eat_word("else") { Some(Box::new(self.statement()?)) } else { None };
                Ok(Stmt::If(c, then, other))
            }
            "while" => {
                self.i += 1;
                self.expect("(")?;
                let c = self.expression()?;
                self.expect(")")?;
                Ok(Stmt::While(c, Box::new(self.statement()?)))
            }
            "do" => {
                self.i += 1;
                let body = Box::new(self.statement()?);
                if !self.eat_word("while") {
                    return self.unexpected();
                }
                self.expect("(")?;
                let c = self.expression()?;
                self.expect(")")?;
                self.eat(";");
                Ok(Stmt::DoWhile(body, c))
            }
            "for" => {
                self.i += 1;
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
            "with" | "import" | "export" => self.fail(format!("{word} is not supported")),
            _ => self.expression_statement(),
        }
    }

    fn expression_statement(&mut self) -> Result<Stmt, SyntaxError> {
        let e = self.expression()?;
        self.semicolon()?;
        Ok(Stmt::Expr(e))
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
        loop {
            let target = self.binding_pattern()?;
            let init = if self.eat("=") { Some(self.assign_in(allow_in)?) } else { None };
            if init.is_none()
                && allow_in
                && (kind == DeclKind::Const || !matches!(target, Pattern::Name(_)))
            {
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
                let d = if self.eat("=") { Some(self.assign()?) } else { None };
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
                    rest = Some(Box::new(Pattern::Name(self.ident()?)));
                    self.expect("}")?;
                    break;
                }
                let key = self.prop_name()?;
                let target = if self.eat(":") {
                    self.binding_pattern()?
                } else {
                    match &key {
                        PropName::Fixed(n) => Pattern::Name(n.clone()),
                        PropName::Computed(_) => return self.unexpected(),
                    }
                };
                let d = if self.eat("=") { Some(self.assign()?) } else { None };
                props.push((key, target, d));
                if !self.is("}") {
                    self.expect(",")?;
                }
            }
            return Ok(Pattern::Object(props, rest));
        }
        Ok(Pattern::Name(self.ident()?))
    }

    fn for_rest(&mut self) -> Result<Stmt, SyntaxError> {
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
            let target = self.binding_pattern()?;
            if self.is_word("of") || self.is_word("in") {
                let of = self.is_word("of");
                self.i += 1;
                let obj = if of { self.assign()? } else { self.expression()? };
                self.expect(")")?;
                let body = Box::new(self.statement()?);
                return Ok(Stmt::ForIn { decl: Some(kind), target, of, obj, body });
            }
            self.i = start;
            Some(ForInit::Decl(kind, self.declarations(kind, false)?))
        } else if self.is(";") {
            None
        } else {
            let start = self.i;
            let e = self.lhs()?;
            if self.is_word("of") || self.is_word("in") {
                let of = self.is_word("of");
                self.i += 1;
                let target = to_pattern(e).map_or_else(|m| self.fail(m), Ok)?;
                let obj = if of { self.assign()? } else { self.expression()? };
                self.expect(")")?;
                let body = Box::new(self.statement()?);
                return Ok(Stmt::ForIn { decl: None, target, of, obj, body });
            }
            self.i = start;
            Some(ForInit::Expr(self.expression_in(false)?))
        };
        self.expect(";")?;
        let test = if self.is(";") { None } else { Some(self.expression()?) };
        self.expect(";")?;
        let update = if self.is(")") { None } else { Some(self.expression()?) };
        self.expect(")")?;
        Ok(Stmt::For(init, test, update, Box::new(self.statement()?)))
    }

    fn try_rest(&mut self) -> Result<Stmt, SyntaxError> {
        self.expect("{")?;
        let body = self.block_rest()?;
        let catch = if self.eat_word("catch") {
            let param = if self.eat("(") {
                let p = self.binding_pattern()?;
                self.expect(")")?;
                Some(p)
            } else {
                None
            };
            self.expect("{")?;
            Some((param, self.block_rest()?))
        } else {
            None
        };
        let finally = if self.eat_word("finally") {
            self.expect("{")?;
            Some(self.block_rest()?)
        } else {
            None
        };
        if catch.is_none() && finally.is_none() {
            return self.fail("missing catch or finally after try".into());
        }
        Ok(Stmt::Try(body, catch, finally))
    }

    fn switch_rest(&mut self) -> Result<Stmt, SyntaxError> {
        self.expect("(")?;
        let d = self.expression()?;
        self.expect(")")?;
        self.expect("{")?;
        let mut cases = Vec::new();
        while !self.eat("}") {
            let test = if self.eat_word("case") {
                Some(self.expression()?)
            } else if self.eat_word("default") {
                None
            } else {
                return self.unexpected();
            };
            self.expect(":")?;
            let mut body = Vec::new();
            while !self.is("}") && !self.is_word("case") && !self.is_word("default") {
                if self.at_eof() {
                    return self.unexpected();
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
        let (mut line, mut col, mut idx) = (1u32, 1u32, 0usize);
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

    fn params(&mut self) -> Result<Params, SyntaxError> {
        self.expect("(")?;
        let mut params = Vec::new();
        let mut rest = None;
        while !self.eat(")") {
            if self.eat("...") {
                rest = Some(self.binding_pattern()?);
                self.expect(")")?;
                break;
            }
            let p = self.binding_pattern()?;
            let d = if self.eat("=") { Some(self.assign()?) } else { None };
            params.push((p, d));
            if !self.is(")") {
                self.expect(",")?;
            }
        }
        Ok((params, rest))
    }

    /// `function` already read: an optional `*` (refused), the name, parameters and body.
    fn function_rest(&mut self, start: Pos, method: bool) -> Result<Rc<FuncDef>, SyntaxError> {
        if self.is("*") {
            return self.fail("generators are not supported".into());
        }
        let name = if !method && matches!(self.tok(), Tok::Ident(_)) && !self.is("(") {
            Some(self.ident()?)
        } else {
            None
        };
        self.function_body(start, name, false)
    }

    fn function_body(
        &mut self,
        start: Pos,
        name: Option<Name>,
        derived_ctor: bool,
    ) -> Result<Rc<FuncDef>, SyntaxError> {
        let (params, rest) = self.params()?;
        self.expect("{")?;
        let body = self.block_rest()?;
        Ok(Rc::new(FuncDef {
            name,
            params,
            rest,
            body,
            arrow: false,
            is_ctor_of_derived: derived_ctor,
            source: self.source_from(start),
        }))
    }

    fn class_rest(&mut self) -> Result<Rc<ClassDef>, SyntaxError> {
        let name = if matches!(self.tok(), Tok::Ident(w) if &**w != "extends") {
            Some(self.ident()?)
        } else {
            None
        };
        let parent = if self.eat_word("extends") { Some(Box::new(self.lhs()?)) } else { None };
        self.expect("{")?;
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
            let mut kind = PropKind::Value;
            if (self.is_word("get") || self.is_word("set"))
                && !matches!(self.peek_at(1), Tok::Punct("(" | "=" | ";" | "}"))
            {
                kind = if self.is_word("get") { PropKind::Get } else { PropKind::Set };
                self.i += 1;
            }
            let key = self.prop_name()?;
            if self.is("(") {
                let is_ctor =
                    !is_static && matches!(&key, PropName::Fixed(n) if &**n == "constructor");
                let f = self.function_body(start, None, is_ctor && parent.is_some())?;
                if is_ctor {
                    ctor = Some(f);
                } else {
                    members.push(ClassMember { is_static, key, kind, field: None, func: Some(f) });
                }
            } else {
                let init = if self.eat("=") { Some(self.assign()?) } else { Some(Expr::Undefined) };
                self.semicolon()?;
                members.push(ClassMember { is_static, key, kind, field: init, func: None });
            }
        }
        Ok(Rc::new(ClassDef { name, parent, ctor, members }))
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
            Tok::Punct("[") => {
                self.i += 1;
                let e = self.assign()?;
                self.expect("]")?;
                Ok(PropName::Computed(Box::new(e)))
            }
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
            all.push(self.assign_in(allow_in)?);
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

    fn arrow(&mut self) -> Result<Expr, SyntaxError> {
        let start = self.pos();
        let (params, rest) = if let Tok::Ident(_) = self.tok() {
            (vec![(Pattern::Name(self.ident()?), None)], None)
        } else {
            self.params()?
        };
        self.expect("=>")?;
        let body = if self.eat("{") {
            self.block_rest()?
        } else {
            vec![Stmt::Return(Some(self.assign()?))]
        };
        Ok(Expr::Func(Rc::new(FuncDef {
            name: None,
            params,
            rest,
            body,
            arrow: true,
            is_ctor_of_derived: false,
            source: self.source_from(start),
        })))
    }

    fn assign_in(&mut self, allow_in: bool) -> Result<Expr, SyntaxError> {
        self.enter()?;
        let r = self.assign_inner(allow_in);
        self.depth -= 1;
        r
    }

    fn assign_inner(&mut self, allow_in: bool) -> Result<Expr, SyntaxError> {
        if self.is_word("async") && !matches!(self.peek_at(1), Tok::Punct("=>" | "." | "=")) {
            return self.fail("async functions are not supported".into());
        }
        if self.is_word("yield") {
            return self.fail("generators are not supported".into());
        }
        if self.is_arrow_ahead() {
            return self.arrow();
        }
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
            to_pattern(left)
        } else {
            match left {
                e @ (Expr::Ident(..) | Expr::Member { .. }) => Ok(Pattern::Expr(Box::new(e))),
                _ => Err("invalid assignment left-hand side".to_owned()),
            }
        }
        .map_or_else(|m| self.fail(m), Ok)?;
        let value = self.assign_in(allow_in)?;
        Ok(Expr::Assign { op, target: Box::new(target), value: Box::new(value) })
    }

    fn conditional(&mut self, allow_in: bool) -> Result<Expr, SyntaxError> {
        let c = self.binary(0, allow_in)?;
        if !self.eat("?") {
            return Ok(c);
        }
        let a = self.assign_in(true)?;
        self.expect(":")?;
        let b = self.assign_in(allow_in)?;
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
            "<" | ">" | "<=" | ">=" | "instanceof" | "in" => 8,
            "<<" | ">>" | ">>>" => 9,
            "+" | "-" => 10,
            "*" | "/" | "%" => 11,
            "**" => 12,
            _ => return None,
        };
        Some((op, prec))
    }

    fn binary(&mut self, min: u8, allow_in: bool) -> Result<Expr, SyntaxError> {
        let mut left = self.unary()?;
        while let Some((op, prec)) = self.binary_op(allow_in) {
            if prec < min {
                break;
            }
            self.i += 1;
            // `**` is right-associative.
            let next = if op == "**" { prec } else { prec + 1 };
            let right = self.binary(next, allow_in)?;
            left = match op {
                "&&" | "||" | "??" => Expr::Logical(op, Box::new(left), Box::new(right)),
                _ => Expr::Binary(op, Box::new(left), Box::new(right)),
            };
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr, SyntaxError> {
        self.enter()?;
        let r = self.unary_inner();
        self.depth -= 1;
        r
    }

    fn unary_inner(&mut self) -> Result<Expr, SyntaxError> {
        match self.tok().clone() {
            Tok::Punct(op @ ("!" | "-" | "+" | "~")) => {
                self.i += 1;
                let e = self.unary()?;
                if self.is("**") {
                    return self.fail("unparenthesized unary expression can't appear on the left-hand side of '**'".into());
                }
                Ok(Expr::Unary(op, Box::new(e)))
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
                self.i += 1;
                Ok(Expr::Typeof(Box::new(self.unary()?)))
            }
            Tok::Ident(w) if &*w == "void" => {
                self.i += 1;
                Ok(Expr::Unary("void", Box::new(self.unary()?)))
            }
            Tok::Ident(w) if &*w == "delete" => {
                self.i += 1;
                Ok(Expr::Delete(Box::new(self.unary()?)))
            }
            Tok::Ident(w) if &*w == "await" => {
                self.fail("await is only valid in async functions".into())
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
        let mut args = Vec::new();
        while !self.eat(")") {
            if self.eat("...") {
                args.push(Expr::Spread(Box::new(self.assign()?)));
            } else {
                args.push(self.assign()?);
            }
            if !self.is(")") && !self.eat(",") {
                return self.expect(")").map(|_| Vec::new());
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
                let name = self.prop_ident()?;
                e = Expr::Member {
                    obj: Box::new(e),
                    prop: Box::new(MemberProp::Name(name)),
                    optional: false,
                    pos,
                };
            } else if self.eat("?.") {
                chained = true;
                if self.eat("(") {
                    let args = self.arguments()?;
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
                    let name = self.prop_ident()?;
                    e = Expr::Member {
                        obj: Box::new(e),
                        prop: Box::new(MemberProp::Name(name)),
                        optional: true,
                        pos,
                    };
                }
            } else if self.eat("[") {
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
                e = Expr::Call { callee: Box::new(e), args, optional: false, pos };
            } else if let Tok::Template(strs, subs) = self.tok().clone() {
                // A tagged template: the tag gets the strings and the values.
                self.i += 1;
                let parts = Expr::Array(strs.iter().map(|s| Some(Expr::Str(s.clone()))).collect());
                let mut args = vec![parts];
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
                let name = self.prop_ident()?;
                e = Expr::Member {
                    obj: Box::new(e),
                    prop: Box::new(MemberProp::Name(name)),
                    optional: false,
                    pos,
                };
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

    fn substitution(&self, src: &str) -> Result<Expr, SyntaxError> {
        parse_expression(src).map_err(|mut e| {
            let p = self.pos();
            e.line = p.line;
            e
        })
    }

    fn primary(&mut self) -> Result<Expr, SyntaxError> {
        let pos = self.pos();
        let t = self.tok().clone();
        self.i += 1;
        Ok(match t {
            Tok::Num(n) => Expr::Num(n),
            Tok::Str(s) => Expr::Str(s),
            Tok::Template(strs, subs) => {
                let subs = subs.iter().map(|s| self.substitution(s)).collect::<Result<_, _>>()?;
                Expr::Template(strs, subs)
            }
            Tok::Regex(b, f) => Expr::Regex(b, f),
            Tok::Punct("(") => {
                let e = self.expression()?;
                self.expect(")")?;
                e
            }
            Tok::Punct("[") => {
                let mut items = Vec::new();
                while !self.eat("]") {
                    if self.eat(",") {
                        items.push(None);
                        continue;
                    }
                    let item = if self.eat("...") {
                        Expr::Spread(Box::new(self.assign()?))
                    } else {
                        self.assign()?
                    };
                    items.push(Some(item));
                    if !self.is("]") {
                        self.expect(",")?;
                    }
                }
                Expr::Array(items)
            }
            Tok::Punct("{") => self.object_literal()?,
            Tok::Ident(w) => match &*w {
                "function" => Expr::Func(self.function_rest(pos, false)?),
                "class" => Expr::Class(self.class_rest()?),
                "this" => Expr::This,
                "null" => Expr::Null,
                "true" => Expr::Bool(true),
                "false" => Expr::Bool(false),
                "super" => {
                    if self.eat("(") {
                        Expr::SuperCall(self.arguments()?, pos)
                    } else if self.eat(".") {
                        Expr::SuperMember(Box::new(Expr::Str(self.prop_ident()?)))
                    } else if self.eat("[") {
                        let e = self.expression()?;
                        self.expect("]")?;
                        Expr::SuperMember(Box::new(e))
                    } else {
                        self.i -= 1;
                        return self.unexpected();
                    }
                }
                _ if is_reserved(&w) => {
                    self.i -= 1;
                    let what = describe(self.tok());
                    return self.fail(format!("expected expression, got {what}"));
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

    fn object_literal(&mut self) -> Result<Expr, SyntaxError> {
        let mut props = Vec::new();
        while !self.eat("}") {
            let start = self.pos();
            if self.eat("...") {
                props.push(ObjProp::Spread(self.assign()?));
            } else {
                let mut kind = PropKind::Value;
                if (self.is_word("get") || self.is_word("set"))
                    && !matches!(self.peek_at(1), Tok::Punct("(" | ":" | "," | "}"))
                {
                    kind = if self.is_word("get") { PropKind::Get } else { PropKind::Set };
                    self.i += 1;
                }
                if self.is("*")
                    || (self.is_word("async")
                        && !matches!(self.peek_at(1), Tok::Punct("(" | ":" | "," | "}")))
                {
                    return self.fail("generators and async methods are not supported".into());
                }
                let shorthand = match self.tok() {
                    Tok::Ident(w) if !is_reserved(w) => Some(w.clone()),
                    _ => None,
                };
                let key = self.prop_name()?;
                if kind != PropKind::Value {
                    let f = self.function_body(start, None, false)?;
                    props.push(ObjProp::Accessor(key, kind, f));
                } else if self.eat(":") {
                    props.push(ObjProp::Value(key, self.assign()?));
                } else if self.is("(") {
                    let name = match &key {
                        PropName::Fixed(n) => Some(n.clone()),
                        PropName::Computed(_) => None,
                    };
                    let f = self.function_body(start, name, false)?;
                    props.push(ObjProp::Value(key, Expr::Func(f)));
                } else if let Some(name) = shorthand {
                    // `{a}` and, inside a pattern, `{a = 1}`.
                    let value = Expr::Ident(name, start);
                    if self.eat("=") {
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
                    return self.unexpected();
                }
            }
            if !self.is("}") {
                self.expect(",")?;
            }
        }
        Ok(Expr::Object(props))
    }
}

/// The assignment target an expression stands for (`[a, b] = …`, `({a} = …)`).
fn to_pattern(e: Expr) -> Result<Pattern, String> {
    match e {
        Expr::Ident(n, _) => Ok(Pattern::Name(n)),
        e @ Expr::Member { .. } => Ok(Pattern::Expr(Box::new(e))),
        Expr::Array(items) => {
            let mut out = Vec::new();
            let mut rest = None;
            for item in items {
                match item {
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
