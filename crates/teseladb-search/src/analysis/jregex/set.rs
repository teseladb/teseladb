//! Java character classes written as PCRE2 classes. A class consumes one character; the PCRE2 text
//! it becomes does too. General categories, scripts, `\h` and `\v` keep their PCRE2 escapes; the
//! sets PCRE2 has no syntax for (blocks, binary properties, the `java.lang.Character` classes, the
//! POSIX classes with `UNICODE_CHARACTER_CLASS`) are written as ranges computed from the tables of
//! [`crate::analysis::props`]. Intersections become lookaheads and case-insensitive matching is
//! expanded to the characters Java treats as equal.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::{Arc, OnceLock};

use super::class::Prop;
use crate::analysis::props::{self, CATEGORIES};
use crate::analysis::unicode::lowercase;

/// Sorted ranges that neither overlap nor touch.
pub type Ranges = Vec<(char, char)>;

/// How a class compares case: exactly, in ASCII (`CASE_INSENSITIVE`) or in Unicode
/// (`CASE_INSENSITIVE` with `UNICODE_CASE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Case {
    /// Exact.
    Exact,
    /// ASCII letters only.
    Ascii,
    /// Simple Unicode case mappings.
    Unicode,
}

fn next(c: char) -> Option<char> {
    match c {
        '\u{D7FF}' => Some('\u{E000}'),
        _ => char::from_u32(u32::from(c) + 1),
    }
}

fn prev(c: char) -> Option<char> {
    match c {
        '\u{E000}' => Some('\u{D7FF}'),
        _ => u32::from(c).checked_sub(1).and_then(char::from_u32),
    }
}

/// Sorts and joins overlapping or adjacent ranges.
pub fn normalize(mut ranges: Ranges) -> Ranges {
    ranges.sort_unstable();
    let mut out: Ranges = Vec::with_capacity(ranges.len());
    for (lo, hi) in ranges {
        match out.last_mut() {
            Some(last) if next(last.1).is_none_or(|n| n >= lo) => last.1 = last.1.max(hi),
            _ => out.push((lo, hi)),
        }
    }
    out
}

/// The scalar values that are not in `ranges` (normalized).
pub fn complement(ranges: &[(char, char)]) -> Ranges {
    let mut out = Vec::new();
    let mut from = Some('\0');
    for &(lo, hi) in ranges {
        if let (Some(f), Some(p)) = (from, prev(lo))
            && f <= p
        {
            out.push((f, p));
        }
        from = next(hi);
    }
    if let Some(f) = from {
        out.push((f, char::MAX));
    }
    out
}

/// The characters of `p`, computed once per property and thread: each core keeps its own, so no
/// lock is shared between cores (012 FR-008).
pub fn prop_ranges(p: Prop) -> Arc<Ranges> {
    thread_local! {
        static CACHE: RefCell<HashMap<Prop, Arc<Ranges>>> = RefCell::new(HashMap::new());
    }
    if let Some(found) = CACHE.with(|m| m.borrow().get(&p).cloned()) {
        return found;
    }
    let mut out: Ranges = Vec::new();
    for c in ('\0'..=char::MAX).filter(|&c| p.contains(c)) {
        match out.last_mut() {
            Some(last) if next(last.1) == Some(c) => last.1 = c,
            _ => out.push((c, c)),
        }
    }
    let out = Arc::new(out);
    CACHE.with(|m| m.borrow_mut().insert(p, Arc::clone(&out)));
    out
}

/// The characters whose simple case mappings change them, with their variants.
fn cased() -> &'static [(char, [char; 3])] {
    static CASED: OnceLock<Vec<(char, [char; 3])>> = OnceLock::new();
    CASED.get_or_init(|| {
        ('\0'..=char::MAX)
            .filter_map(|c| {
                let upper = props::uppercase(c);
                let v = [upper, lowercase(c), lowercase(upper)];
                v.iter().any(|&x| x != c).then_some((c, v))
            })
            .collect()
    })
}

fn contains(ranges: &[(char, char)], c: char) -> bool {
    let i = ranges.partition_point(|r| r.1 < c);
    ranges.get(i).is_some_and(|r| r.0 <= c)
}

/// Adds to `ranges` the characters one of whose case variants is in it: under [`Case::Ascii`] the
/// other case of ASCII letters, under [`Case::Unicode`] every character `c` with `c`, its
/// uppercase, its lowercase or the lowercase of its uppercase in the set.
pub fn fold(ranges: Ranges, case: Case) -> Ranges {
    let ranges = normalize(ranges);
    let extra: Vec<char> = match case {
        Case::Exact => return ranges,
        Case::Ascii => ('A'..='Z')
            .chain('a'..='z')
            .filter(|&c| {
                contains(&ranges, c.to_ascii_lowercase())
                    || contains(&ranges, c.to_ascii_uppercase())
            })
            .collect(),
        Case::Unicode => cased()
            .iter()
            .filter(|(_, v)| v.iter().any(|&x| contains(&ranges, x)))
            .map(|&(c, _)| c)
            .chain(ranges.iter().flat_map(|&(lo, hi)| {
                cased().iter().filter(move |(c, _)| (lo..=hi).contains(c)).flat_map(|(_, v)| *v)
            }))
            .collect(),
    };
    normalize(ranges.into_iter().chain(extra.into_iter().map(|c| (c, c))).collect())
}

