//! Sets of documents by their local identifiers (009 FR-023), as a plan of several index scans
//! builds them from the entries it reads: each entry gives its document's primary key and, when
//! the range numbered the document, its local identifier.
//!
//! Two documents of one range may carry the same number after a merge joined two ranges that
//! numbered their documents apart. A number in a set therefore says that some document with that
//! number is in it: [`Held`] answers "maybe" and the plan reads the document and checks its whole
//! filter, and [`Seen`] keeps the primary keys behind each number so that it never takes one
//! document for another. A document without a number is kept by its primary key.

use std::collections::{BTreeSet, HashMap};

use crate::roaring::Roaring;

/// The documents of the entries of one scan, for an intersection (`$and`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Held {
    ids: Roaring,
    /// The primary keys of the documents without a number.
    unnumbered: BTreeSet<Vec<u8>>,
}

impl Held {
    /// The set of the documents of `entries` (primary key and local identifier).
    pub fn of<'a>(entries: impl IntoIterator<Item = (&'a [u8], Option<u32>)>) -> Held {
        let mut ids = Vec::new();
        let mut unnumbered = BTreeSet::new();
        for (pk, id) in entries {
            match id {
                Some(id) => ids.push(id),
                None => {
                    unnumbered.insert(pk.to_vec());
                }
            }
        }
        Held { ids: Roaring::from_ids(ids), unnumbered }
    }

    /// Whether the document of primary key `pk` and number `id` may be in the set: certainly not
    /// when it says no. A document without a number may be in it under the number it has in the
    /// set, which the entry does not say.
    pub fn may_hold(&self, pk: &[u8], id: Option<u32>) -> bool {
        match id {
            Some(id) => self.ids.contains(id) || self.unnumbered.contains(pk),
            None => true,
        }
    }

    /// Whether every document of the set has a number.
    pub fn all_numbered(&self) -> bool {
        self.unnumbered.is_empty()
    }

    /// The numbers of the set.
    pub fn ids(&self) -> &Roaring {
        &self.ids
    }
}

/// The documents a plan has given, so that a union (`$or`, `$in`, a multikey scan) gives each once.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Seen {
    ids: Roaring,
    /// The primary keys behind each number given.
    keys: HashMap<u32, Vec<Vec<u8>>>,
    unnumbered: BTreeSet<Vec<u8>>,
    /// The primary keys of the numbered documents, kept from the first document without a
    /// number on: one document may have a number in the entries of one index and none in those
    /// of another, whose fields its last write did not change.
    numbered: std::cell::OnceCell<BTreeSet<Vec<u8>>>,
}

impl Seen {
    /// The empty set.
    pub fn new() -> Seen {
        Seen::default()
    }

    /// Whether the document of primary key `pk` and number `id` is in the set.
    pub fn contains(&self, pk: &[u8], id: Option<u32>) -> bool {
        if self.unnumbered.contains(pk) {
            return true;
        }
        match id {
            Some(id) => {
                self.ids.contains(id)
                    && self.keys.get(&id).is_some_and(|ks| ks.iter().any(|k| k == pk))
            }
            None => self
                .numbered
                .get_or_init(|| self.keys.values().flatten().cloned().collect())
                .contains(pk),
        }
    }

    /// Adds the document; whether it was not in the set yet.
    pub fn insert(&mut self, pk: &[u8], id: Option<u32>) -> bool {
        if self.contains(pk, id) {
            return false;
        }
        match id {
            Some(id) => {
                self.ids.insert(id);
                self.keys.entry(id).or_default().push(pk.to_vec());
                if let Some(n) = self.numbered.get_mut() {
                    n.insert(pk.to_vec());
                }
            }
            None => {
                self.unnumbered.insert(pk.to_vec());
            }
        }
        true
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    /// A range's documents: primary key and number, some unnumbered, some numbers repeated as
    /// after a merge.
    fn documents(rng: &mut Rng) -> Vec<(Vec<u8>, Option<u32>)> {
        let n = rng.below(3000) + 1;
        (0..n)
            .map(|i| {
                let pk = i.to_be_bytes().to_vec();
                let id = match rng.below(10) {
                    0 => None,
                    1 => Some(rng.below(n) as u32),
                    _ => Some(i as u32),
                };
                (pk, id)
            })
            .collect()
    }

    fn subset(rng: &mut Rng, docs: &[(Vec<u8>, Option<u32>)]) -> Vec<usize> {
        let keep = rng.below(4) + 1;
        (0..docs.len()).filter(|_| rng.below(keep + 1) == 0).collect()
    }

    #[test]
    fn an_intersection_keeps_every_document_in_both_scans() {
        let mut rng = Rng(0x0009_F023_0000_0001);
        for _ in 0..80 {
            let docs = documents(&mut rng);
            let (a, b) = (subset(&mut rng, &docs), subset(&mut rng, &docs));
            let held = Held::of(a.iter().map(|&i| (docs[i].0.as_slice(), docs[i].1)));
            let in_a: BTreeSet<usize> = a.iter().copied().collect();
            for &i in &b {
                let (pk, id) = (&docs[i].0, docs[i].1);
                // No document of both is ever missed; a document that is not in `a` is kept
                // only when its number is or it has none.
                if in_a.contains(&i) {
                    assert!(held.may_hold(pk, id), "document {i}");
                } else if held.may_hold(pk, id) {
                    let shares = id.is_none_or(|id| a.iter().any(|&j| docs[j].1 == Some(id)));
                    assert!(shares, "document {i} kept without reason");
                }
            }
            assert_eq!(held.all_numbered(), a.iter().all(|&i| docs[i].1.is_some()));
        }
    }

    #[test]
    fn a_union_gives_each_document_once_even_with_repeated_numbers() {
        let mut rng = Rng(0x0009_F023_0000_0002);
        for _ in 0..80 {
            let docs = documents(&mut rng);
            let mut seen = Seen::new();
            let mut want = BTreeSet::new();
            for _ in 0..rng.below(6000) {
                let i = rng.below(docs.len() as u64) as usize;
                // An entry of another index may carry no number for a numbered document.
                let id = docs[i].1.filter(|_| rng.below(8) != 0);
                let pk = &docs[i].0;
                assert_eq!(seen.contains(pk, id), want.contains(&i), "document {i}");
                assert_eq!(seen.insert(pk, id), want.insert(i), "document {i}");
                assert!(seen.contains(pk, id));
            }
        }
    }
}
