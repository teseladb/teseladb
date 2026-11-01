//! Regular expressions of the `regex` operator and patterns of the `wildcard` operator (FR-018),
//! compiled to a Thompson NFA (Thompson, CACM 1968, doi:10.1145/363347.363387) that
//! [`crate::query::fuzzy::Utf8`] runs over the dictionary as a lazily built DFA: each state is the
//! set of NFA states reachable after the characters read.
//!
//! The syntax is the one RefDb Search documents for `regex` (a subset of PCRE, whole-term
//! matches, no anchors): `.` any character, `?`, `+` and `*` on the preceding unit, `|`, `{n}`,
//! `{n,}` and `{n,m}`, `(…)`, `[…]` with ranges and a leading `^` for negation, `<n-m>` for a
//! numeric range and `#` for the empty language. Every reserved character
//! (`. ? + * | { } [ ] ( ) < > " \ @ #`) is literal after `\`. RefDb lists `"` and `@` as
//! reserved without describing them; here `"…"` matches its contents literally and `@` matches
//! any string, the meaning those two characters have in the regular-expression syntax
//! `refsearch` reads. The battery (T020) compares them, and `<n-m>` (see [`Regex::new`]), with
//! `refsearch`.
//!
//! `wildcard` patterns use `*` for any string, `?` for any character and `\` to escape.

use crate::query::fuzzy::CharAutomaton;

/// The most NFA states a pattern may compile to; a bigger one is rejected as too complex.
pub const MAX_STATES: usize = 10_000;

/// The largest count a `{n,m}` repetition may name.
pub const MAX_REPEAT: u32 = 1_000;

/// Why a pattern does not compile, with the character position where it was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegexError {
    /// A character that cannot appear where it is.
    Unexpected(usize),
    /// The pattern ends inside a group, class, quote, repetition or interval, or after `\`.
    UnexpectedEnd,
    /// A `{n,m}` with `m < n` or a count above [`MAX_REPEAT`].
    BadRepeat(usize),
    /// A repetition whose counts are not followed by `}`, at the position of the character
    /// that should be it.
    ExpectedBrace(usize),
    /// A repetition without a count where one goes.
    ExpectedInteger(usize),
    /// A group, class, quote or interval that the pattern ends inside, with the character that
    /// should close it and the position where it should be.
    ExpectedClose(char, usize),
    /// A class range whose end is below its start, or an empty class.
    BadClass(usize),
    /// A `<n-m>` whose bounds are not decimal numbers that fit in 64 bits.
    BadInterval(usize),
    /// More than [`MAX_STATES`] states.
    TooComplex,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    /// The empty string.
    Empty,
    /// No string (`#`).
    Nothing,
    /// One character in one of the ranges.
    Class(Vec<(char, char)>),
    /// Any string (`@`).
    AnyString,
    Concat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat(Box<Node>, u32, Option<u32>),
}

fn literal(c: char) -> Node {
    Node::Class(vec![(c, c)])
}

/// The unit `\c` stands for: the classes `\d`, `\s` and `\w` and their complements `\D`, `\S`
/// and `\W` (`\w` matched the term `g` in `refsearch`), else `c` itself.
fn escaped(c: char) -> Node {
    let digits = vec![('0', '9')];
    let spaces = vec![('\t', '\r'), (' ', ' ')];
    let word = vec![('0', '9'), ('A', 'Z'), ('_', '_'), ('a', 'z')];
    match c {
        'd' => Node::Class(digits),
        's' => Node::Class(spaces),
        'w' => Node::Class(word),
        'D' => Node::Class(complement(&digits)),
        'S' => Node::Class(complement(&spaces)),
        'W' => Node::Class(complement(&word)),
        c => literal(c),
    }
}

