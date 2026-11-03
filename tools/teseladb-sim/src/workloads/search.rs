//! The searchers of the `search` and `vector` workloads (011 T004, FR-009, FR-010, FR-013, SC-002,
//! SC-006): the
//! `changestream` workload, whose ranges split and merge while transactions write documents on
//! both sides of the cuts, with clients that search the documents as a node's search index does.
//!
//! A searcher keeps the segments of a dynamic search index (`teseladb_search::indexer::Indexer`)
//! or, in `vector`, of a vector index of two dimensions (`VectorIndexer`), one per range. A search takes a leader's mark as its target, then reads `[01, 02)` range by range at
//! each range's leader after the mark its segments applied, with `IntervalRead` as the server's
//! change reads do; the segments follow the ranges of the read and apply the newest version of
//! each document it found. Once the read reaches the target (`segments_reached`, the test the
//! server's search runs) the searcher runs a `text` query for one word, or an exact `$vectorSearch`
//! near one point, and logs what it answered.
//!
//! The `search_results` checker builds one segment over the committed documents at the mark the
//! segments reached and compares the documents and the bits of their scores.

use std::collections::BTreeMap;

use teseladb_bson::{Document, Value};
use teseladb_changestream::changes::{Versioned, changes};
use teseladb_changestream::read::IntervalRead;
use teseladb_hlc::Hlc;
use teseladb_range::host::{RangeError, Reply};
use teseladb_wire::commands::search_ops::segments_reached;

use crate::checkers::Violation;
use crate::checkers::search_results::{self, SearchLog, Segments, Write};
use crate::workloads::changestream::{END, START};

/// Searchers.
pub(crate) const SEARCHERS: u32 = 2;
/// The words of the documents: a transaction's tag picks two.
const WORDS: [&str; 4] = ["alfa", "beta", "gama", "delta"];

/// Points of the vectors: a tag picks one of each.
const POINTS: [f64; 5] = [-2.0, -0.5, 0.0, 1.0, 3.0];
/// Results of a `$vectorSearch`.
const VECTOR_LIMIT: i32 = 3;

/// The document a transaction with tag `tag` writes: two words, the same one twice now and then,
/// so that term frequencies and lengths vary, and a point of two dimensions.
pub(crate) fn document_of(tag: u64) -> Document {
    let first = WORDS[(tag % 4) as usize];
    let second = WORDS[((tag / 4) % 4) as usize];
    let v = [POINTS[(tag % 5) as usize], POINTS[((tag / 5) % 5) as usize]];
    Document::new()
        .with("t", format!("{first} {second}"))
        .with("v", Value::Array(v.iter().map(|x| Value::Double(*x)).collect()))
}

/// Query number `n`: a `text` query for a word on `t`, or an exact `$vectorSearch` on `v`.
fn query(n: u64, vector: bool) -> Document {
    if !vector {
        let word = WORDS[(n % WORDS.len() as u64) as usize];
        return Document::new().with("text", Document::new().with("query", word).with("path", "t"));
    }
    let x = POINTS[(n % 5) as usize];
    let y = POINTS[((n / 5) % 5) as usize];
    Document::new()
        .with("index", "default")
        .with("path", "v")
        .with("queryVector", Value::Array(vec![Value::Double(x), Value::Double(y)]))
        .with("exact", true)
        .with("limit", VECTOR_LIMIT)
}

/// A write a searcher applied: its key, its mark and its tag (`None`: a deletion).
pub(crate) type Applied = (Vec<u8>, Hlc, Option<u64>);

/// A searcher.
pub(crate) struct Searcher {
    segments: Segments,
    vector: bool,
    /// The mark its segments applied.
    applied: Hlc,
    /// The search in progress: its mark and its query.
    target: Option<(Hlc, Document)>,
    read: Option<IntervalRead<Versioned>>,
    /// The ranges of the read in flight, `[start, stop)` in key order.
    bounds: Vec<(Vec<u8>, Vec<u8>)>,
    /// Where the read in flight stops, while one is.
    stop: Option<Vec<u8>>,
    /// The writes it applied.
    pub(crate) seen: Vec<Applied>,
    /// The searches it answered.
    pub(crate) log: Vec<SearchLog>,
}

