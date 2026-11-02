//! The tokens of ECMAScript source (ECMA-262 §12): identifiers, punctuators, numbers, strings,
//! template pieces and regular expression literals, each with its line and column.

use std::rc::Rc;

/// One token.
#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Ident(Rc<str>),
    Num(f64),
    /// A `BigInt` literal (`10n`).
    BigInt(Rc<crate::bigint::BigInt>),
    Str(Rc<str>),
    /// A whole template literal: its cooked strings and the source of each substitution.
    Template(Vec<Rc<str>>, Vec<String>),
    Regex(Rc<str>, Rc<str>),
    Punct(&'static str),
    Eof,
}

/// A token with where it starts and whether a line break comes before it.
#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub line: u32,
    pub col: u32,
    pub newline_before: bool,
}

/// A syntax error with its position.
#[derive(Clone, Debug, PartialEq)]
pub struct SyntaxError {
    pub message: String,
    pub line: u32,
    pub col: u32,
}

/// The punctuators, longest first.
const PUNCTS: &[&str] = &[
    ">>>=", "...", "===", "!==", "**=", "<<=", ">>=", ">>>", "&&=", "||=", "??=", "=>", "==", "!=",
    "<=", ">=", "&&", "||", "??", "?.", "++", "--", "+=", "-=", "*=", "/=", "%=", "&=", "|=", "^=",
    "**", "<<", ">>", "{", "}", "(", ")", "[", "]", ";", ",", "<", ">", "+", "-", "*", "/", "%",
    "&", "|", "^", "!", "~", "?", ":", "=", ".", "@", "#",
];

/// Splits `src` into tokens.
pub fn tokenize(src: &str) -> Result<Vec<Token>, SyntaxError> {
    let mut lx = Lexer { s: src.chars().collect(), i: 0, line: 1, col: 1, out: Vec::new() };
    lx.run()?;
    Ok(lx.out)
}

struct Lexer {
    s: Vec<char>,
    i: usize,
    line: u32,
    col: u32,
    out: Vec<Token>,
}

fn is_id_start(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphabetic()
}

fn is_id_part(c: char) -> bool {
    is_id_start(c) || c.is_alphanumeric() || c == '\u{200c}' || c == '\u{200d}'
}

