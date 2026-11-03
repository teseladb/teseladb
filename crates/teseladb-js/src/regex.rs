//! ECMAScript regular expressions (ECMA-262 §22.2) with a backtracking matcher: classes,
//! escapes, groups (named too), alternation, greedy and lazy quantifiers, anchors, word
//! boundaries, back references and lookaround, with the flags `g`, `i`, `m`, `s`, `u` and `y`.
//! Each step of the matcher counts against the invocation's step limit.

use crate::interp::{Interp, R};
use crate::strings::substitute;
use crate::value::*;

#[derive(Clone, Debug)]
enum Node {
    /// A code point, or a UTF-16 unit without the `u` flag.
    Char(u32),
    Any,
    Class(Vec<ClassItem>, bool),
    Start,
    End,
    WordB(bool),
    Group(Box<Node>, Option<usize>),
    Alt(Vec<Node>),
    Seq(Vec<Node>),
    Repeat(Box<Node>, usize, usize, bool),
    BackRef(usize),
    Look(Box<Node>, bool, bool),
}

#[derive(Clone, Debug)]
enum ClassItem {
    Range(u32, u32),
    Digit(bool),
    Word(bool),
    Space(bool),
}

struct Parser<'a> {
    s: &'a [u32],
    unicode: bool,
    i: usize,
    groups: usize,
    names: Vec<(String, usize)>,
    /// Whether the pattern has a named group: `\k` is then a reference (Annex B).
    has_names: bool,
    /// The named groups a first reading found (`None` during it), for references written before
    /// their group.
    known: Option<Vec<(String, usize)>>,
}

/// A compiled pattern.
struct Regex {
    node: Node,
    groups: usize,
    names: Vec<(String, usize)>,
    icase: bool,
    multiline: bool,
    dotall: bool,
    /// The `d` flag: a match has `indices`.
    indices: bool,
}

/// What the matcher reads of `s`: its code points with the `u` flag, else its UTF-16 units.
fn elements(s: &str, unicode: bool) -> Vec<u32> {
    if unicode {
        crate::text::cps(s).collect()
    } else {
        crate::strings::units(s).into_iter().map(u32::from).collect()
    }
}

/// The string of the elements `e`.
fn text_of(e: &[u32]) -> String {
    let mut u = Vec::with_capacity(e.len());
    for &c in e {
        match u16::try_from(c) {
            Ok(x) => u.push(x),
            Err(_) => {
                let v = c - 0x1_0000;
                u.extend([0xD800 | (v >> 10) as u16, 0xDC00 | (v & 0x3FF) as u16]);
            }
        }
    }
    crate::strings::from_units(&u)
}

fn is_unicode(flags: &str) -> bool {
    flags.contains('u') || flags.contains('v')
}

