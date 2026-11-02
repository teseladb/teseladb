//! Escapes and character classes of the parser: `\d`, `\p{..}`, `\x{..}`, back references by
//! number and name, `\Q...\E`, and `[...]` with ranges, POSIX classes and properties.

use super::{Flags, PResult, ParseError, Parser, group_limit, is_name_char};
use crate::regex::ast::{Assertion, Case, Class, Item, Node, Posix, Prop, Set};
use crate::regex::unicode::{bidi_index, binary_index, category_mask, loose, script_index};

/// A braced hexadecimal code point without its `}`.
const NON_HEX: &str = "non-hex character in \\x{} (closing brace missing?)";
/// A braced octal code point without its `}`.
const NON_OCTAL: &str = "non-octal character in \\o{} (closing brace missing?)";
/// A relative reference or call of `+0` or `-0`.
pub(super) const RELATIVE_ZERO: &str = "a relative value of zero is not allowed";
/// A reference to a group the pattern does not have.
const NO_SUCH_GROUP: &str = "reference to non-existent subpattern";

impl Parser {
    /// An escape outside a class.
    pub(super) fn escape(&mut self, flags: &mut Flags) -> PResult<Option<Node>> {
        let Some(c) = self.peek() else { return Err("\\ at end of pattern".into()) };
        self.pos += 1;
        let (dig, spc, wrd) = self.ucp_escapes(flags);
        let set = |set: Set, negate: bool| {
            Node::Class(Class {
                negated: false,
                items: vec![Item::Set { set, negate }],
                caseless: Case::Sensitive,
            })
        };
        let node = match c {
            'd' | 'D' => set(Set::Digit { ucp: dig }, c == 'D'),
            'w' | 'W' => set(Set::Word { ucp: wrd }, c == 'W'),
            's' | 'S' => set(Set::Space { ucp: spc }, c == 'S'),
            'h' | 'H' => set(Set::HSpace, c == 'H'),
            'v' | 'V' => set(Set::VSpace, c == 'V'),
            'N' if self.peek() == Some('{') && !self.quantifier_at(self.pos + 1) => {
                Node::Char { c: self.unicode_name()?, caseless: flags.case() }
            }
            'N' => Node::Dot { dotall: false },
            'p' | 'P' => {
                let (prop, negated) = self.property()?;
                set(Set::Prop(prop), (c == 'P') != negated)
            }
            'R' => Node::NewlineSeq,
            'X' => Node::Grapheme,
            'C' => Node::Dot { dotall: true },
            'b' if flags.translated && self.eat_str("{g}") => {
                Node::Assert(Assertion::GraphemeBoundary)
            }
            'b' | 'B' => Node::Assert(Assertion::WordBoundary { negate: c == 'B', ucp: wrd }),
            'A' => Node::Assert(Assertion::SubjectStart),
            'Z' => Node::Assert(Assertion::SubjectEndNewline),
            'z' => Node::Assert(Assertion::SubjectEnd),
            'G' => Node::Assert(Assertion::MatchStart),
            'K' if self.looks > 0 => {
                return Err(
                    "\\K is not allowed in lookarounds (but see PCRE2_EXTRA_ALLOW_LOOKAROUND_BSK)"
                        .into(),
                );
            }
            'K' => Node::KeepOut,
            'Q' => return Ok(self.quoted(flags)),
            'E' => return Ok(None),
            'g' => return self.g_reference(flags).map(Some),
            'k' => {
                let close = match self.peek() {
                    Some('<') => '>',
                    Some('\'') => '\'',
                    Some('{') => '}',
                    _ => {
                        return Err(
                            "\\k is not followed by a braced, angle-bracketed, or quoted name"
                                .into(),
                        );
                    }
                };
                self.pos += 1;
                if close == '}' {
                    self.skip_brace_space();
                    let name = self.take_while(is_name_char);
                    self.skip_brace_space();
                    Node::NamedBackref {
                        name: self.checked_name(name, '}')?,
                        caseless: flags.case(),
                    }
                } else {
                    Node::NamedBackref { name: self.name(close)?, caseless: flags.case() }
                }
            }
            '1'..='9' => {
                self.pos -= 1;
                let start = self.pos;
                let digits = self.take_while(|c| c.is_ascii_digit());
                let n: usize = digits.parse().unwrap_or(usize::MAX);
                if n < 10 || n <= self.groups {
                    self.max_backref = self.max_backref.max(n);
                    return Ok(Some(Node::Backref { groups: vec![n], caseless: flags.case() }));
                }
                self.pos = start;
                if let Some(first @ ('8' | '9')) = digits.chars().next() {
                    // `\8…` and `\9…` are back references, up to PCRE2's largest group; a
                    // number past its 32-bit integers is the digit itself (`\9999999999`
                    // matches ten nines).
                    if digits.parse::<i32>().is_err() {
                        self.pos += 1;
                        return Ok(Some(Node::Char { c: first, caseless: flags.case() }));
                    }
                    let n = group_limit(&digits)?;
                    self.max_backref = self.max_backref.max(n);
                    self.pos += digits.len();
                    return Ok(Some(Node::Backref { groups: vec![n], caseless: flags.case() }));
                }
                match self.octal_after_digit()? {
                    Some(ch) => Node::Char { c: ch, caseless: flags.case() },
                    None => {
                        let d = self.peek().unwrap_or('8');
                        self.pos += 1;
                        Node::Char { c: d, caseless: flags.case() }
                    }
                }
            }
            _ => {
                self.pos -= 1;
                let ch = self.char_escape(false)?;
                Node::Char { c: ch, caseless: flags.case() }
            }
        };
        Ok(Some(node))
    }

