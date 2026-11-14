//! The live documents of each collection a range holds (007 FR-039, D-149, H-004), with the
//! bytes of their BSON and of their keys: what `count` without a filter, `estimatedDocumentCount`,
//! `collStats` and `$collStats` add up instead of reading every document.
//!
//! A live document is a version of a key of the documents of a collection (index
//! [`DOCUMENTS`](teseladb_keys::space::DOCUMENTS) of the data space) whose value is a document
//! ([`DOCUMENT_VALUE`]); a version that only serves the history ([`crate::HISTORY_VALUE`]), a
//! change meta or a capped collection's state is none. The leader computes the change a write
//! makes from the versions it read under latches, as it computes the change of the live bytes.

use std::collections::BTreeMap;

use teseladb_keys::space::{DOCUMENTS, KeyParts};

use crate::command::Put;

pub use teseladb_lsm::DOCUMENT_VALUE;

/// A collection: tenant and collection number.
pub type Coll = (u32, u32);

/// The bytes of a collection's live documents: of their BSON (the table of offsets is not data,
/// 003 FR-021) and of their keys.
pub type Size = (u64, u64);

/// The change a write makes to a collection's live documents and, from the commands of
/// [`crate::command::FORMAT_SIZED`] on, to their bytes of BSON and of keys.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Change {
    /// Documents added (positive) or removed.
    pub docs: i64,
    /// Bytes of BSON and of keys added or removed; `None` in a command of an earlier format.
    pub size: Option<(i64, i64)>,
}

impl Change {
    /// A change of `docs` documents that says nothing of their bytes.
    pub fn docs(docs: i64) -> Change {
        Change { docs, size: None }
    }

    /// A change of `docs` documents, `bytes` bytes of BSON and `keys` bytes of keys.
    pub fn sized(docs: i64, bytes: i64, keys: i64) -> Change {
        Change { docs, size: Some((bytes, keys)) }
    }
}

/// The bytes of BSON of a stored document's value.
fn bson_len(value: &[u8]) -> i64 {
    let len = teseladb_keys::stored::bson(value).map_or(value.len(), <[u8]>::len);
    i64::try_from(len).unwrap_or(i64::MAX)
}

fn key_len(key: &[u8]) -> i64 {
    i64::try_from(key.len()).unwrap_or(i64::MAX)
}

/// The collection whose document `key` is, if it is a document's key.
pub fn document_of(key: &[u8]) -> Option<Coll> {
    match KeyParts::of(key).ok()? {
        KeyParts::Data { tenant, collection, index: DOCUMENTS, .. } => Some((tenant, collection)),
        _ => None,
    }
}

/// The collection of a live document stored at `key` with a value of type `value_type`.
pub fn live_document(key: &[u8], value_type: u8) -> Option<Coll> {
    (value_type == DOCUMENT_VALUE).then(|| document_of(key)).flatten()
}

/// The change of each collection's live documents that a write makes: one more for each put of
/// a document where there was none, one less for each document a put or a delete replaces with
/// something else, and with `sized` the bytes of BSON and of keys of those documents. `current`
/// holds the versions the leader read, in the order of `read`; a key it did not read had no
/// version. Collections whose documents and bytes do not change are left out.
pub fn doc_delta(
    read: &[Vec<u8>],
    current: &[Option<(u8, &[u8])>],
    (puts, deletes): (&[Put], &[Vec<u8>]),
    sized: bool,
) -> Vec<(Coll, Change)> {
    let old = |k: &[u8]| {
        let at = read.iter().position(|r| r.as_slice() == k);
        let (t, v) = at.and_then(|i| current.get(i).copied().flatten())?;
        live_document(k, t).map(|c| (c, bson_len(v)))
    };
    let mut delta: BTreeMap<Coll, (i64, i64, i64)> = BTreeMap::new();
    let mut add = |c: Option<(Coll, i64)>, key: &[u8], sign: i64| {
        if let Some((c, bytes)) = c {
            let d = delta.entry(c).or_default();
            d.0 += sign;
            d.1 += sign * bytes;
            d.2 += sign * key_len(key);
        }
    };
    for p in puts {
        let new = live_document(&p.key, p.value_type).map(|c| (c, bson_len(&p.value)));
        add(new, &p.key, 1);
        add(old(&p.key), &p.key, -1);
    }
    for k in deletes {
        add(old(k), k, -1);
    }
    delta
        .into_iter()
        .filter(|(_, (n, b, k))| *n != 0 || (sized && (*b != 0 || *k != 0)))
        .map(|(c, (n, b, k))| (c, if sized { Change::sized(n, b, k) } else { Change::docs(n) }))
        .collect()
}

