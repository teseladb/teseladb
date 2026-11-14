//! The terms and posting lists of one text field of the segment in memory (011 FR-032).
//!
//! The terms live in a table of open addressing (linear probing over a power of two of slots,
//! at most half full), their texts one after another in one byte area. Each term's posting list
//! is a chain of 64-byte chunks taken from one pool: 4 bytes link to the next chunk and 60 hold
//! the list, so a list grows by linking a chunk and is never copied. The pool allocates its
//! chunks in slabs that stay where they are, so growing it moves no list either. A list holds one entry per
//! document, in increasing document order: the gap from the previous document, the number of
//! positions and the positions, each a variable-length integer (7 bits a byte) that may run on
//! into the next chunk.
//!
//! A deleted document stays in the lists, marked in a set of dead documents that the readers
//! skip, and each of its terms counts one document less; the terms of each document are kept to
//! find them. When the dead entries outnumber the live ones the table is written again without
//! them. Sorted iteration, as a dump needs it, sorts the terms when it is asked for.

use std::collections::BTreeMap;

/// Bytes of a chunk.
const CHUNK: usize = 64;
/// Bytes of the link to the next chunk at the start of each chunk.
const LINK: usize = 4;
/// Bytes of a chunk that hold the list.
const PAYLOAD: usize = CHUNK - LINK;
/// No chunk, no term.
const NONE: u32 = u32::MAX;
/// Slots of a table that has terms, at first.
const FIRST_SLOTS: usize = 16;
/// Chunks of a slab of the pool.
const SLAB: usize = 256;
/// Dead entries below which the table is not written again.
const COMPACT_FLOOR: usize = 1024;

/// A term of the table and the state of its list.
#[derive(Clone, Debug)]
struct Term {
    /// Where its text starts in `bytes`, and its length.
    start: u32,
    len: u32,
    hash: u64,
    /// First and last chunk of the list ([`NONE`] while empty), and bytes used in the last.
    head: u32,
    tail: u32,
    used: u32,
    /// Entries in the list, dead documents included.
    entries: u32,
    /// Entries of documents that are not dead.
    live: u32,
    /// Document of the last entry.
    last: u32,
}

/// The terms and posting lists of a field.
#[derive(Clone, Debug, Default)]
pub(crate) struct TermTable {
    slots: Vec<u32>,
    terms: Vec<Term>,
    bytes: Vec<u8>,
    chunks: Chunks,
    /// The terms of each document: its range of `doc_terms`.
    doc_terms: Vec<u32>,
    doc_ranges: BTreeMap<u32, (u32, u32)>,
    /// Bit `d`: document `d` is dead.
    dead: Vec<u64>,
    entries: usize,
    dead_entries: usize,
}

/// The chunks of a table, in slabs that never move once allocated.
#[derive(Clone, Debug, Default)]
struct Chunks {
    slabs: Vec<Box<[[u8; CHUNK]; SLAB]>>,
    len: usize,
}

impl Chunks {
    /// A new chunk without a next one; its number.
    fn push(&mut self) -> u32 {
        if self.len == self.slabs.len() * SLAB {
            self.slabs.push(Box::new([[0; CHUNK]; SLAB]));
        }
        let i = self.len;
        self.len += 1;
        self.get_mut(i as u32)[..LINK].copy_from_slice(&NONE.to_le_bytes());
        i as u32
    }

    fn get(&self, i: u32) -> &[u8; CHUNK] {
        &self.slabs[i as usize / SLAB][i as usize % SLAB]
    }

    fn get_mut(&mut self, i: u32) -> &mut [u8; CHUNK] {
        &mut self.slabs[i as usize / SLAB][i as usize % SLAB]
    }
}

