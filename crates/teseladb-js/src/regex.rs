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
}

/// A compiled pattern.
struct Regex {
    node: Node,
    groups: usize,
    names: Vec<(String, usize)>,
    icase: bool,
    multiline: bool,
    dotall: bool,
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
    let mut p = Parser { s: &chars, unicode, i: 0, groups: 0, names: Vec::new() };
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
                            return Err("numbers out of order in {} quantifier".into());
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
                        self.names.push((n, self.groups));
                    }
                    Some(self.groups)
                } else {
                    None
                };
                let inner = self.alt()?;
                if self.peek() != Some(')') {
                    return Err("missing ) in regular expression".into());
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
            'k' if self.peek() == Some('<') => {
                self.i += 1;
                let start = self.i;
                while self.peek().is_some_and(|c| c != '>') {
                    self.i += 1;
                }
                let name = self.text_from(start);
                self.i += 1;
                let idx = self.names.iter().find(|(n, _)| *n == name).map(|(_, i)| *i);
                Node::BackRef(idx.ok_or("invalid named reference")?)
            }
            '1'..='9' if !in_class => {
                self.i -= 1;
                Node::BackRef(self.number().unwrap_or(0))
            }
            'x' | 'u' => {
                let n = if c == 'x' { 2 } else { 4 };
                if c == 'u' && self.peek() == Some('{') {
                    self.i += 1;
                    let start = self.i;
                    while self.peek().is_some_and(|c| c != '}') {
                        self.i += 1;
                    }
                    let hex = self.text_from(start);
                    self.i += 1;
                    let v = u32::from_str_radix(&hex, 16).map_err(|_| "invalid Unicode escape")?;
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
            'c' => match self.peek() {
                Some(l) if l.is_ascii_alphabetic() => {
                    self.i += 1;
                    Node::Char(l as u32 % 32)
                }
                _ => Node::Char(u32::from(b'\\')),
            },
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
            let Some(c) = self.peek() else { return Err("missing ] after character class".into()) };
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

fn fold(c: u32, icase: bool) -> u32 {
    match char::from_u32(c) {
        Some(ch) if icase => ch.to_lowercase().next().map_or(c, u32::from),
        _ => c,
    }
}

fn upper(c: u32) -> u32 {
    char::from_u32(c).and_then(|ch| ch.to_uppercase().next()).map_or(c, u32::from)
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
                (*lo..=*hi).contains(&c)
                    || (self.re.icase
                        && ((*lo..=*hi).contains(&fold(c, true))
                            || (*lo..=*hi).contains(&upper(c))))
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
                    // Lookbehind: some start before `pos` whose match ends at `pos`.
                    let found = (0..=pos).rev().any(|st| {
                        let mut c2 = caps.clone();
                        self.node(inner, st, &mut c2, &mut |_, p, _| p == pos)
                    });
                    return found != *neg && k(self, pos, caps);
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
    match v {
        Val::Obj(o) => match &o.borrow().kind {
            Kind::RegExp(s, f) => Ok((s.clone(), f.clone())),
            _ => Err(crate::builtins::incompatible(it, "RegExp.prototype method")),
        },
        _ => Err(crate::builtins::incompatible(it, "RegExp.prototype method")),
    }
}

fn compiled(it: &Interp, src: &str, flags: &str) -> R<Regex> {
    compile(src, flags)
        .map_err(|e| it.error("SyntaxError", &format!("invalid regular expression: {e}")))
}

/// Checks a pattern when a literal or `new RegExp` makes it.
pub fn check(it: &mut Interp, src: &str, flags: &str) -> R<()> {
    let mut seen = String::new();
    for c in flags.chars() {
        if !"dgimsuyv".contains(c) || seen.contains(c) {
            return Err(it.error("SyntaxError", &format!("invalid regular expression flag {c}")));
        }
        seen.push(c);
    }
    compiled(it, src, flags).map(|_| ())
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
    Ok(Val::Bool(!matches!(exec(it, this, a)?, Val::Null)))
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

fn as_regexp(it: &mut Interp, v: &Val) -> R<Val> {
    if matches!(v, Val::Obj(o) if matches!(o.borrow().kind, Kind::RegExp(..))) {
        return Ok(v.clone());
    }
    let src = if matches!(v, Val::Undef) { "(?:)".into() } else { it.str_of(v)? };
    crate::builtins::make_regexp(it, src, "".into())
}

/// `String.prototype.match`.
pub fn string_match(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let s = crate::strings::this_str(it, this)?;
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
    let s = crate::strings::this_str(it, this)?;
    let rv = as_regexp(it, &a.first().cloned().unwrap_or(Val::Undef))?;
    let (src, flags) = parts(it, &rv)?;
    let re = compiled(it, &src, &flags)?;
    let chars = elements(&s, is_unicode(&flags));
    Ok(Val::Num(match find(it, &re, &chars, 0, false)? {
        Some(c) => utf16_pos(&chars, c[0].map_or(0, |(x, _)| x)) as f64,
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
        let r = substitute(it, replacement, &matched, utf16_pos(&chars, x), s, &groups, &named)?;
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