    /// `\Q...\E`: literal characters up to `\E` or the end.
    fn quoted(&mut self, flags: &Flags) -> Option<Node> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '\\' && self.peek_at(1) == Some('E') {
                self.pos += 2;
                break;
            }
            self.pos += 1;
            items.push(Node::Char { c, caseless: flags.case() });
        }
        match items.len() {
            0 => None,
            1 => items.pop(),
            _ => Some(Node::Group(Box::new(Node::Concat(items)), None)),
        }
    }

    /// The `}` that closes the digits of a braced code point (`\x{}`, `\o{}`, `\N{U+}`):
    /// at the brace or at the end of the pattern without digits they are missing; any other
    /// character where the brace goes is not a digit (`unclosed`, as PCRE2 words it).
    fn closing_brace(&mut self, digits: &str, unclosed: &str) -> PResult<()> {
        let closed = self.eat('}');
        if digits.is_empty() && (closed || self.peek().is_none()) {
            return Err("digits missing in \\x{} or \\o{} or \\N{U+}".into());
        }
        if !closed {
            return Err(unclosed.into());
        }
        Ok(())
    }

    /// `\N{U+hh..}` after the `N`: a character by its code point.
    fn unicode_name(&mut self) -> PResult<char> {
        self.eat('{');
        self.skip_brace_space();
        if !self.eat_str("U+") {
            return Err(unsupported_escape());
        }
        let digits = self.take_while(|c| c.is_ascii_hexdigit());
        self.skip_brace_space();
        self.closing_brace(&digits, NON_HEX)?;
        code_point(u32::from_str_radix(&digits, 16).ok())
    }

    /// `\g`: `\gn`, `\g{n}`, `\g{-n}`, `\g{name}` are back references; `\g<...>` and `\g'...'`
    /// call a group by name or number (Oniguruma's syntax).
    fn g_reference(&mut self, flags: &Flags) -> PResult<Node> {
        if let Some(open) = self.peek().filter(|c| matches!(c, '<' | '\'')) {
            self.pos += 1;
            let close = if open == '<' { '>' } else { '\'' };
            if self.peek().is_some_and(|c| c.is_ascii_digit() || c == '+' || c == '-') {
                let group = self.group_number()?;
                if !self.eat(close) {
                    return Err("\\g is not followed by a braced, angle-bracketed, or quoted name/number or by a plain number".into());
                }
                return Ok(Node::Call(group));
            }
            return Ok(Node::NamedCall(self.name(close)?));
        }
        let braced = self.eat('{');
        if braced {
            self.skip_brace_space();
        }
        let negative = self.eat('-');
        let positive = !negative && self.eat('+');
        let digits = self.take_while(|c| c.is_ascii_digit());
        if digits.is_empty() {
            if braced && !negative && !positive {
                let name = self.take_while(is_name_char);
                self.skip_brace_space();
                let name = self.checked_name(name, '}')?;
                return Ok(Node::NamedBackref { name, caseless: flags.case() });
            }
            return Err("a numbered reference must not be zero".into());
        }
        if braced {
            self.skip_brace_space();
        }
        if braced && !self.eat('}') {
            return Err("\\g is not followed by a braced, angle-bracketed, or quoted name/number or by a plain number".into());
        }
        let n = group_limit(&digits)?;
        if (negative || positive) && n == 0 {
            return Err(RELATIVE_ZERO.into());
        }
        let group = if negative {
            (self.groups + 1).checked_sub(n).filter(|g| *g > 0).ok_or(NO_SUCH_GROUP)?
        } else if positive {
            self.groups + n
        } else {
            n
        };
        // `\g0` names the whole pattern, which no back reference can be (measured).
        if group == 0 {
            return Err(NO_SUCH_GROUP.into());
        }
        self.max_backref = self.max_backref.max(group);
        Ok(Node::Backref { groups: vec![group], caseless: flags.case() })
    }

    /// Up to three octal digits starting at a digit that is not a back reference.
    fn octal_after_digit(&mut self) -> PResult<Option<char>> {
        let start = self.pos;
        let digits: String =
            self.chars[start..].iter().take(3).take_while(|c| ('0'..='7').contains(c)).collect();
        if digits.is_empty() {
            return Ok(None);
        }
        self.pos += digits.len();
        let v = u32::from_str_radix(&digits, 8).map_err(|_| "invalid octal".to_string())?;
        Ok(char::from_u32(v))
    }

    /// A character escape (after `\`, at the escaped character); `in_class` reads `\b` as a
    /// backspace.
    fn char_escape(&mut self, in_class: bool) -> PResult<char> {
        let Some(c) = self.peek() else { return Err("\\ at end of pattern".into()) };
        self.pos += 1;
        Ok(match c {
            'a' => '\u{7}',
            'e' => '\u{1B}',
            'f' => '\u{C}',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'b' if in_class => '\u{8}',
            '0' => {
                let digits: String = self.chars[self.pos..]
                    .iter()
                    .take(2)
                    .take_while(|c| ('0'..='7').contains(c))
                    .collect();
                self.pos += digits.len();
                char::from_u32(u32::from_str_radix(&format!("0{digits}"), 8).unwrap_or(0))
                    .unwrap_or('\0')
            }
            'o' => {
                if !self.eat('{') {
                    return Err("missing opening brace after \\o".into());
                }
                self.skip_brace_space();
                let digits = self.take_while(|c| ('0'..='7').contains(&c));
                self.skip_brace_space();
                self.closing_brace(&digits, NON_OCTAL)?;
                code_point(u32::from_str_radix(&digits, 8).ok())?
            }
            'x' => {
                if self.eat('{') {
                    self.skip_brace_space();
                    let digits = self.take_while(|c| c.is_ascii_hexdigit());
                    self.skip_brace_space();
                    self.closing_brace(&digits, NON_HEX)?;
                    code_point(u32::from_str_radix(&digits, 16).ok())?
                } else {
                    let digits: String = self.chars[self.pos..]
                        .iter()
                        .take(2)
                        .take_while(|c| c.is_ascii_hexdigit())
                        .collect();
                    self.pos += digits.len();
                    code_point(Some(u32::from_str_radix(&digits, 16).unwrap_or(0)))?
                }
            }
            'c' => {
                if self.peek().is_none() {
                    return Err("\\c at end of pattern".into());
                }
                let Some(x) = self.peek().filter(char::is_ascii) else {
                    return Err("\\c must be followed by a printable ASCII character".into());
                };
                self.pos += 1;
                char::from(x.to_ascii_uppercase() as u8 ^ 0x40)
            }
            'u' | 'U' | 'L' | 'l' => return Err(unsupported_escape()),
            c if c.is_ascii_alphanumeric() => {
                return Err("unrecognized character follows \\".into());
            }
            c => c,
        })
    }

    /// The property of `\p` or `\P`, after the letter, and whether `{^...}` negates it.
    fn property(&mut self) -> PResult<(Prop, bool)> {
        let (name, negated) = if self.eat('{') {
            let neg = self.eat('^');
            let name = self.take_while(|c| c != '}');
            if !self.eat('}') {
                return Err("malformed \\P or \\p sequence".into());
            }
            (name, neg)
        } else {
            match self.peek() {
                Some(c) if c.is_ascii_alphabetic() => {
                    self.pos += 1;
                    (c.to_string(), false)
                }
                _ => return Err("malformed \\P or \\p sequence".into()),
            }
        };
        let prop = resolve_property(&name)
            .ok_or_else(|| "unknown property after \\P or \\p".to_string())?;
        Ok((prop, negated))
    }

    /// A class after its `[`.
    pub(super) fn class(&mut self, flags: &Flags) -> PResult<Class> {
        // `\E` and an empty `\Q\E` at the start are ignored: `^` and `]` keep their meaning.
        self.skip_empty_quotes();
        let negated = self.eat('^');
        self.skip_empty_quotes();
        let mut items = Vec::new();
        let mut first = true;
        loop {
            if flags.extended_more {
                while matches!(self.peek(), Some(' ' | '\t')) {
                    self.pos += 1;
                }
            }
            let Some(c) = self.peek() else {
                return Err("missing terminating ] for character class".into());
            };
            if c == ']' && !first && !self.class_quote {
                self.pos += 1;
                break;
            }
            first = false;
            if c == '['
                && !self.class_quote
                && self.peek_at(1) == Some(':')
                && let Some(item) = self.posix(flags)?
            {
                items.push(item);
                if self.peek() == Some('-') && self.peek_at(1).is_some_and(|c| c != ']') {
                    return Err("invalid range in character class".into());
                }
                continue;
            }
            let start = match self.class_atom(flags)? {
                ClassAtom::Char(ch) => ch,
                ClassAtom::Item(item) => {
                    items.push(item);
                    if self.peek() == Some('-') && self.peek_at(1).is_some_and(|c| c != ']') {
                        return Err("invalid range in character class".into());
                    }
                    continue;
                }
                ClassAtom::Nothing => continue,
            };
            let (before_dash, quoted) = (self.pos, self.class_quote);
            if self.eat_str("\\E") {
                self.class_quote = false;
            }
            while self.eat_str("\\E") {}
            // A `-` inside `\Q...\E` is literal.
            if self.peek() == Some('-') && !self.class_quote {
                self.pos += 1;
                self.skip_empty_quotes();
                if self.eat_str("\\Q") {
                    self.class_quote = true;
                }
                if self.peek().is_none_or(|c| c == ']') {
                    // A `-` before the `]` is literal.
                    (self.pos, self.class_quote) = (before_dash, quoted);
                }
            } else {
                (self.pos, self.class_quote) = (before_dash, quoted);
            }
            if self.pos > before_dash {
                if self.peek() == Some('[') && self.peek_at(1) == Some(':') {
                    return Err("invalid range in character class".into());
                }
                let end = match self.class_atom(flags)? {
                    ClassAtom::Char(ch) => ch,
                    _ => return Err("invalid range in character class".into()),
                };
                if end < start {
                    return Err("range out of order in character class".into());
                }
                items.push(Item::Range(start, end));
            } else {
                items.push(Item::Range(start, start));
            }
        }
        Ok(Class { negated, items, caseless: flags.case() })
    }

    /// Skips `\E` and empty `\Q\E` in a class.
    fn skip_empty_quotes(&mut self) {
        while self.eat_str("\\E") || self.eat_str("\\Q\\E") {}
    }

    /// Spaces and tabs inside the braces of `\x{}`, `\o{}`, `\N{U+}`, `\g{}` and `\k{}`.
    fn skip_brace_space(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.pos += 1;
        }
    }

    /// Whether `\d`, `\s` and `\w` (with `\b`) use Unicode, by UCP and the `(?a)` options.
    fn ucp_escapes(&self, flags: &Flags) -> (bool, bool, bool) {
        let ucp = self.ucp;
        (ucp && !flags.ascii_bsd, ucp && !flags.ascii_bss, ucp && !flags.ascii_bsw)
    }

    /// `[:name:]` or `[:^name:]` at a `[`; `None` if it is not one.
    fn posix(&mut self, flags: &Flags) -> PResult<Option<Item>> {
        let rest: String = self.chars[self.pos + 2..].iter().take(12).collect();
        let Some(end) = rest.find(":]") else { return Ok(None) };
        let body = &rest[..end];
        let (negate, name) = match body.strip_prefix('^') {
            Some(n) => (true, n),
            None => (false, body),
        };
        let class = match name {
            "alnum" => Posix::Alnum,
            "alpha" => Posix::Alpha,
            "ascii" => Posix::Ascii,
            "blank" => Posix::Blank,
            "cntrl" => Posix::Cntrl,
            "digit" => Posix::Digit,
            "graph" => Posix::Graph,
            "lower" => Posix::Lower,
            "print" => Posix::Print,
            "punct" => Posix::Punct,
            "space" => Posix::Space,
            "upper" => Posix::Upper,
            "word" => Posix::Word,
            "xdigit" => Posix::Xdigit,
            _ => return Err("unknown POSIX class name".into()),
        };
        self.pos += 2 + body.chars().count() + 2;
        let ascii = flags.ascii_posix
            || (flags.ascii_digit && matches!(class, Posix::Digit | Posix::Xdigit));
        Ok(Some(Item::Set { set: Set::Posix { class, ucp: self.ucp && !ascii }, negate }))
    }

    fn class_atom(&mut self, flags: &Flags) -> PResult<ClassAtom> {
        let Some(c) = self.peek() else {
            return Err("missing terminating ] for character class".into());
        };
        self.pos += 1;
        if self.class_quote {
            // Inside `\Q...\E` each character is itself, and can be the end of a range.
            if c == '\\' && self.eat('E') {
                self.class_quote = false;
                return Ok(ClassAtom::Nothing);
            }
            return Ok(ClassAtom::Char(c));
        }
        if c != '\\' {
            return Ok(ClassAtom::Char(c));
        }
        let Some(e) = self.peek() else { return Err("\\ at end of pattern".into()) };
        let (dig, spc, wrd) = self.ucp_escapes(flags);
        let set = |set: Set, negate: bool| ClassAtom::Item(Item::Set { set, negate });
        self.pos += 1;
        Ok(match e {
            'd' | 'D' => set(Set::Digit { ucp: dig }, e == 'D'),
            'w' | 'W' => set(Set::Word { ucp: wrd }, e == 'W'),
            's' | 'S' => set(Set::Space { ucp: spc }, e == 'S'),
            // In a class `\g` is the letter.
            'g' => ClassAtom::Char('g'),
            'h' | 'H' => set(Set::HSpace, e == 'H'),
            'v' | 'V' => set(Set::VSpace, e == 'V'),
            'p' | 'P' => {
                let (prop, negated) = self.property()?;
                set(Set::Prop(prop), (e == 'P') != negated)
            }
            'Q' => {
                self.class_quote = true;
                ClassAtom::Nothing
            }
            'E' => ClassAtom::Nothing,
            'N' if self.peek() == Some('{') => ClassAtom::Char(self.unicode_name()?),
            'N' | 'R' | 'X' | 'B' => {
                return Err("escape sequence is invalid in character class".into());
            }
            '1'..='9' => {
                self.pos -= 1;
                match self.octal_after_digit()? {
                    Some(ch) => ClassAtom::Char(ch),
                    None => {
                        self.pos += 1;
                        ClassAtom::Char(e)
                    }
                }
            }
            _ => {
                self.pos -= 1;
                ClassAtom::Char(self.char_escape(true)?)
            }
        })
    }
}

