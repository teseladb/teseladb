//! The PCRE2 syntax that RefDb accepts, read into a [`Node`] tree: literals and escapes,
//! classes with ranges, POSIX classes and Unicode properties, groups (capturing, named,
//! non-capturing, atomic, branch reset), inline options, quantifiers (greedy, lazy and
//! possessive), back references, lookahead and lookbehind, conditional groups, `\K`, `\Q...\E`,
//! comments and the start-of-pattern verbs.

mod escape;

/// A condition `(?(*` whose assertion PCRE2 does not know or that lacks its `:`.
const ALPHA_ASSERTION: &str = "(*alpha_assertion) not recognized";

use super::ast::{Assertion, Case, Condition, Node, RepeatKind, Verb};
use super::compile::{Bsr, Newline, NotEmpty, Options};
use super::unicode::{category_bit, in_categories};

/// Largest count of a `{}` quantifier.
const MAX_REPEAT: u32 = 65535;
/// Longest group name.
const MAX_NAME: usize = 32;
/// The largest group number PCRE2 takes; past it a number is "too big".
pub(super) const MAX_GROUP: usize = 65535;

/// Options in force at a point of the pattern.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Flags {
    pub caseless: bool,
    pub multiline: bool,
    pub dotall: bool,
    pub extended: bool,
    pub extended_more: bool,
    pub no_auto_capture: bool,
    pub ungreedy: bool,
    pub dupnames: bool,
    /// `(?r)`: caseless matching keeps ASCII and non-ASCII apart.
    pub caseless_restrict: bool,
    /// `(?aD)`: `\d` stays ASCII under UCP.
    pub ascii_bsd: bool,
    /// `(?aS)`: `\s` stays ASCII under UCP.
    pub ascii_bss: bool,
    /// `(?aW)`: `\w` and `\b` stay ASCII under UCP.
    pub ascii_bsw: bool,
    /// `(?aP)`: POSIX classes stay ASCII under UCP.
    pub ascii_posix: bool,
    /// `(?aT)`: `[:digit:]` and `[:xdigit:]` stay ASCII under UCP.
    pub ascii_digit: bool,
    /// The pattern comes from a translation ([`crate::regex::Regex::without_length_limit`]), which
    /// may use `\b{g}`, a grapheme cluster boundary that PCRE2 lacks.
    pub translated: bool,
}

impl Flags {
    /// How letters match their other cases.
    pub(super) fn case(&self) -> Case {
        match (self.caseless, self.caseless_restrict) {
            (false, _) => Case::Sensitive,
            (true, false) => Case::Insensitive,
            (true, true) => Case::Restricted,
        }
    }
}

/// A parsed pattern.
#[derive(Debug)]
pub(super) struct Parsed {
    /// The tree.
    pub node: Node,
    /// Number of capture groups.
    pub groups: usize,
    /// Named groups with their numbers.
    pub names: Vec<(String, usize)>,
    /// What the verbs at its start set.
    pub options: Options,
    /// Groups numbered inside a branch reset group: their numbers may be shared.
    pub reset_groups: Vec<usize>,
}

/// A syntax error, with PCRE2's message.
pub(super) type ParseError = String;

pub(super) type PResult<T> = Result<T, ParseError>;

pub(super) struct Parser {
    chars: Vec<char>,
    pos: usize,
    groups: usize,
    names: Vec<(String, usize)>,
    ucp: bool,
    max_backref: usize,
    not_empty: Option<NotEmpty>,
    newline: Newline,
    bsr: Bsr,
    start_opt: bool,
    /// Lookarounds open at this point, where `\K` is not allowed.
    looks: usize,
    /// Inside `\Q...\E` in a class.
    pub(super) class_quote: bool,
    /// Groups numbered inside a branch reset group.
    reset_groups: Vec<usize>,
    /// An error PCRE2 finds while compiling, reported only when the whole pattern parses and
    /// every referenced group exists (`(?(1)a|b|c)` without a group 1 names the group).
    deferred: Option<String>,
}

/// The PCRE2 version whose syntax the parser follows, for `(?(VERSION>=n.m)`.
const VERSION: (u32, u32) = (10, 43);
/// Longest name of a backtracking verb.
const MAX_VERB_NAME: usize = 255;

