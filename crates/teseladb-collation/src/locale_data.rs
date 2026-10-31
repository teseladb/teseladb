//! The per-locale tables (FR-016) and their binary form, `data/locales.bin`: the ICU bundles with
//! their collation types (what locale IDs resolve to) and each tailoring the accepted IDs reach,
//! built by the generator from ICU 57.1's rules (D-043).
//!
//! Layout, little-endian: the magic `TSLLOC01`, the bundles (name, whether they have collations,
//! default type or empty, types), then the tailorings (bundle, type, settings, mappings, ranges
//! whose root contractions are off), each section as a count followed by its records.

use std::collections::BTreeMap;

use crate::data::{Ce, DataError, Reader, Writer};
use crate::locales::{BundleInfo, LocaleModel};
use crate::options::{CaseFirst, MaxVariable, Settings};
use crate::sortkey::{Alternate, Strength};

const MAGIC: &[u8; 8] = b"TSLLOC01";

/// One tailoring: what its rules set and map.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TailoringData {
    /// The options its rules set.
    pub settings: Settings,
    /// Context (empty if none), NFD string and elements of each mapping.
    pub mappings: Vec<(Vec<u32>, Vec<u32>, Vec<Ce>)>,
    /// Ranges of code points whose root contractions and contexts are off.
    pub suppressed: Vec<(u32, u32)>,
}

/// The bundles and the tailorings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocaleTables {
    /// Each bundle by name.
    pub bundles: BTreeMap<String, BundleInfo>,
    /// Bundle, type and tailoring.
    pub tailorings: Vec<(String, String, TailoringData)>,
}

fn strength_code(s: Option<Strength>) -> u8 {
    s.map_or(0, |s| s as u8)
}

fn strength_of(c: u8) -> Result<Option<Strength>, DataError> {
    Ok(match c {
        0 => None,
        1 => Some(Strength::Primary),
        2 => Some(Strength::Secondary),
        3 => Some(Strength::Tertiary),
        4 => Some(Strength::Quaternary),
        5 => Some(Strength::Identical),
        _ => return Err(DataError("an unknown strength")),
    })
}

/// An option as one byte: 0 if unset, else 1 + its position in `values`.
fn code<T: PartialEq>(v: Option<T>, values: &[T]) -> u8 {
    v.map_or(0, |v| {
        let i = values.iter().position(|x| *x == v).expect("listed");
        1 + u8::try_from(i).expect("few values")
    })
}

fn decode<T: Copy>(c: u8, values: &[T]) -> Result<Option<T>, DataError> {
    match c {
        0 => Ok(None),
        n => values
            .get(usize::from(n) - 1)
            .copied()
            .map(Some)
            .ok_or(DataError("an unknown option value")),
    }
}

const ALTERNATES: [Alternate; 2] = [Alternate::NonIgnorable, Alternate::Shifted];
const MAX_VARIABLES: [MaxVariable; 4] =
    [MaxVariable::Space, MaxVariable::Punct, MaxVariable::Symbol, MaxVariable::Currency];
const CASE_FIRSTS: [CaseFirst; 3] = [CaseFirst::Off, CaseFirst::Lower, CaseFirst::Upper];
const BOOLS: [bool; 2] = [false, true];

