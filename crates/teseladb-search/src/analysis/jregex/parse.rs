//! Reads a pattern in the syntax of `java.util.regex.Pattern` (Java 21 Javadoc) and writes the
//! PCRE2 pattern that matches the same strings with the same groups. Options apply where Java
//! applies them, so the PCRE2 text carries none: case-insensitive characters become the classes
//! of characters Java treats as equal, `.`, `^`, `$` and `\Z` become classes and lookarounds over
//! Java's line terminators, `\b` uses Java's word characters and comments are dropped.

use super::class::{Posix, Prop, property};
use super::names::char_by_name;
use super::set::{ANY, Case, ClassExpr, Part, Ranges, fold, literal, ranges_text};

/// Largest count of a `{n,m}` quantifier: PCRE2's limit, which the engine keeps.
pub const MAX_REPEAT: u32 = 65_535;

/// Why a pattern does not compile, as `PatternSyntaxException` describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// `*`, `+`, `?` or `{` with nothing to repeat.
    DanglingMeta,
    /// A `{` that does not start a valid repetition, or `{n,m}` with `m < n`.
    IllegalRepetition,
    /// A count above [`MAX_REPEAT`].
    RepetitionTooLarge,
    /// A `(` without its `)`.
    UnclosedGroup,
    /// A `)` without its `(`.
    UnmatchedClose,
    /// A `[` without its `]`.
    UnclosedClass,
    /// A range whose end is below its start, or whose end is a class.
    IllegalRange,
    /// A `\` before a letter with no meaning, or at the end.
    IllegalEscape,
    /// A malformed `\0`, `\x`, `\u`, `\c` or `\N{…}`.
    IllegalCharacter,
    /// An unknown `\p{…}`.
    UnknownProperty,
    /// An unknown inline flag or a malformed group construct.
    UnknownGroup,
    /// A group name that is malformed or used twice, or a `\k` to a name that does not exist.
    BadGroupName,
}

/// A syntax error and the character index where it was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyntaxError {
    /// What is wrong.
    pub kind: ErrorKind,
    /// Character index in the pattern.
    pub index: usize,
}

type PResult<T> = Result<T, SyntaxError>;

/// The inline flags of Java.
#[derive(Clone, Copy, Debug, Default)]
struct Flags {
    case_insensitive: bool,
    unicode_case: bool,
    unix_lines: bool,
    multiline: bool,
    dotall: bool,
    comments: bool,
    unicode_class: bool,
}

impl Flags {
    fn case(self) -> Case {
        match (self.case_insensitive, self.unicode_case || self.unicode_class) {
            (false, _) => Case::Exact,
            (true, false) => Case::Ascii,
            (true, true) => Case::Unicode,
        }
    }

    fn terminators(self) -> &'static str {
        if self.unix_lines { "\\x{A}" } else { "\\x{A}\\x{D}\\x{85}\\x{2028}\\x{2029}" }
    }

    /// `$` without `MULTILINE` and `\Z`: the end, or before a final line terminator.
    fn end_before_terminator(self) -> String {
        if self.unix_lines {
            return "(?=\\x{A}?\\z)".to_string();
        }
        format!(
            "(?:(?=(?:\\x{{D}}\\x{{A}}|[{}])?\\z)(?!(?<=\\x{{D}})\\x{{A}}))",
            self.terminators()
        )
    }

    fn set(&mut self, flag: char, on: bool) -> bool {
        let slot = match flag {
            'i' => &mut self.case_insensitive,
            'u' => &mut self.unicode_case,
            'd' => &mut self.unix_lines,
            'm' => &mut self.multiline,
            's' => &mut self.dotall,
            'x' => &mut self.comments,
            'U' => &mut self.unicode_class,
            _ => return false,
        };
        *slot = on;
        true
    }
}

/// The result of reading a pattern.
#[derive(Debug)]
pub struct Translated {
    /// The PCRE2 pattern.
    pub pcre: String,
    /// Number of capturing groups.
    pub groups: usize,
    /// Group names with their numbers.
    pub names: Vec<(String, usize)>,
}