/// The characters Java treats as equal to `c` under `case`.
pub fn literal(c: char, case: Case) -> Ranges {
    match case {
        Case::Exact => vec![(c, c)],
        Case::Ascii => normalize(vec![
            (c, c),
            (c.to_ascii_lowercase(), c.to_ascii_lowercase()),
            (c.to_ascii_uppercase(), c.to_ascii_uppercase()),
        ]),
        Case::Unicode => {
            let key = |x: char| lowercase(props::uppercase(x));
            let (upper, k) = (props::uppercase(c), key(c));
            let same = cased()
                .iter()
                .map(|&(d, _)| d)
                .filter(|&d| props::uppercase(d) == upper || key(d) == k);
            normalize(same.chain([c]).map(|d| (d, d)).collect())
        }
    }
}

/// A part of a class union.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    /// Explicit characters, already folded for case.
    Ranges(Ranges),
    /// A property, negated when the flag is set.
    Prop(Prop, bool),
    /// A nested class.
    Class(ClassExpr),
}

/// A Java class: the union of its parts, intersected with each of `and`, then negated.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassExpr {
    /// The union.
    pub parts: Vec<Part>,
    /// The classes the union is intersected with (`&&`).
    pub and: Vec<ClassExpr>,
    /// `[^…]`: Java 9 and later negate the whole class, unions and intersections included.
    pub negated: bool,
}

/// The PCRE2 escape of `p` when PCRE2 has one, usable inside a class.
fn escape(p: Prop, negated: bool) -> Option<String> {
    let (lower, upper) = if negated { ('P', 'p') } else { ('p', 'P') };
    Some(match p {
        Prop::Category(i) => format!("\\{lower}{{{}}}", CATEGORIES[i]),
        Prop::Major(m) => format!("\\{lower}{{{m}}}"),
        Prop::CasedLetter => format!("\\{lower}{{L&}}"),
        Prop::Assigned => format!("\\{upper}{{Cn}}"),
        Prop::Script(s) => {
            let long = props::script_names().get(s)?.split(';').next()?.to_string();
            format!("\\{lower}{{sc:{long}}}")
        }
        Prop::Horizontal => (if negated { "\\H" } else { "\\h" }).to_string(),
        Prop::Vertical => (if negated { "\\V" } else { "\\v" }).to_string(),
        _ => return None,
    })
}

fn push_range(out: &mut String, lo: char, hi: char) {
    let _ = write!(out, "\\x{{{:X}}}", u32::from(lo));
    if hi != lo {
        let _ = write!(out, "-\\x{{{:X}}}", u32::from(hi));
    }
}

/// Text of a PCRE2 class holding `ranges`, or a pattern that never matches when it is empty.
pub fn ranges_text(ranges: &[(char, char)]) -> String {
    if ranges.is_empty() {
        return "(?!)".to_string();
    }
    let mut out = "[".to_string();
    ranges.iter().for_each(|&(lo, hi)| push_range(&mut out, lo, hi));
    out.push(']');
    out
}

/// Any character.
pub const ANY: &str = "[\\x{0}-\\x{10FFFF}]";

impl ClassExpr {
    /// Escapes and ranges of a union that fits in one PCRE2 class, and the nested classes that do
    /// not.
    fn flatten(&self, escapes: &mut Vec<String>, ranges: &mut Ranges, others: &mut Vec<String>) {
        for part in &self.parts {
            match part {
                Part::Ranges(r) => ranges.extend(r.iter().copied()),
                Part::Prop(p, negated) => match escape(*p, *negated) {
                    Some(e) => escapes.push(e),
                    None if *negated => ranges.extend(complement(&prop_ranges(*p))),
                    None => ranges.extend(prop_ranges(*p).iter().copied()),
                },
                Part::Class(c) if c.and.is_empty() && !c.negated => {
                    c.flatten(escapes, ranges, others)
                }
                Part::Class(c) => others.push(c.text()),
            }
        }
    }