impl RegexError {
    /// The message `refsearch` gives for this error in `pattern` (measured 02-11-2026 for an
    /// unclosed group or branch, `a{2`, `a{2,1}` and `[z-a]`).
    pub fn message(&self, pattern: &str) -> String {
        let chars: Vec<char> = pattern.chars().collect();
        match *self {
            RegexError::UnexpectedEnd => "unexpected end-of-string".to_owned(),
            RegexError::ExpectedBrace(at) => format!("expected '}}' at position {at}"),
            RegexError::ExpectedInteger(at) => format!("integer expected at position {at}"),
            RegexError::Unexpected(at) => format!("end-of-string expected at position {at}"),
            RegexError::BadRepeat(at) => {
                let inner: String = chars[at + 1..].iter().take_while(|&&c| c != '}').collect();
                let (min, max) = inner.split_once(',').unwrap_or((&inner, &inner));
                format!("invalid repetition range(out of order): {min}..{max}")
            }
            RegexError::BadClass(at) => match class_inversion(&chars[at..]) {
                Some((from, to)) => {
                    format!("invalid range: from ({}) cannot be > to ({})", from as u32, to as u32)
                }
                None => "unexpected end-of-string".to_owned(),
            },
            RegexError::ExpectedClose(c, at) => format!("expected '{c}' at position {at}"),
            RegexError::BadInterval(at) => {
                let inner: String = chars[at + 1..].iter().take_while(|&&c| c != '>').collect();
                if inner.contains('-') {
                    format!("interval syntax error at position {at}")
                } else {
                    format!("'{inner}' not found")
                }
            }
            RegexError::TooComplex => "the pattern has too many states".to_owned(),
        }
    }
}

/// The first range `c-e` with `e < c` of the class that `class` starts.
fn class_inversion(class: &[char]) -> Option<(char, char)> {
    let mut i = 1;
    while i + 2 < class.len() && class[i] != ']' {
        if class[i + 1] == '-' && class[i + 2] < class[i] {
            return Some((class[i], class[i + 2]));
        }
        i += 1;
    }
    None
}

fn any_char() -> Node {
    Node::Class(vec![('\0', char::MAX)])
}

fn digits(lo: u8, hi: u8) -> Node {
    Node::Class(vec![(char::from(b'0' + lo), char::from(b'0' + hi))])
}

/// The ranges that complement `ranges` over the Unicode scalar values. `ranges` are sorted and
/// neither overlap nor touch, as [`normalize`] leaves them, so every gap between two of them holds
/// at least one character.
fn complement(ranges: &[(char, char)]) -> Vec<(char, char)> {
    let mut out = Vec::new();
    if let Some(&(lo, _)) = ranges.first()
        && lo > '\0'
    {
        out.push(('\0', before(lo)));
    }
    for pair in ranges.windows(2) {
        out.push((after(pair[0].1).expect("a later range follows"), before(pair[1].0)));
    }
    match ranges.last() {
        Some(&(_, hi)) => out.extend(after(hi).map(|n| (n, char::MAX))),
        None => out.push(('\0', char::MAX)),
    }
    out
}

fn after(c: char) -> Option<char> {
    match c {
        '\u{D7FF}' => Some('\u{E000}'),
        char::MAX => None,
        _ => char::from_u32(u32::from(c) + 1),
    }
}

fn before(c: char) -> char {
    match c {
        '\u{E000}' => '\u{D7FF}',
        _ => char::from_u32(u32::from(c) - 1).expect("callers pass a character above another"),
    }
}