/// Parses `pattern` with the starting options `flags`.
pub(super) fn parse(pattern: &str, flags: Flags) -> PResult<Parsed> {
    let mut p = Parser {
        chars: pattern.chars().collect(),
        pos: 0,
        groups: 0,
        names: Vec::new(),
        ucp: false,
        max_backref: 0,
        not_empty: None,
        newline: Newline::Lf,
        bsr: Bsr::Unicode,
        start_opt: true,
        looks: 0,
        class_quote: false,
        reset_groups: Vec::new(),
        deferred: None,
    };
    p.start_verbs()?;
    let mut flags = flags;
    let mut node = p.alternation(&mut flags)?;
    if p.pos < p.chars.len() {
        return Err("unmatched closing parenthesis".into());
    }
    if p.max_backref > p.groups {
        return Err("reference to non-existent subpattern".into());
    }
    if let Some(e) = p.deferred {
        return Err(e);
    }
    p.resolve(&mut node)?;
    let options =
        Options { not_empty: p.not_empty, newline: p.newline, bsr: p.bsr, start_opt: p.start_opt };
    Ok(Parsed { node, groups: p.groups, names: p.names, options, reset_groups: p.reset_groups })
}

impl Parser {
    pub(super) fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    /// The next character, consumed.
    fn next_char(&mut self) -> Option<char> {
        let c = self.peek();
        self.pos += 1;
        c
    }

    pub(super) fn peek_at(&self, n: usize) -> Option<char> {
        self.chars.get(self.pos + n).copied()
    }

    pub(super) fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    pub(super) fn eat_str(&mut self, s: &str) -> bool {
        let n = s.chars().count();
        if self.chars.len() >= self.pos + n
            && self.chars[self.pos..self.pos + n].iter().copied().eq(s.chars())
        {
            self.pos += n;
            true
        } else {
            false
        }
    }