/// Whether a write of `puts` and `deletes` may change a collection's live documents: it writes a
/// document's key. A write that carries no change of the counts and does this leaves them estimated.
pub fn touches_documents(puts: &[Put], deletes: &[Vec<u8>]) -> bool {
    puts.iter()
        .map(|p| p.key.as_slice())
        .chain(deletes.iter().map(Vec::as_slice))
        .any(|k| document_of(k).is_some())
}

/// A range's live documents of each collection and their bytes, the index of the last entry that
/// changed documents without saying by how many (0: none since they were exact), and the one
/// that changed them without saying their bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Live documents of each collection; a collection without documents is not listed.
    pub docs: BTreeMap<Coll, u64>,
    /// The last entry that left them estimated; 0 while they are exact.
    pub dirty_at: u64,
    /// The bytes of BSON and of keys of the live documents of each collection.
    pub sizes: BTreeMap<Coll, Size>,
    /// The last entry that left the bytes estimated (or the documents: every change of the
    /// estimated documents leaves the bytes estimated too); 0 while they are exact.
    pub unsized_at: u64,
}

impl Counts {
    /// Counts that are not known: estimated since entry `index`.
    pub fn unknown(index: u64) -> Counts {
        Counts { dirty_at: index.max(1), unsized_at: index.max(1), ..Counts::default() }
    }

    /// Whether they are exact.
    pub fn exact(&self) -> bool {
        self.dirty_at == 0
    }

    /// Whether the bytes are exact too.
    pub fn sized(&self) -> bool {
        self.dirty_at == 0 && self.unsized_at == 0
    }

    /// Adds the change of write `index`. A count never goes below zero: a change computed over
    /// counts that were estimated cannot wrap it. A change that does not say its bytes leaves
    /// the bytes estimated from `index`.
    pub fn apply(&mut self, delta: &[(Coll, Change)], index: u64) {
        for (c, change) in delta {
            let now = self.docs.get(c).copied().unwrap_or(0).saturating_add_signed(change.docs);
            match now {
                0 => self.docs.remove(c),
                _ => self.docs.insert(*c, now),
            };
            let Some((bytes, keys)) = change.size else {
                self.unsized_at = self.unsized_at.max(index).max(1);
                continue;
            };
            let (b, k) = self.sizes.get(c).copied().unwrap_or_default();
            let size = (b.saturating_add_signed(bytes), k.saturating_add_signed(keys));
            match (now, size) {
                (0, _) | (_, (0, 0)) => self.sizes.remove(c),
                _ => self.sizes.insert(*c, size),
            };
        }
    }

    /// Entry `index` changed documents without its change: the counts are estimated from it.
    pub fn dirty(&mut self, index: u64) {
        self.dirty_at = self.dirty_at.max(index).max(1);
        self.unsized_at = self.unsized_at.max(index).max(1);
    }

