//! Unicode character properties the analyzers need (FR-019). From the Unicode 12.1.0 database that
//! `scripts/unicode-tokenizer.sha256` and `scripts/unicode-emoji-tokenizer.sha256` pin, the version
//! the reference search server 1.70.6 tokenizes by: word-break classes (UAX #29), Extended_Pictographic, Script=Han,
//! Script=Hiragana, Script=Hangul and Line_Break=SA. From the Unicode 15.1.0 database that
//! `scripts/unicode-search.sha256` pins: letters and numbers, combining classes, decompositions,
//! canonical compositions, NFKC_Casefold and simple lowercase mappings.
//!
//! The tables live in `data/unicode.bin`, embedded in the binary. `tests/unicode_data.rs` rebuilds
//! them from the pinned files and fails if they differ; `TESELADB_WRITE_TABLES=1` writes them.
//!
//! Layout: the magic `TSLUNI01`, then seven sections, each a `u32` count followed by that many
//! records of little-endian `u32`s: word-break ranges `(first, last, class)`, combining-class
//! ranges `(first, last, ccc)`, decompositions `(code point, compatibility, length, offset)`,
//! compositions `(first, second, composed)`, NFKC_Casefold `(code point, length, offset)`,
//! lowercase `(code point, lowercase)` and the pool of code points the offsets point into.

use std::sync::OnceLock;

/// The word-break classes of UAX #29.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordBreak {
    /// No class (`Other`).
    Other,
    /// `CR`.
    Cr,
    /// `LF`.
    Lf,
    /// `Newline`.
    Newline,
    /// `Extend`.
    Extend,
    /// `ZWJ`.
    Zwj,
    /// `Regional_Indicator`.
    RegionalIndicator,
    /// `Format`.
    Format,
    /// `Katakana`.
    Katakana,
    /// `Hebrew_Letter`.
    HebrewLetter,
    /// `ALetter`.
    ALetter,
    /// `Single_Quote`.
    SingleQuote,
    /// `Double_Quote`.
    DoubleQuote,
    /// `MidNumLet`.
    MidNumLet,
    /// `MidLetter`.
    MidLetter,
    /// `MidNum`.
    MidNum,
    /// `Numeric`.
    Numeric,
    /// `ExtendNumLet`.
    ExtendNumLet,
    /// `WSegSpace`.
    WSegSpace,
}

impl WordBreak {
    /// Every class, in the order of their codes in the tables.
    pub const ALL: [WordBreak; 19] = [
        WordBreak::Other,
        WordBreak::Cr,
        WordBreak::Lf,
        WordBreak::Newline,
        WordBreak::Extend,
        WordBreak::Zwj,
        WordBreak::RegionalIndicator,
        WordBreak::Format,
        WordBreak::Katakana,
        WordBreak::HebrewLetter,
        WordBreak::ALetter,
        WordBreak::SingleQuote,
        WordBreak::DoubleQuote,
        WordBreak::MidNumLet,
        WordBreak::MidLetter,
        WordBreak::MidNum,
        WordBreak::Numeric,
        WordBreak::ExtendNumLet,
        WordBreak::WSegSpace,
    ];

    /// The name the character database uses.
    pub fn ucd_name(self) -> &'static str {
        match self {
            WordBreak::Other => "Other",
            WordBreak::Cr => "CR",
            WordBreak::Lf => "LF",
            WordBreak::Newline => "Newline",
            WordBreak::Extend => "Extend",
            WordBreak::Zwj => "ZWJ",
            WordBreak::RegionalIndicator => "Regional_Indicator",
            WordBreak::Format => "Format",
            WordBreak::Katakana => "Katakana",
            WordBreak::HebrewLetter => "Hebrew_Letter",
            WordBreak::ALetter => "ALetter",
            WordBreak::SingleQuote => "Single_Quote",
            WordBreak::DoubleQuote => "Double_Quote",
            WordBreak::MidNumLet => "MidNumLet",
            WordBreak::MidLetter => "MidLetter",
            WordBreak::MidNum => "MidNum",
            WordBreak::Numeric => "Numeric",
            WordBreak::ExtendNumLet => "ExtendNumLet",
            WordBreak::WSegSpace => "WSegSpace",
        }
    }
}