    /// `(*UTF)`, `(*UTF8)`, `(*UCP)` and the other verbs allowed at the start of a pattern.
    fn start_verbs(&mut self) -> PResult<()> {
        const IGNORED: &[&str] = &["UTF", "UTF8", "NO_AUTO_POSSESS", "NO_DOTSTAR_ANCHOR", "NO_JIT"];
        const LIMITS: &[&str] = &["LIMIT_MATCH=", "LIMIT_DEPTH=", "LIMIT_HEAP="];
        while self.peek() == Some('(') && self.peek_at(1) == Some('*') {
            let start = self.pos;
            self.pos += 2;
            let name: String =
                self.take_while(|c| c.is_ascii_alphanumeric() || c == '_' || c == '=');
            if !self.eat(')') {
                self.pos = start;
                return Ok(());
            }
            let limit = LIMITS.iter().any(|l| {
                name.strip_prefix(l)
                    .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            });
            match name.as_str() {
                "UCP" => self.ucp = true,
                "NOTEMPTY" => self.not_empty = Some(NotEmpty::Anywhere),
                "NOTEMPTY_ATSTART" => self.not_empty = self.not_empty.or(Some(NotEmpty::AtStart)),
                "NO_START_OPT" => self.start_opt = false,
                "CR" => self.newline = Newline::Cr,
                "LF" => self.newline = Newline::Lf,
                "CRLF" => self.newline = Newline::Crlf,
                "ANYCRLF" => self.newline = Newline::AnyCrlf,
                "ANY" => self.newline = Newline::Any,
                "NUL" => self.newline = Newline::Nul,
                "BSR_ANYCRLF" => self.bsr = Bsr::AnyCrlf,
                "BSR_UNICODE" => self.bsr = Bsr::Unicode,
                n if limit || IGNORED.contains(&n) => {}
                _ => {
                    self.pos = start;
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    pub(super) fn take_while(&mut self, f: impl Fn(char) -> bool) -> String {
        let start = self.pos;
        while self.peek().is_some_and(&f) {
            self.pos += 1;
        }
        self.chars[start..self.pos].iter().collect()
    }

    /// Alternatives up to a `)` or the end; `flags` changes inside carry to later alternatives.
    fn alternation(&mut self, flags: &mut Flags) -> PResult<Node> {
        let mut alts = vec![self.sequence(flags)?];
        while self.eat('|') {
            alts.push(self.sequence(flags)?);
        }
        Ok(if alts.len() == 1 { alts.pop().unwrap_or(Node::Empty) } else { Node::Alt(alts) })
    }

    /// Alternatives of a branch reset group: each one numbers its groups from the same base.
    fn branch_reset(&mut self, flags: &mut Flags) -> PResult<Node> {
        let base = self.groups;
        let mut top = base;
        let mut alts = Vec::new();
        loop {
            self.groups = base;
            alts.push(self.sequence(flags)?);
            top = top.max(self.groups);
            if !self.eat('|') {
                break;
            }
        }
        self.groups = top;
        self.reset_groups.extend(base + 1..=top);
        Ok(Node::Alt(alts))
    }

    fn sequence(&mut self, flags: &mut Flags) -> PResult<Node> {
        let mut items = Vec::new();
        loop {
            self.skip_extended(flags);
            match self.peek() {
                None | Some('|') | Some(')') => break,
                _ => {}
            }
            let Some(atom) = self.atom(flags)? else { continue };
            let atom = self.quantified(atom, flags)?;
            items.push(atom);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(items),
        })
    }

    /// `(?#...)` comments, and white space and `#` comments with the `x` option.
    fn skip_extended(&mut self, flags: &Flags) {
        loop {
            match self.peek() {
                Some('(') if self.peek_at(1) == Some('?') && self.peek_at(2) == Some('#') => {
                    // An unterminated comment is left for `group` to report.
                    match self.chars[self.pos..].iter().position(|c| *c == ')') {
                        Some(n) => self.pos += n + 1,
                        None => return,
                    }
                }
                Some(c) if flags.extended && is_pattern_white_space(c) => self.pos += 1,
                Some('#') if flags.extended => {
                    while self.peek().is_some_and(|c| c != '\n') {
                        self.pos += 1;
                    }
                }
                _ => return,
            }
        }
    }

    /// One atom; `None` for what matches nothing (`(?i)`, `(?#...)`, an empty `\Q\E`).
    fn atom(&mut self, flags: &mut Flags) -> PResult<Option<Node>> {
        let Some(c) = self.peek() else { return Ok(None) };
        self.pos += 1;
        let node = match c {
            '(' => return self.group(flags),
            '[' => Node::Class(self.class(flags)?),
            '.' => Node::Dot { dotall: flags.dotall },
            '^' => Node::Assert(Assertion::Start { multiline: flags.multiline }),
            '$' => Node::Assert(Assertion::End { multiline: flags.multiline }),
            '\\' => return self.escape(flags),
            '*' | '+' | '?' => return Err("quantifier does not follow a repeatable item".into()),
            '{' if self.quantifier_at(self.pos) => {
                return Err("quantifier does not follow a repeatable item".into());
            }
            c => Node::Char { c, caseless: flags.case() },
        };
        Ok(Some(node))
    }

    /// Whether the text at `at`, after a `{`, completes a valid quantifier.
    fn quantifier_at(&self, at: usize) -> bool {
        self.brace_parts(at).is_some()
    }

    /// The parts of a quantifier whose text starts at `at`, after its `{`: the first number,
    /// whether there is a comma, the second number, and where the `}` ends. `{n}`, `{n,}`,
    /// `{n,m}` and `{,m}`, with spaces and tabs around the numbers.
    fn brace_parts(&self, at: usize) -> Option<(String, bool, String, usize)> {
        let rest = self.chars.get(at..)?;
        let mut i = 0;
        let scan = |i: &mut usize, pred: fn(char) -> bool| {
            let start = *i;
            while rest.get(*i).is_some_and(|&c| pred(c)) {
                *i += 1;
            }
            rest[start..*i].iter().collect::<String>()
        };
        scan(&mut i, is_quantifier_space);
        let first = scan(&mut i, |c| c.is_ascii_digit());
        scan(&mut i, is_quantifier_space);
        let comma = rest.get(i) == Some(&',');
        let mut second = String::new();
        if comma {
            i += 1;
            scan(&mut i, is_quantifier_space);
            second = scan(&mut i, |c| c.is_ascii_digit());
            scan(&mut i, is_quantifier_space);
        }
        let valid = rest.get(i) == Some(&'}') && (!first.is_empty() || !second.is_empty());
        valid.then_some((first, comma, second, at + i + 1))
    }

    /// The atom with the quantifier that follows it, if any; a second quantifier is an error.
    fn quantified(&mut self, atom: Node, flags: &Flags) -> PResult<Node> {
        let Some((min, max)) = self.quantifier(flags)? else { return Ok(atom) };
        if !repeatable(&atom) {
            return Err("quantifier does not follow a repeatable item".into());
        }
        self.skip_extended(flags);
        let kind = if self.eat('+') {
            RepeatKind::Possessive
        } else if self.eat('?') != flags.ungreedy {
            RepeatKind::Lazy
        } else {
            RepeatKind::Greedy
        };
        if self.quantifier(flags)?.is_some() {
            return Err("quantifier does not follow a repeatable item".into());
        }
        Ok(Node::Repeat { node: Box::new(atom), min, max, kind })
    }

    /// A quantifier at the current position, consumed.
    fn quantifier(&mut self, flags: &Flags) -> PResult<Option<(u32, Option<u32>)>> {
        self.skip_extended(flags);
        let q = match self.peek() {
            Some('*') => (0, None),
            Some('+') => (1, None),
            Some('?') => (0, Some(1)),
            Some('{') if self.quantifier_at(self.pos + 1) => {
                self.pos += 1;
                return self.braces().map(Some);
            }
            _ => return Ok(None),
        };
        self.pos += 1;
        Ok(Some(q))
    }

    /// The quantifier after a `{`, consumed.
    fn braces(&mut self) -> PResult<(u32, Option<u32>)> {
        let Some((first, comma, second, end)) = self.brace_parts(self.pos) else {
            return Err("quantifier does not follow a repeatable item".into());
        };
        self.pos = end;
        let min = if first.is_empty() { 0 } else { number(&first)? };
        let max = match (comma, second.is_empty()) {
            (false, _) => Some(min),
            (true, true) => None,
            (true, false) => Some(number(&second)?),
        };
        if max.is_some_and(|m| m < min) {
            return Err("numbers out of order in {} quantifier".into());
        }
        Ok((min, max))
    }

    fn group(&mut self, flags: &mut Flags) -> PResult<Option<Node>> {
        if self.eat('*') {
            return self.verb(flags).map(Some);
        }
        if !self.eat('?') {
            let index = if flags.no_auto_capture { None } else { Some(self.new_group()) };
            return self
                .group_body(*flags, |p, f| p.alternation(f))
                .map(|n| Some(Node::Group(Box::new(n), index)));
        }
        let Some(c) = self.peek() else { return Err("missing closing parenthesis".into()) };
        self.pos += 1;
        let node = match c {
            '#' => {
                while self.peek().is_some_and(|c| c != ')') {
                    self.pos += 1;
                }
                if !self.eat(')') {
                    return Err("missing ) after (?# comment".into());
                }
                return Ok(None);
            }
            ':' => Node::Group(Box::new(self.group_body(*flags, |p, f| p.alternation(f))?), None),
            '|' => Node::Group(Box::new(self.group_body(*flags, |p, f| p.branch_reset(f))?), None),
            '>' => Node::Atomic(Box::new(self.group_body(*flags, |p, f| p.alternation(f))?)),
            '=' | '!' | '*' => self.look(false, c == '!', c != '*', flags)?,
            '<' if matches!(self.peek(), Some('=' | '!' | '*')) => {
                let c = self.peek().unwrap_or('=');
                self.pos += 1;
                self.look(true, c == '!', c != '*', flags)?
            }
            'C' => {
                self.callout()?;
                return Ok(None);
            }
            '<' => self.named_group('>', flags)?,
            '\'' => self.named_group('\'', flags)?,
            'P' if self.eat('<') => self.named_group('>', flags)?,
            'P' if self.eat('=') => {
                let name = self.name(')')?;
                Node::NamedBackref { name, caseless: flags.case() }
            }
            'P' if self.eat('>') => Node::NamedCall(self.name(')')?),
            '&' => Node::NamedCall(self.name(')')?),
            'R' if self.eat(')') => Node::Call(0),
            'R' => {
                let msg = "(?R (recursive pattern call) must be followed by a closing parenthesis";
                return Err(msg.into());
            }
            '0'..='9' | '+' | '-'
                if c != '-' || self.peek().is_some_and(|d| d.is_ascii_digit()) =>
            {
                self.pos -= 1;
                let group = self.group_number()?;
                if !self.eat(')') {
                    return Err("missing closing parenthesis".into());
                }
                Node::Call(group)
            }
            '(' => self.conditional(flags)?,
            _ => {
                self.pos -= 1;
                return self.option_setting(flags);
            }
        };
        Ok(Some(node))
    }

    /// A lookaround's body after its opening.
    fn look(&mut self, behind: bool, negate: bool, atomic: bool, flags: &Flags) -> PResult<Node> {
        self.looks += 1;
        let node = Box::new(self.group_body(*flags, |p, f| p.alternation(f))?);
        self.looks -= 1;
        Ok(Node::Look { behind, negate, atomic, node })
    }

    /// A group number for a call: `n`, `+n` (relative to the next group) or `-n` (to the
    /// previous ones).
    pub(super) fn group_number(&mut self) -> PResult<usize> {
        let sign = self.peek().filter(|c| matches!(c, '+' | '-'));
        if sign.is_some() {
            self.pos += 1;
        }
        let digits = self.take_while(|c| c.is_ascii_digit());
        let n: usize = digits.parse().map_err(|_| "digit expected after (?+ or (?-".to_string())?;
        let group = match sign {
            Some(_) if n == 0 => return Err(escape::RELATIVE_ZERO.into()),
            Some('+') => self.groups + n,
            Some(_) => (self.groups + 1)
                .checked_sub(n)
                .filter(|g| *g > 0)
                .ok_or("reference to non-existent subpattern")?,
            None => n,
        };
        self.max_backref = self.max_backref.max(group);
        Ok(group)
    }

    /// `(?C)`, `(?Cn)` and `(?C"text")` after the `C`: callouts, which do nothing without a
    /// callout function, and RefDb sets none.
    fn callout(&mut self) -> PResult<()> {
        const DELIMITERS: &str = "`'\"^%#$";
        match self.peek() {
            Some(c) if c.is_ascii_digit() => {
                let n = self.take_while(|c| c.is_ascii_digit());
                if n.parse::<u32>().map_or(true, |n| n > 255) {
                    return Err("number after (?C is greater than 255".into());
                }
            }
            Some(open) if open == '{' || DELIMITERS.contains(open) => {
                let close = if open == '{' { '}' } else { open };
                self.pos += 1;
                loop {
                    match self.peek() {
                        None => {
                            return Err(
                                "missing terminating delimiter for callout with string argument"
                                    .into(),
                            );
                        }
                        Some(c) if c == close => {
                            self.pos += 1;
                            // A doubled delimiter stands for itself.
                            if !self.eat(close) {
                                break;
                            }
                        }
                        Some(_) => self.pos += 1,
                    }
                }
            }
            Some(')') | None => {}
            Some(_) => return Err("unrecognized string delimiter follows (?C".into()),
        }
        if !self.eat(')') {
            return Err("closing parenthesis for (?C expected".into());
        }
        Ok(())
    }

    pub(super) fn new_group(&mut self) -> usize {
        self.groups += 1;
        self.groups
    }

    /// The body of a group up to its `)`, with options that do not leak out of it.
    fn group_body(
        &mut self,
        flags: Flags,
        body: impl FnOnce(&mut Parser, &mut Flags) -> PResult<Node>,
    ) -> PResult<Node> {
        let mut inner = flags;
        let node = body(self, &mut inner)?;
        if !self.eat(')') {
            return Err("missing closing parenthesis".into());
        }
        Ok(node)
    }

    fn named_group(&mut self, close: char, flags: &mut Flags) -> PResult<Node> {
        let name = self.name(close)?;
        let index = self.new_group();
        if self.names.iter().any(|(n, i)| *n == name && *i != index) && !flags.dupnames {
            return Err("two named subpatterns have the same name (PCRE2_DUPNAMES not set)".into());
        }
        if self.names.iter().any(|(n, i)| *n != name && *i == index) {
            return Err("different names for subpatterns of the same number are not allowed".into());
        }
        self.names.push((name, index));
        let node = self.group_body(*flags, |p, f| p.alternation(f))?;
        Ok(Node::Group(Box::new(node), Some(index)))
    }

    /// A group name followed by `close`.
    pub(super) fn name(&mut self, close: char) -> PResult<String> {
        let name = self.take_while(is_name_char);
        self.checked_name(name, close)
    }

    /// `name`, already read, checked and followed by `close`.
    pub(super) fn checked_name(&mut self, name: String, close: char) -> PResult<String> {
        let digit = |c: char| in_categories(c, category_bit("Nd"));
        if name.is_empty() {
            return Err("subpattern name expected".into());
        }
        if name.starts_with(digit) {
            return Err("subpattern name must start with a non-digit".into());
        }
        if name.chars().count() > MAX_NAME {
            return Err("subpattern name is too long (maximum 32 code units)".into());
        }
        if !self.eat(close) {
            return Err("syntax error in subpattern name (missing terminator?)".into());
        }
        Ok(name)
    }

    /// `(?i)`, `(?-s)`, `(?^)` and `(?i:...)`.
    fn option_setting(&mut self, flags: &mut Flags) -> PResult<Option<Node>> {
        let mut new = *flags;
        let mut on = true;
        let reset = self.eat('^');
        if reset {
            // `(?^)` is `(?-imnrsx)`.
            new.caseless = false;
            new.multiline = false;
            new.no_auto_capture = false;
            new.caseless_restrict = false;
            new.dotall = false;
            new.extended = false;
            new.extended_more = false;
        }
        loop {
            let Some(c) = self.peek() else { return Err("missing closing parenthesis".into()) };
            self.pos += 1;
            match c {
                '-' if on && !reset => on = false,
                'i' => new.caseless = on,
                'r' => new.caseless_restrict = on,
                'a' => match self.next_char() {
                    Some('D') => new.ascii_bsd = on,
                    Some('S') => new.ascii_bss = on,
                    Some('W') => new.ascii_bsw = on,
                    Some('P') => {
                        new.ascii_posix = on;
                        new.ascii_digit = on;
                    }
                    Some('T') => new.ascii_digit = on,
                    _ => {
                        (new.ascii_bsd, new.ascii_bss, new.ascii_bsw) = (on, on, on);
                        (new.ascii_posix, new.ascii_digit) = (on, on);
                        self.pos -= 1;
                    }
                },
                'm' => new.multiline = on,
                's' => new.dotall = on,
                'n' => new.no_auto_capture = on,
                'U' => new.ungreedy = on,
                'J' => new.dupnames = on,
                'x' => {
                    let more = !self.take_while(|c| c == 'x').is_empty();
                    new.extended = on;
                    new.extended_more = on && more;
                }
                ')' => {
                    *flags = new;
                    return Ok(None);
                }
                ':' => {
                    let node = self.group_body(new, |p, f| p.alternation(f))?;
                    return Ok(Some(Node::Group(Box::new(node), None)));
                }
                _ => return Err("unrecognized character after (? or (?-".into()),
            }
        }
    }

    /// After `(*`: a backtracking control verb, or an assertion or group by its alphabetic name
    /// (`(*pla:`, `(*atomic:`, `(*sr:`...).
    fn verb(&mut self, flags: &Flags) -> PResult<Node> {
        let word = self.take_while(|c| c.is_ascii_alphabetic() || c == '_');
        let malformed = || "(*VERB) not recognized or malformed".to_string();
        if self.eat(':') {
            let body = |p: &mut Parser| p.group_body(*flags, |p, f| p.alternation(f));
            let node = match word.as_str() {
                "pla" | "positive_lookahead" => Some(self.look(false, false, true, flags)?),
                "nla" | "negative_lookahead" => Some(self.look(false, true, true, flags)?),
                "plb" | "positive_lookbehind" => Some(self.look(true, false, true, flags)?),
                "nlb" | "negative_lookbehind" => Some(self.look(true, true, true, flags)?),
                "napla" | "non_atomic_positive_lookahead" => {
                    Some(self.look(false, false, false, flags)?)
                }
                "naplb" | "non_atomic_positive_lookbehind" => {
                    Some(self.look(true, false, false, flags)?)
                }
                "atomic" => Some(Node::Atomic(Box::new(body(self)?))),
                "sr" | "script_run" => Some(Node::ScriptRun(Box::new(body(self)?))),
                "asr" | "atomic_script_run" => {
                    Some(Node::ScriptRun(Box::new(Node::Atomic(Box::new(body(self)?)))))
                }
                _ => None,
            };
            if let Some(node) = node {
                return Ok(node);
            }
        } else if self.peek() == Some(')') {
            self.pos += 1;
            return self.verb_node(&word, None).ok_or_else(malformed)?;
        } else {
            return Err(malformed());
        }
        let arg = self.take_while(|c| c != ')');
        if !self.eat(')') {
            return Err(malformed());
        }
        if arg.chars().count() > MAX_VERB_NAME {
            return Err("name is too long in (*MARK), (*PRUNE), (*SKIP), or (*THEN)".into());
        }
        let arg = (!arg.is_empty()).then_some(arg);
        self.verb_node(&word, arg).ok_or_else(malformed)?
    }

    /// The node of the verb `word` with its optional name; `None` for an unknown verb.
    fn verb_node(&self, word: &str, arg: Option<String>) -> Option<PResult<Node>> {
        let marked = |verb: Node, arg: Option<String>| match arg {
            Some(name) => Node::Concat(vec![Node::Verb(Verb::Mark(name)), verb]),
            None => verb,
        };
        Some(Ok(match (word, arg) {
            ("ACCEPT", arg) => marked(Node::Verb(Verb::Accept), arg),
            ("FAIL" | "F", arg) => marked(Node::Fail, arg),
            ("COMMIT", _) => Node::Verb(Verb::Commit),
            ("PRUNE", _) => Node::Verb(Verb::Prune),
            ("SKIP", None) => Node::Verb(Verb::Skip),
            ("SKIP", Some(name)) => Node::Verb(Verb::SkipTo(name)),
            ("THEN", _) => Node::Verb(Verb::Then),
            ("MARK" | "", Some(name)) => Node::Verb(Verb::Mark(name)),
            ("MARK" | "", None) => return Some(Err("(*MARK) must have an argument".into())),
            _ => return None,
        }))
    }

    /// `(?(` already read: the condition, then the one or two branches.
    fn conditional(&mut self, flags: &mut Flags) -> PResult<Node> {
        if self.peek() == Some('?') && self.peek_at(1) == Some('C') {
            // A callout before the assertion of the condition.
            self.pos += 2;
            self.callout()?;
            self.skip_extended(flags);
            while self.eat_str("\\Q\\E") {}
            if !self.eat_str("(") {
                return Err("assertion expected after (?( or (?(?C)".into());
            }
        }
        let cond = if matches!(self.peek(), Some('?'))
            && matches!(self.peek_at(1), Some('=' | '!' | '<'))
        {
            self.pos += 1;
            let behind = self.eat('<');
            let negate = self.eat('!');
            if !negate && !self.eat('=') {
                return Err("assertion expected after (?( or (?(?C)".into());
            }
            self.looks += 1;
            let node = self.group_body(*flags, |p, f| p.alternation(f))?;
            self.looks -= 1;
            Condition::Look { behind, negate, node: Box::new(node) }
        } else if self.peek() == Some('*') {
            self.pos += 1;
            let word = self.take_while(|c| c.is_ascii_alphabetic() || c == '_');
            let (behind, negate) = match word.as_str() {
                "pla" | "positive_lookahead" => (false, false),
                "nla" | "negative_lookahead" => (false, true),
                "plb" | "positive_lookbehind" => (true, false),
                "nlb" | "negative_lookbehind" => (true, true),
                _ => return Err(ALPHA_ASSERTION.into()),
            };
            if !self.eat(':') {
                return Err(ALPHA_ASSERTION.into());
            }
            self.looks += 1;
            let node = self.group_body(*flags, |p, f| p.alternation(f))?;
            self.looks -= 1;
            Condition::Look { behind, negate, node: Box::new(node) }
        } else if self.peek().is_some_and(|c| c.is_ascii_digit() || c == '+' || c == '-') {
            let sign = self.peek().filter(|c| matches!(c, '+' | '-'));
            if sign.is_some() {
                self.pos += 1;
            }
            let digits = self.take_while(|c| c.is_ascii_digit());
            if digits.is_empty() {
                return Err("subpattern name expected".into());
            }
            let n = group_limit(&digits)?;
            if !self.eat(')') {
                return Err(MISSING_CONDITION_CLOSE.into());
            }
            let group = match sign {
                Some('+') => self.groups + n,
                Some(_) => (self.groups + 1)
                    .checked_sub(n)
                    .filter(|g| *g > 0)
                    .ok_or("reference to non-existent subpattern")?,
                None => n,
            };
            if group == 0 {
                return Err("invalid condition (?(0)".into());
            }
            self.max_backref = self.max_backref.max(group);
            Condition::Group(vec![group])
        } else if self.eat('<') {
            Condition::QuotedName(self.name('>')?).with_close(self)?
        } else if self.eat('\'') {
            Condition::QuotedName(self.name('\'')?).with_close(self)?
        } else if self.eat_str("DEFINE)") {
            Condition::Define
        } else if self.eat_str("R&") {
            Condition::RecursionName(self.name(')')?)
        } else if self.eat_str("VERSION") {
            Condition::Fixed(self.version()?)
        } else {
            let name = self.name(')')?;
            Condition::Name(name)
        };
        let mut inner = *flags;
        let body = self.alternation(&mut inner)?;
        if !self.eat(')') {
            return Err("missing closing parenthesis".into());
        }
        let (yes, no) = match body {
            Node::Alt(mut alts) if alts.len() == 2 => {
                let no = alts.pop().unwrap_or(Node::Empty);
                (alts.pop().unwrap_or(Node::Empty), no)
            }
            Node::Alt(mut alts) => {
                let msg = "conditional subpattern contains more than two branches";
                self.deferred.get_or_insert_with(|| msg.to_owned());
                let no = alts.pop().unwrap_or(Node::Empty);
                (alts.pop().unwrap_or(Node::Empty), no)
            }
            n => (n, Node::Empty),
        };
        if cond == Condition::Define && no != Node::Empty {
            return Err("DEFINE subpattern contains more than one branch".into());
        }
        Ok(Node::Cond { cond, yes: Box::new(yes), no: Box::new(no) })
    }

    /// `>=n.m)` or `=n.m)` after `(?(VERSION`: whether this version satisfies it.
    fn version(&mut self) -> PResult<bool> {
        let at_least = self.eat('>');
        let malformed = || "syntax error or number too big in (?(VERSION condition".to_string();
        if !self.eat('=') {
            return Err(malformed());
        }
        let major: u32 =
            self.take_while(|c| c.is_ascii_digit()).parse().map_err(|_| malformed())?;
        let minor = if self.eat('.') {
            let digits = self.take_while(|c| c.is_ascii_digit());
            if digits.is_empty() || digits.len() > 2 {
                return Err(malformed());
            }
            // `10.4` is 10.40: the minor version has two digits.
            let n: u32 = digits.parse().map_err(|_| malformed())?;
            if digits.len() == 1 { n * 10 } else { n }
        } else {
            0
        };
        if !self.eat(')') {
            return Err(malformed());
        }
        let wanted = (major, minor);
        Ok(if at_least { VERSION >= wanted } else { VERSION == wanted })
    }

    /// Resolves named references and checks every referenced group exists.
    fn resolve(&self, node: &mut Node) -> PResult<()> {
        match node {
            Node::NamedBackref { name, caseless } => {
                let groups = self.groups_named(name)?;
                *node = Node::Backref { groups, caseless: *caseless };
            }
            Node::NamedCall(name) => {
                let groups = self.groups_named(name)?;
                *node = Node::Call(groups[0]);
            }
            Node::Group(n, _)
            | Node::Atomic(n)
            | Node::ScriptRun(n)
            | Node::Repeat { node: n, .. }
            | Node::Look { node: n, .. } => {
                self.resolve(n)?;
            }
            Node::Concat(items) | Node::Alt(items) => {
                for n in items {
                    self.resolve(n)?;
                }
            }
            Node::Cond { cond, yes, no } => {
                match cond {
                    Condition::Name(name) => *cond = self.name_condition(name)?,
                    Condition::QuotedName(name) => {
                        *cond = Condition::Group(self.groups_named(name)?);
                    }
                    Condition::RecursionName(name) => {
                        *cond = Condition::Recursion(Some(self.groups_named(name)?));
                    }
                    Condition::Look { node, .. } => self.resolve(node)?,
                    Condition::Group(_)
                    | Condition::Define
                    | Condition::Recursion(_)
                    | Condition::Fixed(_) => {}
                }
                self.resolve(yes)?;
                self.resolve(no)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// `(?(name)`: a group by name, else `(?(R)` or `(?(Rn)`, a recursion condition.
    fn name_condition(&self, name: &str) -> PResult<Condition> {
        if self.names.iter().any(|(n, _)| n == name) {
            return Ok(Condition::Group(self.groups_named(name)?));
        }
        match name.strip_prefix('R') {
            Some("") => Ok(Condition::Recursion(None)),
            Some(digits) if digits.chars().all(|c| c.is_ascii_digit()) => {
                let n = group_limit(digits)?;
                if n > self.groups {
                    return Err("reference to non-existent subpattern".into());
                }
                Ok(Condition::Recursion(Some(vec![n])))
            }
            _ => self.groups_named(name).map(Condition::Group),
        }
    }

    fn groups_named(&self, name: &str) -> PResult<Vec<usize>> {
        let groups: Vec<usize> =
            self.names.iter().filter(|(n, _)| n == name).map(|(_, i)| *i).collect();
        if groups.is_empty() {
            return Err("reference to non-existent subpattern".into());
        }
        Ok(groups)
    }
}

impl Condition {
    /// A name condition in `<>` or `''` is followed by the condition's `)`.
    fn with_close(self, p: &mut Parser) -> PResult<Condition> {
        if !p.eat(')') {
            return Err(MISSING_CONDITION_CLOSE.into());
        }
        Ok(self)
    }
}

/// PCRE2's message for a condition's number or name not followed by `)`.
const MISSING_CONDITION_CLOSE: &str = "missing closing parenthesis for condition";

/// A group number written with `digits`, or PCRE2's error past [`MAX_GROUP`].
pub(super) fn group_limit(digits: &str) -> PResult<usize> {
    digits
        .parse::<usize>()
        .ok()
        .filter(|n| *n <= MAX_GROUP)
        .ok_or_else(|| "subpattern number is too big".to_string())
}

/// A count of a `{}` quantifier.
fn number(digits: &str) -> PResult<u32> {
    match digits.parse::<u32>() {
        Ok(n) if n <= MAX_REPEAT => Ok(n),
        _ => Err("number too big in {} quantifier".into()),
    }
}

/// Characters of a group name in UTF mode: letters, decimal digits and `_`.
pub(super) fn is_name_char(c: char) -> bool {
    const LETTERS_AND_DIGITS: &[&str] = &["Lu", "Ll", "Lt", "Lm", "Lo", "Nd"];
    c == '_' || LETTERS_AND_DIGITS.iter().any(|m| in_categories(c, category_bit(m)))
}

/// The characters the `x` option skips (Unicode's Pattern_White_Space).
fn is_pattern_white_space(c: char) -> bool {
    matches!(c, '\t'..='\r' | ' ' | '\u{85}' | '\u{200E}' | '\u{200F}' | '\u{2028}' | '\u{2029}')
}

/// Spaces and tabs that PCRE2 10.43 allows inside a `{}` quantifier, as Perl 5.34 does.
fn is_quantifier_space(c: char) -> bool {
    matches!(c, ' ' | '\t')
}

/// Whether a quantifier may follow `node`.
fn repeatable(node: &Node) -> bool {
    match node {
        Node::Verb(v) => *v == Verb::Accept,
        Node::Fail => false,
        Node::Concat(items) => items.last().is_some_and(|n| *n == Node::Verb(Verb::Accept)),
        n => !matches!(n, Node::Assert(_) | Node::KeepOut | Node::Empty),
    }
}