impl Writer {
    fn string(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    fn settings(&mut self, s: &Settings) {
        self.0.extend_from_slice(&[
            strength_code(s.strength),
            code(s.alternate, &ALTERNATES),
            code(s.max_variable, &MAX_VARIABLES),
            code(s.case_first, &CASE_FIRSTS),
            code(s.case_level, &BOOLS),
            code(s.normalization, &BOOLS),
            code(s.numeric_ordering, &BOOLS),
            u8::from(s.backwards),
            u8::from(s.reorder.is_some()),
        ]);
        let reorder = s.reorder.as_deref().unwrap_or_default();
        self.count(reorder.len());
        for r in reorder {
            self.string(r);
        }
    }

    fn ce(&mut self, ce: &Ce) {
        self.u32(ce.primary);
        self.u16(ce.secondary);
        self.u16(ce.tertiary);
    }
}

impl Reader<'_> {
    fn string(&mut self) -> Result<String, DataError> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| DataError("a name not UTF-8"))
    }

    fn settings(&mut self) -> Result<Settings, DataError> {
        let b = self.take(9)?.to_vec();
        let mut s = Settings {
            strength: strength_of(b[0])?,
            alternate: decode(b[1], &ALTERNATES)?,
            max_variable: decode(b[2], &MAX_VARIABLES)?,
            case_first: decode(b[3], &CASE_FIRSTS)?,
            case_level: decode(b[4], &BOOLS)?,
            normalization: decode(b[5], &BOOLS)?,
            numeric_ordering: decode(b[6], &BOOLS)?,
            backwards: b[7] == 1,
            reorder: None,
        };
        let reorder = (0..self.count()?).map(|_| self.string()).collect::<Result<Vec<_>, _>>()?;
        if b[8] == 1 {
            s.reorder = Some(reorder);
        }
        Ok(s)
    }

    fn ce(&mut self) -> Result<Ce, DataError> {
        Ok(Ce { primary: self.u32()?, secondary: self.u16()?, tertiary: self.u16()? })
    }
}

impl LocaleTables {
    /// The tables in their binary form.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        w.0.extend_from_slice(MAGIC);
        w.count(self.bundles.len());
        for (name, b) in &self.bundles {
            w.string(name);
            w.0.push(u8::from(b.has_collations));
            w.string(b.default.as_deref().unwrap_or_default());
            w.count(b.types.len());
            for t in &b.types {
                w.string(t);
            }
        }
        w.count(self.tailorings.len());
        for (bundle, ty, t) in &self.tailorings {
            w.string(bundle);
            w.string(ty);
            w.settings(&t.settings);
            w.count(t.mappings.len());
            for (before, cps, ces) in &t.mappings {
                w.cps(before);
                w.cps(cps);
                w.count(ces.len());
                for ce in ces {
                    w.ce(ce);
                }
            }
            w.count(t.suppressed.len());
            for (lo, hi) in &t.suppressed {
                w.u32(*lo);
                w.u32(*hi);
            }
        }
        w.0
    }

    /// The tables read back from their binary form.
    pub fn from_bytes(b: &[u8]) -> Result<LocaleTables, DataError> {
        let mut r = Reader { b, pos: 0 };
        if r.take(8)? != MAGIC {
            return Err(DataError("another format or version"));
        }
        let mut bundles = BTreeMap::new();
        for _ in 0..r.count()? {
            let name = r.string()?;
            let has_collations = r.u8()? == 1;
            let default = Some(r.string()?).filter(|d| !d.is_empty());
            let types = (0..r.count()?).map(|_| r.string()).collect::<Result<_, _>>()?;
            bundles.insert(name, BundleInfo { has_collations, default, types });
        }
        let mut tailorings = Vec::new();
        for _ in 0..r.count()? {
            let (bundle, ty) = (r.string()?, r.string()?);
            let settings = r.settings()?;
            let mappings = (0..r.count()?)
                .map(|_| {
                    let (before, cps) = (r.cps()?, r.cps()?);
                    let ces = (0..r.count()?).map(|_| r.ce()).collect::<Result<_, _>>()?;
                    Ok((before, cps, ces))
                })
                .collect::<Result<_, DataError>>()?;
            let suppressed = (0..r.count()?)
                .map(|_| Ok((r.u32()?, r.u32()?)))
                .collect::<Result<_, DataError>>()?;
            tailorings.push((bundle, ty, TailoringData { settings, mappings, suppressed }));
        }
        if r.pos != b.len() {
            return Err(DataError("bytes left over"));
        }
        Ok(LocaleTables { bundles, tailorings })
    }

    /// How locale IDs resolve over the bundles.
    pub fn model(&self) -> Result<LocaleModel, String> {
        LocaleModel::new(self.bundles.clone())
    }
}