    /// The bytes of the local key that keeps them: the documents of each collection, then the
    /// entry that left the bytes estimated and the bytes of each collection. The counts of an
    /// earlier binary end after the documents.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + self.docs.len() * 16 + self.sizes.len() * 24);
        out.extend_from_slice(&self.dirty_at.to_be_bytes());
        out.extend_from_slice(&(self.docs.len() as u64).to_be_bytes());
        for ((tenant, collection), n) in &self.docs {
            out.extend_from_slice(&tenant.to_be_bytes());
            out.extend_from_slice(&collection.to_be_bytes());
            out.extend_from_slice(&n.to_be_bytes());
        }
        out.extend_from_slice(&self.unsized_at.to_be_bytes());
        out.extend_from_slice(&(self.sizes.len() as u64).to_be_bytes());
        for ((tenant, collection), (bytes, keys)) in &self.sizes {
            out.extend_from_slice(&tenant.to_be_bytes());
            out.extend_from_slice(&collection.to_be_bytes());
            out.extend_from_slice(&bytes.to_be_bytes());
            out.extend_from_slice(&keys.to_be_bytes());
        }
        out
    }

    /// The counts of the bytes of their local key; `None` if they are not such bytes. Counts an
    /// earlier binary wrote have no bytes: those are estimated.
    pub fn decode(v: &[u8]) -> Option<Counts> {
        let word = |at: usize| Some(u64::from_be_bytes(v.get(at..at + 8)?.try_into().ok()?));
        let half = |at: usize| Some(u32::from_be_bytes(v.get(at..at + 4)?.try_into().ok()?));
        let (dirty_at, n) = (word(0)?, usize::try_from(word(8)?).ok()?);
        let sized_at = 16usize.checked_add(n.checked_mul(16)?)?;
        let mut docs = BTreeMap::new();
        for i in 0..n {
            let at = 16 + i * 16;
            docs.insert((half(at)?, half(at + 4)?), word(at + 8)?);
        }
        if v.len() == sized_at {
            return Some(Counts { docs, dirty_at, sizes: BTreeMap::new(), unsized_at: 1 });
        }
        let (unsized_at, m) = (word(sized_at)?, usize::try_from(word(sized_at + 8)?).ok()?);
        if v.len() != sized_at.checked_add(16)?.checked_add(m.checked_mul(24)?)? {
            return None;
        }
        let mut sizes = BTreeMap::new();
        for i in 0..m {
            let at = sized_at + 16 + i * 24;
            sizes.insert((half(at)?, half(at + 4)?), (word(at + 8)?, word(at + 16)?));
        }
        Some(Counts { docs, dirty_at, sizes, unsized_at })
    }
}

/// Versions a recount passes in one step (007 FR-039): its leader takes a step at each poll, and
/// the requests of its core go on between two. A recount that walked its range in one step held
/// the core for up to 31 ms (H-060 of the native tests).
pub const RECOUNT_STEP: usize = 512;

/// A recount of the live documents of each collection in `[start, end)` at the newest versions,
/// [`RECOUNT_STEP`] versions at a time, as the leader runs it for
/// [`crate::command::Command::AdjustCounts`].
pub struct DocScan {
    cursor: teseladb_lsm::Scan,
    docs: BTreeMap<Coll, u64>,
    sizes: BTreeMap<Coll, Size>,
}

/// How a [`DocScan`] ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Recount {
    /// The live documents of each collection, and their bytes of BSON and of keys.
    Counted(BTreeMap<Coll, u64>, BTreeMap<Coll, Size>),
    /// An intent of a transaction lies on a document: what is below it is not known from the
    /// scan, and the leader counts again later.
    Intent,
    /// The LSM failed.
    Failed,
}

impl DocScan {
    /// A recount of `[start, end)`.
    pub fn new(lsm: &std::cell::RefCell<teseladb_lsm::Lsm>, start: &[u8], end: &[u8]) -> DocScan {
        let newest = teseladb_hlc::Hlc { physical: u64::MAX, logical: u32::MAX };
        let cursor = lsm.borrow().view().scan(start, end, newest);
        DocScan { cursor, docs: BTreeMap::new(), sizes: BTreeMap::new() }
    }

