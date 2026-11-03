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
    /// A private name, `#` included (`#x`).
    Private(Rc<str>),
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
    /// Why strict mode code may not hold this token: a legacy octal number or an octal escape
    /// (the parser checks it, [`crate::parser`]).
    pub legacy: Option<&'static str>,
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
    "&", "|", "^", "!", "~", "?", ":", "=", ".",
];

const LEGACY_NUMBER: &str =
    "\"0\"-prefixed octal literals are deprecated; use the \"0o\" prefix instead";
const LEGACY_ESCAPE: &str =
    "octal escape sequences can't be used in untagged template literals or in strict mode code";
const ESCAPE_89: &str =
    "the escapes \\8 and \\9 can't be used in untagged template literals or in strict mode code";
const MALFORMED_U: &str = "malformed Unicode character escape sequence";
const MALFORMED_X: &str = "malformed hexadecimal character escape sequence";
const TOO_BIG: &str = "Unicode codepoint must not be greater than 0x10FFFF in escape sequence";
const BAD_IDENT_ESCAPE: &str = "invalid escape sequence";
const NO_EXPONENT: &str = "missing exponent";
const NUMBER_THEN_IDENT: &str = "identifier starts immediately after numeric literal";

/// Splits `src` into tokens.
pub fn tokenize(src: &str) -> Result<Vec<Token>, SyntaxError> {
    tokenize_at(src, 1, 1)
}

/// Splits `src`, which starts at `line` and `col` of a larger source, into tokens.
pub fn tokenize_at(src: &str, line: u32, col: u32) -> Result<Vec<Token>, SyntaxError> {
    let mut lx = Lexer { s: src.chars().collect(), i: 0, line, col, out: Vec::new(), legacy: None };
    lx.run()?;
    Ok(lx.out)
}

struct Lexer {
    s: Vec<char>,
    i: usize,
    line: u32,
    col: u32,
    out: Vec<Token>,
    /// The `legacy` of the token being read.
    legacy: Option<&'static str>,
}

fn is_id_start(c: char) -> bool {
    c == '$' || c == '_' || c.is_alphabetic()
}

/// `ID_Continue` (and ZWNJ, ZWJ): letters, decimal digits, combining marks and connector
/// punctuation; a superscript or a fraction is not one (`a²` is `illegal character U+00B2` on
/// a `refrouter` 9.0.4, measured on 02-11).
fn is_id_part(c: char) -> bool {
    if is_id_start(c) || c.is_ascii_digit() || c == '\u{200c}' || c == '\u{200d}' {
        return true;
    }
    if c.is_ascii() {
        return false;
    }
    const MARKS_AND_CONNECTORS: &[(char, char)] = &[
        ('\u{0300}', '\u{036F}'),
        ('\u{1AB0}', '\u{1AFF}'),
        ('\u{1DC0}', '\u{1DFF}'),
        ('\u{203F}', '\u{2040}'),
        ('\u{2054}', '\u{2054}'),
        ('\u{20D0}', '\u{20FF}'),
        ('\u{FE20}', '\u{FE2F}'),
        ('\u{FE33}', '\u{FE34}'),
        ('\u{FE4D}', '\u{FE4F}'),
        ('\u{FF3F}', '\u{FF3F}'),
    ];
    // Numbers that are not decimal digits (`No`).
    const OTHER_NUMBERS: &[(char, char)] = &[
        ('\u{00B2}', '\u{00B3}'),
        ('\u{00B9}', '\u{00B9}'),
        ('\u{00BC}', '\u{00BE}'),
        ('\u{2070}', '\u{2089}'),
        ('\u{2150}', '\u{215F}'),
        ('\u{2189}', '\u{2189}'),
        ('\u{2460}', '\u{24FF}'),
        ('\u{2776}', '\u{2793}'),
        ('\u{3192}', '\u{3195}'),
        ('\u{3220}', '\u{3229}'),
        ('\u{3248}', '\u{325F}'),
        ('\u{3280}', '\u{3289}'),
        ('\u{32B1}', '\u{32BF}'),
    ];
    let within = |table: &[(char, char)]| table.iter().any(|&(a, b)| (a..=b).contains(&c));
    within(MARKS_AND_CONNECTORS) || (c.is_numeric() && !within(OTHER_NUMBERS))
}