/// Bit of a word-break record: the character is Extended_Pictographic.
const PICTOGRAPHIC: u32 = 0x80;
/// Bit of a word-break record: the character is a letter or a number (general category L or N).
const ALPHANUMERIC: u32 = 0x40;
/// Bits of a word-break record: Script=Han, Script=Hiragana, Script=Hangul, Line_Break=SA.
const HAN: u32 = 0x100;
const HIRAGANA: u32 = 0x200;
const HANGUL: u32 = 0x400;
const COMPLEX_CONTEXT: u32 = 0x800;
/// Mask of the class in a word-break record.
const CLASS: u32 = 0x3F;

const MAGIC: &[u8; 8] = b"TSLUNI01";
const DATA: &[u8] = include_bytes!("../../data/unicode.bin");

/// The parsed tables.
#[derive(Debug, PartialEq, Eq)]
struct Tables {
    word_break: Vec<[u32; 3]>,
    ccc: Vec<[u32; 3]>,
    decompositions: Vec<[u32; 4]>,
    compositions: Vec<[u32; 3]>,
    casefold: Vec<[u32; 3]>,
    lowercase: Vec<[u32; 2]>,
    pool: Vec<u32>,
}

fn section<const N: usize>(bytes: &[u8], at: &mut usize) -> Option<Vec<[u32; N]>> {
    let word = |at: usize| -> Option<u32> {
        Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
    };
    let count = usize::try_from(word(*at)?).ok()?;
    *at += 4;
    let mut out = Vec::with_capacity(count.min(bytes.len() / (4 * N)));
    for _ in 0..count {
        let mut record = [0_u32; N];
        for slot in &mut record {
            *slot = word(*at)?;
            *at += 4;
        }
        out.push(record);
    }
    Some(out)
}

fn parse(bytes: &[u8]) -> Option<Tables> {
    if bytes.get(..8)? != MAGIC {
        return None;
    }
    let mut at = 8;
    let tables = Tables {
        word_break: section(bytes, &mut at)?,
        ccc: section(bytes, &mut at)?,
        decompositions: section(bytes, &mut at)?,
        compositions: section(bytes, &mut at)?,
        casefold: section(bytes, &mut at)?,
        lowercase: section(bytes, &mut at)?,
        pool: section::<1>(bytes, &mut at)?.into_iter().map(|[c]| c).collect(),
    };
    (at == bytes.len()).then_some(tables)
}

fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| parse(DATA).expect("data/unicode.bin is checked by its test"))
}

/// The value of the range of `ranges` that holds `c`, or 0.
fn in_ranges(ranges: &[[u32; 3]], c: char) -> u32 {
    let c = u32::from(c);
    let i = ranges.partition_point(|r| r[1] < c);
    ranges.get(i).filter(|r| r[0] <= c).map_or(0, |r| r[2])
}

fn word_record(c: char) -> u32 {
    in_ranges(&tables().word_break, c)
}

/// The word-break class of `c`.
pub fn word_break(c: char) -> WordBreak {
    WordBreak::ALL[(word_record(c) & CLASS) as usize]
}

/// Whether `c` is Extended_Pictographic.
pub fn is_pictographic(c: char) -> bool {
    word_record(c) & PICTOGRAPHIC != 0
}

/// Whether `c` is of the Han script (Unicode 12.1).
pub fn is_han(c: char) -> bool {
    word_record(c) & HAN != 0
}

/// Whether `c` is of the Hiragana script (Unicode 12.1).
pub fn is_hiragana(c: char) -> bool {
    word_record(c) & HIRAGANA != 0
}

/// Whether `c` is of the Hangul script (Unicode 12.1).
pub fn is_hangul(c: char) -> bool {
    word_record(c) & HANGUL != 0
}

/// Whether `c` has Line_Break=SA, the scripts written without spaces between words (Unicode
/// 12.1).
pub fn is_complex_context(c: char) -> bool {
    word_record(c) & COMPLEX_CONTEXT != 0
}