/// Sorts and joins overlapping or adjacent ranges.
fn normalize(mut ranges: Vec<(char, char)>) -> Vec<(char, char)> {
    ranges.sort_unstable();
    let mut out: Vec<(char, char)> = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        match out.last_mut() {
            Some(last) if after(last.1).is_none_or(|n| n >= lo) => last.1 = last.1.max(hi),
            _ => out.push((lo, hi)),
        }
    }
    out
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn next(&mut self) -> Result<char, RegexError> {
        let c = self.peek().ok_or(RegexError::UnexpectedEnd)?;
        self.at += 1;
        Ok(c)
    }

    fn alternation(&mut self) -> Result<Node, RegexError> {
        let mut branches = vec![self.concatenation()?];
        while self.peek() == Some('|') {
            self.at += 1;
            // As in `refsearch`, a branch cannot be empty at the end of the pattern.
            if self.at == self.chars.len() {
                return Err(RegexError::UnexpectedEnd);
            }
            branches.push(self.concatenation()?);
        }
        Ok(if branches.len() == 1 { branches.remove(0) } else { Node::Alt(branches) })
    }

    fn concatenation(&mut self) -> Result<Node, RegexError> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom()?;
            items.push(self.postfix(atom)?);
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.remove(0),
            _ => Node::Concat(items),
        })
    }

    fn postfix(&mut self, mut node: Node) -> Result<Node, RegexError> {
        loop {
            node = match self.peek() {
                Some('*') => Node::Repeat(Box::new(node), 0, None),
                Some('+') => Node::Repeat(Box::new(node), 1, None),
                Some('?') => Node::Repeat(Box::new(node), 0, Some(1)),
                Some('{') => {
                    let start = self.at;
                    self.at += 1;
                    let (min, max) = self.repeat_counts(start)?;
                    node = Node::Repeat(Box::new(node), min, max);
                    continue;
                }
                _ => return Ok(node),
            };
            self.at += 1;
        }
    }

    /// `n}`, `n,}` or `n,m}` after the `{` at `start`.
    fn repeat_counts(&mut self, start: usize) -> Result<(u32, Option<u32>), RegexError> {
        let min = self.number(start)?;
        let max = if self.peek() == Some(',') {
            self.at += 1;
            if self.peek() == Some('}') { None } else { Some(self.number(start)?) }
        } else {
            Some(min)
        };
        if self.peek() != Some('}') {
            return Err(RegexError::ExpectedBrace(self.at));
        }
        self.at += 1;
        let bounded = |n: u32| n <= MAX_REPEAT;
        if !bounded(min) || max.is_some_and(|m| m < min || !bounded(m)) {
            return Err(RegexError::BadRepeat(start));
        }
        Ok((min, max))
    }

    fn number(&mut self, start: usize) -> Result<u32, RegexError> {
        let text = self.digit_run();
        if text.is_empty() {
            return Err(RegexError::ExpectedInteger(self.at));
        }
        text.parse().map_err(|_| RegexError::BadRepeat(start))
    }

    fn digit_run(&mut self) -> String {
        let mut text = String::new();
        while let Some(c) = self.peek().filter(char::is_ascii_digit) {
            text.push(c);
            self.at += 1;
        }
        text
    }

    fn atom(&mut self) -> Result<Node, RegexError> {
        let at = self.at;
        match self.next()? {
            '.' => Ok(any_char()),
            '#' => Ok(Node::Nothing),
            '@' => Ok(Node::AnyString),
            '\\' => Ok(escaped(self.next()?)),
            '(' => {
                if self.peek() == Some(')') {
                    self.at += 1;
                    return Ok(Node::Empty);
                }
                if self.peek().is_none() {
                    return Err(RegexError::UnexpectedEnd);
                }
                let inner = self.alternation()?;
                match self.peek() {
                    Some(')') => {
                        self.at += 1;
                        Ok(inner)
                    }
                    _ => Err(RegexError::ExpectedClose(')', self.at)),
                }
            }
            '[' => self.class(at),
            '"' => self.quoted(),
            '<' => self.interval(at),
            // Where a unit starts, `refsearch` reads any other character as itself (`[[:alpha:]]`
            // is a class and a `]`).
            c => Ok(literal(c)),
        }
    }

    fn quoted(&mut self) -> Result<Node, RegexError> {
        let mut items = Vec::new();
        loop {
            match self.peek() {
                None => return Err(RegexError::ExpectedClose('"', self.at)),
                Some('"') => {
                    self.at += 1;
                    return Ok(Node::Concat(items));
                }
                Some(c) => {
                    self.at += 1;
                    items.push(literal(c));
                }
            }
        }
    }

    fn class(&mut self, start: usize) -> Result<Node, RegexError> {
        let negated = self.peek() == Some('^');
        if negated {
            self.at += 1;
        }
        let mut ranges = Vec::new();
        loop {
            // The first character of a class is itself, `]` too (measured: `[]` wants a `]` at 2).
            let c = match self.peek() {
                None => return Err(RegexError::ExpectedClose(']', self.at)),
                Some(']') if !ranges.is_empty() => {
                    self.at += 1;
                    break;
                }
                Some('\\') => {
                    self.at += 1;
                    self.next()?
                }
                Some(c) => {
                    self.at += 1;
                    c
                }
            };
            if self.peek() == Some('-') && self.chars.get(self.at + 1).is_some_and(|&n| n != ']') {
                self.at += 1;
                let end = match self.next()? {
                    '\\' => self.next()?,
                    e => e,
                };
                if end < c {
                    return Err(RegexError::BadClass(start));
                }
                ranges.push((c, end));
            } else {
                ranges.push((c, c));
            }
        }
        let ranges = normalize(ranges);
        Ok(Node::Class(if negated { complement(&ranges) } else { ranges }))
    }

    /// `n-m>` after the `<` at `start`.
    fn interval(&mut self, start: usize) -> Result<Node, RegexError> {
        let from = self.at;
        while self.peek().is_some_and(|c| c != '>') {
            self.at += 1;
        }
        if self.peek().is_none() {
            return Err(RegexError::ExpectedClose('>', self.at));
        }
        let inner: String = self.chars[from..self.at].iter().collect();
        self.at += 1;
        let (lo, hi) = inner.split_once('-').ok_or(RegexError::BadInterval(start))?;
        let digits = |t: &str| !t.is_empty() && t.chars().all(|c| c.is_ascii_digit());
        if !digits(lo) || !digits(hi) {
            return Err(RegexError::BadInterval(start));
        }
        let (lo_n, hi_n) = match (lo.parse::<u64>(), hi.parse::<u64>()) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return Err(RegexError::BadInterval(start)),
        };
        let (lo_n, hi_n) = (lo_n.min(hi_n), lo_n.max(hi_n));
        let width =
            (lo.len() == hi.len() && lo.len() > 1 && lo.starts_with('0')).then_some(lo.len());
        Ok(numeric_range(lo_n, hi_n, width))
    }
}