/// FNV-1a of `bytes`.
fn hash(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

impl TermTable {
    /// The text of term `t`.
    fn text(&self, t: &Term) -> &str {
        let bytes = &self.bytes[t.start as usize..(t.start + t.len) as usize];
        // Only whole `&str` go into `bytes`.
        std::str::from_utf8(bytes).unwrap_or_default()
    }

    /// The slot of `term` (whose hash is `h`): the one that holds it, or the empty one where it
    /// would go.
    fn slot(&self, term: &str, h: u64) -> usize {
        let mask = self.slots.len() - 1;
        let mut i = (h as usize) & mask;
        loop {
            let id = self.slots[i];
            if id == NONE {
                return i;
            }
            let t = &self.terms[id as usize];
            if t.hash == h && self.text(t) == term {
                return i;
            }
            i = (i + 1) & mask;
        }
    }

    /// The number of `term`, if the table has it (also with no live document).
    fn find(&self, term: &str) -> Option<u32> {
        if self.slots.is_empty() {
            return None;
        }
        let id = self.slots[self.slot(term, hash(term.as_bytes()))];
        (id != NONE).then_some(id)
    }

    /// Doubles the slots and places every term again.
    fn grow(&mut self) {
        let n = (self.slots.len() * 2).max(FIRST_SLOTS);
        self.slots = vec![NONE; n];
        for (id, t) in self.terms.iter().enumerate() {
            let mut i = (t.hash as usize) & (n - 1);
            while self.slots[i] != NONE {
                i = (i + 1) & (n - 1);
            }
            self.slots[i] = id as u32;
        }
    }

    /// The number of `term`, added if the table lacks it. The text is copied only then.
    pub(crate) fn intern(&mut self, term: &str) -> u32 {
        if (self.terms.len() + 1) * 2 > self.slots.len() {
            self.grow();
        }
        let h = hash(term.as_bytes());
        let i = self.slot(term, h);
        if self.slots[i] != NONE {
            return self.slots[i];
        }
        let id = self.terms.len() as u32;
        self.terms.push(Term {
            start: self.bytes.len() as u32,
            len: term.len() as u32,
            hash: h,
            head: NONE,
            tail: NONE,
            used: 0,
            entries: 0,
            live: 0,
            last: 0,
        });
        self.bytes.extend_from_slice(term.as_bytes());
        self.slots[i] = id;
        id
    }

    /// Appends `b` to the list of term `t`, linking a chunk when the last one is full.
    fn push_byte(&mut self, t: usize, b: u8) {
        let term = &self.terms[t];
        if term.head == NONE || term.used as usize == PAYLOAD {
            let new = self.chunks.push();
            let term = &mut self.terms[t];
            if term.head == NONE {
                term.head = new;
            } else {
                self.chunks.get_mut(term.tail)[..LINK].copy_from_slice(&new.to_le_bytes());
            }
            term.tail = new;
            term.used = 0;
        }
        let term = &mut self.terms[t];
        self.chunks.get_mut(term.tail)[LINK + term.used as usize] = b;
        term.used += 1;
    }

    fn push_varint(&mut self, t: usize, mut v: u32) {
        while v >= 0x80 {
            self.push_byte(t, (v as u8) | 0x80);
            v >>= 7;
        }
        self.push_byte(t, v as u8);
    }

    /// Appends the entry of `doc`, after every document already in the list of term `t`.
    fn append(&mut self, t: u32, doc: u32, positions: impl ExactSizeIterator<Item = u32>) {
        let i = t as usize;
        let gap = doc - self.terms[i].last;
        self.push_varint(i, gap);
        self.push_varint(i, positions.len() as u32);
        for p in positions {
            self.push_varint(i, p);
        }
        let term = &mut self.terms[i];
        term.last = doc;
        term.entries += 1;
        term.live += 1;
        self.entries += 1;
    }

    /// Adds document `doc`, newer than every document of the table, with its `(term, position)`
    /// pairs in the order they came; `pending` is left empty.
    pub(crate) fn add_doc(&mut self, doc: u32, pending: &mut Vec<(u32, u32)>) {
        if pending.is_empty() {
            return;
        }
        // Stable: each term keeps its positions in the order they came.
        pending.sort_by_key(|&(t, _)| t);
        let start = self.doc_terms.len() as u32;
        let mut i = 0;
        while i < pending.len() {
            let t = pending[i].0;
            let j = i + pending[i..].iter().take_while(|&&(u, _)| u == t).count();
            self.append(t, doc, pending[i..j].iter().map(|&(_, p)| p));
            self.doc_terms.push(t);
            i = j;
        }
        let end = self.doc_terms.len() as u32;
        self.doc_ranges.insert(doc, (start, end - start));
        pending.clear();
    }

    /// Whether document `doc` is dead.
    fn is_dead(&self, doc: u32) -> bool {
        self.dead.get(doc as usize / 64).is_some_and(|w| w & (1 << (doc % 64)) != 0)
    }

    /// Takes document `doc` out of every list; the table is written again once dead entries
    /// outnumber the live ones.
    pub(crate) fn delete(&mut self, doc: u32) {
        let Some((start, len)) = self.doc_ranges.remove(&doc) else { return };
        for k in start..start + len {
            let t = self.doc_terms[k as usize] as usize;
            self.terms[t].live -= 1;
        }
        let word = doc as usize / 64;
        if self.dead.len() <= word {
            self.dead.resize(word + 1, 0);
        }
        self.dead[word] |= 1 << (doc % 64);
        self.dead_entries += len as usize;
        if self.dead_entries >= COMPACT_FLOOR && self.dead_entries * 2 > self.entries {
            self.compact();
        }
    }

    /// The table written again with its live entries only.
    fn compact(&mut self) {
        let lists: Vec<(String, Vec<(u32, Vec<u32>)>)> = self
            .iter()
            .map(|(term, list)| {
                (term.to_owned(), list.iter().map(|(d, ps)| (d, ps.collect())).collect())
            })
            .collect();
        *self = TermTable::from_lists(lists);
    }

    /// A table with the lists of `lists`: each term with its documents in increasing order.
    pub(crate) fn from_lists(
        lists: impl IntoIterator<Item = (String, Vec<(u32, Vec<u32>)>)>,
    ) -> TermTable {
        let mut table = TermTable::default();
        let mut by_doc: Vec<(u32, u32)> = Vec::new();
        for (term, docs) in lists {
            if docs.is_empty() {
                continue;
            }
            let t = table.intern(&term);
            for (doc, positions) in docs {
                table.append(t, doc, positions.into_iter());
                by_doc.push((doc, t));
            }
        }
        // Stable: each document keeps its terms in the order of their numbers.
        by_doc.sort_by_key(|&(d, _)| d);
        let mut i = 0;
        while i < by_doc.len() {
            let doc = by_doc[i].0;
            let start = table.doc_terms.len() as u32;
            while i < by_doc.len() && by_doc[i].0 == doc {
                table.doc_terms.push(by_doc[i].1);
                i += 1;
            }
            let len = table.doc_terms.len() as u32 - start;
            table.doc_ranges.insert(doc, (start, len));
        }
        table
    }

    /// The posting list of `term`, if a live document has it.
    pub(crate) fn get(&self, term: &str) -> Option<Postings<'_>> {
        let id = self.find(term)?;
        let t = &self.terms[id as usize];
        (t.live > 0).then_some(Postings { table: self, term: t })
    }

    /// The terms with a live document and their lists, in the order the terms came.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&str, Postings<'_>)> + '_ {
        self.terms
            .iter()
            .filter(|t| t.live > 0)
            .map(|t| (self.text(t), Postings { table: self, term: t }))
    }

    /// [`TermTable::iter`] in term order.
    pub(crate) fn sorted(&self) -> Vec<(&str, Postings<'_>)> {
        let mut terms: Vec<(&str, Postings<'_>)> = self.iter().collect();
        terms.sort_unstable_by(|a, b| a.0.cmp(b.0));
        terms
    }

    /// The bytes the table takes: its terms, slots, chunks and the terms of each document.
    pub(crate) fn memory_bytes(&self) -> usize {
        self.bytes.len()
            + 4 * self.slots.len()
            + std::mem::size_of::<Term>() * self.terms.len()
            + CHUNK * SLAB * self.chunks.slabs.len()
            + 4 * self.doc_terms.len()
            + 12 * self.doc_ranges.len()
            + 8 * self.dead.len()
    }
}

