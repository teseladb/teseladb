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
    /// A whole template literal: its pieces and the source of each substitution with the line
    /// and column where it starts.
    Template(Rc<TemplateParts>, Vec<(String, u32, u32)>),
    Regex(Rc<str>, Rc<str>),
    Punct(&'static str),
    Eof,
}

/// The pieces of a template literal between its substitutions.
#[derive(Clone, Debug, PartialEq)]
pub struct TemplateParts {
    /// Each piece with its escapes read, `None` where an escape is not valid (allowed only in
    /// a tagged template).
    pub cooked: Vec<Option<Rc<str>>>,
    /// Each piece as written, line breaks as `\n`.
    pub raw: Vec<Rc<str>>,
    /// The first escape that is not valid: its message and where it is.
    pub invalid: Option<SyntaxError>,
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
    tokenize_at(src, 1, 1)
}

/// Splits `src`, which starts at `line` and `col` of a larger source, into tokens.
pub fn tokenize_at(src: &str, line: u32, col: u32) -> Result<Vec<Token>, SyntaxError> {
    let mut lx = Lexer { s: src.chars().collect(), i: 0, line, col, out: Vec::new() };
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
                    | "yield"
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
            if w.is_empty() {
                // A `\` that does not start `\u`: nothing was read (found by fuzzing).
                return self.err("illegal character U+005C");
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
                let Some(d) = c.to_digit(16) else {
                    return self.err("malformed Unicode character escape sequence");
                };
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
        let mut parts = TemplateParts { cooked: Vec::new(), raw: Vec::new(), invalid: None };
        let mut subs = Vec::new();
        let (mut cur, mut raw, mut valid) = (String::new(), String::new(), true);
        loop {
            let Some(c) = self.bump() else { return self.err("unterminated template literal") };
            match c {
                '`' | '$' if c == '`' || self.peek(0) == Some('{') => {
                    parts.cooked.push(valid.then(|| std::mem::take(&mut cur).into()));
                    parts.raw.push(std::mem::take(&mut raw).into());
                    (cur, valid) = (String::new(), true);
                    if c == '`' {
                        return Ok(Tok::Template(Rc::new(parts), subs));
                    }
                    self.bump();
                    let (line, col) = (self.line, self.col);
                    subs.push((self.substitution()?, line, col));
                }
                '\\' => {
                    let (line, col) = (self.line, self.col - 1);
                    let from = self.i;
                    match self.template_escape(&mut cur) {
                        Ok(()) => {}
                        Err(message) => {
                            valid = false;
                            if parts.invalid.is_none() {
                                parts.invalid =
                                    Some(SyntaxError { message: message.into(), line, col });
                            }
                        }
                    }
                    raw.push('\\');
                    for &ch in &self.s[from..self.i] {
                        raw.push(ch);
                    }
                    normalize_breaks(&mut raw);
                }
                '\r' => {
                    if self.peek(0) == Some('\n') {
                        self.bump();
                    }
                    cur.push('\n');
                    raw.push('\n');
                }
                c => {
                    cur.push(c);
                    raw.push(c);
                }
            }
        }
    }

    /// An escape of a template after `\\`, or the message of why it is not valid (its first
    /// character read).
    fn template_escape(&mut self, out: &mut String) -> Result<(), &'static str> {
        const OCTAL: &str = "octal escape sequences can't be used in untagged template literals or in strict mode code";
        const UNICODE: &str = "malformed Unicode character escape sequence";
        const HEX: &str = "malformed hexadecimal character escape sequence";
        let Some(c) = self.peek(0) else { return Ok(()) };
        match c {
            '0' if self.peek(1).is_some_and(|d| d.is_ascii_digit()) => {
                self.bump();
                Err(OCTAL)
            }
            '1'..='9' => {
                self.bump();
                Err(OCTAL)
            }
            'x' => {
                self.bump();
                let hex: Option<u32> = (0..2)
                    .map(|k| self.peek(k).and_then(|d| d.to_digit(16)))
                    .try_fold(0, |v, d| d.map(|d| v * 16 + d));
                let Some(v) = hex else { return Err(HEX) };
                self.bump();
                self.bump();
                out.push(char::from_u32(v).unwrap_or('\u{fffd}'));
                Ok(())
            }
            'u' => {
                self.bump();
                let code = if self.peek(0) == Some('{') {
                    let mut k = 1;
                    let mut v = 0u32;
                    while let Some(d) = self.peek(k).and_then(|d| d.to_digit(16)) {
                        v = v.saturating_mul(16).saturating_add(d);
                        k += 1;
                    }
                    if k == 1 || self.peek(k) != Some('}') || v > 0x10_FFFF {
                        return Err(UNICODE);
                    }
                    for _ in 0..=k {
                        self.bump();
                    }
                    v
                } else {
                    let hex: Option<u32> = (0..4)
                        .map(|k| self.peek(k).and_then(|d| d.to_digit(16)))
                        .try_fold(0, |v, d| d.map(|d| v * 16 + d));
                    let Some(v) = hex else { return Err(UNICODE) };
                    for _ in 0..4 {
                        self.bump();
                    }
                    // A surrogate pair written as two escapes.
                    if (0xD800..0xDC00).contains(&v)
                        && self.peek(0) == Some('\\')
                        && self.peek(1) == Some('u')
                    {
                        let low: Option<u32> = (2..6)
                            .map(|k| self.peek(k).and_then(|d| d.to_digit(16)))
                            .try_fold(0, |a, d| d.map(|d| a * 16 + d));
                        if let Some(low) = low.filter(|l| (0xDC00..0xE000).contains(l)) {
                            for _ in 0..6 {
                                self.bump();
                            }
                            out.push(
                                char::from_u32(0x10000 + ((v - 0xD800) << 10) + (low - 0xDC00))
                                    .unwrap_or('\u{fffd}'),
                            );
                            return Ok(());
                        }
                    }
                    v
                };
                out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
                Ok(())
            }
            _ => self.escape(out).map_err(|_| UNICODE),
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

/// `\r\n` and `\r` in the raw text of a template become `\n` (ECMA-262 §13.2.8.6).
fn normalize_breaks(raw: &mut String) {
    if raw.contains('\r') {
        *raw = raw.replace("\r\n", "\n").replace('\r', "\n");
    }
}