/// Whether `c` ends a line.
fn line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
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
            if line_break(c) {
                newline = true;
                self.bump();
                continue;
            }
            if c.is_whitespace() || c == '\u{feff}' {
                self.bump();
                continue;
            }
            // `//`, and the comments of HTML: `<!--`, and `-->` first on a line (Annex B.1.1).
            let html_open = c == '<' && (1..4).all(|k| self.peek(k) == "<!--".chars().nth(k));
            let html_close = c == '-'
                && self.peek(1) == Some('-')
                && self.peek(2) == Some('>')
                && (newline || self.out.is_empty());
            if (c == '/' && self.peek(1) == Some('/')) || html_open || html_close {
                while self.peek(0).is_some_and(|c| !line_break(c)) {
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
            let legacy = self.legacy.take();
            self.out.push(Token { tok, line, col, newline_before: newline, legacy });
            newline = false;
        }
        self.out.push(Token {
            tok: Tok::Eof,
            line: self.line,
            col: self.col,
            newline_before: true,
            legacy: None,
        });
        Ok(())
    }

    /// An identifier, its `\u` escapes read: each must give a character an identifier can
    /// hold there.
    fn word(&mut self) -> Result<String, SyntaxError> {
        let mut w = String::new();
        while let Some(c) = self.peek(0) {
            if is_id_part(c) {
                w.push(c);
                self.bump();
            } else if c == '\\' {
                self.bump();
                let code = if self.bump() == Some('u') { self.hex_escape() } else { None };
                let fits = |ch: char| if w.is_empty() { is_id_start(ch) } else { is_id_part(ch) };
                let Some(ch) = code.and_then(char::from_u32).filter(|&ch| fits(ch)) else {
                    return self.err(BAD_IDENT_ESCAPE);
                };
                w.push(ch);
            } else {
                break;
            }
        }
        Ok(w)
    }

    fn token(&mut self, c: char) -> Result<Tok, SyntaxError> {
        if c == '#' {
            self.bump();
            if !self.peek(0).is_some_and(|n| is_id_start(n) || n == '\\') {
                return self.err("'#' not followed by identifier");
            }
            let w = self.word()?;
            return Ok(Tok::Private(format!("#{w}").into()));
        }
        if is_id_start(c) || c == '\\' {
            return Ok(Tok::Ident(self.word()?.into()));
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
        let prefix = match (self.peek(0), self.peek(1)) {
            (Some('0'), Some('x' | 'X')) => Some((16, "hexadecimal", 'x')),
            (Some('0'), Some('o' | 'O')) => Some((8, "octal", 'o')),
            (Some('0'), Some('b' | 'B')) => Some((2, "binary", 'b')),
            _ => None,
        };
        if let Some((radix, name, letter)) = prefix {
            self.bump();
            self.bump();
            let text = self.digits(radix)?;
            if text.is_empty() {
                return self.err(format!("missing {name} digits after '0{letter}'"));
            }
            let v = text
                .chars()
                .filter_map(|c| c.to_digit(radix))
                .fold(0f64, |v, d| v * f64::from(radix) + f64::from(d));
            return self.after_number(v, Some((&text, radix)));
        }
        if self.peek(0) == Some('0') && self.peek(1).is_some_and(|d| d.is_ascii_digit() || d == '_')
        {
            return self.legacy_number();
        }
        let text = self.digits(10)?;
        self.decimal(text)
    }

    /// The digits of `radix` from here, with `_` allowed only between two of them.
    fn digits(&mut self, radix: u32) -> Result<String, SyntaxError> {
        let mut text = String::new();
        loop {
            match self.peek(0) {
                Some('_') if !text.is_empty() => {
                    if self.peek(1) == Some('_') {
                        return self.err("number cannot contain multiple adjacent underscores");
                    }
                    if !self.peek(1).is_some_and(|d| d.is_digit(radix)) {
                        return self.err(
                            "underscore can appear only between digits, not after the last digit in a number",
                        );
                    }
                    self.bump();
                }
                Some(c) if c.is_digit(radix) => {
                    text.push(c);
                    self.bump();
                }
                _ => return Ok(text),
            }
        }
    }

    /// A decimal number whose integer digits `text` are read: its fraction and exponent.
    fn decimal(&mut self, mut text: String) -> Result<Tok, SyntaxError> {
        let mut integer = true;
        if self.peek(0) == Some('.') {
            self.bump();
            text = format!("0{text}.{}0", self.digits(10)?);
            integer = false;
        }
        if matches!(self.peek(0), Some('e' | 'E')) {
            self.bump();
            text.push('e');
            if let Some(s @ ('+' | '-')) = self.peek(0) {
                self.bump();
                text.push(s);
            }
            let exponent = self.digits(10)?;
            if exponent.is_empty() {
                return self.err(NO_EXPONENT);
            }
            text.push_str(&exponent);
            integer = false;
        }
        let v = text.parse::<f64>().unwrap_or(f64::NAN);
        self.after_number(v, integer.then_some((text.as_str(), 10)))
    }

    /// A number that starts with `0` and a digit: octal when every digit is below 8, decimal
    /// otherwise (Annex B.1.1), and not allowed in strict mode code.
    fn legacy_number(&mut self) -> Result<Tok, SyntaxError> {
        const SEPARATOR: &str =
            "numeric separators '_' are not allowed in numbers that start with '0'";
        let mut text = String::new();
        while let Some(c) = self.peek(0).filter(char::is_ascii_digit) {
            text.push(c);
            self.bump();
        }
        if self.peek(0) == Some('_') {
            return self.err(SEPARATOR);
        }
        if self.peek(0) == Some('n') {
            return self.err("invalid BigInt syntax");
        }
        self.legacy = Some(LEGACY_NUMBER);
        if text.bytes().all(|b| b < b'8') {
            let v = text.bytes().fold(0f64, |v, b| v * 8.0 + f64::from(b - b'0'));
            return self.after_number(v, None);
        }
        self.decimal(text)
    }

    /// The token of a number read as `v`; `digits` are its integer digits and radix, which a
    /// `n` after them makes a `BigInt`.
    fn after_number(&mut self, v: f64, digits: Option<(&str, u32)>) -> Result<Tok, SyntaxError> {
        let bigint = match digits {
            Some((d, r)) if self.peek(0) == Some('n') => crate::bigint::BigInt::parse_radix(d, r),
            _ => None,
        };
        if let Some(b) = bigint {
            self.bump();
            if self.peek(0).is_some_and(is_id_start) {
                return self.err(NUMBER_THEN_IDENT);
            }
            return Ok(Tok::BigInt(Rc::new(b)));
        }
        if self.peek(0).is_some_and(|c| is_id_start(c) || c == '\\') {
            return self.err(NUMBER_THEN_IDENT);
        }
        Ok(Tok::Num(v))
    }

    /// The value of a `\\u` escape after the `u`: four hexadecimal digits or `{` digits `}`;
    /// `None` when it is malformed.
    fn hex_escape(&mut self) -> Option<u32> {
        if self.peek(0) != Some('{') {
            let mut v = 0u32;
            for _ in 0..4 {
                v = v * 16 + self.peek(0)?.to_digit(16)?;
                self.bump();
            }
            return Some(v);
        }
        self.bump();
        let mut v = 0u32;
        let mut any = false;
        while let Some(d) = self.peek(0).and_then(|c| c.to_digit(16)) {
            v = v.saturating_mul(16).saturating_add(d);
            any = true;
            self.bump();
        }
        if !any || self.peek(0) != Some('}') {
            return None;
        }
        self.bump();
        Some(v)
    }

    /// A `\\u` escape of a string or template after the `u`: a code point, a lone surrogate, or
    /// two escapes that make a surrogate pair; or the message of why it is not valid.
    fn u_escape(&mut self) -> Result<u32, &'static str> {
        let v = self.hex_escape().ok_or(MALFORMED_U)?;
        if v > 0x10_FFFF {
            return Err(TOO_BIG);
        }
        if (0xD800..0xDC00).contains(&v) && self.peek(0) == Some('\\') && self.peek(1) == Some('u')
        {
            let save = (self.i, self.line, self.col);
            self.bump();
            self.bump();
            match self.hex_escape() {
                Some(low) if (0xDC00..0xE000).contains(&low) => {
                    return Ok(0x10000 + ((v - 0xD800) << 10) + (low - 0xDC00));
                }
                _ => (self.i, self.line, self.col) = save,
            }
        }
        Ok(v)
    }

    /// A `\\x` escape after the `x`.
    fn x_escape(&mut self) -> Option<char> {
        let mut v = 0u32;
        for _ in 0..2 {
            v = v * 16 + self.peek(0)?.to_digit(16)?;
            self.bump();
        }
        char::from_u32(v)
    }

    /// An escape after `\\` in a string written with `what` (`''`, `""` or a template's).
    fn escape(&mut self, out: &mut String, what: &str) -> Result<(), SyntaxError> {
        let Some(c) = self.bump() else {
            return self.err(format!(
                "reached end of script in the middle of an escape sequence in a {what} literal"
            ));
        };
        match c {
            '0' if !self.peek(0).is_some_and(|d| d.is_ascii_digit()) => out.push('\0'),
            // A legacy octal escape: up to three digits below `\\400` (Annex B.1.2).
            '0'..='7' => {
                let longest = if c <= '3' { 3 } else { 2 };
                let mut v = c.to_digit(8).unwrap_or(0);
                for _ in 1..longest {
                    let Some(d) = self.peek(0).and_then(|d| d.to_digit(8)) else { break };
                    v = v * 8 + d;
                    self.bump();
                }
                crate::text::push_cp(out, v);
                self.legacy = self.legacy.or(Some(LEGACY_ESCAPE));
            }
            '8' | '9' => {
                out.push(c);
                self.legacy = self.legacy.or(Some(ESCAPE_89));
            }
            'x' => match self.x_escape() {
                Some(ch) => out.push(ch),
                None => return self.err(MALFORMED_X),
            },
            'u' => match self.u_escape() {
                Ok(code) => crate::text::push_cp(out, code),
                Err(message) => return self.err(message),
            },
            other => self.plain_escape(other, out),
        }
        Ok(())
    }

    /// An escape after `\\` that is the same in strings and templates.
    fn plain_escape(&mut self, c: char, out: &mut String) {
        match c {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            'r' => out.push('\r'),
            'b' => out.push('\u{8}'),
            'f' => out.push('\u{c}'),
            'v' => out.push('\u{b}'),
            '0' => out.push('\0'),
            // A line continuation.
            '\r' => {
                if self.peek(0) == Some('\n') {
                    self.bump();
                }
            }
            '\n' | '\u{2028}' | '\u{2029}' => {}
            other => out.push(other),
        }
    }

    fn string(&mut self, quote: char) -> Result<String, SyntaxError> {
        let what = if quote == '"' { "\"\"" } else { "''" };
        let mut out = String::new();
        loop {
            match self.bump() {
                None => {
                    return self.err(format!("{what} literal not terminated before end of script"));
                }
                Some('\n' | '\r') => {
                    return self
                        .err(format!("{what} string literal contains an unescaped line break"));
                }
                Some(c) if c == quote => return Ok(out),
                Some('\\') => self.escape(&mut out, what)?,
                Some(c) => out.push(c),
            }
        }
    }

    fn template(&mut self) -> Result<Tok, SyntaxError> {
        let mut parts = TemplateParts { cooked: Vec::new(), raw: Vec::new(), invalid: None };
        let mut subs = Vec::new();
        let (mut cur, mut raw, mut valid) = (String::new(), String::new(), true);
        loop {
            let Some(c) = self.bump() else {
                return self.err("`` literal not terminated before end of script");
            };
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
                '\\' if self.peek(0).is_none() => {
                    return self.err(
                        "reached end of script in the middle of an escape sequence in a `` literal",
                    );
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
        match self.peek(0) {
            Some('0') if self.peek(1).is_some_and(|d| d.is_ascii_digit()) => {
                self.bump();
                Err(LEGACY_ESCAPE)
            }
            Some('1'..='7') => {
                self.bump();
                Err(LEGACY_ESCAPE)
            }
            Some('8' | '9') => {
                self.bump();
                Err(ESCAPE_89)
            }
            Some('x') => {
                self.bump();
                out.push(self.x_escape().ok_or(MALFORMED_X)?);
                Ok(())
            }
            Some('u') => {
                self.bump();
                crate::text::push_cp(out, self.u_escape()?);
                Ok(())
            }
            Some(c) => {
                self.bump();
                self.plain_escape(c, out);
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// The source of a `${…}` up to its closing brace, with nested braces, strings and templates.
    fn substitution(&mut self) -> Result<String, SyntaxError> {
        let mut depth = 0usize;
        let mut out = String::new();
        loop {
            let Some(c) = self.bump() else {
                if out.trim().is_empty() {
                    return self.err("expected expression, got end of script");
                }
                return self.err("missing } in template string");
            };
            match c {
                '{' => depth += 1,
                '}' if depth == 0 => return Ok(out),
                '}' => depth -= 1,
                '"' | '\'' | '`' => {
                    out.push(c);
                    loop {
                        let Some(d) = self.bump() else {
                            return self.err(format!(
                                "{c}{c} literal not terminated before end of script"
                            ));
                        };
                        out.push(d);
                        if d == '\\' {
                            let Some(e) = self.bump() else {
                                return self.err(format!(
                                    "reached end of script in the middle of an escape sequence in \
                                     a {c}{c} literal"
                                ));
                            };
                            out.push(e);
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