/// Reads the bytes of a list across its chunks.
#[derive(Clone, Debug)]
struct Reader<'a> {
    chunks: &'a Chunks,
    chunk: u32,
    at: usize,
}

impl Reader<'_> {
    fn byte(&mut self) -> u8 {
        if self.at == PAYLOAD {
            let link = &self.chunks.get(self.chunk)[..LINK];
            self.chunk = u32::from_le_bytes([link[0], link[1], link[2], link[3]]);
            self.at = 0;
        }
        let b = self.chunks.get(self.chunk)[LINK + self.at];
        self.at += 1;
        b
    }

    fn varint(&mut self) -> u32 {
        let mut v = 0u32;
        let mut shift = 0;
        loop {
            let b = self.byte();
            v |= u32::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return v;
            }
            shift += 7;
        }
    }
}

/// The posting list of a term: its live documents, each with its positions.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Postings<'a> {
    table: &'a TermTable,
    term: &'a Term,
}

impl<'a> Postings<'a> {
    /// The number of live documents.
    pub(crate) fn len(&self) -> usize {
        self.term.live as usize
    }

    /// The live documents in increasing order, each with its positions.
    pub(crate) fn iter(&self) -> Entries<'a> {
        Entries {
            table: self.table,
            reader: Reader { chunks: &self.table.chunks, chunk: self.term.head, at: 0 },
            left: self.term.entries,
            doc: 0,
        }
    }

    /// The positions of `doc`, if it has the term.
    pub(crate) fn get(&self, doc: u32) -> Option<Positions<'a>> {
        self.iter().take_while(|(d, _)| *d <= doc).find(|(d, _)| *d == doc).map(|(_, ps)| ps)
    }

    /// A reader of the list for documents asked in increasing order, each found without going
    /// back to the start.
    pub(crate) fn cursor(&self) -> Cursor<'a> {
        let mut entries = self.iter();
        let head = entries.next();
        Cursor { entries, head }
    }

    /// The documents, in increasing order.
    pub(crate) fn docs(self) -> impl Iterator<Item = u32> + 'a {
        self.iter().map(|(d, _)| d)
    }
}