    /// Moves the recount on by at most [`RECOUNT_STEP`] versions: how it ended, or `None` while
    /// a block is being read or there are versions left for its next step. The stored value of
    /// a version is its type followed by its value.
    pub fn step(&mut self, lsm: &std::cell::RefCell<teseladb_lsm::Lsm>) -> Option<Recount> {
        for _ in 0..RECOUNT_STEP {
            match self.cursor.next(&mut lsm.borrow_mut()) {
                teseladb_lsm::Step::Item(i) => {
                    let Some(coll) = document_of(&i.key) else { continue };
                    if i.kind == teseladb_lsm::Kind::Intent {
                        return Some(Recount::Intent);
                    }
                    if i.value.first() == Some(&DOCUMENT_VALUE) {
                        *self.docs.entry(coll).or_default() += 1;
                        let size = self.sizes.entry(coll).or_default();
                        size.0 += bson_len(&i.value[1..]).unsigned_abs();
                        size.1 += key_len(&i.key).unsigned_abs();
                    }
                }
                teseladb_lsm::Step::Done => {
                    let docs = std::mem::take(&mut self.docs);
                    return Some(Recount::Counted(docs, std::mem::take(&mut self.sizes)));
                }
                teseladb_lsm::Step::Miss(_) => return None,
                teseladb_lsm::Step::Failed(_) => return Some(Recount::Failed),
            }
        }
        None
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_keys::space::KeyBuilder;

    use super::*;

    fn doc(c: u32, pk: &[u8]) -> Vec<u8> {
        KeyBuilder::document_of_pk(1, c, pk).as_bytes().to_vec()
    }

    fn put(key: &[u8], value_type: u8) -> Put {
        Put { key: key.to_vec(), value_type, value: b"v".to_vec() }
    }

    #[test]
    fn inserts_replacements_and_deletions_change_the_count_of_their_collection() {
        let (a, b, c) = (doc(7, b"a"), doc(7, b"b"), doc(8, b"c"));
        let index = KeyBuilder::index_prefix(1, 7, 1).as_bytes().to_vec();
        let read = vec![a.clone(), b.clone(), c.clone()];
        let old = [Some((DOCUMENT_VALUE, &b"x"[..])), None, Some((DOCUMENT_VALUE, &b"y"[..]))];
        // `a` replaced, `b` inserted, `c` deleted, and an index entry that is no document.
        let puts = [put(&a, DOCUMENT_VALUE), put(&b, DOCUMENT_VALUE), put(&index, 2)];
        let got = doc_delta(&read, &old, (&puts, std::slice::from_ref(&c)), false);
        assert_eq!(got, vec![((1, 7), Change::docs(1)), ((1, 8), Change::docs(-1))]);
        // With their bytes: `a` replaced one byte with one, `b` added one, `c` took one away.
        let got = doc_delta(&read, &old, (&puts, std::slice::from_ref(&c)), true);
        let (kb, kc) = (key_len(&b), key_len(&c));
        assert_eq!(
            got,
            vec![((1, 7), Change::sized(1, 1, kb)), ((1, 8), Change::sized(-1, -1, -kc))]
        );
        assert!(touches_documents(&puts, &[]));
        assert!(!touches_documents(&[put(&index, 2)], &[]));
    }

    #[test]
    fn counts_add_changes_and_read_back_from_their_bytes() {
        let mut c = Counts::default();
        c.apply(&[((1, 7), Change::sized(3, 300, 30)), ((1, 8), Change::sized(1, 10, 1))], 1);
        c.apply(&[((1, 8), Change::sized(-1, -10, -1)), ((1, 9), Change::sized(-2, -5, -5))], 2);
        assert_eq!(c.docs, BTreeMap::from([((1, 7), 3)]), "none below zero, none listed at zero");
        assert_eq!(c.sizes, BTreeMap::from([((1, 7), (300, 30))]));
        assert!(c.sized());
        assert_eq!(Counts::decode(&c.encode()), Some(c.clone()));
        // A change without its bytes leaves the bytes estimated, not the documents.
        c.apply(&[((1, 7), Change::docs(1))], 9);
        assert!(c.exact() && !c.sized());
        assert_eq!(c.unsized_at, 9);
        c.dirty(12);
        c.dirty(5);
        assert_eq!((c.dirty_at, c.unsized_at), (12, 12));
        assert_eq!(Counts::decode(&c.encode()), Some(c.clone()));
        assert_eq!(Counts::decode(&c.encode()[..20]), None);
        assert_eq!(Counts::unknown(0).dirty_at, 1);
        // The counts of an earlier binary end after the documents: their bytes are estimated.
        let docs_only = c.encode()[..16 + 16].to_vec();
        let old = Counts::decode(&docs_only).expect("reads");
        assert_eq!((old.docs, old.dirty_at, old.unsized_at), (c.docs.clone(), 12, 1));
    }

    #[test]
    fn a_version_that_only_serves_the_history_is_no_document() {
        let a = doc(7, b"a");
        let old = [Some((DOCUMENT_VALUE, &b"x"[..]))];
        // A delete that leaves a history version in place of the document.
        let puts = [put(&a, crate::HISTORY_VALUE)];
        let got = doc_delta(std::slice::from_ref(&a), &old, (&puts, &[]), false);
        assert_eq!(got, vec![((1, 7), Change::docs(-1))]);
        // A key outside the data space, as a range's local key, belongs to no collection.
        assert_eq!(document_of(&[0, 1, 2]), None);
        assert_eq!(live_document(&a, 2), None);
    }
}