/// The decimal strings of the numbers from `lo` to `hi`: without leading zeros, or padded with
/// zeros to `width` characters.
fn numeric_range(lo: u64, hi: u64, width: Option<usize>) -> Node {
    match width {
        Some(w) => same_length(&format!("{lo:0w$}"), &format!("{hi:0w$}")),
        None => {
            let (lo_len, hi_len) = (lo.to_string().len(), hi.to_string().len());
            let parts = (lo_len..=hi_len)
                .map(|len| {
                    let from = if len == lo_len { lo.to_string() } else { pow10_str(len - 1) };
                    let to = if len == hi_len { hi.to_string() } else { "9".repeat(len) };
                    same_length(&from, &to)
                })
                .collect();
            Node::Alt(parts)
        }
    }
}

fn pow10_str(zeros: usize) -> String {
    format!("1{}", "0".repeat(zeros))
}

/// The strings of the same length as `lo` and `hi` between them, digit by digit.
fn same_length(lo: &str, hi: &str) -> Node {
    let (lo, hi) = (lo.as_bytes(), hi.as_bytes());
    if lo.iter().all(|&d| d == b'0') && hi.iter().all(|&d| d == b'9') {
        let n = lo.len() as u32;
        return Node::Repeat(Box::new(digits(0, 9)), n, Some(n));
    }
    let (a, b) = (lo[0] - b'0', hi[0] - b'0');
    let tail = |l: &[u8], h: &[u8]| {
        same_length(std::str::from_utf8(l).expect("ascii"), std::str::from_utf8(h).expect("ascii"))
    };
    if a == b {
        return Node::Concat(vec![digits(a, a), tail(&lo[1..], &hi[1..])]);
    }
    let rest = lo.len() - 1;
    let nines = vec![b'9'; rest];
    let zeros = vec![b'0'; rest];
    let mut parts = vec![Node::Concat(vec![digits(a, a), tail(&lo[1..], &nines)])];
    if a + 1 < b {
        parts.push(Node::Concat(vec![
            digits(a + 1, b - 1),
            Node::Repeat(Box::new(digits(0, 9)), rest as u32, Some(rest as u32)),
        ]));
    }
    parts.push(Node::Concat(vec![digits(b, b), tail(&zeros, &hi[1..])]));
    Node::Alt(parts)
}

#[derive(Clone, Debug)]
enum Inst {
    /// Reads a character in one of the ranges.
    Read(Vec<(char, char)>, usize),
    /// Goes on with every target without reading.
    Split(Vec<usize>),
    Match,
    Fail,
}

/// A compiled pattern.
#[derive(Clone, Debug)]
pub struct Regex {
    insts: Vec<Inst>,
    start: Vec<usize>,
    /// Whether each instruction can reach [`Inst::Match`].
    live: Vec<bool>,
}

struct Compiler {
    insts: Vec<Inst>,
}

impl Compiler {
    fn push(&mut self, inst: Inst) -> Result<usize, RegexError> {
        if self.insts.len() >= MAX_STATES {
            return Err(RegexError::TooComplex);
        }
        self.insts.push(inst);
        Ok(self.insts.len() - 1)
    }