/// What an escape stands for.
enum Escape {
    /// One character.
    Char(char),
    /// A set of characters.
    Set(Part),
    /// Pattern text that is not a character (anchors, back references, `\R`, `\X`).
    Text(String),
    /// The characters of `\Q…\E`.
    Quote(Vec<char>),
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    flags: Flags,
    groups: usize,
    names: Vec<(String, usize)>,
}

/// Reads `pattern`.
pub fn translate(pattern: &str) -> PResult<Translated> {
    let mut p = Parser {
        chars: pattern.chars().collect(),
        pos: 0,
        flags: Flags::default(),
        groups: 0,
        names: Vec::new(),
    };
    let pcre = p.alternation()?;
    if p.pos < p.chars.len() {
        return Err(p.error(ErrorKind::UnmatchedClose));
    }
    Ok(Translated { pcre, groups: p.groups, names: p.names })
}

fn hex(c: char) -> String {
    format!("\\x{{{:X}}}", u32::from(c))
}

impl Parser {
    fn error(&self, kind: ErrorKind) -> SyntaxError {
        SyntaxError { kind, index: self.pos }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<char> {
        self.chars.get(self.pos + ahead).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        let found = self.peek() == Some(c);
        self.pos += usize::from(found);
        found
    }

    fn next(&mut self, kind: ErrorKind) -> PResult<char> {
        let c = self.peek().ok_or(self.error(kind))?;
        self.pos += 1;
        Ok(c)
    }

    /// In `COMMENTS` mode, skips white space and `#` comments up to a line terminator.
    fn skip_comments(&mut self) {
        while self.flags.comments {
            match self.peek() {
                Some(' ' | '\t' | '\n' | '\x0B' | '\x0C' | '\r') => self.pos += 1,
                Some('#') => {
                    while self.peek().is_some_and(|c| {
                        !matches!(c, '\n' | '\r' | '\u{85}' | '\u{2028}' | '\u{2029}')
                    }) {
                        self.pos += 1;
                    }
                }
                _ => break,
            }
        }
    }

    fn alternation(&mut self) -> PResult<String> {
        let mut out = self.sequence()?;
        while self.eat('|') {
            out.push('|');
            out.push_str(&self.sequence()?);
        }
        Ok(out)
    }

    fn sequence(&mut self) -> PResult<String> {
        let mut out = String::new();
        loop {
            self.skip_comments();
            match self.peek() {
                None | Some('|' | ')') => return Ok(out),
                Some('*' | '+' | '?') => return Err(self.error(ErrorKind::DanglingMeta)),
                // Java repeats the empty string with a `{…}` where an atom should be (measured:
                // `x{2}{3}` compiles, `{` alone is an illegal repetition).
                Some('{') => out.push_str(&self.quantifier(String::new())?),
                Some(_) => {
                    let atoms = self.atom()?;
                    let count = atoms.len();
                    for (i, atom) in atoms.into_iter().enumerate() {
                        if i + 1 == count {
                            out.push_str(&self.quantifier(atom)?);
                        } else {
                            out.push_str(&atom);
                        }
                    }
                }
            }
        }
    }

    fn number(&mut self) -> PResult<Option<u32>> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        if start == self.pos {
            return Ok(None);
        }
        let digits: String = self.chars[start..self.pos].iter().collect();
        match digits.parse::<u32>() {
            Ok(n) if n <= MAX_REPEAT => Ok(Some(n)),
            _ => Err(self.error(ErrorKind::RepetitionTooLarge)),
        }
    }