impl<'a> IntoIterator for Postings<'a> {
    type Item = (u32, Positions<'a>);
    type IntoIter = Entries<'a>;

    fn into_iter(self) -> Entries<'a> {
        self.iter()
    }
}

/// The entries of a list: see [`Postings::iter`].
#[derive(Clone, Debug)]
pub(crate) struct Entries<'a> {
    table: &'a TermTable,
    reader: Reader<'a>,
    left: u32,
    doc: u32,
}

/// Reads a list for documents asked in increasing order: see [`Postings::cursor`].
#[derive(Clone, Debug)]
pub(crate) struct Cursor<'a> {
    entries: Entries<'a>,
    head: Option<(u32, Positions<'a>)>,
}

impl<'a> Cursor<'a> {
    /// The positions of `doc`, if it has the term; `doc` is never below one asked before.
    pub(crate) fn at(&mut self, doc: u32) -> Option<Positions<'a>> {
        while self.head.as_ref().is_some_and(|(d, _)| *d < doc) {
            self.head = self.entries.next();
        }
        match &self.head {
            Some((d, ps)) if *d == doc => Some(ps.clone()),
            _ => None,
        }
    }
}

impl<'a> Iterator for Entries<'a> {
    type Item = (u32, Positions<'a>);

    fn next(&mut self) -> Option<(u32, Positions<'a>)> {
        while self.left > 0 {
            self.left -= 1;
            self.doc += self.reader.varint();
            let n = self.reader.varint();
            let positions = Positions { reader: self.reader.clone(), left: n };
            for _ in 0..n {
                self.reader.varint();
            }
            if !self.table.is_dead(self.doc) {
                return Some((self.doc, positions));
            }
        }
        None
    }
}

/// The positions of a term in one document, in the order they were indexed.
#[derive(Clone, Debug)]
pub(crate) struct Positions<'a> {
    reader: Reader<'a>,
    left: u32,
}

impl Iterator for Positions<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        Some(self.reader.varint())
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.left as usize, Some(self.left as usize))
    }
}

