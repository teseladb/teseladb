//! The root collation's weights as ICU 57.1's `CollationRootElements` answers about them for the
//! tailoring builder: which weight comes before or after a root weight at each level, and the
//! boundaries between the weight ranges of primary, secondary and tertiary elements.
//!
//! The elements are those of every root mapping and of the `FDD0 …` lines, without case bits,
//! except U+FFFE's (ICU's merge separator, below them all), plus ICU's `[first trailing]`, between
//! the last implicit primary and U+FFFD's.
//! For each primary, the secondary and tertiary pairs it occurs with always include the common
//! pair (`05`, `05`), as ICU assumes. Implicit primaries (lead bytes `E0` to `E6` here) are not
//! listed: each code point's is followed by the next one's.

use std::collections::HashMap;

use teseladb_collation::{COMMON_WEIGHT16, Ce, Tables, implicit_primary};

/// Below the lowest weight of a level for a tailored element placed before a root one.
pub const BEFORE_WEIGHT16: u16 = 0x0100;
/// `[fixed last secondary common byte 45]`: tailored secondaries go above it.
pub const LAST_COMMON_SECONDARY: u16 = 0x4500;
/// `[fixed first ignorable secondary byte 80]`: secondaries of primary elements stay below it.
pub const SECONDARY_BOUNDARY: u16 = 0x8000;
/// `[fixed first ignorable tertiary byte 3C]`: tertiaries of primary and secondary elements stay
/// below it.
pub const TERTIARY_BOUNDARY: u16 = 0x3C00;
/// ICU's `[first trailing]`, after every implicit primary and before U+FFFD's.
pub const FIRST_TRAILING_PRIMARY: u32 = 0xEF00_0000;
/// The tertiary weight without case bits.
pub const ONLY_TERTIARY: u16 = 0x3F3F;
/// Above every tertiary of a tertiary element.
pub const TERTIARY_LIMIT: u32 = 0x4000;
/// Above every secondary of a secondary element.
pub const SECONDARY_LIMIT: u32 = 0x1_0000;

/// One root element without case bits: primary, secondary, tertiary.
pub type Element = (u32, u16, u16);

const COMMON_PAIR: (u16, u16) = (COMMON_WEIGHT16, COMMON_WEIGHT16);

/// The root's elements, indexed by primary.
#[derive(Clone, Debug)]
pub struct RootElements {
    primaries: Vec<u32>,
    pairs: HashMap<u32, Vec<(u16, u16)>>,
    secondary_ces: Vec<(u16, u16)>,
    tertiary_ces: Vec<(u16, u16)>,
    first_implicit: u32,
    last_implicit: u32,
}

/// Whether `p` is an implicit primary.
fn implicit(p: u32) -> bool {
    (0xE0..=0xE6).contains(&(p >> 24))
}

/// The distance from one implicit primary to the next code point's.
fn implicit_step(p: u32) -> u32 {
    if p >> 24 <= 0xE4 { 0x100 } else { 0x10 }
}

impl RootElements {
    /// The elements of `tables` and the root's `homeless` ones.
    pub fn new(tables: &Tables, homeless: &[Ce]) -> RootElements {
        let special: Vec<std::ops::Range<usize>> = tables
            .singles
            .iter()
            .filter(|(cp, _, _)| *cp == 0xFFFE)
            .map(|(_, start, len)| *start as usize..*start as usize + usize::from(*len))
            .collect();
        let mut elements: Vec<Element> = tables
            .ces
            .iter()
            .enumerate()
            .filter(|(i, _)| !special.iter().any(|r| r.contains(i)))
            .map(|(_, ce)| ce)
            .chain(homeless)
            .chain(&[Ce {
                primary: FIRST_TRAILING_PRIMARY,
                secondary: COMMON_WEIGHT16,
                tertiary: COMMON_WEIGHT16,
            }])
            .map(|ce| (ce.primary, ce.secondary, ce.tertiary & ONLY_TERTIARY))
            .filter(|e| *e != (0, 0, 0))
            .collect();
        elements.sort_unstable();
        elements.dedup();
        let mut root = RootElements {
            primaries: Vec::new(),
            pairs: HashMap::new(),
            secondary_ces: Vec::new(),
            tertiary_ces: Vec::new(),
            first_implicit: implicit_primary(
                0x4E00,
                &tables.unified_ideographs,
                &tables.core_han_blocks,
            ),
            last_implicit: implicit_primary(
                0x10FFFF,
                &tables.unified_ideographs,
                &tables.core_han_blocks,
            ),
        };
        for (p, s, t) in elements {
            match (p, s) {
                (0, 0) => root.tertiary_ces.push((s, t)),
                (0, _) => root.secondary_ces.push((s, t)),
                _ => {
                    if root.primaries.last() != Some(&p) {
                        root.primaries.push(p);
                    }
                    root.pairs.entry(p).or_default().push((s, t));
                }
            }
        }
        for pairs in root.pairs.values_mut() {
            if !pairs.contains(&COMMON_PAIR) {
                pairs.push(COMMON_PAIR);
                pairs.sort_unstable();
            }
        }
        root
    }