impl Lexer {
    fn peek(&self, k: usize) -> Option<char> {
        self.s.get(self.i + k).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.s.get(self.i).copied()?;
        self.i += 1;
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn err<T>(&self, message: impl Into<String>) -> Result<T, SyntaxError> {
        Err(SyntaxError { message: message.into(), line: self.line, col: self.col })
    }

    /// Whether a `/` here starts a regular expression (after an operator or at the start).
    fn regex_allowed(&self) -> bool {
        match self.out.last().map(|t| &t.tok) {
            None => true,
            Some(Tok::Punct(p)) => !matches!(*p, ")" | "]" | "}"),
            Some(Tok::Ident(w)) => matches!(
                &**w,
                "return"
                    | "typeof"
                    | "instanceof"
                    | "in"
                    | "of"
                    | "new"
                    | "delete"
                    | "void"
                    | "throw"
                    | "case"
                    | "do"
                    | "else"
            ),
            _ => false,
        }
    }

    fn run(&mut self) -> Result<(), SyntaxError> {
        let mut newline = false;
        while let Some(c) = self.peek(0) {
            if c == '\n' || c == '\r' || c == '\u{2028}' || c == '\u{2029}' {
                newline = true;
                self.bump();
                continue;
            }
            if c.is_whitespace() || c == '\u{feff}' {
                self.bump();
                continue;
            }
            if c == '/' && self.peek(1) == Some('/') {
                while let Some(c) = self.peek(0) {
                    if c == '\n' {
                        break;
                    }
                    self.bump();
                }
                continue;
            }
            if c == '/' && self.peek(1) == Some('*') {
                self.bump();
                self.bump();
                loop {
                    match self.bump() {
                        None => return self.err("unterminated comment"),
                        Some('*') if self.peek(0) == Some('/') => {
                            self.bump();
                            break;
                        }
                        Some('\n') => newline = true,
                        _ => {}
                    }
                }
                continue;
            }
            let (line, col) = (self.line, self.col);
            let tok = self.token(c)?;
            self.out.push(Token { tok, line, col, newline_before: newline });
            newline = false;
        }
        self.out.push(Token {
            tok: Tok::Eof,
            line: self.line,
            col: self.col,
            newline_before: true,
        });
        Ok(())
    }

    fn token(&mut self, c: char) -> Result<Tok, SyntaxError> {
        if is_id_start(c) || c == '\\' {
            let mut w = String::new();
            while let Some(c) = self.peek(0) {
                if is_id_part(c) {
                    w.push(c);
                    self.bump();
                } else if c == '\\' && self.peek(1) == Some('u') {
                    self.bump();
                    self.bump();
                    w.push(self.unicode_escape()?);
                } else {
                    break;
                }
            }
            return Ok(Tok::Ident(w.into()));
        }
        if c.is_ascii_digit() || (c == '.' && self.peek(1).is_some_and(|d| d.is_ascii_digit())) {
            return self.number();
        }
        if c == '"' || c == '\'' {
            self.bump();
            return Ok(Tok::Str(self.string(c)?.into()));
        }
        if c == '`' {
            self.bump();
            return self.template();
        }
        if c == '/' && self.regex_allowed() {
            self.bump();
            return self.regex();
        }
        for p in PUNCTS {
            let n = p.chars().count();
            if (0..n).all(|k| self.peek(k) == p.chars().nth(k)) {
                // `?.` followed by a digit is `?` and a number.
                if *p == "?." && self.peek(2).is_some_and(|d| d.is_ascii_digit()) {
                    continue;
                }
                for _ in 0..n {
                    self.bump();
                }
                return Ok(Tok::Punct(p));
            }
        }
        self.err(format!("illegal character U+{:04X}", c as u32))
    }

    fn number(&mut self) -> Result<Tok, SyntaxError> {
        let radix = match (self.peek(0), self.peek(1)) {
            (Some('0'), Some('x' | 'X')) => 16,
            (Some('0'), Some('o' | 'O')) => 8,
            (Some('0'), Some('b' | 'B')) => 2,
            _ => 10,
        };
        if radix != 10 {
            self.bump();
            self.bump();
            let mut v = 0f64;
            let mut text = String::new();
            while let Some(c) = self.peek(0) {
                if c == '_' {
                    self.bump();
                    continue;
                }
                let Some(d) = c.to_digit(radix) else { break };
                v = v * f64::from(radix) + f64::from(d);
                text.push(c);
                self.bump();
            }
            if text.is_empty() {
                return self.err("missing digits after integer base prefix");
            }
            return self.after_number(v, Some((&text, radix)));
        }
        let mut text = String::new();
        let digits = |lx: &mut Lexer, text: &mut String| {
            while let Some(c) = lx.peek(0) {
                if c.is_ascii_digit() {
                    text.push(c);
                } else if c != '_' {
                    break;
                }
                lx.bump();
            }
        };
        digits(self, &mut text);
        if self.peek(0) == Some('.') {
            text.push('.');
            self.bump();
            digits(self, &mut text);
        }
        if matches!(self.peek(0), Some('e' | 'E')) {
            let sign = self.peek(1);
            let has_digit = match sign {
                Some('+' | '-') => self.peek(2).is_some_and(|d| d.is_ascii_digit()),
                Some(d) => d.is_ascii_digit(),
                None => false,
            };
            if has_digit {
                text.push('e');
                self.bump();
                if let Some(s @ ('+' | '-')) = self.peek(0) {
                    text.push(s);
                    self.bump();
                }
                digits(self, &mut text);
            }
        }
        let v = text.parse::<f64>().or_else(|_| self.err("invalid number"))?;
        let integer = text.bytes().all(|b| b.is_ascii_digit());
        self.after_number(v, integer.then_some((text.as_str(), 10)))
    }

    /// The token of a number read as `v`; `digits` are its integer digits and radix, which a
    /// `n` after them makes a `BigInt`.
    fn after_number(&mut self, v: f64, digits: Option<(&str, u32)>) -> Result<Tok, SyntaxError> {
        if self.peek(0) == Some('n') {
            let Some(b) = digits.and_then(|(d, r)| crate::bigint::BigInt::parse_radix(d, r)) else {
                return self.err("invalid BigInt literal");
            };
            self.bump();
            if self.peek(0).is_some_and(is_id_start) {
                return self.err("identifier starts immediately after numeric literal");
            }
            return Ok(Tok::BigInt(Rc::new(b)));
        }
        if self.peek(0).is_some_and(is_id_start) {
            return self.err("identifier starts immediately after numeric literal");
        }
        Ok(Tok::Num(v))
    }

    fn unicode_escape(&mut self) -> Result<char, SyntaxError> {
        let code = if self.peek(0) == Some('{') {
            self.bump();
            let mut v = 0u32;
            while let Some(c) = self.bump() {
                if c == '}' {
                    break;
                }
                let Some(d) = c.to_digit(16) else { return self.err("malformed Unicode escape") };
                v = v.saturating_mul(16).saturating_add(d);
            }
            v
        } else {
            let mut v = 0u32;
            for _ in 0..4 {
                let Some(d) = self.bump().and_then(|c| c.to_digit(16)) else {
                    return self.err("malformed Unicode character escape sequence");
                };
                v = v * 16 + d;
            }
            // A surrogate pair written as two escapes.
            if (0xD800..0xDC00).contains(&v)
                && self.peek(0) == Some('\\')
                && self.peek(1) == Some('u')
            {
                let save = (self.i, self.line, self.col);
                self.bump();
                self.bump();
                let low = self.unicode_escape()? as u32;
                if (0xDC00..0xE000).contains(&low) {
                    return Ok(char::from_u32(0x10000 + ((v - 0xD800) << 10) + (low - 0xDC00))
                        .unwrap_or('\u{fffd}'));
                }
                (self.i, self.line, self.col) = save;
            }
            v
        };
        Ok(char::from_u32(code).unwrap_or('\u{fffd}'))
    }

    fn escape(&mut self, out: &mut String) -> Result<(), SyntaxError> {
        let Some(c) = self.bump() else { return self.err("unterminated string literal") };
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' if !self.peek(0).is_some_and(|d| d.is_ascii_digit()) => out.push('\0'),
            'x' => {
                let mut v = 0u32;
                for _ in 0..2 {
                    let Some(d) = self.bump().and_then(|c| c.to_digit(16)) else {
                        return self.err("malformed hexadecimal character escape sequence");
                    };
                    v = v * 16 + d;
                }
                out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
            }
            'u' => out.push(self.unicode_escape()?),
            '\r' => {
                if self.peek(0) == Some('\n') {
                    self.bump();
                }
            }
            '\n' | '\u{2028}' | '\u{2029}' => {}
            other => out.push(other),
        }
        Ok(())
    }

