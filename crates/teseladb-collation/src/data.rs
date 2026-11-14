//! The Unicode and CLDR tables collation needs, and their binary form (FR-018). The generator
//! (`tools/teseladb-unicodegen`) builds them from the official files of the version the reference
//! server uses (D-043) and writes `data/uca.bin`; the crate reads that file back once.
//!
//! The weights are ICU's, from CLDR's `FractionalUCA.txt`: a primary of up to four bytes, a
//! secondary and a tertiary of up to two, left-aligned, with the common weight `05`. The two top
//! bits of the tertiary carry the case (`00` lower, `01` mixed, `10` upper), as in ICU.
//!
//! Layout, little-endian: the magic `TSLCOL01`, the UCA version, then each section as a count
//! followed by its records.

use std::fmt;

const MAGIC: &[u8; 8] = b"TSLCOL04";

/// A collation element: three weights, left-aligned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ce {
    /// Primary weight.
    pub primary: u32,
    /// Secondary weight.
    pub secondary: u16,
    /// Tertiary weight, with the case in its two top bits.
    pub tertiary: u16,
}

/// The primary of U+FFFE, the lowest, which is never variable (ICU's merge separator).
pub const MERGE_SEPARATOR_PRIMARY: u32 = 0x0200_0000;

/// The common secondary and tertiary weight, `05`.
pub const COMMON_WEIGHT16: u16 = 0x0500;

/// The implicit primary of a code point the tables do not list (UCA 8.0 §10.1.3, in ICU's
/// order): first the `Unified_Ideograph` of the two core CJK blocks, then the other
/// `Unified_Ideograph`, then every other code point, each group in code point order, all after
/// `[last regular]` and before `[first trailing]`. Each is one element with common secondary
/// and tertiary, as in ICU, and leaves room after it for tailored primaries.
pub fn implicit_primary(
    cp: u32,
    unified_ideographs: &[(u32, u32)],
    core_han_blocks: &[(u32, u32)],
) -> u32 {
    let within = |ranges: &[(u32, u32)]| ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&cp));
    match (within(unified_ideographs), within(core_han_blocks)) {
        (true, true) => 0xE000_0000 | (cp << 8),
        (true, false) => 0xE100_0000 + (cp << 8),
        _ => 0xE500_0000 + (cp << 4),
    }
}

/// What the generator extracts from Unicode and CLDR.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tables {
    /// UCA version, as `8.0.0`.
    pub version: String,
    /// The highest variable primary (`[variable top]`): with `shifted`, elements with a primary
    /// from 1 to it are ignorable at the first three levels.
    pub variable_top: u32,
    /// Every collation element; mappings point into it.
    pub ces: Vec<Ce>,
    /// Single code points: code point, first element, number of elements; sorted.
    pub singles: Vec<(u32, u32, u8)>,
    /// Contractions: code points, first element, number of elements; sorted.
    pub contractions: Vec<(Vec<u32>, u32, u8)>,
    /// Mappings in context (`L|·`): the code points that must come just before, the string,
    /// first element and number of elements; sorted.
    pub contexts: Vec<(Vec<u32>, Vec<u32>, u32, u8)>,
    /// Canonical decompositions, one level: code point and what it decomposes into; sorted.
    pub decompositions: Vec<(u32, Vec<u32>)>,
    /// Canonical combining classes other than 0; sorted.
    pub combining: Vec<(u32, u8)>,
    /// Ranges of `Unified_Ideograph`, inclusive; sorted.
    pub unified_ideographs: Vec<(u32, u32)>,
    /// The blocks `CJK Unified Ideographs` and `CJK Compatibility Ideographs`, inclusive.
    pub core_han_blocks: Vec<(u32, u32)>,
    /// The first primary of the numbers `numericOrdering` makes (FractionalUCA's `FDD0 0034`).
    pub numeric_primary: u32,
    /// Decimal digits (`Nd`) and their values, for `numericOrdering`; sorted.
    pub digits: Vec<(u32, u8)>,
    /// The groups `[reorder]` and `maxVariable` work on: first primary and codes (`space`,
    /// `punct`, `symbol`, `currency`, `digit`, ISO 15924 scripts); sorted by primary.
    pub groups: Vec<(u32, Vec<String>)>,
}

/// Bytes that are not collation tables of this format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DataError(pub(crate) &'static str);

impl fmt::Display for DataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not collation tables: {}", self.0)
    }
}

impl std::error::Error for DataError {}

