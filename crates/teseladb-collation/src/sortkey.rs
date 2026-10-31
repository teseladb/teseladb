//! Sort keys (UTS #10, step S3, with ICU 57.1's settings): the non-zero weights of each level up to
//! the strength, levels apart by zeros, every weight a fixed-width big-endian number (four bytes
//! for primaries and quaternaries, two for the others), so that comparing two keys byte by byte
//! compares the strings as ICU's compressed sort keys do (`collationkeys.cpp`).
//!
//! With `shifted`, variable elements weigh only at the quaternary level (UTS #10, table 11), and
//! the primary ignorables after them not at all. `caseLevel` adds a level of case weights between
//! the secondary and the tertiary; `caseFirst` brings the case bits into the tertiary;
//! `backwards` reverses the secondaries within each segment between two U+FFFE; a reordering
//! moves primaries by script group; the quaternary level puts shifted primaries below the common
//! weight and tailored `<<<<` differences above it; the identical level appends the NFD code
//! points, U+FFFE below all of them as ICU's merge separator.

use crate::data::{Ce, MERGE_SEPARATOR_PRIMARY};
use crate::options::CaseFirst;
use crate::reorder::Reordering;

/// How many levels a comparison looks at: RefDb's `strength`, 1 to 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Strength {
    /// Base letters only.
    Primary = 1,
    /// Also accents.
    Secondary = 2,
    /// Also case and variants (RefDb's default).
    Tertiary = 3,
    /// Also, with `shifted`, spaces and punctuation.
    Quaternary = 4,
    /// Also the code points themselves.
    Identical = 5,
}

/// How spaces and punctuation weigh: RefDb's `alternate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Alternate {
    /// Like any other character (RefDb's default).
    NonIgnorable,
    /// Only at the fourth level.
    Shifted,
}

/// The tertiary weight without the case bits (ICU's `ONLY_TERTIARY_MASK`).
const ONLY_TERTIARY: u16 = 0x3F3F;
/// The tertiary weight with the case bits (ICU's `CASE_AND_TERTIARY_MASK`).
const CASE_AND_TERTIARY: u16 = 0xFF3F;
/// The common tertiary weight with `caseFirst: upper`: between lowercase below and above it.
const UPPER_FIRST_COMMON: u16 = 0xC500;
/// A secondary between two segments of `backwards` (U+FFFE), below every real one.
const MERGE_SEPARATOR_WEIGHT: u16 = 0x0002;
/// The quaternary weight of what is not variable, and of the tailored `<<<<` above it.
const QUATERNARY_COMMON: u32 = 0xFFFF_FF00;

/// What a sort key looks at beyond the elements (ICU's `CollationSettings`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct KeyOptions<'a> {
    pub(crate) strength: Strength,
    pub(crate) alternate: Alternate,
    /// The highest variable primary.
    pub(crate) variable_top: u32,
    pub(crate) case_first: CaseFirst,
    pub(crate) case_level: bool,
    pub(crate) backwards: bool,
    pub(crate) reorder: Option<&'a Reordering>,
}

/// The tertiary weight to compare: without case unless `caseFirst` is on without `caseLevel`,
/// and with `upper` the order ICU's sort keys give (uppercase, mixed, lowercase).
fn tertiary(ce: &Ce, o: &KeyOptions<'_>) -> u16 {
    let with_case = o.case_first != CaseFirst::Off && !o.case_level;
    let t = ce.tertiary & if with_case { CASE_AND_TERTIARY } else { ONLY_TERTIARY };
    if !with_case || o.case_first == CaseFirst::Lower || t == 0 {
        return t;
    }
    if ce.secondary == 0 {
        // A tertiary-only element keeps its uppercase bits, above everything else.
        return t + 0x4000;
    }
    let inverted = t ^ 0xC000;
    if inverted < UPPER_FIRST_COMMON { inverted - 0x4000 } else { inverted }
}

/// The case-level weight of an element: lowercase, mixed, uppercase in the order `caseFirst`
/// says.
fn case_weight(ce: &Ce, upper_first: bool) -> u16 {
    let case = ce.tertiary >> 14;
    if upper_first { 3 - case } else { 1 + case }
}

/// The sort key of `ces`, the elements of the code points `nfd`. Elements with a primary above
/// U+FFFE's and up to the variable top are variable, as in ICU.
pub(crate) fn sort_key(ces: &[Ce], nfd: &[u32], o: &KeyOptions<'_>) -> Vec<u8> {
    let shifted = o.alternate == Alternate::Shifted;
    let reorder = |p: u32| o.reorder.map_or(p, |r| r.map(p));
    let mut primaries = Vec::new();
    let mut secondaries: Vec<u16> = Vec::new();
    let mut segment = 0;
    let mut cases = Vec::new();
    let mut tertiaries = Vec::new();
    let mut quaternaries = Vec::new();
    let mut after_variable = false;
    for ce in ces {
        let variable = ce.primary > MERGE_SEPARATOR_PRIMARY && ce.primary <= o.variable_top;
        if shifted && variable {
            quaternaries.push(reorder(ce.primary));
            after_variable = true;
            continue;
        }
        if shifted && ce.primary == 0 && after_variable {
            continue;
        }
        if ce.primary != 0 {
            after_variable = false;
            primaries.push(reorder(ce.primary));
        }
        if ce.secondary == 0 && ce.tertiary == 0 {
            continue;
        }
        if o.backwards && ce.primary == MERGE_SEPARATOR_PRIMARY {
            secondaries[segment..].reverse();
            secondaries.push(MERGE_SEPARATOR_WEIGHT);
            segment = secondaries.len();
        } else if ce.secondary != 0 {
            secondaries.push(ce.secondary);
        }
        let cased =
            if o.strength == Strength::Primary { ce.primary != 0 } else { ce.secondary != 0 };
        if o.case_level && cased {
            cases.push(case_weight(ce, o.case_first == CaseFirst::Upper));
        }
        let t = tertiary(ce, o);
        if t != 0 {
            tertiaries.push(t);
        }
        let q = u32::from((ce.tertiary >> 6) & 3);
        quaternaries.push(QUATERNARY_COMMON + q);
    }
    if o.backwards {
        secondaries[segment..].reverse();
    }
    let mut out = Vec::new();
    for p in &primaries {
        out.extend_from_slice(&p.to_be_bytes());
    }
    let two = |out: &mut Vec<u8>, level: &[u16]| {
        out.extend_from_slice(&[0, 0]);
        for w in level {
            out.extend_from_slice(&w.to_be_bytes());
        }
    };
    if o.strength >= Strength::Secondary {
        two(&mut out, &secondaries);
    }
    if o.case_level {
        two(&mut out, &cases);
    }
    if o.strength >= Strength::Tertiary {
        two(&mut out, &tertiaries);
    }
    if o.strength >= Strength::Quaternary {
        out.extend_from_slice(&[0, 0, 0, 0]);
        for q in &quaternaries {
            out.extend_from_slice(&q.to_be_bytes());
        }
    }
    if o.strength == Strength::Identical {
        out.extend_from_slice(&[0, 0]);
        for cp in nfd {
            // U+FFFE, ICU's merge separator, below every code point (U+0000 included).
            let w = if *cp == 0xFFFE { 0 } else { cp + 1 };
            out.extend_from_slice(&w.to_be_bytes()[1..]);
        }
    }
    out
}