    /// The PCRE2 text matching one character of the class.
    pub fn text(&self) -> String {
        let (mut escapes, mut ranges, mut others) = (Vec::new(), Vec::new(), Vec::new());
        self.flatten(&mut escapes, &mut ranges, &mut others);
        let ranges = normalize(ranges);
        let simple = others.is_empty() && self.and.is_empty();
        if simple && (!escapes.is_empty() || !ranges.is_empty()) {
            let mut out = if self.negated { "[^" } else { "[" }.to_string();
            escapes.iter().for_each(|e| out.push_str(e));
            ranges.iter().for_each(|&(lo, hi)| push_range(&mut out, lo, hi));
            out.push(']');
            return out;
        }
        let mut union: Vec<String> = others;
        if !escapes.is_empty() || !ranges.is_empty() {
            let mut class = "[".to_string();
            escapes.iter().for_each(|e| class.push_str(e));
            ranges.iter().for_each(|&(lo, hi)| push_range(&mut class, lo, hi));
            class.push(']');
            union.insert(0, class);
        }
        let union = match union.len() {
            0 => "(?!)".to_string(),
            1 => union.remove(0),
            _ => format!("(?:{})", union.join("|")),
        };
        let lookaheads: String = self.and.iter().map(|a| format!("(?={})", a.text())).collect();
        let positive =
            if lookaheads.is_empty() { union } else { format!("(?:{lookaheads}{union})") };
        if self.negated { format!("(?:(?!{positive}){ANY})") } else { positive }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::analysis::jregex::class::Posix;

    #[test]
    fn range_algebra() {
        assert_eq!(normalize(vec![('c', 'd'), ('a', 'b'), ('x', 'x')]), [('a', 'd'), ('x', 'x')]);
        assert_eq!(complement(&[]), [('\0', char::MAX)]);
        assert_eq!(complement(&[('\0', 'a')]), [('b', char::MAX)]);
        assert_eq!(
            complement(&[('b', 'c'), ('\u{E000}', char::MAX)]),
            [('\0', 'a'), ('d', '\u{D7FF}')]
        );
        assert_eq!(
            normalize(vec![('\u{D7FF}', '\u{D7FF}'), ('\u{E000}', '\u{E000}')]),
            [('\u{D7FF}', '\u{E000}')]
        );
        assert_eq!(*prop_ranges(Prop::Ascii), [('\0', '\x7F')]);
        assert_eq!(*prop_ranges(Prop::Ascii), [('\0', '\x7F')]);
    }

    #[test]
    fn case_folding() {
        assert_eq!(fold(vec![('a', 'b')], Case::Ascii), [('A', 'B'), ('a', 'b')]);
        assert_eq!(fold(vec![('a', 'a')], Case::Exact), [('a', 'a')]);
        let k = fold(vec![('k', 'k')], Case::Unicode);
        assert!(contains(&k, 'K') && contains(&k, '\u{212A}'));
        assert_eq!(literal('é', Case::Ascii), [('é', 'é')]);
        assert_eq!(literal('q', Case::Ascii), [('Q', 'Q'), ('q', 'q')]);
        let sigma = literal('σ', Case::Unicode);
        assert!(contains(&sigma, 'Σ') && contains(&sigma, 'ς'));
        assert_eq!(literal('1', Case::Unicode), [('1', '1')]);
    }

    #[test]
    fn class_text() {
        let digits =
            ClassExpr { parts: vec![Part::Ranges(vec![('0', '9')])], ..ClassExpr::default() };
        assert_eq!(digits.text(), "[\\x{30}-\\x{39}]");
        let not = ClassExpr { negated: true, ..digits.clone() };
        assert_eq!(not.text(), "[^\\x{30}-\\x{39}]");
        let letters = ClassExpr {
            parts: vec![Part::Prop(Prop::Major('L'), false), Part::Prop(Prop::Category(9), true)],
            and: vec![not.clone()],
            negated: true,
        };
        assert_eq!(
            letters.text(),
            "(?:(?!(?:(?=[^\\x{30}-\\x{39}])[\\p{L}\\P{Nd}]))[\\x{0}-\\x{10FFFF}])"
        );
        let nested = ClassExpr {
            parts: vec![Part::Class(not), Part::Class(digits)],
            ..ClassExpr::default()
        };
        assert_eq!(nested.text(), "(?:[\\x{30}-\\x{39}]|[^\\x{30}-\\x{39}])");
        assert_eq!(ClassExpr::default().text(), "(?!)");
        assert_eq!(ranges_text(&[]), "(?!)");
        assert_eq!(ranges_text(&[('a', 'a')]), "[\\x{61}]");
        let posix = ClassExpr {
            parts: vec![Part::Prop(Prop::Posix(Posix::Digit, false), true)],
            ..ClassExpr::default()
        };
        assert!(posix.text().starts_with("[\\x{0}-\\x{2F}\\x{3A}-"));
        for p in
            [Prop::Assigned, Prop::CasedLetter, Prop::Horizontal, Prop::Vertical, Prop::Script(0)]
        {
            assert!(escape(p, false).is_some() && escape(p, true).is_some());
        }
        assert_eq!(escape(Prop::Script(usize::MAX), false), None);
    }
}
