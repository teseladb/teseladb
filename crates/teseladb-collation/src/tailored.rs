//! A tailoring's mappings over a base collation (the root), in the same tables per context: a
//! string the tailoring maps in a table takes its elements, any other falls back to the base, as
//! in ICU, where a tailored character keeps the base contractions and contexts of its strings.
//! `[suppressContractions]` turns off the base contractions and contexts of the strings starting
//! with the characters it lists, and removes those the tailoring had added until then.

use std::collections::HashMap;

use crate::fxhash::{FxMap, FxSet};

use crate::data::Ce;
use crate::elements::Mappings;

/// Mappings of a tailoring over `base`.
#[derive(Clone, Debug)]
pub struct Tailored<'a, M> {
    base: &'a M,
    /// By context (empty: none), then string.
    tables: FxMap<Vec<u32>, FxMap<Vec<u32>, Vec<Ce>>>,
    /// Every proper prefix of one of the tailoring's strings, by context.
    prefixes: FxMap<Vec<u32>, FxSet<Vec<u32>>>,
    /// The tailoring's contexts of the strings starting with each code point, longest first.
    contexts: FxMap<u32, Vec<Vec<u32>>>,
    /// Ranges of code points whose base contractions and contexts are off.
    suppressed: Vec<(u32, u32)>,
}

impl<'a, M: Mappings<Element = Ce>> Tailored<'a, M> {
    /// No mappings of its own yet.
    pub fn new(base: &'a M) -> Tailored<'a, M> {
        Tailored {
            base,
            tables: FxMap::default(),
            prefixes: FxMap::default(),
            contexts: FxMap::default(),
            suppressed: Vec::new(),
        }
    }

    /// Maps the NFD string `cps` to `ces`, replacing what it mapped to.
    pub fn insert(&mut self, cps: &[u32], ces: Vec<Ce>) {
        self.insert_context(&[], cps, ces);
    }

    /// Maps the NFD string `cps` to `ces` when the text before it ends with `context`.
    pub fn insert_context(&mut self, context: &[u32], cps: &[u32], ces: Vec<Ce>) {
        self.tables.entry(context.to_vec()).or_default().insert(cps.to_vec(), ces);
        self.index(context, cps);
    }

    fn index(&mut self, context: &[u32], cps: &[u32]) {
        let set = self.prefixes.entry(context.to_vec()).or_default();
        set.extend((1..cps.len()).map(|n| cps[..n].to_vec()));
        if !context.is_empty() {
            let list = self.contexts.entry(cps[0]).or_default();
            if !list.iter().any(|c| c == context) {
                list.push(context.to_vec());
                list.sort_by_key(|c| std::cmp::Reverse(c.len()));
            }
        }
    }

    /// Turns off the base contractions and contexts of strings starting with a code point in
    /// `lo..=hi`, and removes the tailoring's own.
    pub fn suppress_contractions(&mut self, lo: u32, hi: u32) {
        self.suppressed.push((lo, hi));
        for (context, table) in &mut self.tables {
            table.retain(|cps, _| {
                !(lo..=hi).contains(&cps[0]) || (context.is_empty() && cps.len() == 1)
            });
        }
        self.prefixes.clear();
        self.contexts.clear();
        let strings: Vec<(Vec<u32>, Vec<u32>)> = self
            .tables
            .iter()
            .flat_map(|(c, table)| table.keys().map(move |cps| (c.clone(), cps.clone())))
            .collect();
        for (context, cps) in strings {
            self.index(&context, &cps);
        }
    }

    /// The tailoring's own mappings: context (empty if none), string and elements.
    pub fn own(&self) -> impl Iterator<Item = (Vec<u32>, Vec<u32>, &[Ce])> + '_ {
        self.tables.iter().flat_map(|(context, table)| {
            table.iter().map(move |(cps, ces)| (context.clone(), cps.clone(), ces.as_slice()))
        })
    }

    /// Replaces every element of the tailoring's own mappings by `f` of it.
    pub fn map_own(&mut self, mut f: impl FnMut(Ce) -> Ce) {
        for ces in self.tables.values_mut().flat_map(HashMap::values_mut) {
            for ce in ces.iter_mut() {
                *ce = f(*ce);
            }
        }
    }

    /// Whether the base's contractions and contexts of strings starting with `cp` apply.
    fn base_on(&self, cp: u32) -> bool {
        !self.suppressed.iter().any(|(lo, hi)| (*lo..=*hi).contains(&cp))
    }
}

impl<M: Mappings<Element = Ce>> Mappings for Tailored<'_, M> {
    type Element = Ce;

    fn lookup(&self, context: &[u32], cps: &[u32]) -> Option<&[Ce]> {
        let own = self.tables.get(context).and_then(|t| t.get(cps)).map(Vec::as_slice);
        let plain_single = context.is_empty() && cps.len() == 1;
        own.or_else(|| {
            (plain_single || self.base_on(cps[0])).then(|| self.base.lookup(context, cps)).flatten()
        })
    }

    fn starts(&self, context: &[u32], cps: &[u32]) -> bool {
        self.prefixes.get(context).is_some_and(|set| set.contains(cps))
            || (self.base_on(cps[0]) && self.base.starts(context, cps))
    }

    fn contexts(&self, cp: u32) -> Vec<&[u32]> {
        let mut all: Vec<&[u32]> =
            self.contexts.get(&cp).map_or_else(Vec::new, |l| l.iter().map(Vec::as_slice).collect());
        if self.base_on(cp) {
            for c in self.base.contexts(cp) {
                if !all.contains(&c) {
                    all.push(c);
                }
            }
        }
        all.sort_by_key(|c| std::cmp::Reverse(c.len()));
        all
    }

    fn combining_class(&self, cp: u32) -> u8 {
        self.base.combining_class(cp)
    }

    fn implicit(&self, cp: u32) -> Ce {
        self.base.implicit(cp)
    }

    fn digit(&self, cp: u32) -> Option<u8> {
        // A digit the tailoring maps is not read as part of a number, as in ICU.
        let own = self.tables.get(&[][..]).is_some_and(|t| t.contains_key(&[cp][..]));
        if own { None } else { self.base.digit(cp) }
    }

    fn numeric(&self, p: u32) -> Ce {
        self.base.numeric(p)
    }

    fn numeric_primary(&self) -> u32 {
        self.base.numeric_primary()
    }
}
