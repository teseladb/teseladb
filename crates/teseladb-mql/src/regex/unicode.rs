//! Unicode properties of a code point (general category, script, script extensions, grapheme
//! cluster break) from the generated tables, and the simple case folding of PCRE2's caseless
//! matching.

use std::collections::HashMap;
use std::sync::OnceLock;

use super::unicode_data::{
    BIDI_CLASSES, BIDI_RANGES, BINARY, BINARY_RANGES, CATEGORIES, CATEGORY_RANGES,
    GRAPHEME_BREAK_RANGES, PICTOGRAPHIC_RANGES, SCRIPT_EXTENSION_RANGES, SCRIPT_RANGES,
    SCRIPT_SETS, SCRIPTS,
};

/// The value of `cp` in a table of sorted ranges.
fn lookup(table: &[(u32, u32, u8)], cp: u32) -> Option<u8> {
    let i = table.partition_point(|&(_, last, _)| last < cp);
    table.get(i).filter(|&&(first, _, _)| first <= cp).map(|&(_, _, v)| v)
}

/// Index of the general category `Cn` (unassigned).
const UNASSIGNED: u8 = 2;

/// The general category of `c` as an index of [`CATEGORIES`].
pub(super) fn category(c: char) -> u8 {
    lookup(CATEGORY_RANGES, u32::from(c)).unwrap_or(UNASSIGNED)
}

/// The bit mask of the general categories whose name, matched loosely, is `name` (`L`, `Lu`,
/// `L&`, `LC`...).
pub(super) fn category_mask(name: &str) -> Option<u32> {
    let name = loose(name);
    if name == "l&" || name == "lc" {
        return Some(["Lu", "Ll", "Lt"].iter().map(|n| category_bit(n)).sum());
    }
    let mut mask = 0u32;
    for (i, cat) in CATEGORIES.iter().enumerate() {
        let cat = cat.to_ascii_lowercase();
        let hit = match name.len() {
            1 => cat.as_bytes()[0] == name.as_bytes()[0],
            _ => cat == name,
        };
        if hit {
            mask |= 1 << i;
        }
    }
    (mask != 0).then_some(mask)
}

/// The index of a binary property by its name or alias, matched loosely.
pub(super) fn binary_index(name: &str) -> Option<u8> {
    let wanted = loose(name);
    let found = BINARY.iter().position(|(long, short)| {
        loose(long) == wanted || (!short.is_empty() && loose(short) == wanted)
    });
    // `space` is a second alias of `White_Space`.
    let found = found.or_else(|| {
        (wanted == "space").then(|| BINARY.iter().position(|(long, _)| *long == "White_Space"))?
    });
    found.and_then(|i| u8::try_from(i).ok())
}