impl Tables {
    /// The tables in their binary form.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.0.extend_from_slice(MAGIC);
        w.bytes(self.version.as_bytes());
        w.u32(self.variable_top);
        w.count(self.ces.len());
        for ce in &self.ces {
            w.u32(ce.primary);
            w.u16(ce.secondary);
            w.u16(ce.tertiary);
        }
        w.count(self.singles.len());
        for (cp, start, len) in &self.singles {
            w.u32(*cp);
            w.u32(*start);
            w.0.push(*len);
        }
        w.count(self.contractions.len());
        for (cps, start, len) in &self.contractions {
            w.cps(cps);
            w.u32(*start);
            w.0.push(*len);
        }
        w.count(self.contexts.len());
        for (before, cps, start, len) in &self.contexts {
            w.cps(before);
            w.cps(cps);
            w.u32(*start);
            w.0.push(*len);
        }
        w.count(self.decompositions.len());
        for (cp, to) in &self.decompositions {
            w.u32(*cp);
            w.cps(to);
        }
        w.count(self.combining.len());
        for (cp, class) in &self.combining {
            w.u32(*cp);
            w.0.push(*class);
        }
        for ranges in [&self.unified_ideographs, &self.core_han_blocks] {
            w.count(ranges.len());
            for (lo, hi) in ranges {
                w.u32(*lo);
                w.u32(*hi);
            }
        }
        w.u32(self.numeric_primary);
        w.count(self.digits.len());
        for (cp, v) in &self.digits {
            w.u32(*cp);
            w.0.push(*v);
        }
        w.count(self.groups.len());
        for (start, codes) in &self.groups {
            w.u32(*start);
            w.count(codes.len());
            for c in codes {
                w.bytes(c.as_bytes());
            }
        }
        w.0
    }

    /// The tables read back from their binary form.
    pub fn from_bytes(b: &[u8]) -> Result<Tables, DataError> {
        let mut r = Reader { b, pos: 0 };
        if r.take(8)? != MAGIC {
            return Err(DataError("another format or version"));
        }
        let version =
            String::from_utf8(r.bytes()?.to_vec()).map_err(|_| DataError("version not UTF-8"))?;
        let variable_top = r.u32()?;
        let ces = (0..r.count()?)
            .map(|_| Ok(Ce { primary: r.u32()?, secondary: r.u16()?, tertiary: r.u16()? }))
            .collect::<Result<_, _>>()?;
        let singles =
            (0..r.count()?).map(|_| Ok((r.u32()?, r.u32()?, r.u8()?))).collect::<Result<_, _>>()?;
        let contractions =
            (0..r.count()?).map(|_| Ok((r.cps()?, r.u32()?, r.u8()?))).collect::<Result<_, _>>()?;
        let contexts = (0..r.count()?)
            .map(|_| Ok((r.cps()?, r.cps()?, r.u32()?, r.u8()?)))
            .collect::<Result<_, _>>()?;
        let decompositions =
            (0..r.count()?).map(|_| Ok((r.u32()?, r.cps()?))).collect::<Result<_, _>>()?;
        let combining =
            (0..r.count()?).map(|_| Ok((r.u32()?, r.u8()?))).collect::<Result<_, _>>()?;
        let unified_ideographs =
            (0..r.count()?).map(|_| Ok((r.u32()?, r.u32()?))).collect::<Result<_, _>>()?;
        let core_han_blocks =
            (0..r.count()?).map(|_| Ok((r.u32()?, r.u32()?))).collect::<Result<_, _>>()?;
        let numeric_primary = r.u32()?;
        let digits = (0..r.count()?).map(|_| Ok((r.u32()?, r.u8()?))).collect::<Result<_, _>>()?;
        let groups = (0..r.count()?)
            .map(|_| {
                let start = r.u32()?;
                let codes = (0..r.count()?)
                    .map(|_| {
                        String::from_utf8(r.bytes()?.to_vec())
                            .map_err(|_| DataError("a code not UTF-8"))
                    })
                    .collect::<Result<_, _>>()?;
                Ok((start, codes))
            })
            .collect::<Result<_, _>>()?;
        if r.pos != b.len() {
            return Err(DataError("bytes left over"));
        }
        Ok(Tables {
            version,
            variable_top,
            ces,
            singles,
            contractions,
            contexts,
            decompositions,
            combining,
            unified_ideographs,
            core_han_blocks,
            numeric_primary,
            digits,
            groups,
        })
    }
}

pub(crate) struct Writer(pub(crate) Vec<u8>);

impl Writer {
    pub(crate) fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub(crate) fn count(&mut self, n: usize) {
        self.u32(u32::try_from(n).expect("tables fit in u32 counts"));
    }
    pub(crate) fn bytes(&mut self, b: &[u8]) {
        self.count(b.len());
        self.0.extend_from_slice(b);
    }
    pub(crate) fn cps(&mut self, cps: &[u32]) {
        self.0.push(u8::try_from(cps.len()).expect("short sequences"));
        for cp in cps {
            self.u32(*cp);
        }
    }
}

pub(crate) struct Reader<'a> {
    pub(crate) b: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], DataError> {
        let s = self.b.get(self.pos..self.pos + n).ok_or(DataError("cut short"))?;
        self.pos += n;
        Ok(s)
    }
    pub(crate) fn u8(&mut self) -> Result<u8, DataError> {
        Ok(self.take(1)?[0])
    }
    pub(crate) fn u16(&mut self) -> Result<u16, DataError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().expect("2 bytes")))
    }
    pub(crate) fn u32(&mut self) -> Result<u32, DataError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("4 bytes")))
    }
    pub(crate) fn count(&mut self) -> Result<usize, DataError> {
        Ok(self.u32()? as usize)
    }
    pub(crate) fn bytes(&mut self) -> Result<&'a [u8], DataError> {
        let n = self.count()?;
        self.take(n)
    }
    pub(crate) fn cps(&mut self) -> Result<Vec<u32>, DataError> {
        let n = self.u8()?;
        (0..n).map(|_| self.u32()).collect()
    }
}