    /// Compiles `node` so that it continues at `next`; returns its entry.
    fn compile(&mut self, node: &Node, next: usize) -> Result<usize, RegexError> {
        match node {
            Node::Empty => Ok(next),
            Node::Nothing => self.push(Inst::Fail),
            Node::Class(ranges) => self.push(Inst::Read(ranges.clone(), next)),
            Node::AnyString => self.star(&any_char(), next),
            Node::Concat(items) => {
                items.iter().rev().try_fold(next, |acc, item| self.compile(item, acc))
            }
            Node::Alt(branches) => {
                let targets =
                    branches.iter().map(|b| self.compile(b, next)).collect::<Result<_, _>>()?;
                self.push(Inst::Split(targets))
            }
            Node::Repeat(inner, min, max) => {
                let mut entry = match max {
                    None => self.star(inner, next)?,
                    Some(max) => {
                        let mut acc = next;
                        for _ in *min..*max {
                            let body = self.compile(inner, acc)?;
                            acc = self.push(Inst::Split(vec![body, next]))?;
                        }
                        acc
                    }
                };
                for _ in 0..*min {
                    entry = self.compile(inner, entry)?;
                }
                Ok(entry)
            }
        }
    }

    fn star(&mut self, inner: &Node, next: usize) -> Result<usize, RegexError> {
        let split = self.push(Inst::Split(Vec::new()))?;
        let body = self.compile(inner, split)?;
        self.insts[split] = Inst::Split(vec![body, next]);
        Ok(split)
    }
}

impl Regex {
    /// Compiles a `regex` query.
    ///
    /// `<n-m>` matches the decimal numbers from `n` to `m` (the bounds in either order). When both
    /// bounds have the same number of digits and `n` starts with `0`, the numbers are padded with
    /// zeros to that width (`<01-10>` matches `01` to `10`); otherwise they are written without
    /// leading zeros.
    pub fn new(pattern: &str) -> Result<Regex, RegexError> {
        let mut parser = Parser { chars: pattern.chars().collect(), at: 0 };
        let node = parser.alternation()?;
        if parser.at < parser.chars.len() {
            return Err(RegexError::Unexpected(parser.at));
        }
        Regex::compile(&node)
    }

    /// Compiles a `wildcard` query.
    pub fn wildcard(pattern: &str) -> Result<Regex, RegexError> {
        let mut items = Vec::new();
        let mut chars = pattern.chars();
        while let Some(c) = chars.next() {
            items.push(match c {
                '*' => Node::AnyString,
                '?' => any_char(),
                '\\' => literal(chars.next().ok_or(RegexError::UnexpectedEnd)?),
                c => literal(c),
            });
        }
        Regex::compile(&Node::Concat(items))
    }

    fn compile(node: &Node) -> Result<Regex, RegexError> {
        let mut compiler = Compiler { insts: Vec::new() };
        let accept = compiler.push(Inst::Match)?;
        let entry = compiler.compile(node, accept)?;
        let insts = compiler.insts;
        let live = liveness(&insts);
        let mut regex = Regex { insts, start: Vec::new(), live };
        regex.start = regex.closure(&[entry]);
        Ok(regex)
    }

    /// The live `Read` and `Match` instructions reachable from `from` without reading.
    fn closure(&self, from: &[usize]) -> Vec<usize> {
        let mut seen = vec![false; self.insts.len()];
        let mut stack: Vec<usize> = from.to_vec();
        let mut out = Vec::new();
        while let Some(i) = stack.pop() {
            if seen[i] || !self.live[i] {
                continue;
            }
            seen[i] = true;
            match &self.insts[i] {
                Inst::Split(targets) => stack.extend(targets.iter().rev()),
                Inst::Read(..) | Inst::Match => out.push(i),
                Inst::Fail => {}
            }
        }
        out.sort_unstable();
        out
    }

    /// Whether the whole of `term` matches.
    pub fn is_full_match(&self, term: &str) -> bool {
        let mut state = self.start();
        for c in term.chars() {
            match self.step(&state, c) {
                Some(next) => state = next,
                None => return false,
            }
        }
        self.is_match(&state)
    }
}

/// Which instructions reach [`Inst::Match`], by fixed point over the edges.
fn liveness(insts: &[Inst]) -> Vec<bool> {
    let mut live: Vec<bool> = insts.iter().map(|i| matches!(i, Inst::Match)).collect();
    let mut changed = true;
    while changed {
        changed = false;
        for (i, inst) in insts.iter().enumerate() {
            let reaches = match inst {
                Inst::Read(_, next) => live[*next],
                Inst::Split(targets) => targets.iter().any(|&t| live[t]),
                Inst::Match | Inst::Fail => false,
            };
            if reaches && !live[i] {
                live[i] = true;
                changed = true;
            }
        }
    }
    live
}

impl CharAutomaton for Regex {
    type State = Vec<usize>;