/// Whether `c` has the binary property `index`.
pub(super) fn has_binary(c: char, index: u8) -> bool {
    let pairs = BINARY_RANGES[usize::from(index)];
    let cp = u32::from(c);
    let (mut lo, mut hi) = (0, pairs.len() / 2);
    while lo < hi {
        let mid = (lo + hi) / 2;
        if pairs[2 * mid + 1] < cp {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    pairs.get(2 * lo).is_some_and(|&first| first <= cp)
}

/// The index of a bidi class by its short name, ignoring case.
pub(super) fn bidi_index(name: &str) -> Option<u8> {
    let wanted = loose(name);
    BIDI_CLASSES
        .iter()
        .position(|c| c.to_ascii_lowercase() == wanted)
        .and_then(|i| u8::try_from(i).ok())
}

/// The bidi class of `c`.
pub(super) fn bidi_class(c: char) -> u8 {
    lookup(BIDI_RANGES, u32::from(c)).unwrap_or(0)
}

/// The mask bit of one general category.
pub(super) fn category_bit(name: &str) -> u32 {
    CATEGORIES.iter().position(|c| *c == name).map_or(0, |i| 1 << i)
}

/// Whether `c` belongs to one of the general categories of `mask`.
pub(super) fn in_categories(c: char, mask: u32) -> bool {
    mask & (1 << category(c)) != 0
}

/// Index of the script `Unknown`.
fn unknown_script() -> u8 {
    script_index("Unknown").unwrap_or(0)
}

/// The script of `c`.
pub(super) fn script(c: char) -> u8 {
    lookup(SCRIPT_RANGES, u32::from(c)).unwrap_or_else(unknown_script)
}

/// Whether `script` is among the script extensions of `c`.
pub(super) fn has_script_extension(c: char, script_idx: u8) -> bool {
    match lookup(SCRIPT_EXTENSION_RANGES, u32::from(c)) {
        Some(set) => script(c) == script_idx || SCRIPT_SETS[usize::from(set)].contains(&script_idx),
        None => script(c) == script_idx,
    }
}

/// Loose matching of property names (UAX #44 LM3): case, spaces, `-` and `_` are ignored.
pub(super) fn loose(name: &str) -> String {
    name.chars().filter(|c| !matches!(c, ' ' | '-' | '_')).flat_map(char::to_lowercase).collect()
}

/// The index of a script by its long name or short alias, matched loosely.
pub(super) fn script_index(name: &str) -> Option<u8> {
    let wanted = loose(name);
    SCRIPTS
        .iter()
        .position(|(long, short)| loose(long) == wanted || loose(short) == wanted)
        .and_then(|i| u8::try_from(i).ok())
}

/// Grapheme cluster break values of [`GRAPHEME_BREAK_RANGES`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Gcb {
    Other,
    Cr,
    Lf,
    Control,
    Extend,
    Zwj,
    RegionalIndicator,
    Prepend,
    SpacingMark,
    L,
    V,
    T,
    Lv,
    Lvt,
}

/// The grapheme cluster break of `c`.
pub(super) fn grapheme_break(c: char) -> Gcb {
    const ALL: [Gcb; 14] = [
        Gcb::Other,
        Gcb::Cr,
        Gcb::Lf,
        Gcb::Control,
        Gcb::Extend,
        Gcb::Zwj,
        Gcb::RegionalIndicator,
        Gcb::Prepend,
        Gcb::SpacingMark,
        Gcb::L,
        Gcb::V,
        Gcb::T,
        Gcb::Lv,
        Gcb::Lvt,
    ];
    lookup(GRAPHEME_BREAK_RANGES, u32::from(c)).map_or(Gcb::Other, |v| ALL[usize::from(v)])
}

/// Whether `c` is extended pictographic.
pub(super) fn pictographic(c: char) -> bool {
    lookup(PICTOGRAPHIC_RANGES, u32::from(c)).is_some()
}

/// The only character of `chars`, if there is exactly one.
fn single(mut chars: impl Iterator<Item = char>) -> Option<char> {
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

/// The simple case fold of `c`: the lowercase of its uppercase when both are one character.
pub(super) fn fold(c: char) -> char {
    let upper = single(c.to_uppercase()).unwrap_or(c);
    single(upper.to_lowercase()).or_else(|| single(c.to_lowercase())).unwrap_or(c)
}

/// The characters whose fold is another character, grouped by that fold.
fn fold_sets() -> &'static HashMap<char, Vec<char>> {
    static SETS: OnceLock<HashMap<char, Vec<char>>> = OnceLock::new();
    SETS.get_or_init(|| {
        let mut sets: HashMap<char, Vec<char>> = HashMap::new();
        for c in (0..=0x10FFFF).filter_map(char::from_u32) {
            let f = fold(c);
            if f != c {
                sets.entry(f).or_default().push(c);
            }
        }
        sets
    })
}

/// Every character that matches `c` caselessly, `c` included.
pub(super) fn case_variants(c: char) -> Vec<char> {
    let f = fold(c);
    let mut out = fold_sets().get(&f).cloned().unwrap_or_default();
    out.push(c);
    out.push(f);
    out.sort_unstable();
    out.dedup();
    out
}

/// The scripts of `c`'s script extensions.
fn script_extensions(c: char) -> Vec<u8> {
    match lookup(SCRIPT_EXTENSION_RANGES, u32::from(c)) {
        Some(set) => SCRIPT_SETS[usize::from(set)].to_vec(),
        None => vec![script(c)],
    }
}

/// The zero of the set of ten decimal digits `c` belongs to: the general category tables keep
/// each run of `Nd` digits in one range, and each run is made of whole sets of ten.
fn digit_zero(c: char) -> u32 {
    let cp = u32::from(c);
    let i = CATEGORY_RANGES.partition_point(|&(_, last, _)| last < cp);
    let first = CATEGORY_RANGES.get(i).map_or(cp, |&(first, _, _)| first);
    first + (cp - first) / 10 * 10
}

/// Virtual scripts of Unicode TS #39 for the mixtures of Han: Japanese, Korean and Han with
/// Bopomofo, numbered after the real scripts.
const JAPANESE: u16 = 0x100;
const KOREAN: u16 = 0x101;
const HAN_BOPOMOFO: u16 = 0x102;

/// Whether `s` is a script run as PCRE2 defines it (`pcre2unicode`, «Script runs»): fewer than two
/// characters, or every character whose script extensions are not just `Common` or `Inherited`
/// shares a script with the others, Han mixing with Hiragana and Katakana, with Hangul or with
/// Bopomofo; and all the decimal digits come from one set of ten.
pub(super) fn is_script_run(s: &str) -> bool {
    if s.chars().nth(1).is_none() {
        return true;
    }
    let id = |name: &str| script_index(name).map_or(u16::MAX, u16::from);
    let (common, inherited, unknown) = (id("Common"), id("Inherited"), id("Unknown"));
    let (han, hiragana, katakana, hangul, bopomofo) =
        (id("Han"), id("Hiragana"), id("Katakana"), id("Hangul"), id("Bopomofo"));
    let mut shared: Option<Vec<u16>> = None;
    let mut zero = None;
    for c in s.chars() {
        if in_categories(c, category_bit("Nd")) {
            let z = digit_zero(c);
            if zero.is_some_and(|zero| zero != z) {
                return false;
            }
            zero = Some(z);
        }
        let mut scripts: Vec<u16> = script_extensions(c).into_iter().map(u16::from).collect();
        if scripts == [common] || scripts == [inherited] {
            continue;
        }
        if scripts == [unknown] {
            return false;
        }
        for s in scripts.clone() {
            if s == han {
                scripts.extend([JAPANESE, KOREAN, HAN_BOPOMOFO]);
            } else if s == hiragana || s == katakana {
                scripts.push(JAPANESE);
            } else if s == hangul {
                scripts.push(KOREAN);
            } else if s == bopomofo {
                scripts.push(HAN_BOPOMOFO);
            }
        }
        let next = match shared {
            None => scripts,
            Some(prev) => prev.into_iter().filter(|s| scripts.contains(s)).collect(),
        };
        if next.is_empty() {
            return false;
        }
        shared = Some(next);
    }
    true
}