impl Searcher {
    /// A searcher whose segments, of a vector index or not, have applied nothing.
    pub(crate) fn new(vector: bool) -> Searcher {
        Searcher {
            segments: Segments::new(vector),
            vector,
            applied: Hlc { physical: 0, logical: 0 },
            target: None,
            read: None,
            bounds: Vec::new(),
            stop: None,
            seen: Vec::new(),
            log: Vec::new(),
        }
    }

    /// Whether it has no search in progress.
    pub(crate) fn idle(&self) -> bool {
        self.target.is_none()
    }

    /// Starts a search at mark `at` with query number `n`.
    pub(crate) fn begin(&mut self, at: Hlc, n: u64) {
        self.target = Some((at, query(n, self.vector)));
    }

    /// Whether a read is in flight.
    pub(crate) fn busy(&self) -> bool {
        self.stop.is_some()
    }

    /// The first key of the next range to read, starting a read if none is going on.
    pub(crate) fn next(&mut self) -> Option<Vec<u8>> {
        let from = self.applied;
        if self.read.is_none() {
            self.bounds.clear();
        }
        let read = self.read.get_or_insert_with(|| IntervalRead::new(START, END, from));
        read.next().map(<[u8]>::to_vec)
    }

    /// The read of the range that starts at `start` and ends at `range_end` is sent.
    pub(crate) fn sent(&mut self, start: &[u8], range_end: &[u8]) {
        self.stop = self.read.as_ref().map(|r| r.stop(range_end));
        if let Some(stop) = &self.stop {
            self.bounds.push((start.to_vec(), stop.clone()));
        }
    }

    /// Where the read in flight stops.
    pub(crate) fn stop_key(&self) -> Vec<u8> {
        self.stop.clone().unwrap_or_default()
    }

    /// The mark its segments applied: the next read starts after it.
    pub(crate) fn from(&self) -> Hlc {
        self.applied
    }

    /// The mark of the search in progress, which its reads ask the leaders to reach as the
    /// server's do (011 FR-010).
    pub(crate) fn mark(&self) -> Option<Hlc> {
        self.target.as_ref().map(|(at, _)| *at)
    }

    /// The searcher is dropped in the middle of a read (its node crashed): the read starts over.
    pub(crate) fn lost(&mut self) {
        self.stop = None;
        self.read = None;
    }

    /// The answer of the read in flight. A refusal leaves the read where it was, without the
    /// range it asked for.
    pub(crate) fn answered(
        &mut self,
        r: Result<Reply, RangeError>,
        tag_of: fn(&[u8]) -> Option<u64>,
    ) {
        let Some(stop) = self.stop.take() else { return };
        let Ok(Reply::Changes { rows, to, oldest_intent }) = r else {
            self.bounds.pop();
            return;
        };
        let Some(read) = self.read.as_mut() else { return };
        let rows = rows.into_iter().map(|v| Versioned { key: v.key, ts: v.ts, value: v.value });
        read.answered(stop, rows.collect(), to, oldest_intent);
        if read.next().is_none() {
            let done = self.read.take().expect("the read answered above");
            self.apply(done, tag_of);
        }
    }