/// Whether `c` is a letter or a number (general category L or N).
pub fn is_alphanumeric(c: char) -> bool {
    word_record(c) & ALPHANUMERIC != 0
}

/// The canonical combining class of `c`.
pub fn ccc(c: char) -> u8 {
    in_ranges(&tables().ccc, c) as u8
}

fn chars(offset: u32, len: u32) -> impl Iterator<Item = char> {
    let pool = &tables().pool;
    pool[offset as usize..(offset + len) as usize]
        .iter()
        .map(|&c| char::from_u32(c).expect("the table holds scalar values"))
}

/// The one-level decomposition of `c`: canonical only, or also compatibility when `compat`.
pub fn decomposition(c: char, compat: bool) -> Option<impl Iterator<Item = char>> {
    let t = &tables().decompositions;
    let i = t.binary_search_by_key(&u32::from(c), |r| r[0]).ok()?;
    let [_, is_compat, len, offset] = t[i];
    (compat || is_compat == 0).then(|| chars(offset, len))
}

/// The primary composite of `a` followed by `b`, if there is one (excluding Hangul).
pub fn compose(a: char, b: char) -> Option<char> {
    let t = &tables().compositions;
    let i = t.binary_search_by_key(&(u32::from(a), u32::from(b)), |r| (r[0], r[1])).ok()?;
    char::from_u32(t[i][2])
}

/// NFKC_Casefold of `c`, if it changes `c` (possibly to nothing).
pub fn casefold(c: char) -> Option<impl Iterator<Item = char>> {
    let t = &tables().casefold;
    let i = t.binary_search_by_key(&u32::from(c), |r| r[0]).ok()?;
    Some(chars(t[i][2], t[i][1]))
}

/// The simple lowercase mapping of `c`.
pub fn lowercase(c: char) -> char {
    let t = &tables().lowercase;
    t.binary_search_by_key(&u32::from(c), |r| r[0])
        .ok()
        .and_then(|i| char::from_u32(t[i][1]))
        .unwrap_or(c)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn damaged_tables_are_rejected() {
        assert_eq!(parse(b"TSLUNI00"), None);
        assert_eq!(parse(b"TSL"), None);
        assert_eq!(parse(&DATA[..DATA.len() - 1]), None);
        let mut longer = DATA.to_vec();
        longer.push(0);
        assert_eq!(parse(&longer), None);
    }

    #[test]
    fn lookups() {
        assert_eq!(word_break('a'), WordBreak::ALetter);
        assert_eq!(word_break('7'), WordBreak::Numeric);
        assert_eq!(word_break('\r'), WordBreak::Cr);
        assert_eq!(word_break('ア'), WordBreak::Katakana);
        assert_eq!(word_break('中'), WordBreak::Other);
        assert!(is_alphanumeric('中') && is_alphanumeric('٣') && !is_alphanumeric('-'));
        assert!(is_pictographic('😀') && !is_pictographic('a'));
        assert_eq!(ccc('\u{301}'), 230);
        assert_eq!(ccc('a'), 0);
        assert_eq!(decomposition('é', false).unwrap().collect::<String>(), "e\u{301}");
        assert!(decomposition('ﬁ', false).is_none());
        assert_eq!(decomposition('ﬁ', true).unwrap().collect::<String>(), "fi");
        assert!(decomposition('a', true).is_none());
        assert_eq!(compose('e', '\u{301}'), Some('é'));
        assert_eq!(compose('e', 'e'), None);
        assert_eq!(casefold('A').unwrap().collect::<String>(), "a");
        assert_eq!(casefold('\u{AD}').unwrap().count(), 0);
        assert!(casefold('a').is_none());
        assert_eq!(lowercase('Ä'), 'ä');
        assert_eq!(lowercase('ä'), 'ä');
        let names: BTreeSet<&str> = WordBreak::ALL.iter().map(|w| w.ucd_name()).collect();
        assert_eq!(names.len(), WordBreak::ALL.len());
    }
}