    /// The secondary and tertiary pairs of `p` (only the common one for a primary not listed),
    /// or of the secondary or tertiary elements for `p` = 0 (`secondaries` chooses which).
    fn pairs(&self, p: u32, secondaries: bool) -> &[(u16, u16)] {
        match (p, secondaries) {
            (0, true) => &self.secondary_ces,
            (0, false) => &self.tertiary_ces,
            _ => self.pairs.get(&p).map_or(std::slice::from_ref(&COMMON_PAIR), Vec::as_slice),
        }
    }

    /// The first primary.
    pub fn first_primary(&self) -> u32 {
        self.primaries[0]
    }

    /// The first and last tertiary element (`[first|last secondary ignorable]`).
    pub fn tertiary_ces(&self) -> (Element, Element) {
        let (first, last) = (self.tertiary_ces[0], self.tertiary_ces[self.tertiary_ces.len() - 1]);
        ((0, first.0, first.1), (0, last.0, last.1))
    }

    /// The first and last secondary element (`[first|last primary ignorable]`).
    pub fn secondary_ces(&self) -> (Element, Element) {
        let s = &self.secondary_ces;
        let (first, last) = (s[0], s[s.len() - 1]);
        ((0, first.0, first.1), (0, last.0, last.1))
    }

    /// The first element whose primary is at least `p`, with common weights.
    pub fn first_ce_with_primary_at_least(&self, p: u32) -> Element {
        let i = self.primaries.partition_point(|q| *q < p);
        let q = self.primaries.get(i).copied().unwrap_or(p);
        (q, COMMON_WEIGHT16, COMMON_WEIGHT16)
    }

    /// The last element whose primary is below `p`.
    pub fn last_ce_with_primary_before(&self, p: u32) -> Element {
        let i = self.primaries.partition_point(|q| *q < p);
        let q = if i == 0 { 0 } else { self.primaries[i - 1] };
        let (s, t) = self.pairs(q, true).last().copied().unwrap_or((0, 0));
        (q, s, t)
    }

    /// The root primary after `p`.
    pub fn primary_after(&self, p: u32) -> u32 {
        let i = self.primaries.partition_point(|q| *q <= p);
        let listed = self.primaries.get(i).copied().unwrap_or(u32::MAX);
        let next_implicit = if implicit(p) && p < self.last_implicit {
            p + implicit_step(p)
        } else if p < self.first_implicit {
            self.first_implicit
        } else {
            u32::MAX
        };
        listed.min(next_implicit)
    }

    /// The root primary before `p`.
    pub fn primary_before(&self, p: u32) -> u32 {
        let i = self.primaries.partition_point(|q| *q < p);
        let listed = if i == 0 { 0 } else { self.primaries[i - 1] };
        if implicit(p) && p > self.first_implicit {
            listed.max(p - implicit_step(p))
        } else if p > self.last_implicit {
            listed.max(self.last_implicit)
        } else {
            listed
        }
    }

    /// The secondary after `s` for the primary `p`.
    pub fn secondary_after(&self, p: u32, s: u16) -> u32 {
        let limit = if p == 0 { SECONDARY_LIMIT } else { u32::from(SECONDARY_BOUNDARY) };
        let next = self.pairs(p, true).iter().find(|(s2, _)| *s2 > s);
        next.map_or(limit, |(s2, _)| u32::from(*s2))
    }

    /// The secondary before `s` for the primary `p`.
    pub fn secondary_before(&self, p: u32, s: u16) -> u16 {
        let none = if p == 0 { 0 } else { BEFORE_WEIGHT16 };
        let mut below = self.pairs(p, true).iter().filter(|(s2, _)| *s2 < s);
        below.next_back().map_or(none, |(s2, _)| *s2)
    }

    /// The tertiary after `t` for the primary `p` and secondary `s`.
    pub fn tertiary_after(&self, p: u32, s: u16, t: u16) -> u32 {
        let tertiary_ce = p == 0 && s == 0;
        let limit = if tertiary_ce { TERTIARY_LIMIT } else { u32::from(TERTIARY_BOUNDARY) };
        match self.pairs(p, !tertiary_ce).iter().find(|pair| **pair > (s, t)) {
            Some((s2, t2)) if *s2 == s => u32::from(*t2),
            _ => limit,
        }
    }

    /// The tertiary before `t` for the primary `p` and secondary `s`.
    pub fn tertiary_before(&self, p: u32, s: u16, t: u16) -> u16 {
        let tertiary_ce = p == 0 && s == 0;
        let none = if tertiary_ce { 0 } else { BEFORE_WEIGHT16 };
        let mut below =
            self.pairs(p, !tertiary_ce).iter().filter(|pair| **pair < (s, t) && pair.0 == s);
        below.next_back().map_or(none, |pair| pair.1)
    }
}