    /// Applies a finished read and answers the search if the segments reached its mark.
    fn apply(&mut self, read: IntervalRead<Versioned>, tag_of: fn(&[u8]) -> Option<u64>) {
        let (rows, resolved) = read.finish(|v| v.ts);
        let Ok(found) = changes(rows) else { return };
        let mut newest: BTreeMap<Vec<u8>, Option<Document>> = BTreeMap::new();
        for c in found {
            let tag = c.doc.as_deref().and_then(tag_of);
            self.seen.push((c.event.key.clone(), c.event.ts, tag));
            newest.insert(c.event.key, tag.map(document_of));
        }
        let bounds = std::mem::take(&mut self.bounds);
        if !self.segments.reshape(&bounds) || !self.segments.apply(newest) {
            return;
        }
        self.applied = self.applied.max(resolved);
        let Some((target, q)) = self.target.take() else { return };
        let found = match segments_reached([resolved], target) {
            true => self.segments.search(&q),
            false => None,
        };
        let Some(found) = found else {
            self.target = Some((target, q));
            return;
        };
        let (applied, vector) = (self.applied, self.vector);
        self.log.push(SearchLog { target, applied, query: q, vector, found });
    }
}

/// The `search_results` checker over the searchers: `committed` holds the writes of the
/// transactions that committed, `unknown` the tags whose end is unknown.
pub(crate) fn check(
    committed: &[(u64, Hlc, Vec<Vec<u8>>)],
    unknown: &[u64],
    searchers: &[&Searcher],
) -> Result<(), Violation> {
    let mut writes: Vec<Write> = committed
        .iter()
        .flat_map(|(tag, ts, keys)| {
            keys.iter().map(|k| Write { key: k.clone(), ts: *ts, doc: Some(document_of(*tag)) })
        })
        .collect();
    // A transaction whose end is unknown committed at the mark a search read its writes at.
    for (key, ts, tag) in searchers.iter().flat_map(|s| &s.seen) {
        let Some(tag) = tag.filter(|t| unknown.contains(t)) else { continue };
        let write = Write { key: key.clone(), ts: *ts, doc: Some(document_of(tag)) };
        if !writes.contains(&write) {
            writes.push(write);
        }
    }
    let logs: Vec<SearchLog> = searchers.iter().flat_map(|s| s.log.iter().cloned()).collect();
    search_results::check(&writes, &logs)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn writes_of_unknown_transactions_count_once_a_search_saw_them() {
        let ts = Hlc { physical: 7, logical: 0 };
        let (mut a, mut b) = (Searcher::new(false), Searcher::new(false));
        a.seen = vec![
            (b"k".to_vec(), ts, Some(3)),
            (b"j".to_vec(), ts, Some(4)),
            (b"i".to_vec(), ts, None),
        ];
        b.seen = vec![(b"k".to_vec(), ts, Some(3))];
        // With the writes merged, the searchers answered no search: nothing to judge them by.
        let v = check(&[(1, ts, vec![b"x".to_vec()])], &[3], &[&a, &b]).expect_err("no search");
        assert_eq!((v.detail.as_str(), v.liveness), ("no search answered", true));
    }

    fn no_tag(_: &[u8]) -> Option<u64> {
        None
    }

    fn answer(to: u64) -> Result<Reply, RangeError> {
        let to = Hlc { physical: to, logical: 0 };
        Ok(Reply::Changes { rows: Vec::new(), to, oldest_intent: None })
    }

    #[test]
    fn answers_without_a_read_in_flight_are_dropped_and_a_lost_read_starts_over() {
        let mut s = Searcher::new(false);
        // Nothing sent: an answer changes nothing, and a send without a read stops nowhere.
        s.answered(answer(5), no_tag);
        s.sent(b"a", b"b");
        assert_eq!(s.stop_key(), Vec::<u8>::new());
        s.stop = Some(b"z".to_vec());
        s.answered(answer(5), no_tag);
        assert!(s.stop.is_none() && s.read.is_none());
        // A whole read with nothing in it moves the mark; no search waits for it.
        let first = s.next().expect("a read of the whole interval");
        s.sent(&first, &[]);
        s.answered(answer(9), no_tag);
        assert_eq!(s.from(), Hlc { physical: 9, logical: 0 });
        // A read dropped halfway starts over.
        let _ = s.next();
        s.sent(&first, &[]);
        s.lost();
        assert!(s.stop.is_none() && s.read.is_none());
    }
}