fn compile(src: &str, flags: &str) -> Result<Regex, String> {
    let unicode = is_unicode(flags);
    let chars = elements(src, unicode);
    let has_names = src.match_indices("(?<").any(|(i, _)| !src[i + 3..].starts_with(['=', '!']));
    // A first reading finds the names, which a reference may use before its group.
    let mut first =
        Parser { s: &chars, unicode, i: 0, groups: 0, names: Vec::new(), has_names, known: None };
    let known = match first.alt() {
        Ok(_) => first.names,
        Err(_) => Vec::new(),
    };
    let mut p = Parser {
        s: &chars,
        unicode,
        i: 0,
        groups: 0,
        names: Vec::new(),
        has_names,
        known: Some(known),
    };
    let node = p.alt()?;
    if p.i < chars.len() {
        return Err("unmatched ) in regular expression".into());
    }
    Ok(Regex {
        node,
        groups: p.groups,
        names: p.names,
        icase: flags.contains('i'),
        multiline: flags.contains('m'),
        dotall: flags.contains('s'),
        indices: flags.contains('d'),
    })
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).map(|&c| char::from_u32(c).unwrap_or('\u{fffd}'))
    }

    /// The text of the elements from `start` to the current one.
    fn text_from(&self, start: usize) -> String {
        text_of(&self.s[start..self.i])
    }

    fn alt(&mut self) -> Result<Node, String> {
        let mut alts = vec![self.seq()?];
        while self.peek() == Some('|') {
            self.i += 1;
            alts.push(self.seq()?);
        }
        Ok(if alts.len() == 1 {
            alts.pop().unwrap_or(Node::Seq(Vec::new()))
        } else {
            Node::Alt(alts)
        })
    }

    fn seq(&mut self) -> Result<Node, String> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom()?;
            items.push(self.quantifier(atom)?);
        }
        Ok(Node::Seq(items))
    }

    fn number(&mut self) -> Option<usize> {
        let start = self.i;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        if self.i == start {
            return None;
        }
        self.text_from(start).parse().ok()
    }

    fn quantifier(&mut self, atom: Node) -> Result<Node, String> {
        let (min, max) = match self.peek() {
            Some('*') => (0, usize::MAX),
            Some('+') => (1, usize::MAX),
            Some('?') => (0, 1),
            Some('{') => {
                let save = self.i;
                self.i += 1;
                match self.number() {
                    Some(min) => {
                        let max = if self.peek() == Some(',') {
                            self.i += 1;
                            self.number().unwrap_or(usize::MAX)
                        } else {
                            min
                        };
                        if self.peek() != Some('}') {
                            self.i = save;
                            return Ok(atom);
                        }
                        if max < min {
                            return Err("numbers out of order in {} quantifier.".into());
                        }
                        (min, max)
                    }
                    None => {
                        self.i = save;
                        return Ok(atom);
                    }
                }
            }
            _ => return Ok(atom),
        };
        if matches!(atom, Node::Start | Node::End | Node::WordB(_)) {
            return Err("nothing to repeat".into());
        }
        self.i += 1;
        let greedy = if self.peek() == Some('?') {
            self.i += 1;
            false
        } else {
            true
        };
        Ok(Node::Repeat(Box::new(atom), min, max, greedy))
    }

    fn atom(&mut self) -> Result<Node, String> {
        let Some(c) = self.peek() else { return Err("unexpected end".into()) };
        self.i += 1;
        Ok(match c {
            '.' => Node::Any,
            '^' => Node::Start,
            '$' => Node::End,
            '(' => {
                let mut capture = true;
                let mut look = None;
                let mut name = None;
                if self.peek() == Some('?') {
                    self.i += 1;
                    match self.peek() {
                        Some(':') => capture = false,
                        Some('=') => look = Some((true, false)),
                        Some('!') => look = Some((true, true)),
                        Some('<') if matches!(self.s.get(self.i + 1), Some(0x3D | 0x21)) => {
                            self.i += 1;
                            look = Some((false, self.peek() == Some('!')));
                        }
                        Some('<') => {
                            self.i += 1;
                            let start = self.i;
                            while self.peek().is_some_and(|c| c != '>') {
                                self.i += 1;
                            }
                            name = Some(self.text_from(start));
                        }
                        _ => return Err("invalid regexp group".into()),
                    }
                    self.i += 1;
                }
                let index = if capture && look.is_none() {
                    self.groups += 1;
                    if let Some(n) = name {
                        if self.names.iter().any(|(m, _)| *m == n) {
                            return Err("duplicate capture group name in regular expression".into());
                        }
                        self.names.push((n, self.groups));
                    }
                    Some(self.groups)
                } else {
                    None
                };
                let inner = self.alt()?;
                if self.peek() != Some(')') {
                    return Err("unterminated parenthetical".into());
                }
                self.i += 1;
                match look {
                    Some((ahead, neg)) => Node::Look(Box::new(inner), ahead, neg),
                    None => Node::Group(Box::new(inner), index),
                }
            }
            ')' => return Err("unmatched ) in regular expression".into()),
            '[' => self.class()?,
            '\\' => self.escape(false)?,
            '*' | '+' | '?' => return Err("nothing to repeat".into()),
            _ => Node::Char(self.s[self.i - 1]),
        })
    }

    fn escape(&mut self, in_class: bool) -> Result<Node, String> {
        let Some(c) = self.peek() else { return Err("\\ at end of pattern".into()) };
        self.i += 1;
        let class = |item| Node::Class(vec![item], false);
        Ok(match c {
            // The engine of a `refrouter` 9.0.4 has no Unicode properties (measured on 02-11).
            'p' | 'P' if self.unicode => {
                return Err("invalid property name in regular expression".into());
            }
            'd' => class(ClassItem::Digit(false)),
            'D' => class(ClassItem::Digit(true)),
            'w' => class(ClassItem::Word(false)),
            'W' => class(ClassItem::Word(true)),
            's' => class(ClassItem::Space(false)),
            'S' => class(ClassItem::Space(true)),
            'b' if !in_class => Node::WordB(true),
            'B' if !in_class => Node::WordB(false),
            'b' => Node::Char(0x08),
            'n' => Node::Char(0x0A),
            'r' => Node::Char(0x0D),
            't' => Node::Char(0x09),
            'v' => Node::Char(0x0B),
            'f' => Node::Char(0x0C),
            '0' => Node::Char(0),
            'k' if self.peek() == Some('<') && (self.unicode || self.has_names) => {
                self.i += 1;
                let start = self.i;
                while self.peek().is_some_and(|c| c != '>') {
                    self.i += 1;
                }
                let name = self.text_from(start);
                self.i += 1;
                let idx = match &self.known {
                    Some(k) => k.iter().find(|(n, _)| *n == name).map(|(_, i)| *i),
                    // The first reading: any name, until the groups are known.
                    None => Some(0),
                };
                Node::BackRef(idx.ok_or("invalid named capture reference in regular expression")?)
            }
            '1'..='9' if !in_class => {
                self.i -= 1;
                Node::BackRef(self.number().unwrap_or(0))
            }
            'x' | 'u' => {
                let n = if c == 'x' { 2 } else { 4 };
                // `\u{…}` is a code point only with `u`: otherwise `{…}` repeats `u`.
                if c == 'u' && self.unicode && self.peek() == Some('{') {
                    self.i += 1;
                    let start = self.i;
                    while self.peek().is_some_and(|c| c != '}') {
                        self.i += 1;
                    }
                    let hex = self.text_from(start);
                    self.i += 1;
                    let v = u32::from_str_radix(&hex, 16)
                        .ok()
                        .filter(|v| *v <= 0x10_FFFF)
                        .ok_or("invalid unicode escape in regular expression")?;
                    return Ok(Node::Char(v));
                }
                let hex = self.s.get(self.i..self.i + n).map(text_of).unwrap_or_default();
                match u32::from_str_radix(&hex, 16) {
                    Ok(v) if hex.len() == n => {
                        self.i += n;
                        Node::Char(self.low_surrogate_after(v))
                    }
                    _ => Node::Char(c as u32),
                }
            }
            // Annex B: in a class `\c` also takes a digit or `_`; a `\c` without its letter is a
            // `\` and the `c` is read again.
            'c' => match self.peek() {
                Some(l)
                    if l.is_ascii_alphabetic()
                        || (in_class && (l.is_ascii_digit() || l == '_')) =>
                {
                    self.i += 1;
                    Node::Char(l as u32 % 32)
                }
                _ => {
                    self.i -= 1;
                    Node::Char(u32::from(b'\\'))
                }
            },
            '1'..='7' if in_class && self.unicode => {
                return Err("invalid decimal escape in regular expression".into());
            }
            '8' | '9' if in_class && self.unicode => {
                return Err("invalid identity escape in regular expression".into());
            }
            // Annex B: an octal escape in a class (`[\1]`).
            '1'..='7' if in_class => {
                let mut v = c as u32 - u32::from(b'0');
                while v < 32 && self.peek().is_some_and(|d| ('0'..='7').contains(&d)) {
                    v = v * 8 + (self.s[self.i] - u32::from(b'0'));
                    self.i += 1;
                }
                Node::Char(v)
            }
            _ => Node::Char(self.s[self.i - 1]),
        })
    }

    /// With the `u` flag, the high surrogate `v` of a `\\u` escape joined to the low one of the
    /// escape that follows it, as one code point.
    fn low_surrogate_after(&mut self, v: u32) -> u32 {
        if !self.unicode || !(0xD800..0xDC00).contains(&v) {
            return v;
        }
        let next = self.s.get(self.i..self.i + 6).map(text_of);
        let low = next
            .as_deref()
            .and_then(|t| t.strip_prefix("\\u"))
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .filter(|l| (0xDC00..0xE000).contains(l));
        match low {
            Some(l) => {
                self.i += 6;
                0x1_0000 + ((v - 0xD800) << 10) + (l - 0xDC00)
            }
            None => v,
        }
    }

    fn class(&mut self) -> Result<Node, String> {
        let negated = self.peek() == Some('^');
        if negated {
            self.i += 1;
        }
        let mut items = Vec::new();
        loop {
            let Some(c) = self.peek() else { return Err("unterminated character class".into()) };
            self.i += 1;
            if c == ']' {
                break;
            }
            let raw = self.s[self.i - 1];
            let lo = if c == '\\' {
                match self.escape(true)? {
                    Node::Char(ch) => ch,
                    Node::Class(mut v, _) => {
                        items.append(&mut v);
                        continue;
                    }
                    _ => continue,
                }
            } else {
                raw
            };
            if self.peek() == Some('-') && self.s.get(self.i + 1).is_some_and(|&n| n != 0x5D) {
                self.i += 1;
                let c2 = self.peek().unwrap_or('\\');
                let raw2 = self.s[self.i];
                self.i += 1;
                let hi = if c2 == '\\' {
                    match self.escape(true)? {
                        Node::Char(ch) => ch,
                        _ => return Err("invalid range in character class".into()),
                    }
                } else {
                    raw2
                };
                if hi < lo {
                    return Err("invalid range in character class".into());
                }
                items.push(ClassItem::Range(lo, hi));
            } else {
                items.push(ClassItem::Range(lo, lo));
            }
        }
        Ok(Node::Class(items, negated))
    }
}