    fn start(&self) -> Vec<usize> {
        self.start.clone()
    }

    fn step(&self, state: &Vec<usize>, c: char) -> Option<Vec<usize>> {
        let targets: Vec<usize> = state
            .iter()
            .filter_map(|&i| match &self.insts[i] {
                Inst::Read(ranges, next) if ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi) => {
                    Some(*next)
                }
                _ => None,
            })
            .collect();
        let next = self.closure(&targets);
        (!next.is_empty()).then_some(next)
    }

    fn is_match(&self, state: &Vec<usize>) -> bool {
        state.iter().any(|&i| matches!(self.insts[i], Inst::Match))
    }

    fn can_match(&self, state: &Vec<usize>) -> bool {
        !state.is_empty()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::query::fuzzy::Utf8;
    use crate::segment::fst::{Builder, Fst};

    fn full(pattern: &str, term: &str) -> bool {
        Regex::new(pattern).unwrap().is_full_match(term)
    }

    #[test]
    fn documented_operators_match_whole_terms() {
        assert!(full("ref.b", "refdb"));
        assert!(!full("ref", "refdb"));
        assert!(full("colou?r", "color") && full("colou?r", "colour"));
        assert!(full("ab+c", "abbbc") && !full("ab+c", "ac"));
        assert!(full("ab*c", "ac") && full("ab*c", "abbc"));
        assert!(full("cat|dog", "dog") && !full("cat|dog", "cow"));
        assert!(full("a{3}", "aaa") && !full("a{3}", "aa") && !full("a{3}", "aaaa"));
        assert!(full("a{2,}", "aaaaa") && !full("a{2,}", "a"));
        assert!(full("a{1,3}", "aaa") && !full("a{1,3}", "aaaa") && !full("a{1,3}", ""));
        assert!(full("(ab)+", "ababab") && !full("(ab)+", "aba"));
        assert!(full("[a-c]x", "bx") && !full("[a-c]x", "dx"));
        assert!(full("[^a-c]x", "dx") && !full("[^a-c]x", "ax"));
        assert!(full("[-a]", "-") && full("[a-]", "-") && full("[a\\-z]", "-"));
        assert!(!full("#", "") && !full("a#", "a"));
        assert!(full("a|#", "a"));
        assert!(full("", "") && !full("", "a"));
        assert!(full("x*?", "xx"));
    }

    #[test]
    fn reserved_characters_are_literal_after_a_backslash() {
        for c in
            ['.', '?', '+', '*', '|', '{', '}', '[', ']', '(', ')', '<', '>', '"', '\\', '@', '#']
        {
            let pattern = format!("a\\{c}b");
            let term = format!("a{c}b");
            assert!(full(&pattern, &term), "{pattern}");
            assert!(!full(&pattern, "axb"), "{pattern}");
        }
        assert!(full("a~&b", "a~&b"));
    }

    #[test]
    fn quotes_and_any_string() {
        assert!(full("\"a.b*\"", "a.b*") && !full("\"a.b*\"", "axb"));
        assert!(full("mon@", "refdb") && full("@db", "refdb") && full("@", ""));
        assert!(full("\"\"x", "x"));
    }

    #[test]
    fn numeric_intervals() {
        let r = Regex::new("<7-123>").unwrap();
        for n in 0..300_u32 {
            assert_eq!(r.is_full_match(&n.to_string()), (7..=123).contains(&n), "{n}");
        }
        assert!(!r.is_full_match("07") && !r.is_full_match("007"));
        let swapped = Regex::new("<20-3>").unwrap();
        assert!(
            swapped.is_full_match("3")
                && swapped.is_full_match("20")
                && !swapped.is_full_match("21")
        );
        let padded = Regex::new("<01-10>").unwrap();
        assert!(padded.is_full_match("01") && padded.is_full_match("10"));
        assert!(
            !padded.is_full_match("1")
                && !padded.is_full_match("00")
                && !padded.is_full_match("11")
        );
        let wide = Regex::new("<0050-0200>").unwrap();
        for n in 0..400_u32 {
            assert_eq!(wide.is_full_match(&format!("{n:04}")), (50..=200).contains(&n), "{n}");
        }
        let all = Regex::new("<0-18446744073709551615>").unwrap();
        assert!(all.is_full_match("18446744073709551615") && all.is_full_match("0"));
        assert!(!all.is_full_match("18446744073709551616"));
        assert!(full("x<5-5>y", "x5y"));
    }

    #[test]
    fn malformed_patterns_are_rejected_with_their_position() {
        assert_eq!(Regex::new("ab)").unwrap_err(), RegexError::Unexpected(2));
        assert_eq!(Regex::new("(ab").unwrap_err(), RegexError::ExpectedClose(')', 3));
        assert_eq!(Regex::new("(a]").unwrap_err(), RegexError::ExpectedClose(')', 3));
        assert!(Regex::new("*a").unwrap().is_full_match("*a"));
        assert!(Regex::new("a|+").unwrap().is_full_match("+"));
        assert!(Regex::new("a}").unwrap().is_full_match("a}"));
        assert_eq!(Regex::new("a\\").unwrap_err(), RegexError::UnexpectedEnd);
        assert_eq!(Regex::new("[ab").unwrap_err(), RegexError::ExpectedClose(']', 3));
        assert_eq!(Regex::new("[]").unwrap_err(), RegexError::ExpectedClose(']', 2));
        assert_eq!(Regex::new("x[z-a]").unwrap_err(), RegexError::BadClass(1));
        assert_eq!(Regex::new("\"abc").unwrap_err(), RegexError::ExpectedClose('"', 4));
        assert_eq!(Regex::new("a{").unwrap_err(), RegexError::ExpectedInteger(2));
        assert_eq!(Regex::new("a{x}").unwrap_err(), RegexError::ExpectedInteger(2));
        assert_eq!(Regex::new("a{2").unwrap_err(), RegexError::ExpectedBrace(3));
        assert_eq!(Regex::new("a{2x").unwrap_err(), RegexError::ExpectedBrace(3));
        assert_eq!(Regex::new("a{3,2}").unwrap_err(), RegexError::BadRepeat(1));
        assert_eq!(Regex::new("a{1001}").unwrap_err(), RegexError::BadRepeat(1));
        assert_eq!(Regex::new("a{99999999999}").unwrap_err(), RegexError::BadRepeat(1));
        assert_eq!(Regex::new("<1-2").unwrap_err(), RegexError::ExpectedClose('>', 4));
        assert_eq!(Regex::new("<1x2>").unwrap_err(), RegexError::BadInterval(0));
        assert_eq!(Regex::new("<-2>").unwrap_err(), RegexError::BadInterval(0));
        assert_eq!(Regex::new("<1->").unwrap_err(), RegexError::BadInterval(0));
        assert_eq!(Regex::new("<1-2]").unwrap_err(), RegexError::ExpectedClose('>', 5));
        assert_eq!(Regex::new("<1-99999999999999999999>").unwrap_err(), RegexError::BadInterval(0));
        assert!(Regex::new("a>").unwrap().is_full_match("a>"));
        assert_eq!(Regex::new("(.{1000}){1000}").unwrap_err(), RegexError::TooComplex);
    }

    #[test]
    fn classes_complement_over_unicode() {
        assert!(full("[^a]", "ñ") && full("[^a]", "\u{10FFFF}") && full("[^a]", "\u{E000}"));
        assert!(full("[^\u{D7FF}]", "\u{E000}") && !full("[^\u{D7FF}]", "\u{D7FF}"));
        assert!(full("[^\u{E000}]", "\u{D7FF}"));
        assert!(!full("[^\u{0}-\u{10FFFF}]x", "ax"));
        assert!(full("[^\u{10FFFF}]", "a") && !full("[^\u{10FFFF}]", "\u{10FFFF}"));
        assert!(full("[a-cb-dx]", "d") && full("[a-cd-e]", "e") && !full("[a-cx]", "e"));
        assert!(full("[\\]]", "]"));
        assert!(full("[\\a-\\c]", "b"));
        assert_eq!(complement(&[('\0', char::MAX)]), vec![]);
        assert_eq!(complement(&[]), vec![('\0', char::MAX)]);
        assert_eq!(complement(&[('\0', 'a')]), vec![('b', char::MAX)]);
        assert_eq!(normalize(vec![('a', 'b'), ('c', 'd')]), vec![('a', 'd')]);
        assert_eq!(normalize(vec![('a', char::MAX), ('b', 'c')]), vec![('a', char::MAX)]);
    }

    #[test]
    fn wildcard_patterns() {
        let w = |p: &str, t: &str| Regex::wildcard(p).unwrap().is_full_match(t);
        assert!(w("mon*", "refdb") && w("*db", "refdb") && w("m?ngo*", "mango"));
        assert!(!w("m?ngo", "mngo") && w("a\\*b", "a*b") && !w("a\\*b", "axb"));
        assert!(w("", "") && !w("", "x"));
        assert_eq!(Regex::wildcard("a\\").unwrap_err(), RegexError::UnexpectedEnd);
        assert_eq!(Regex::wildcard(&"?".repeat(MAX_STATES)).unwrap_err(), RegexError::TooComplex);
    }

    /// A slow reference matcher over the parse tree: every way of splitting the term.
    fn reference(node: &Node, s: &[char]) -> bool {
        match node {
            Node::Empty => s.is_empty(),
            Node::Nothing => false,
            Node::Class(r) => s.len() == 1 && r.iter().any(|&(lo, hi)| lo <= s[0] && s[0] <= hi),
            Node::AnyString => true,
            Node::Concat(items) => match items.split_first() {
                None => s.is_empty(),
                Some((first, rest)) => (0..=s.len()).any(|k| {
                    reference(first, &s[..k]) && reference(&Node::Concat(rest.to_vec()), &s[k..])
                }),
            },
            Node::Alt(b) => b.iter().any(|n| reference(n, s)),
            Node::Repeat(inner, min, max) => {
                if *min == 0 && s.is_empty() {
                    return true;
                }
                if *max == Some(0) {
                    return false;
                }
                let rest = Node::Repeat(inner.clone(), min.saturating_sub(1), max.map(|m| m - 1));
                // An empty first copy only helps while it lowers the minimum.
                let first = if *min > 0 { 0 } else { 1 };
                (first..=s.len()).any(|k| reference(inner, &s[..k]) && reference(&rest, &s[k..]))
            }
        }
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn pick<T: Copy>(&mut self, items: &[T]) -> T {
            items[(self.next() % items.len() as u64) as usize]
        }
    }

    fn random_pattern(rng: &mut Rng, depth: u32) -> String {
        let atom = |rng: &mut Rng| -> String {
            rng.pick(&["a", "b", ".", "[ab]", "[^a]", "#", "@", "\"ab\"", "<1-12>"]).to_string()
        };
        let mut out = String::new();
        for _ in 0..1 + rng.next() % 3 {
            let mut part = if depth > 0 && rng.next().is_multiple_of(3) {
                format!("({})", random_pattern(rng, depth - 1))
            } else {
                atom(rng)
            };
            part.push_str(rng.pick(&["", "", "*", "+", "?", "{2}", "{0,2}", "{1,}"]));
            out.push_str(&part);
        }
        if depth > 0 && rng.next().is_multiple_of(4) {
            out = format!("{out}|{}", random_pattern(rng, depth - 1));
        }
        out
    }

    #[test]
    fn the_automaton_agrees_with_a_reference_on_random_patterns() {
        let mut rng = Rng(7);
        let alphabet = ['a', 'b', 'c', '1', '2'];
        for _ in 0..400 {
            let pattern = random_pattern(&mut rng, 2);
            let mut parser = Parser { chars: pattern.chars().collect(), at: 0 };
            let node = parser.alternation().unwrap();
            let regex = Regex::new(&pattern).unwrap();
            for _ in 0..20 {
                let len = rng.next() % 5;
                let term: Vec<char> = (0..len).map(|_| rng.pick(&alphabet)).collect();
                let text: String = term.iter().collect();
                assert_eq!(
                    regex.is_full_match(&text),
                    reference(&node, &term),
                    "{pattern} on {text}"
                );
            }
        }
    }

    #[test]
    fn regex_over_the_dictionary_matches_a_scan() {
        let mut terms = ["refdb", "ref", "mango", "anotherproduct", "m1", "m22", "m300", "ñandú"];
        terms.sort_unstable();
        let mut b = Builder::new();
        for (i, t) in terms.iter().enumerate() {
            b.insert(t.as_bytes(), i as u64);
        }
        let bytes = b.finish();
        let fst = Fst::new(&bytes).unwrap();
        for pattern in ["m.ngo.*", "m<1-30>", "[^m]@", "ñ.*", "#", "@", "(mon|man)go"] {
            let regex = Regex::new(pattern).unwrap();
            let got: Vec<Vec<u8>> = fst.search(&Utf8(&regex)).into_iter().map(|(t, _)| t).collect();
            let expected: Vec<Vec<u8>> = terms
                .iter()
                .filter(|t| regex.is_full_match(t))
                .map(|t| t.as_bytes().to_vec())
                .collect();
            assert_eq!(got, expected, "{pattern}");
        }
    }
}