impl ExactSizeIterator for Positions<'_> {}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    type Lists = BTreeMap<String, BTreeMap<u32, Vec<u32>>>;

    fn lists(t: &TermTable) -> Lists {
        t.iter()
            .map(|(term, p)| (term.to_owned(), p.iter().map(|(d, ps)| (d, ps.collect())).collect()))
            .collect()
    }

    /// A deterministic sequence of numbers.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }
    }

    #[test]
    fn the_table_holds_what_a_map_holds() {
        for seed in 0..40 {
            let mut rng = Rng(seed);
            let mut table = TermTable::default();
            let mut model: Lists = BTreeMap::new();
            let mut pending = Vec::new();
            let terms = 1 + rng.next(300);
            for doc in 0..200u32 {
                if doc > 0 && rng.next(4) == 0 {
                    let gone = rng.next(u64::from(doc)) as u32;
                    table.delete(gone);
                    for docs in model.values_mut() {
                        docs.remove(&gone);
                    }
                    model.retain(|_, d| !d.is_empty());
                }
                for _ in 0..rng.next(40) {
                    let term = format!("t{}", rng.next(terms));
                    // Positions as large as 2^28, to take every length of a variable integer.
                    let position = (rng.next(1 << 28) >> (rng.next(4) * 7)) as u32;
                    let id = table.intern(&term);
                    pending.push((id, position));
                    model.entry(term).or_default().entry(doc).or_default().push(position);
                }
                table.add_doc(doc, &mut pending);
                assert!(pending.is_empty());
            }
            assert_eq!(lists(&table), model, "seed {seed}");
            for (term, docs) in &model {
                let p = table.get(term).unwrap();
                assert_eq!(p.len(), docs.len());
                for (&doc, ps) in docs {
                    assert_eq!(p.get(doc).unwrap().collect::<Vec<_>>(), *ps);
                }
                assert!(p.get(u32::MAX).is_none());
                let mut cursor = p.cursor();
                for doc in 0..200 {
                    let found = cursor.at(doc).map(|ps| ps.collect::<Vec<_>>());
                    assert_eq!(found.as_ref(), docs.get(&doc), "{term} {doc}");
                }
                assert_eq!(p.into_iter().count(), docs.len());
            }
            let sorted: Vec<&str> = table.sorted().into_iter().map(|(t, _)| t).collect();
            assert_eq!(sorted, model.keys().map(String::as_str).collect::<Vec<_>>());
            assert_eq!(table.iter().next().is_none(), model.is_empty());
            let rebuilt = TermTable::from_lists(
                model
                    .iter()
                    .map(|(t, d)| (t.clone(), d.iter().map(|(&d, p)| (d, p.clone())).collect())),
            );
            assert_eq!(lists(&rebuilt), model);
        }
    }

    #[test]
    fn deletes_write_the_table_again_without_dead_entries() {
        let mut table = TermTable::default();
        let mut pending = Vec::new();
        for doc in 0..3000u32 {
            let id = table.intern(if doc % 2 == 0 { "even" } else { "odd" });
            pending.push((id, doc));
            table.add_doc(doc, &mut pending);
        }
        let before = table.chunks.len;
        for doc in 0..2500 {
            table.delete(doc);
        }
        table.delete(0);
        assert!(table.chunks.len < before);
        assert_eq!(table.get("even").unwrap().len(), 250);
        assert_eq!(table.get("odd").unwrap().docs().next(), Some(2501));
        assert!(table.memory_bytes() > 0);
        for doc in 2500..3000 {
            table.delete(doc);
        }
        assert!(table.iter().next().is_none());
        assert!(table.get("odd").is_none());
        assert!(table.find("odd").is_some() || table.terms.is_empty());
    }

    #[test]
    fn lists_grow_by_linking_chunks() {
        let mut table = TermTable::default();
        let id = table.intern("x");
        let mut pending: Vec<(u32, u32)> = (0..100).map(|p| (id, 1000 + p)).collect();
        table.add_doc(7, &mut pending);
        let term = &table.terms[0];
        assert!(table.chunks.len > 1);
        assert_ne!(term.head, term.tail);
        let positions: Vec<u32> = table.get("x").unwrap().get(7).unwrap().collect();
        assert_eq!(positions, (1000..1100).collect::<Vec<_>>());
        // Chunks never move: the first one stays where it was while slabs are added.
        let first = table.chunks.get(0).as_ptr();
        let mut doc = 8;
        while table.chunks.len < 3 * SLAB {
            let id = table.intern(&format!("t{doc}"));
            pending.extend((0..20).map(|p| (id, p)));
            table.add_doc(doc, &mut pending);
            doc += 1;
        }
        assert_eq!(table.chunks.get(0).as_ptr(), first);
        assert_eq!(table.get("x").unwrap().get(7).unwrap().count(), 100);
        let mut empty = Vec::new();
        table.add_doc(doc, &mut empty);
        assert!(table.get("y").is_none());
        assert!(TermTable::default().get("x").is_none());
    }
}