    /// Reads a quantifier after `atom`, if there is one.
    fn quantifier(&mut self, atom: String) -> PResult<String> {
        self.skip_comments();
        let q = match self.peek() {
            Some(c @ ('*' | '+' | '?')) => {
                self.pos += 1;
                c.to_string()
            }
            Some('{') => {
                self.pos += 1;
                let min = self.number()?.ok_or(self.error(ErrorKind::IllegalRepetition))?;
                let max = if self.eat(',') { self.number()? } else { Some(min) };
                if !self.eat('}') || max.is_some_and(|m| m < min) {
                    return Err(self.error(ErrorKind::IllegalRepetition));
                }
                match max {
                    Some(m) if m == min => format!("{{{min}}}"),
                    Some(m) => format!("{{{min},{m}}}"),
                    None => format!("{{{min},}}"),
                }
            }
            _ => return Ok(atom),
        };
        let mode = match self.peek() {
            Some(c @ ('?' | '+')) => {
                self.pos += 1;
                c.to_string()
            }
            _ => String::new(),
        };
        Ok(format!("(?:{atom}){q}{mode}"))
    }

    fn literal_text(&self, c: char) -> String {
        let r = literal(c, self.flags.case());
        if r.len() == 1 && r[0].0 == r[0].1 { hex(c) } else { ranges_text(&r) }
    }

    /// Reads one atom; `\Q…\E` gives one atom per character.
    fn atom(&mut self) -> PResult<Vec<String>> {
        let c = self.next(ErrorKind::DanglingMeta)?;
        let text = match c {
            '(' => self.group()?,
            '[' => self.class()?.text(),
            '.' if self.flags.dotall => ANY.to_string(),
            '.' => format!("[^{}]", self.flags.terminators()),
            '^' => self.start_anchor(),
            '$' => self.end_anchor(),
            '\\' => match self.escape(false)? {
                Escape::Char(c) => self.literal_text(c),
                Escape::Set(part) => ClassExpr { parts: vec![part], ..ClassExpr::default() }.text(),
                Escape::Text(t) => t,
                Escape::Quote(chars) => {
                    return Ok(chars.into_iter().map(|c| self.literal_text(c)).collect());
                }
            },
            c => self.literal_text(c),
        };
        Ok(vec![text])
    }

    fn start_anchor(&self) -> String {
        match (self.flags.multiline, self.flags.unix_lines) {
            (false, _) => "\\A".to_string(),
            (true, true) => "(?:\\A|(?<=\\x{A})(?!\\z))".to_string(),
            (true, false) => {
                "(?:\\A|(?:(?<=[\\x{A}\\x{85}\\x{2028}\\x{2029}])|(?<=\\x{D})(?!\\x{A}))(?!\\z))"
                    .to_string()
            }
        }
    }

    fn end_anchor(&self) -> String {
        if !self.flags.multiline {
            return self.flags.end_before_terminator();
        }
        if self.flags.unix_lines {
            return "(?=\\x{A}|\\z)".to_string();
        }
        format!("(?:(?=[{}]|\\z)(?!(?<=\\x{{D}})\\x{{A}}))", self.flags.terminators())
    }

    fn word_boundary(&self, negate: bool) -> String {
        let w = ClassExpr {
            parts: vec![Part::Prop(Prop::Posix(Posix::Word, self.flags.unicode_class), false)],
            ..ClassExpr::default()
        }
        .text();
        if negate {
            format!("(?:(?<={w})(?={w})|(?<!{w})(?!{w}))")
        } else {
            format!("(?:(?<={w})(?!{w})|(?<!{w})(?={w}))")
        }
    }

