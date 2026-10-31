//! The CLDR root collation loaded once from `data/uca.bin`: lookups of single code points and
//! contractions (with every prefix of a contraction, to know when to keep extending a match),
//! canonical decompositions and combining classes, and the implicit weight of code points the
//! table does not list.

use crate::fxhash::{FxMap, FxSet};
use std::sync::OnceLock;

use crate::data::{COMMON_WEIGHT16, Ce, Tables, implicit_primary};
use crate::elements::Mappings;
use crate::options::MaxVariable;

/// The generated tables, in the binary.
const DATA: &[u8] = include_bytes!("../data/uca.bin");

/// The root collation, ready for lookups.
pub struct Root {
    /// UCA version.
    pub version: String,
    /// The highest variable primary.
    pub variable_top: u32,
    ces: Vec<Ce>,
    singles: FxMap<u32, (u32, u8)>,
    contractions: FxMap<Vec<u32>, (u32, u8)>,
    /// Mappings in context, by context and then string.
    in_context: FxMap<Vec<u32>, FxMap<Vec<u32>, (u32, u8)>>,
    /// The contexts of the strings starting with each code point, longest first.
    contexts: FxMap<u32, Vec<Vec<u32>>>,
    /// Every proper prefix of a string, by context (empty for the contractions).
    prefixes: FxMap<Vec<u32>, FxSet<Vec<u32>>>,
    decompositions: FxMap<u32, Vec<u32>>,
    combining: FxMap<u32, u8>,
    unified_ideographs: Vec<(u32, u32)>,
    core_han_blocks: Vec<(u32, u32)>,
    numeric_primary: u32,
    digits: FxMap<u32, u8>,
    groups: Vec<(u32, Vec<String>)>,
}

/// The root collation of the committed tables.
pub fn root() -> &'static Root {
    static ROOT: OnceLock<Root> = OnceLock::new();
    ROOT.get_or_init(|| {
        Root::new(Tables::from_bytes(DATA).expect("the tables the generator wrote"))
    })
}

impl Root {
    /// The root collation of `t`.
    pub fn new(t: Tables) -> Root {
        let strings = t
            .contractions
            .iter()
            .map(|(cps, _, _)| (Vec::new(), cps))
            .chain(t.contexts.iter().map(|(c, cps, _, _)| (c.clone(), cps)));
        let mut prefixes: FxMap<Vec<u32>, FxSet<Vec<u32>>> = FxMap::default();
        for (c, cps) in strings {
            let set = prefixes.entry(c).or_default();
            set.extend((1..cps.len()).map(|n| cps[..n].to_vec()));
        }
        let mut contexts: FxMap<u32, Vec<Vec<u32>>> = FxMap::default();
        for (c, cps, _, _) in &t.contexts {
            let list = contexts.entry(cps[0]).or_default();
            if !list.contains(c) {
                list.push(c.clone());
            }
        }
        for list in contexts.values_mut() {
            list.sort_by_key(|c| std::cmp::Reverse(c.len()));
        }
        let mut in_context: FxMap<Vec<u32>, FxMap<Vec<u32>, (u32, u8)>> = FxMap::default();
        for (c, cps, s, n) in t.contexts {
            in_context.entry(c).or_default().insert(cps, (s, n));
        }
        Root {
            version: t.version,
            variable_top: t.variable_top,
            ces: t.ces,
            singles: t.singles.into_iter().map(|(cp, s, n)| (cp, (s, n))).collect(),
            contractions: t.contractions.into_iter().map(|(cps, s, n)| (cps, (s, n))).collect(),
            in_context,
            contexts,
            prefixes,
            decompositions: t.decompositions.into_iter().collect(),
            combining: t.combining.into_iter().collect(),
            unified_ideographs: t.unified_ideographs,
            core_han_blocks: t.core_han_blocks,
            numeric_primary: t.numeric_primary,
            digits: t.digits.into_iter().collect(),
            groups: t.groups,
        }
    }

    fn slice(&self, (start, n): (u32, u8)) -> &[Ce] {
        &self.ces[start as usize..start as usize + usize::from(n)]
    }

    pub(crate) fn decomposition(&self, cp: u32) -> Option<&[u32]> {
        self.decompositions.get(&cp).map(Vec::as_slice)
    }

    /// `cps` in NFD.
    pub fn nfd(&self, cps: impl IntoIterator<Item = u32>) -> Vec<u32> {
        crate::nfd::nfd(self, cps)
    }

    /// `cps` decomposed but not reordered: what ICU reads with `normalization: false`.
    pub fn decompose(&self, cps: impl IntoIterator<Item = u32>) -> Vec<u32> {
        crate::nfd::decompose_all(self, cps)
    }

    /// The groups `[reorder]` and `maxVariable` work on: first primary and codes.
    pub fn groups(&self) -> &[(u32, Vec<String>)] {
        &self.groups
    }

    /// The highest variable primary for `maxVariable`: the end of its group.
    pub fn variable_top_for(&self, max: MaxVariable) -> u32 {
        let next = match max {
            MaxVariable::Punct => return self.variable_top,
            MaxVariable::Space => "punct",
            MaxVariable::Symbol => "currency",
            MaxVariable::Currency => "digit",
        };
        let start = self.groups.iter().find(|(_, codes)| codes.iter().any(|c| c == next));
        start.map_or(self.variable_top, |(p, _)| p - 1)
    }
}

impl Mappings for Root {
    type Element = Ce;

    fn lookup(&self, context: &[u32], cps: &[u32]) -> Option<&[Ce]> {
        let entry = match (context, cps) {
            ([], [cp]) => self.singles.get(cp),
            ([], _) => self.contractions.get(cps),
            _ => self.in_context.get(context).and_then(|m| m.get(cps)),
        };
        entry.map(|e| self.slice(*e))
    }

    fn starts(&self, context: &[u32], cps: &[u32]) -> bool {
        self.prefixes.get(context).is_some_and(|set| set.contains(cps))
    }

    fn contexts(&self, cp: u32) -> Vec<&[u32]> {
        self.contexts.get(&cp).map_or_else(Vec::new, |l| l.iter().map(Vec::as_slice).collect())
    }

    fn combining_class(&self, cp: u32) -> u8 {
        self.combining.get(&cp).copied().unwrap_or(0)
    }

    fn digit(&self, cp: u32) -> Option<u8> {
        self.digits.get(&cp).copied()
    }

    fn numeric(&self, p: u32) -> Ce {
        Ce { primary: p, secondary: COMMON_WEIGHT16, tertiary: COMMON_WEIGHT16 }
    }

    fn numeric_primary(&self) -> u32 {
        self.numeric_primary
    }

    fn implicit(&self, cp: u32) -> Ce {
        let primary = implicit_primary(cp, &self.unified_ideographs, &self.core_han_blocks);
        Ce { primary, secondary: COMMON_WEIGHT16, tertiary: COMMON_WEIGHT16 }
    }
}
