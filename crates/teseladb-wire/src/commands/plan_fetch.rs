//! The documents of an index scan's entries read by batches (010 FR-027): the keys of the
//! documents the scan is about to need go together to the backend, which groups them by range and
//! asks each range's leader for its batch at once, with the scan's residual filter when it decides
//! from the document alone. The first batch is small, or the read's `limit` when that is less, and
//! each batch after doubles the one before up to [`MOST`]: a `findOne` reads no document more than
//! it needs, and a plan's trial stays within its budget of work.

use std::collections::HashMap;

use teseladb_keys::KeyBuilder;
use teseladb_meta::catalog::CollectionEntry;
use teseladb_range::read::ReadConcern;

use super::Backend;
use crate::error::CommandError;

/// The keys of the first batch of a read.
const FIRST: usize = 16;
/// The most keys of one batch.
const MOST: usize = 512;

/// What a backend read for one key of [`Backend::fetch_documents`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fetched {
    /// No document is stored at the key.
    Missing,
    /// The document, which the filter, if any, still has to decide.
    Doc(Vec<u8>),
    /// The document, which the filter sent with the batch accepted.
    Kept(Vec<u8>),
    /// A document the filter sent with the batch examined and refused.
    Refused,
}

/// The key of the document an index entry with primary key `pk` names.
pub(crate) fn document_key(coll: &CollectionEntry, unbucketed: bool, pk: &[u8]) -> Vec<u8> {
    let key = match unbucketed {
        true => KeyBuilder::document_of_sharded_pk(coll.tenant, coll.id, pk),
        false => KeyBuilder::document_of_pk(coll.tenant, coll.id, pk),
    };
    key.as_bytes().to_vec()
}

/// The documents of the entries of one index scan, asked for by batches ahead of the scan.
pub(crate) struct Fetcher<'a> {
    coll: &'a CollectionEntry,
    unbucketed: bool,
    concern: ReadConcern,
    /// The residual filter in BSON, when the leaders may evaluate it.
    filter: Option<Vec<u8>>,
    /// The primary keys of the entries without their document, in the scan's order.
    pending: Vec<Vec<u8>>,
    /// Where the scan is in `pending`.
    at: usize,
    /// What the batches read, by primary key.
    got: HashMap<Vec<u8>, Fetched>,
    /// The keys of the next batch.
    batch: usize,
    /// Whether the backend reads by batches; when it does not, each key is read on its own.
    batches: bool,
}

impl<'a> Fetcher<'a> {
    /// A fetcher for the entries whose documents are in `pending`, in the scan's order, with the
    /// residual `filter` in BSON when the leaders may evaluate it and the read's `limit`.
    pub(crate) fn new(
        (coll, unbucketed): (&'a CollectionEntry, bool),
        pending: Vec<Vec<u8>>,
        filter: Option<Vec<u8>>,
        (concern, limit): (ReadConcern, Option<u64>),
    ) -> Fetcher<'a> {
        let first =
            limit.and_then(|l| usize::try_from(l).ok()).map_or(FIRST, |l| l.clamp(1, FIRST));
        Fetcher {
            coll,
            unbucketed,
            concern,
            filter,
            pending,
            at: 0,
            got: HashMap::new(),
            batch: first,
            batches: true,
        }
    }

    /// The scan passed an entry without its document (`pk`), whether it reads it or not.
    fn pass(&mut self, pk: &[u8]) {
        if self.pending.get(self.at).is_some_and(|p| p == pk) {
            self.at += 1;
        }
    }

    /// The scan passed the entry of `pk` without reading its document.
    pub(crate) fn skip(&mut self, pk: &[u8]) {
        self.pass(pk);
        self.got.remove(pk);
    }

    /// The document of the entry of `pk`, with its key, as the batch that holds it read it;
    /// `most` caps the keys of a batch asked for now (what is left of a trial's budget).
    pub(crate) fn fetch<B: Backend + ?Sized>(
        &mut self,
        backend: &mut B,
        pk: &[u8],
        most: Option<usize>,
    ) -> Result<(Vec<u8>, Fetched), CommandError> {
        self.pass(pk);
        let key = document_key(self.coll, self.unbucketed, pk);
        if let Some(found) = self.got.remove(pk) {
            return Ok((key, found));
        }
        if self.batches {
            // This entry's document and those of the entries after it the batches have not read.
            let cap = most.map_or(self.batch, |m| m.clamp(1, self.batch));
            let mut pks: Vec<Vec<u8>> = vec![pk.to_vec()];
            for p in self.pending.iter().skip(self.at) {
                if pks.len() >= cap {
                    break;
                }
                if !self.got.contains_key(p) && !pks.contains(p) {
                    pks.push(p.clone());
                }
            }
            let keys: Vec<Vec<u8>> =
                pks.iter().map(|p| document_key(self.coll, self.unbucketed, p)).collect();
            match backend.fetch_documents(&keys, self.filter.as_deref(), self.concern)? {
                Some(found) if found.len() == keys.len() => {
                    self.batch = (self.batch * 2).min(MOST);
                    let mut found = found.into_iter();
                    let first = found.next().unwrap_or(Fetched::Missing);
                    self.got.extend(pks.into_iter().skip(1).zip(found));
                    return Ok((key, first));
                }
                // A backend that reads key by key: the scan reads each document on its own.
                _ => self.batches = false,
            }
        }
        let next = [key.as_slice(), &[0]].concat();
        let found = backend.scan(&key, &next, 1, self.concern)?.into_iter().next();
        Ok((key, found.map_or(Fetched::Missing, |(_, doc)| Fetched::Doc(doc))))
    }
}