    fn group_name(&mut self) -> PResult<String> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_alphanumeric()) {
            self.pos += 1;
        }
        let name: String = self.chars[start..self.pos].iter().collect();
        let valid = name.starts_with(|c: char| c.is_ascii_alphabetic());
        if !valid || !self.eat('>') {
            return Err(self.error(ErrorKind::BadGroupName));
        }
        Ok(name)
    }

    /// Reads a group after its `(`.
    fn group(&mut self) -> PResult<String> {
        let saved = self.flags;
        let open = if !self.eat('?') {
            self.groups += 1;
            "(".to_string()
        } else {
            match self.next(ErrorKind::UnknownGroup)? {
                ':' => "(?:".to_string(),
                '=' => "(?=".to_string(),
                '!' => "(?!".to_string(),
                '>' => "(?>".to_string(),
                '<' if self.eat('=') => "(?<=".to_string(),
                '<' if self.eat('!') => "(?<!".to_string(),
                '<' => {
                    let name = self.group_name()?;
                    if self.names.iter().any(|(n, _)| *n == name) {
                        return Err(self.error(ErrorKind::BadGroupName));
                    }
                    self.groups += 1;
                    self.names.push((name.clone(), self.groups));
                    format!("(?<{name}>")
                }
                _ => {
                    self.pos -= 1;
                    return self.inline_flags(saved);
                }
            }
        };
        let body = self.alternation()?;
        if !self.eat(')') {
            return Err(self.error(ErrorKind::UnclosedGroup));
        }
        self.flags = saved;
        Ok(format!("{open}{body})"))
    }

    /// `(?idmsuxU-idmsuxU)` and `(?idmsuxU-idmsuxU:X)`.
    fn inline_flags(&mut self, saved: Flags) -> PResult<String> {
        let mut on = true;
        loop {
            match self.next(ErrorKind::UnknownGroup)? {
                ')' => return Ok(String::new()),
                ':' => break,
                '-' if on => on = false,
                c if self.flags.set(c, on) => {}
                _ => {
                    return Err(SyntaxError { kind: ErrorKind::UnknownGroup, index: self.pos - 1 });
                }
            }
        }
        let body = self.alternation()?;
        if !self.eat(')') {
            return Err(self.error(ErrorKind::UnclosedGroup));
        }
        self.flags = saved;
        Ok(format!("(?:{body})"))
    }

    fn hex_digits(&mut self, count: usize) -> PResult<u32> {
        let mut value = 0;
        for _ in 0..count {
            let d = self.next(ErrorKind::IllegalCharacter)?.to_digit(16);
            value = value * 16 + d.ok_or(self.error(ErrorKind::IllegalCharacter))?;
        }
        Ok(value)
    }

    fn code_point(&self, value: u32) -> PResult<Escape> {
        Ok(match char::from_u32(value) {
            Some(c) => Escape::Char(c),
            None if (0xD800..=0xDFFF).contains(&value) => Escape::Set(Part::Ranges(Vec::new())),
            None => return Err(self.error(ErrorKind::IllegalCharacter)),
        })
    }

    /// `\u` with a following `\u` low surrogate after a high one combined into one code point.
    fn unicode_escape(&mut self) -> PResult<Escape> {
        let high = self.hex_digits(4)?;
        if (0xD800..0xDC00).contains(&high)
            && self.peek() == Some('\\')
            && self.peek_at(1) == Some('u')
        {
            let save = self.pos;
            self.pos += 2;
            match self.hex_digits(4) {
                Ok(low) if (0xDC00..0xE000).contains(&low) => {
                    return self.code_point(0x10000 + ((high - 0xD800) << 10) + (low - 0xDC00));
                }
                _ => self.pos = save,
            }
        }
        self.code_point(high)
    }

    fn octal(&mut self) -> PResult<Escape> {
        let digit = |c: Option<char>| c.and_then(|c| c.to_digit(8));
        let first = digit(self.peek()).ok_or(self.error(ErrorKind::IllegalCharacter))?;
        self.pos += 1;
        let mut value = first;
        for i in 0..2 {
            match digit(self.peek()) {
                Some(d) if i == 0 || first <= 3 => {
                    value = value * 8 + d;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        self.code_point(value)
    }

    fn property_escape(&mut self, negated: bool) -> PResult<Escape> {
        let name: String = if self.eat('{') {
            let start = self.pos;
            while self.peek().is_some_and(|c| c != '}') {
                self.pos += 1;
            }
            let name = self.chars[start..self.pos].iter().collect();
            if !self.eat('}') {
                return Err(self.error(ErrorKind::UnknownProperty));
            }
            name
        } else {
            self.next(ErrorKind::UnknownProperty)?.to_string()
        };
        let prop = property(&name, self.flags.unicode_class)
            .ok_or(self.error(ErrorKind::UnknownProperty))?;
        Ok(Escape::Set(Part::Prop(prop, negated)))
    }

    fn backref(&mut self, first: u32) -> String {
        let mut group = first as usize;
        while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
            let longer = group * 10 + d as usize;
            if longer > self.groups {
                break;
            }
            group = longer;
            self.pos += 1;
        }
        if group > self.groups {
            return "(?!)".to_string();
        }
        self.case_wrap(format!("\\g{{{group}}}"))
    }

    fn case_wrap(&self, text: String) -> String {
        if self.flags.case_insensitive { format!("(?i:{text})") } else { text }
    }

    fn set(&self, prop: Prop, negated: bool) -> Escape {
        Escape::Set(Part::Prop(prop, negated))
    }

    /// Reads an escape after its `\`.
    fn escape(&mut self, in_class: bool) -> PResult<Escape> {
        let c = self.next(ErrorKind::IllegalEscape)?;
        let ucc = self.flags.unicode_class;
        let anchor = |p: &Self, text: String| {
            if in_class {
                Err(SyntaxError { kind: ErrorKind::IllegalEscape, index: p.pos - 1 })
            } else {
                Ok(Escape::Text(text))
            }
        };
        Ok(match c {
            '0' => self.octal()?,
            '1'..='9' if !in_class => Escape::Text(self.backref(c as u32 - '0' as u32)),
            'x' if self.eat('{') => {
                let start = self.pos;
                while self.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                    self.pos += 1;
                }
                let digits: String = self.chars[start..self.pos].iter().collect();
                let value = u32::from_str_radix(&digits, 16).ok().filter(|_| self.eat('}'));
                self.code_point(value.ok_or(self.error(ErrorKind::IllegalCharacter))?)?
            }
            'x' => {
                let v = self.hex_digits(2)?;
                self.code_point(v)?
            }
            'u' => self.unicode_escape()?,
            't' => Escape::Char('\t'),
            'n' => Escape::Char('\n'),
            'r' => Escape::Char('\r'),
            'f' => Escape::Char('\x0C'),
            'a' => Escape::Char('\x07'),
            'e' => Escape::Char('\x1B'),
            'c' => {
                let x = self.next(ErrorKind::IllegalCharacter)?;
                self.code_point(u32::from(x) ^ 0x40)?
            }
            'N' => {
                if !self.eat('{') {
                    return Err(self.error(ErrorKind::IllegalCharacter));
                }
                let start = self.pos;
                while self.peek().is_some_and(|c| c != '}') {
                    self.pos += 1;
                }
                let name: String = self.chars[start..self.pos].iter().collect();
                let found = char_by_name(&name).filter(|_| self.eat('}'));
                Escape::Char(found.ok_or(self.error(ErrorKind::IllegalCharacter))?)
            }
            'd' | 'D' => self.set(Prop::Posix(Posix::Digit, ucc), c == 'D'),
            's' | 'S' => self.set(Prop::Posix(Posix::Space, ucc), c == 'S'),
            'w' | 'W' => self.set(Prop::Posix(Posix::Word, ucc), c == 'W'),
            'h' | 'H' => self.set(Prop::Horizontal, c == 'H'),
            'v' | 'V' => self.set(Prop::Vertical, c == 'V'),
            'p' | 'P' => self.property_escape(c == 'P')?,
            'Q' => {
                let mut quoted = Vec::new();
                while let Some(q) = self.peek() {
                    if q == '\\' && self.peek_at(1) == Some('E') {
                        self.pos += 2;
                        break;
                    }
                    quoted.push(q);
                    self.pos += 1;
                }
                Escape::Quote(quoted)
            }
            'b' if self.peek() == Some('{') => {
                if !(self.eat('{') && self.eat('g') && self.eat('}')) {
                    return Err(SyntaxError {
                        kind: ErrorKind::IllegalEscape,
                        index: self.pos - 1,
                    });
                }
                anchor(self, "\\b{g}".to_string())?
            }
            'b' | 'B' => anchor(self, self.word_boundary(c == 'B'))?,
            'A' => anchor(self, "\\A".to_string())?,
            'G' => anchor(self, "\\G".to_string())?,
            'z' => anchor(self, "\\z".to_string())?,
            'Z' => anchor(self, self.flags.end_before_terminator())?,
            'R' => anchor(self, "\\R".to_string())?,
            'X' => anchor(self, "\\X".to_string())?,
            'k' if !in_class => {
                if !self.eat('<') {
                    return Err(self.error(ErrorKind::BadGroupName));
                }
                let name = self.group_name()?;
                if !self.names.iter().any(|(n, _)| *n == name) {
                    return Err(self.error(ErrorKind::BadGroupName));
                }
                Escape::Text(self.case_wrap(format!("\\k<{name}>")))
            }
            c if c.is_ascii_alphanumeric() => {
                return Err(SyntaxError { kind: ErrorKind::IllegalEscape, index: self.pos - 1 });
            }
            c => Escape::Char(c),
        })
    }

    /// Reads a class after its `[`.
    fn class(&mut self) -> PResult<ClassExpr> {
        let negated = self.eat('^');
        let mut operands: Vec<Vec<Part>> = vec![Vec::new()];
        let mut first = true;
        loop {
            self.skip_comments();
            let Some(c) = self.peek() else {
                return Err(self.error(ErrorKind::UnclosedClass));
            };
            let current = operands.len() - 1;
            match c {
                ']' if !first => {
                    self.pos += 1;
                    break;
                }
                '[' => {
                    self.pos += 1;
                    let nested = self.class()?;
                    operands[current].push(Part::Class(nested));
                }
                '&' if self.peek_at(1) == Some('&') => {
                    self.pos += 2;
                    operands.push(Vec::new());
                }
                _ => {
                    let part = self.class_item()?;
                    operands[current].push(part);
                }
            }
            first = false;
        }
        let mut operands = operands.into_iter().filter(|o| !o.is_empty());
        let parts = operands.next().unwrap_or_default();
        let and = operands.map(|parts| ClassExpr { parts, ..ClassExpr::default() }).collect();
        Ok(ClassExpr { parts, and, negated })
    }

    /// One character of a class, with its escapes.
    fn class_char(&mut self) -> PResult<Result<char, Part>> {
        if !self.eat('\\') {
            return self.next(ErrorKind::UnclosedClass).map(Ok);
        }
        Ok(match self.escape(true)? {
            Escape::Char(c) => Ok(c),
            Escape::Set(part) => Err(part),
            Escape::Quote(chars) => Err(Part::Ranges(fold(
                chars.into_iter().map(|c| (c, c)).collect(),
                self.flags.case(),
            ))),
            Escape::Text(_) => return Err(self.error(ErrorKind::IllegalEscape)),
        })
    }

    /// A character, a range or a set inside a class.
    fn class_item(&mut self) -> PResult<Part> {
        let lo = match self.class_char()? {
            Ok(c) => c,
            Err(part) => return Ok(part),
        };
        self.skip_comments();
        let range = self.peek() == Some('-') && !matches!(self.peek_at(1), Some(']' | '[') | None);
        let hi = if range {
            self.pos += 1;
            self.skip_comments();
            match self.class_char()? {
                Ok(hi) if hi >= lo => hi,
                _ => return Err(self.error(ErrorKind::IllegalRange)),
            }
        } else {
            lo
        };
        let ranges: Ranges = vec![(lo, hi)];
        Ok(Part::Ranges(fold(ranges, self.flags.case())))
    }
}