    fn string(&mut self, quote: char) -> Result<String, SyntaxError> {
        let mut out = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => return self.err("unterminated string literal"),
                Some(c) if c == quote => return Ok(out),
                Some('\\') => self.escape(&mut out)?,
                Some(c) => out.push(c),
            }
        }
    }

    fn template(&mut self) -> Result<Tok, SyntaxError> {
        let (mut strs, mut subs) = (Vec::new(), Vec::new());
        let mut cur = String::new();
        loop {
            match self.bump() {
                None => return self.err("unterminated template literal"),
                Some('`') => {
                    strs.push(cur.into());
                    return Ok(Tok::Template(strs, subs));
                }
                Some('\\') => self.escape(&mut cur)?,
                Some('$') if self.peek(0) == Some('{') => {
                    self.bump();
                    strs.push(std::mem::take(&mut cur).into());
                    subs.push(self.substitution()?);
                }
                Some(c) => cur.push(c),
            }
        }
    }

    /// The source of a `${…}` up to its closing brace, with nested braces, strings and templates.
    fn substitution(&mut self) -> Result<String, SyntaxError> {
        let mut depth = 0usize;
        let mut out = String::new();
        loop {
            let Some(c) = self.bump() else { return self.err("unterminated template literal") };
            match c {
                '{' => depth += 1,
                '}' if depth == 0 => return Ok(out),
                '}' => depth -= 1,
                '"' | '\'' | '`' => {
                    out.push(c);
                    loop {
                        let Some(d) = self.bump() else {
                            return self.err("unterminated template literal");
                        };
                        out.push(d);
                        if d == '\\' {
                            if let Some(e) = self.bump() {
                                out.push(e);
                            }
                        } else if d == c {
                            break;
                        }
                    }
                    continue;
                }
                _ => {}
            }
            out.push(c);
        }
    }

    fn regex(&mut self) -> Result<Tok, SyntaxError> {
        let mut body = String::new();
        let mut class = false;
        loop {
            match self.bump() {
                None | Some('\n') => return self.err("unterminated regular expression literal"),
                Some('\\') => {
                    body.push('\\');
                    match self.bump() {
                        Some(c) if c != '\n' => body.push(c),
                        _ => return self.err("unterminated regular expression literal"),
                    }
                }
                Some('[') => {
                    class = true;
                    body.push('[');
                }
                Some(']') => {
                    class = false;
                    body.push(']');
                }
                Some('/') if !class => break,
                Some(c) => body.push(c),
            }
        }
        let mut flags = String::new();
        while let Some(c) = self.peek(0) {
            if !is_id_part(c) {
                break;
            }
            if !"dgimsuyv".contains(c) || flags.contains(c) {
                return self.err(format!("invalid regular expression flag {c}"));
            }
            flags.push(c);
            self.bump();
        }
        Ok(Tok::Regex(body.into(), flags.into()))
    }
}