enum ClassAtom {
    Char(char),
    Item(Item),
    Nothing,
}

fn code_point(v: Option<u32>) -> PResult<char> {
    match v.and_then(char::from_u32) {
        Some(c) => Ok(c),
        None => Err("character code point value in \\x{} or \\o{} is too large".into()),
    }
}

fn unsupported_escape() -> ParseError {
    "PCRE2 does not support \\F, \\L, \\l, \\N{name}, \\U, or \\u".into()
}

/// A `\p` name: a general category, `L&`, `Any`, a script (with `sc:` or `scx:` or without
/// prefix, by script extensions) or one of PCRE2's special properties.
fn resolve_property(name: &str) -> Option<Prop> {
    let lower = loose(name);
    match lower.as_str() {
        "any" => return Some(Prop::Any),
        "xan" => return Some(Prop::Xan),
        "xps" => return Some(Prop::Xps),
        "xsp" => return Some(Prop::Xsp),
        "xwd" => return Some(Prop::Xwd),
        "xuc" => return Some(Prop::Xuc),
        _ => {}
    }
    if let Some(mask) = category_mask(name) {
        return Some(Prop::Categories(mask));
    }
    for prefix in ["bidiclass:", "bidiclass=", "bc:", "bc="] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            return bidi_index(rest).map(Prop::Bidi);
        }
    }
    if let Some(p) = binary_index(name) {
        return Some(Prop::Binary(p));
    }
    for (prefix, extension) in [
        ("sc:", false),
        ("script:", false),
        ("sc=", false),
        ("script=", false),
        ("scx:", true),
        ("scriptextensions:", true),
        ("scx=", true),
        ("scriptextensions=", true),
    ] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let s = script_index(rest)?;
            return Some(if extension { Prop::ScriptExtension(s) } else { Prop::Script(s) });
        }
    }
    script_index(name).map(Prop::ScriptExtension)
}