fn is_word(c: u32) -> bool {
    char::from_u32(c).is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Canonicalize (ECMA-262 §22.2.2.7.3, without the `u` flag), which the engine of a `refrouter`
/// 9.0.4 also applies with it (measured on 02-11: `/\u212A/iu` does not match `k`): the upper
/// case of one character, unless it takes a non-ASCII character to ASCII.
fn fold(c: u32, icase: bool) -> u32 {
    if !icase {
        return c;
    }
    let mut up = char::from_u32(c).map(char::to_uppercase);
    match (up.as_mut().and_then(Iterator::next), up.as_mut().and_then(Iterator::next)) {
        (Some(u), None) if !(c >= 128 && u32::from(u) < 128) => u32::from(u),
        _ => c,
    }
}

fn lower(c: u32) -> u32 {
    char::from_u32(c).and_then(|ch| ch.to_lowercase().next()).map_or(c, u32::from)
}

/// The capture slots: start and end of each group, by character positions.
type Caps = Vec<Option<(usize, usize)>>;

struct Matcher<'a, 'i> {
    re: &'a Regex,
    s: &'a [u32],
    it: &'a mut Interp<'i>,
    fatal: Option<crate::interp::Ctrl>,
}

impl Matcher<'_, '_> {
    fn tick(&mut self) -> bool {
        if self.fatal.is_some() {
            return false;
        }
        match self.it.step() {
            Ok(()) => true,
            Err(e) => {
                self.fatal = Some(e);
                false
            }
        }
    }

    fn class_has(&self, items: &[ClassItem], c: u32) -> bool {
        let ch = char::from_u32(c);
        items.iter().any(|it| match it {
            ClassItem::Range(lo, hi) => {
                // A member whose canonical form is the character's.
                let same = |m: u32| (*lo..=*hi).contains(&m) && fold(m, true) == fold(c, true);
                (*lo..=*hi).contains(&c)
                    || (self.re.icase && (same(fold(c, true)) || same(lower(c))))
            }
            ClassItem::Digit(neg) => ch.is_some_and(|c| c.is_ascii_digit()) != *neg,
            ClassItem::Word(neg) => is_word(c) != *neg,
            ClassItem::Space(neg) => {
                ch.is_some_and(|c| c.is_whitespace() || c == '\u{feff}') != *neg
            }
        })
    }

    /// Matches `nodes` at `pos`, then the continuation.
    fn seq(
        &mut self,
        nodes: &[Node],
        pos: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(&mut Self, usize, &mut Caps) -> bool,
    ) -> bool {
        if !self.tick() {
            return false;
        }
        let Some((first, rest)) = nodes.split_first() else { return k(self, pos, caps) };
        self.node(first, pos, caps, &mut |m: &mut Self, p, c: &mut Caps| m.seq(rest, p, c, k))
    }

    fn node(
        &mut self,
        n: &Node,
        pos: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(&mut Self, usize, &mut Caps) -> bool,
    ) -> bool {
        if !self.tick() {
            return false;
        }
        let s = self.s;
        match n {
            Node::Char(c) => {
                pos < s.len()
                    && fold(s[pos], self.re.icase) == fold(*c, self.re.icase)
                    && k(self, pos + 1, caps)
            }
            Node::Any => {
                pos < s.len()
                    && (self.re.dotall || !matches!(s[pos], 0x0A | 0x0D | 0x2028 | 0x2029))
                    && k(self, pos + 1, caps)
            }
            Node::Class(items, neg) => {
                pos < s.len() && self.class_has(items, s[pos]) != *neg && k(self, pos + 1, caps)
            }
            Node::Start => {
                (pos == 0 || (self.re.multiline && s[pos - 1] == 0x0A)) && k(self, pos, caps)
            }
            Node::End => {
                (pos == s.len() || (self.re.multiline && s[pos] == 0x0A)) && k(self, pos, caps)
            }
            Node::WordB(want) => {
                let a = pos > 0 && is_word(s[pos - 1]);
                let b = pos < s.len() && is_word(s[pos]);
                ((a != b) == *want) && k(self, pos, caps)
            }
            Node::Seq(items) => self.seq(items, pos, caps, k),
            Node::Alt(alts) => alts.iter().any(|a| {
                let mut c2 = caps.clone();
                if self.node(a, pos, &mut c2, k) {
                    *caps = c2;
                    true
                } else {
                    false
                }
            }),
            Node::Group(inner, idx) => {
                let idx = *idx;
                self.node(inner, pos, caps, &mut |m: &mut Self, p, c: &mut Caps| {
                    let saved = idx.map(|i| c[i]);
                    if let Some(i) = idx {
                        c[i] = Some((pos, p));
                    }
                    if k(m, p, c) {
                        return true;
                    }
                    if let (Some(i), Some(sv)) = (idx, saved) {
                        c[i] = sv;
                    }
                    false
                })
            }
            Node::BackRef(i) => {
                let Some(Some((a, b))) = caps.get(*i).copied() else { return k(self, pos, caps) };
                let len = b - a;
                if pos + len > s.len() {
                    return false;
                }
                let same = (0..len)
                    .all(|j| fold(s[a + j], self.re.icase) == fold(s[pos + j], self.re.icase));
                same && k(self, pos + len, caps)
            }
            Node::Look(inner, ahead, neg) => {
                if !*ahead {
                    // Lookbehind: some start before `pos` whose match ends at `pos`, the
                    // farthest first, as the backward match of a greedy group takes the most
                    // (measured: `/(?<=(\d+))x/` captures `123` of `123x`); a positive one keeps
                    // its captures.
                    let found = (0..=pos).find_map(|st| {
                        let mut c2 = caps.clone();
                        self.node(inner, st, &mut c2, &mut |_, p, _| p == pos).then_some(c2)
                    });
                    if found.is_some() == *neg {
                        return false;
                    }
                    if !*neg {
                        *caps = found.unwrap_or_default();
                    }
                    return k(self, pos, caps);
                }
                let mut c2 = caps.clone();
                let found = self.node(inner, pos, &mut c2, &mut |_, _, _| true);
                if found != *neg {
                    if !*neg {
                        *caps = c2;
                    }
                    k(self, pos, caps)
                } else {
                    false
                }
            }
            Node::Repeat(inner, min, max, greedy) => {
                self.repeat(inner, *min, *max, *greedy, pos, caps, k, 0)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn repeat(
        &mut self,
        inner: &Node,
        min: usize,
        max: usize,
        greedy: bool,
        pos: usize,
        caps: &mut Caps,
        k: &mut dyn FnMut(&mut Self, usize, &mut Caps) -> bool,
        count: usize,
    ) -> bool {
        if !self.tick() {
            return false;
        }
        let more = |m: &mut Self,
                    caps: &mut Caps,
                    k: &mut dyn FnMut(&mut Self, usize, &mut Caps) -> bool| {
            count < max
                && m.node(inner, pos, caps, &mut |m2: &mut Self, p, c: &mut Caps| {
                    // An empty iteration past the minimum ends the loop.
                    if p == pos && count >= min {
                        return false;
                    }
                    m2.repeat(inner, min, max, greedy, p, c, k, count + 1)
                })
        };
        if count < min {
            return more(self, caps, k);
        }
        if greedy {
            let mut c2 = caps.clone();
            if more(self, &mut c2, k) {
                *caps = c2;
                return true;
            }
            k(self, pos, caps)
        } else {
            if k(self, pos, caps) {
                return true;
            }
            more(self, caps, k)
        }
    }
}

/// The first match at or after `start` (character positions): its captures.
fn find(it: &mut Interp, re: &Regex, s: &[u32], start: usize, sticky: bool) -> R<Option<Caps>> {
    let mut m = Matcher { re, s, it, fatal: None };
    let last = if sticky { start } else { s.len() };
    for st in start..=last.min(s.len()) {
        let mut caps: Caps = vec![None; re.groups + 1];
        let mut end = None;
        let ok = m.node(&re.node, st, &mut caps, &mut |_, p, _| {
            end = Some(p);
            true
        });
        if let Some(e) = m.fatal.take() {
            return Err(e);
        }
        if ok {
            caps[0] = Some((st, end.unwrap_or(st)));
            return Ok(Some(caps));
        }
    }
    Ok(None)
}

fn parts(it: &Interp, v: &Val) -> R<(Name, Name)> {
    if let Val::Obj(o) = v
        && let Kind::RegExp(s, f) = &o.borrow().kind
    {
        return Ok((s.clone(), f.clone()));
    }
    // Measured on a `refrouter` 9.0.4 on 02-11: `exec method called on incompatible Object`.
    let class = crate::builtins::class_name(v);
    Err(it.error("TypeError", &format!("{} method called on incompatible {class}", it.native)))
}

fn compiled(it: &Interp, src: &str, flags: &str) -> R<Regex> {
    let re = compile(src, flags).map_err(|e| it.error("SyntaxError", &e))?;
    // The engine compiles the pattern on its first match, each group nested in another taking a
    // slice of its native stack ([`crate::recursion::NESTING`]).
    let room = crate::recursion::NATIVE_BUDGET.saturating_sub(it.units);
    if nesting(&re.node).saturating_mul(crate::recursion::NESTING) > room {
        return Err(it.error("Error", "regexp too big"));
    }
    Ok(re)
}

/// How many groups and lookarounds `n` nests in one another.
fn nesting(n: &Node) -> usize {
    match n {
        Node::Group(x, _) | Node::Look(x, ..) => 1 + nesting(x),
        Node::Repeat(x, ..) => nesting(x),
        Node::Alt(xs) | Node::Seq(xs) => xs.iter().map(nesting).max().unwrap_or(0),
        _ => 0,
    }
}

/// Checks a pattern when a literal or `new RegExp` makes it.
pub fn check(it: &mut Interp, src: &str, flags: &str) -> R<()> {
    syntax(src, flags).map_err(|e| it.error("SyntaxError", &e))
}

/// The early error of a pattern and its flags, if any (a literal's is a `SyntaxError` of the
/// code that holds it).
pub fn syntax(src: &str, flags: &str) -> Result<(), String> {
    let mut seen = String::new();
    for c in flags.chars() {
        if !"dgimsuyv".contains(c) || seen.contains(c) {
            return Err(format!("invalid regular expression flag {c}"));
        }
        seen.push(c);
    }
    compile(src, flags).map(|_| ())
}

fn utf16_len_of(c: u32) -> usize {
    if c > 0xFFFF { 2 } else { 1 }
}

fn utf16_pos(s: &[u32], pos: usize) -> usize {
    s[..pos].iter().map(|&c| utf16_len_of(c)).sum()
}

fn char_pos(s: &[u32], unit: usize) -> usize {
    let mut u = 0;
    for (i, c) in s.iter().enumerate() {
        if u >= unit {
            return i;
        }
        u += utf16_len_of(*c);
    }
    s.len()
}

/// Keeps `caps` as the last match, for the legacy statics of `RegExp`.
fn remember(it: &mut Interp, s: &[u32], caps: &Caps, input: &Name) {
    let units = caps.iter().map(|c| c.map(|(a, b)| (utf16_pos(s, a), utf16_pos(s, b)))).collect();
    it.last_match = Some((input.clone(), units));
}

/// The legacy statics of `RegExp` (Annex B, measured on a `refrouter` 9.0.4 on 02-11): `$1`…`$9`,
/// `input` (`$_`), `lastMatch` (`$&`), `lastParen` (`$+`), `leftContext` (`` $` ``) and
/// `rightContext` (`$'`), from the last successful match of the call.
pub fn legacy_static(it: &mut Interp, _: &Val, _: &[Val]) -> R {
    let name = it.native.clone();
    let (input, caps) = it.last_match.clone().unwrap_or_default();
    let units = crate::strings::units(&input);
    let piece = |r: Option<(usize, usize)>| match r {
        Some((a, b)) if a <= b && b <= units.len() => crate::strings::from_units(&units[a..b]),
        _ => String::new(),
    };
    let whole = caps.first().copied().flatten();
    let text = match &*name {
        "input" | "$_" => return Ok(Val::Str(input)),
        // Before any match there is no `lastMatch` (measured: `undefined`).
        "lastMatch" | "$&" if whole.is_none() => return Ok(Val::Undef),
        "lastMatch" | "$&" => piece(whole),
        "lastParen" | "$+" => {
            piece(if caps.len() > 1 { caps.last().copied().flatten() } else { None })
        }
        "leftContext" | "$`" => piece(whole.map(|(a, _)| (0, a))),
        "rightContext" | "$'" => piece(whole.map(|(_, b)| (b, units.len()))),
        n => {
            let i: usize = n.trim_start_matches('$').parse().unwrap_or(0);
            piece(caps.get(i).copied().flatten())
        }
    };
    it.string(text)
}

/// Setting `RegExp.input` (or `$_`) changes the input the statics read.
pub fn legacy_input_setter(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let v = a.first().cloned().unwrap_or(Val::Undef);
    let text = it.str_of(&v)?;
    let caps = it.last_match.take().map(|(_, c)| c).unwrap_or_default();
    it.last_match = Some((text, caps));
    Ok(Val::Undef)
}

/// The result array of a match: the strings, `index`, `input` and `groups`.
fn result(it: &mut Interp, re: &Regex, s: &[u32], caps: &Caps, input: &Name) -> R {
    let mut items = Vec::new();
    for c in caps {
        items.push(match c {
            Some((a, b)) => Val::Str(text_of(&s[*a..*b]).into()),
            None => Val::Undef,
        });
    }
    let arr = it.array(items.clone())?;
    let Val::Obj(o) = &arr else { return Ok(arr) };
    let start = caps[0].map_or(0, |(a, _)| a);
    it.define(o, "index", Prop::data(Val::Num(utf16_pos(s, start) as f64)))?;
    it.define(o, "input", Prop::data(Val::Str(input.clone())))?;
    let groups = if re.names.is_empty() {
        Val::Undef
    } else {
        let g = it.object()?;
        for (n, i) in &re.names {
            it.define(&g, n, Prop::data(items[*i].clone()))?;
        }
        Val::Obj(g)
    };
    it.define(o, "groups", Prop::data(groups))?;
    if re.indices {
        // `[start, end]` of each capture in UTF-16 units, and the same by name.
        let mut pairs = Vec::new();
        for c in caps {
            pairs.push(match c {
                Some((a, b)) => {
                    let pair =
                        vec![Val::Num(utf16_pos(s, *a) as f64), Val::Num(utf16_pos(s, *b) as f64)];
                    it.array(pair)?
                }
                None => Val::Undef,
            });
        }
        let ind = it.array(pairs.clone())?;
        let named = if re.names.is_empty() {
            Val::Undef
        } else {
            let g = it.object()?;
            for (n, i) in &re.names {
                it.define(&g, n, Prop::data(pairs[*i].clone()))?;
            }
            Val::Obj(g)
        };
        if let Val::Obj(io) = &ind {
            it.define(io, "groups", Prop::data(named))?;
        }
        it.define(o, "indices", Prop::data(ind))?;
    }
    Ok(arr)
}

fn last_index(it: &mut Interp, re: &Val) -> R<usize> {
    let v = it.get(re, "lastIndex")?;
    let n = it.number_of(&v)?;
    Ok(if n.is_finite() && n > 0.0 { n as usize } else { 0 })
}

/// `RegExp.prototype.exec`.
pub fn exec(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let (src, flags) = parts(it, this)?;
    let input = it.str_of(&a.first().cloned().unwrap_or(Val::Undef))?;
    let re = compiled(it, &src, &flags)?;
    let s = elements(&input, is_unicode(&flags));
    let global = flags.contains('g') || flags.contains('y');
    let start = if global { char_pos(&s, last_index(it, this)?) } else { 0 };
    if start > s.len() {
        it.set(this, "lastIndex", Val::Num(0.0))?;
        return Ok(Val::Null);
    }
    match find(it, &re, &s, start, flags.contains('y'))? {
        Some(caps) => {
            if global {
                let end = caps[0].map_or(0, |(_, b)| b);
                it.set(this, "lastIndex", Val::Num(utf16_pos(&s, end) as f64))?;
            }
            remember(it, &s, &caps, &input);
            result(it, &re, &s, &caps, &input)
        }
        None => {
            if global {
                it.set(this, "lastIndex", Val::Num(0.0))?;
            }
            Ok(Val::Null)
        }
    }
}

/// `RegExp.prototype.test`.
pub fn test(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    // Measured on a `refrouter` 9.0.4 on 02-11: `number is not a non-null object`.
    if !matches!(this, Val::Obj(_)) {
        return it.type_error(&crate::builtins::non_object(this));
    }
    // Another object is matched by its own `exec` (ECMA-262 §22.2.7.1 RegExpExec).
    if !is_regexp(this) {
        let s = Val::Str(it.str_of(&a.first().cloned().unwrap_or(Val::Undef))?);
        return Ok(Val::Bool(!matches!(regexp_exec(it, this, &s)?, Val::Null)));
    }
    Ok(Val::Bool(!matches!(exec(it, this, a)?, Val::Null)))
}

fn is_regexp(v: &Val) -> bool {
    matches!(v, Val::Obj(o) if matches!(o.borrow().kind, Kind::RegExp(..)))
}

/// RegExpExec (ECMA-262 §22.2.7.1) on an object that is not a `RegExp`: its own `exec`, whose
/// result is an object or null.
fn regexp_exec(it: &mut Interp, rx: &Val, s: &Val) -> R {
    let f = it.get(rx, "exec")?;
    if !f.is_callable() {
        return exec(it, rx, std::slice::from_ref(s));
    }
    let r = it.call(&f, rx, std::slice::from_ref(s))?;
    match r {
        Val::Obj(_) | Val::Null => Ok(r),
        _ => it.type_error("RegExp exec method should return object or null"),
    }
}

/// The results of matching `rx`, an object that is not a `RegExp`, against `s`: one, or every
/// one with the `g` flag, moving `lastIndex` past an empty match (§22.2.6.8, §22.2.6.11).
fn generic_results(it: &mut Interp, rx: &Val, s: &Val, global: bool) -> R<Vec<Val>> {
    if global {
        it.set(rx, "lastIndex", Val::Num(0.0))?;
    }
    let mut out = Vec::new();
    loop {
        let r = regexp_exec(it, rx, s)?;
        if matches!(r, Val::Null) {
            return Ok(out);
        }
        out.push(r.clone());
        if !global {
            return Ok(out);
        }
        let m = it.get(&r, "0")?;
        if it.str_of(&m)?.is_empty() {
            let i = it.get(rx, "lastIndex")?;
            let i = to_length(it.number_of(&i)?);
            it.set(rx, "lastIndex", Val::Num(i + 1.0))?;
        }
    }
}

fn to_length(n: f64) -> f64 {
    if n.is_nan() { 0.0 } else { n.trunc().clamp(0.0, 9_007_199_254_740_991.0) }
}

fn has_flag(it: &mut Interp, rx: &Val, flag: char) -> R<bool> {
    let f = it.get(rx, "flags")?;
    Ok(it.str_of(&f)?.contains(flag))
}

/// `RegExp.prototype[Symbol.match]` on an object that is not a `RegExp`.
fn generic_match(it: &mut Interp, rx: &Val, s: &Val) -> R {
    if !has_flag(it, rx, 'g')? {
        return regexp_exec(it, rx, s);
    }
    let mut items = Vec::new();
    for r in generic_results(it, rx, s, true)? {
        let m = it.get(&r, "0")?;
        items.push(Val::Str(it.str_of(&m)?));
    }
    if items.is_empty() { Ok(Val::Null) } else { it.array(items) }
}

/// `RegExp.prototype[Symbol.search]` on an object that is not a `RegExp`.
fn generic_search(it: &mut Interp, rx: &Val, s: &Val) -> R {
    let before = it.get(rx, "lastIndex")?;
    if !crate::props::same_value(&before, &Val::Num(0.0)) {
        it.set(rx, "lastIndex", Val::Num(0.0))?;
    }
    let r = regexp_exec(it, rx, s)?;
    let now = it.get(rx, "lastIndex")?;
    if !crate::props::same_value(&now, &before) {
        it.set(rx, "lastIndex", before)?;
    }
    match r {
        Val::Null => Ok(Val::Num(-1.0)),
        r => it.get(&r, "index"),
    }
}

/// `RegExp.prototype[Symbol.replace]` on an object that is not a `RegExp` (§22.2.6.11).
fn generic_replace(it: &mut Interp, rx: &Val, s: &Name, replacement: &Val) -> R {
    let replacement = if replacement.is_callable() {
        replacement.clone()
    } else {
        Val::Str(it.str_of(replacement)?)
    };
    let global = has_flag(it, rx, 'g')?;
    let units = crate::strings::units(s);
    let mut out = String::new();
    let mut next = 0;
    for r in generic_results(it, rx, &Val::Str(s.clone()), global)? {
        let n = it.get(&r, "length")?;
        let n = to_length(it.number_of(&n)?) as usize;
        let m = it.get(&r, "0")?;
        let matched = it.str_of(&m)?;
        let p = it.get(&r, "index")?;
        let p = it.number_of(&p)?;
        let position = if p.is_nan() { 0 } else { (p.trunc().max(0.0) as usize).min(units.len()) };
        let mut groups = Vec::new();
        for i in 1..n.max(1) {
            let c = it.get(&r, &i.to_string())?;
            groups.push(if matches!(c, Val::Undef) { c } else { Val::Str(it.str_of(&c)?) });
        }
        let mut named = Vec::new();
        if let Val::Obj(g) = it.get(&r, "groups")? {
            for k in it.keys(&g, true)? {
                let v = it.get(&Val::Obj(g.clone()), &k)?;
                named.push((k.to_string(), v));
            }
        }
        let text = substitute(it, &replacement, &matched, position, s, &groups, &named)?;
        if position >= next {
            out.push_str(&crate::strings::from_units(&units[next..position]));
            out.push_str(&text);
            next = position + crate::strings::units(&matched).len();
        }
    }
    out.push_str(&crate::strings::from_units(&units[next.min(units.len())..]));
    it.string(out)
}

/// Every match of `re` in `s` from the start: their captures.
fn all(it: &mut Interp, re: &Regex, s: &[u32]) -> R<Vec<Caps>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos <= s.len() {
        let Some(caps) = find(it, re, s, pos, false)? else { break };
        let (a, b) = caps[0].unwrap_or((pos, pos));
        pos = if b == a { b + 1 } else { b };
        out.push(caps);
    }
    Ok(out)
}

/// `String.prototype.<sym>(x, …rest)` where `x` is an object other than a `RegExp` with a method
/// `Symbol.<sym>`: what that method gives for `this` and `rest` (ECMA-262 §22.1.3.13); a method
/// that is neither callable nor `undefined` or `null` is a `TypeError` (GetMethod).
pub(crate) fn delegated(
    it: &mut Interp,
    a: &[Val],
    sym: &str,
    this: &Val,
    rest: &[Val],
) -> R<Option<Val>> {
    let x = a.first().cloned().unwrap_or(Val::Undef);
    if !matches!(&x, Val::Obj(o) if !matches!(o.borrow().kind, Kind::RegExp(..))) {
        return Ok(None);
    }
    let f = it.get(&x, &crate::builtins::well_known(sym))?;
    if f.is_nullish() {
        return Ok(None);
    }
    if !f.is_callable() {
        return it.type_error(&format!("{} is not a function", f.type_of()));
    }
    let mut args = vec![this.clone()];
    args.extend_from_slice(rest);
    it.call(&f, &x, &args).map(Some)
}

fn as_regexp(it: &mut Interp, v: &Val) -> R<Val> {
    if matches!(v, Val::Obj(o) if matches!(o.borrow().kind, Kind::RegExp(..))) {
        return Ok(v.clone());
    }
    let src = if matches!(v, Val::Undef) { "(?:)".into() } else { it.str_of(v)? };
    crate::builtins::make_regexp(it, src, "".into())
}

/// `String.prototype.match`.
pub fn string_match(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let s = crate::strings::this_text(it, this)?;
    if let Some(v) = delegated(it, a, "match", this, &[])? {
        return Ok(v);
    }
    let rv = as_regexp(it, &a.first().cloned().unwrap_or(Val::Undef))?;
    let (src, flags) = parts(it, &rv)?;
    if !flags.contains('g') {
        return exec(it, &rv, &[Val::Str(s)]);
    }
    let re = compiled(it, &src, &flags)?;
    let chars = elements(&s, is_unicode(&flags));
    let found = all(it, &re, &chars)?;
    if found.is_empty() {
        return Ok(Val::Null);
    }
    if let Some(caps) = found.last() {
        remember(it, &chars, caps, &s);
    }
    let items = found
        .iter()
        .map(|c| {
            let (x, y) = c[0].unwrap_or((0, 0));
            Val::Str(text_of(&chars[x..y]).into())
        })
        .collect();
    it.array(items)
}

/// `String.prototype.search`.
pub fn search(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let s = crate::strings::this_text(it, this)?;
    if let Some(v) = delegated(it, a, "search", this, &[])? {
        return Ok(v);
    }
    let rv = as_regexp(it, &a.first().cloned().unwrap_or(Val::Undef))?;
    let (src, flags) = parts(it, &rv)?;
    let re = compiled(it, &src, &flags)?;
    let chars = elements(&s, is_unicode(&flags));
    Ok(Val::Num(match find(it, &re, &chars, 0, false)? {
        Some(c) => {
            remember(it, &chars, &c, &s);
            utf16_pos(&chars, c[0].map_or(0, |(x, _)| x)) as f64
        }
        None => -1.0,
    }))
}

/// `replace` and `replaceAll` with a regular expression.
pub fn replace(it: &mut Interp, s: &Name, rv: &Val, replacement: &Val, all_matches: bool) -> R {
    let (src, flags) = parts(it, rv)?;
    let re = compiled(it, &src, &flags)?;
    let chars = elements(s, is_unicode(&flags));
    let found = if flags.contains('g') || all_matches {
        all(it, &re, &chars)?
    } else {
        find(it, &re, &chars, 0, false)?.into_iter().collect()
    };
    if let Some(caps) = found.last() {
        remember(it, &chars, caps, s);
    }
    let mut out = String::new();
    let mut last = 0;
    for caps in found {
        let (x, y) = caps[0].unwrap_or((0, 0));
        out.push_str(&text_of(&chars[last..x]));
        let matched = text_of(&chars[x..y]);
        let groups: Vec<Val> = caps[1..]
            .iter()
            .map(|c| c.map_or(Val::Undef, |(p, q)| Val::Str(text_of(&chars[p..q]).into())))
            .collect();
        let named: Vec<(String, Val)> =
            re.names.iter().map(|(n, i)| (n.clone(), groups[*i - 1].clone())).collect();
        let position = utf16_pos(&chars, x);
        let helpers =
            if replacement.is_callable() { crate::recursion::REGEXP_REPLACE_FRAMES } else { 0 };
        let r = it.nested(helpers, 0, false, |it| {
            substitute(it, replacement, &matched, position, s, &groups, &named)
        })?;
        out.push_str(&r);
        last = y;
    }
    out.push_str(&text_of(&chars[last.min(chars.len())..]));
    if flags.contains('g') {
        it.set(rv, "lastIndex", Val::Num(0.0))?;
    }
    it.string(out)
}

/// `split` with a regular expression: the pieces between matches, with the captures.
pub fn split(it: &mut Interp, s: &Name, rv: &Val, limit: usize) -> R {
    let (src, flags) = parts(it, rv)?;
    let re = compiled(it, &src, &flags)?;
    let chars = elements(s, is_unicode(&flags));
    let mut out = Vec::new();
    if chars.is_empty() {
        if find(it, &re, &chars, 0, true)?.is_none() {
            out.push(Val::Str(s.clone()));
        }
        return it.array(out);
    }
    let (mut p, mut q) = (0, 0);
    while q < chars.len() && out.len() < limit {
        match find(it, &re, &chars, q, true)? {
            Some(caps) => {
                let e = caps[0].map_or(q, |(_, b)| b);
                if e == p {
                    q += 1;
                    continue;
                }
                remember(it, &chars, &caps, s);
                out.push(Val::Str(text_of(&chars[p..q]).into()));
                for c in &caps[1..] {
                    out.push(c.map_or(Val::Undef, |(x, y)| Val::Str(text_of(&chars[x..y]).into())));
                }
                p = e;
                q = e;
            }
            None => q += 1,
        }
    }
    if out.len() < limit {
        out.push(Val::Str(text_of(&chars[p..]).into()));
    }
    out.truncate(limit);
    it.array(out)
}

/// `String.prototype.matchAll`: a regular expression without `g` is an error; any other value
/// with its own `Symbol.matchAll` method decides; a string is a pattern with `g` (measured on a
/// `refrouter` 9.0.4 on 02-11). The matches are found when it is called.
pub fn match_all(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if this.is_nullish() {
        return it.type_error(&format!(
            "String.prototype.matchAll called on incompatible {}",
            it.show(this)
        ));
    }
    let arg = a.first().cloned().unwrap_or(Val::Undef);
    let is_regexp = matches!(&arg, Val::Obj(o) if matches!(o.borrow().kind, Kind::RegExp(..)));
    if !is_regexp && let Some(v) = delegated(it, a, "matchAll", this, &[])? {
        return Ok(v);
    }
    let s = it.str_of(this);
    let s = it.operand_named(s, "this")?;
    let (src, flags, start) = if is_regexp {
        let (src, flags) = parts(it, &arg)?;
        if !flags.contains('g') {
            return it.type_error("matchAll must be called with a global RegExp");
        }
        let start = last_index(it, &arg)?;
        (src, flags, start)
    } else {
        let src = if matches!(arg, Val::Undef) { "(?:)".into() } else { it.str_of(&arg)? };
        (src, "g".into(), 0)
    };
    let re = compiled(it, &src, &flags)?;
    let chars = elements(&s, is_unicode(&flags));
    let mut pos = char_pos(&chars, start);
    let mut items = Vec::new();
    while pos <= chars.len() {
        let Some(caps) = find(it, &re, &chars, pos, flags.contains('y'))? else { break };
        let (x, y) = caps[0].unwrap_or((pos, pos));
        pos = if x == y { y + 1 } else { y };
        remember(it, &chars, &caps, &s);
        items.push(result(it, &re, &chars, &caps, &s)?);
    }
    crate::builtins::make_iter(it, crate::builtins::REGEXP_STRING_ITERATOR, items)
}

/// `this` of the methods `RegExp.prototype[Symbol.match]` and the others: a regular expression
/// (measured on a `refrouter` 9.0.4 on 02-11: `number is not a non-null object`, `[Symbol.search]
/// method called on incompatible Object`).
fn symbol_this(it: &mut Interp, this: &Val) -> R<()> {
    if !matches!(this, Val::Obj(_)) {
        return it.type_error(&format!("{} is not a non-null object", this.type_of()));
    }
    parts(it, this).map(|_| ())
}

/// The string argument of a `RegExp.prototype[Symbol.*]` method.
fn symbol_str(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    symbol_this(it, this)?;
    Ok(Val::Str(it.str_of(&a.first().cloned().unwrap_or(Val::Undef))?))
}

/// The string argument when `this` is an object that is not a `RegExp`: the generic algorithm.
fn generic_str(it: &mut Interp, this: &Val, a: &[Val]) -> R<Option<Val>> {
    if !matches!(this, Val::Obj(_)) || is_regexp(this) {
        return Ok(None);
    }
    Ok(Some(Val::Str(it.str_of(&a.first().cloned().unwrap_or(Val::Undef))?)))
}

/// The rest of the arguments after the string, with the regular expression first.
fn with_this(this: &Val, a: &[Val]) -> Vec<Val> {
    std::iter::once(this.clone()).chain(a.iter().skip(1).cloned()).collect()
}

pub fn symbol_match(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if let Some(s) = generic_str(it, this, a)? {
        return generic_match(it, this, &s);
    }
    let s = symbol_str(it, this, a)?;
    string_match(it, &s, &with_this(this, a))
}

pub fn symbol_match_all(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let s = symbol_str(it, this, a)?;
    match_all(it, &s, &with_this(this, a))
}

pub fn symbol_replace(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if let Some(Val::Str(s)) = generic_str(it, this, a)? {
        return generic_replace(it, this, &s, &a.get(1).cloned().unwrap_or(Val::Undef));
    }
    let s = symbol_str(it, this, a)?;
    crate::strings::replace(it, &s, &with_this(this, a))
}

pub fn symbol_search(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if let Some(s) = generic_str(it, this, a)? {
        return generic_search(it, this, &s);
    }
    let s = symbol_str(it, this, a)?;
    search(it, &s, &with_this(this, a))
}

pub fn symbol_split(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let s = symbol_str(it, this, a)?;
    crate::strings::split(it, &s, &with_this(this, a))
}
