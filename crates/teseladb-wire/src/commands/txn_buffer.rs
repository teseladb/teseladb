//! The writes a transaction holds until its commit (008 FR-022), and what its own reads see of
//! them: a read of the transaction takes what the ranges hold at its mark and lays its held
//! writes over it, key by key, so that it reads what it wrote as it would read its intents.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use teseladb_range::intents::IntentOp;
use teseladb_types::GroupId;

/// The writes a transaction holds, the last one of each key, and the keys it locked.
#[derive(Debug, Default)]
pub(crate) struct Buffer {
    writes: BTreeMap<Vec<u8>, IntentOp>,
    /// The keys locked in each range, which an abort lets go of.
    locked: BTreeMap<GroupId, BTreeSet<Vec<u8>>>,
    /// What the writes take of the transaction's memory budget, counted as the coordinator
    /// counts the keys of its intents.
    bytes: usize,
}

/// The document a held write leaves at its key: none for a deletion and for a version that
/// only serves the history, as a scan gives none for them.
fn document(op: &IntentOp) -> Option<&[u8]> {
    match op {
        IntentOp::Put { value_type, value } if *value_type != teseladb_range::HISTORY_VALUE => {
            Some(value)
        }
        IntentOp::Put { .. } | IntentOp::Delete => None,
    }
}

impl Buffer {
    /// Whether it holds no write.
    pub(crate) fn is_empty(&self) -> bool {
        self.writes.is_empty()
    }

    /// The bytes its writes count against the transaction's budget.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// Holds `writes`, each over an earlier write of its key.
    pub(crate) fn hold(&mut self, writes: Vec<(Vec<u8>, IntentOp)>) {
        for (key, op) in writes {
            if !self.writes.contains_key(&key) {
                self.bytes = self.bytes.saturating_add(2 * key.len());
            }
            self.writes.insert(key, op);
        }
    }

    /// Notes that `keys` are locked in range `g`.
    pub(crate) fn locked_in(&mut self, g: GroupId, keys: impl IntoIterator<Item = Vec<u8>>) {
        self.locked.entry(g).or_default().extend(keys);
    }

    /// Whether `key` is noted as locked in range `g`.
    pub(crate) fn holds_lock(&self, g: GroupId, key: &[u8]) -> bool {
        self.locked.get(&g).is_some_and(|keys| keys.contains(key))
    }

    /// The ranges with locks and the keys locked in each; nothing is noted as locked after it.
    pub(crate) fn take_locks(&mut self) -> Vec<(GroupId, Vec<Vec<u8>>)> {
        std::mem::take(&mut self.locked)
            .into_iter()
            .map(|(g, keys)| (g, keys.into_iter().collect()))
            .collect()
    }

    /// The ranges with locks and the keys locked in each.
    pub(crate) fn locks(&self) -> Vec<(GroupId, Vec<Vec<u8>>)> {
        self.locked.iter().map(|(g, keys)| (*g, keys.iter().cloned().collect())).collect()
    }

    /// The writes it holds, in key order; it holds none after it.
    pub(crate) fn take_writes(&mut self) -> Vec<(Vec<u8>, IntentOp)> {
        self.bytes = 0;
        std::mem::take(&mut self.writes).into_iter().collect()
    }

    /// `values`, what the ranges hold at each of `keys` with its value type, with the held
    /// writes over them.
    pub(crate) fn over_values(
        &self,
        keys: &[Vec<u8>],
        mut values: Vec<Option<(u8, Vec<u8>)>>,
    ) -> Vec<Option<(u8, Vec<u8>)>> {
        if self.writes.is_empty() {
            return values;
        }
        for (k, slot) in keys.iter().zip(values.iter_mut()) {
            match self.writes.get(k) {
                Some(IntentOp::Put { value_type, value }) => {
                    *slot = Some((*value_type, value.clone()));
                }
                Some(IntentOp::Delete) => *slot = None,
                None => {}
            }
        }
        values
    }

    /// `docs`, the document the ranges hold at each of `keys`, with the held writes over them.
    pub(crate) fn over_points(
        &self,
        keys: &[Vec<u8>],
        mut docs: Vec<Option<Vec<u8>>>,
    ) -> Vec<Option<Vec<u8>>> {
        if self.writes.is_empty() {
            return docs;
        }
        for (k, slot) in keys.iter().zip(docs.iter_mut()) {
            if let Some(op) = self.writes.get(k) {
                *slot = document(op).map(<[u8]>::to_vec);
            }
        }
        docs
    }

    /// Whether a write is held on a key of `[start, end)`.
    pub(crate) fn touches(&self, start: &[u8], end: &[u8]) -> bool {
        start < end && self.held(start, end).next().is_some()
    }

    fn held<'a>(
        &'a self,
        start: &[u8],
        end: &[u8],
    ) -> impl DoubleEndedIterator<Item = (&'a Vec<u8>, &'a IntentOp)> {
        let bounds = (Bound::Included(start.to_vec()), Bound::Excluded(end.to_vec()));
        self.writes.range::<Vec<u8>, _>(bounds)
    }

    /// The documents of `[start, end)` in key order with the held writes over `rows`, the
    /// documents the ranges hold there in the same order: a held document takes the place of the
    /// stored one or goes in at its key, and a held deletion takes the stored one out.
    pub(crate) fn over_rows(
        &self,
        (start, end): (&[u8], &[u8]),
        rows: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        if start >= end {
            return rows;
        }
        let mut held = self.held(start, end).peekable();
        if held.peek().is_none() {
            return rows;
        }
        let mut out = Vec::with_capacity(rows.len());
        for (key, value) in rows {
            while let Some((k, op)) = held.next_if(|(k, _)| **k < key) {
                out.extend(document(op).map(|d| (k.clone(), d.to_vec())));
            }
            match held.next_if(|(k, _)| **k == key) {
                Some((k, op)) => out.extend(document(op).map(|d| (k.clone(), d.to_vec()))),
                None => out.push((key, value)),
            }
        }
        for (k, op) in held {
            out.extend(document(op).map(|d| (k.clone(), d.to_vec())));
        }
        out
    }
}

/// The key just after `key`.
fn after(key: &[u8]) -> Vec<u8> {
    [key, &[0]].concat()
}

/// At most `limit` documents of `[start, end)` in key order with the held writes of `buffer`
/// over what `read` gives: `read(from, to, n)` reads at most `n` stored documents of
/// `[from, to)` in key order. It reads on while held deletions leave it short of `limit`.
pub(crate) fn scan<E>(
    buffer: &Buffer,
    (start, end): (&[u8], &[u8]),
    limit: usize,
    mut read: impl FnMut(&[u8], &[u8], usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>, E>,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, E> {
    if !buffer.touches(start, end) {
        return read(start, end, limit);
    }
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut from = start.to_vec();
    while out.len() < limit && from.as_slice() < end {
        let want = limit - out.len();
        let rows = read(&from, end, want)?;
        // What the read covered: up to its last document when it is full, the rest otherwise.
        let covered = match rows.last() {
            Some((last, _)) if rows.len() >= want => after(last),
            _ => end.to_vec(),
        };
        out.extend(buffer.over_rows((&from, &covered), rows));
        from = covered;
    }
    out.truncate(limit);
    Ok(out)
}

/// As [`scan`] from the last key down: `read(from, to, n)` reads at most `n` stored documents
/// of `[from, to)` from the last one down.
pub(crate) fn scan_back<E>(
    buffer: &Buffer,
    (start, end): (&[u8], &[u8]),
    limit: usize,
    mut read: impl FnMut(&[u8], &[u8], usize) -> Result<Vec<(Vec<u8>, Vec<u8>)>, E>,
) -> Result<Vec<(Vec<u8>, Vec<u8>)>, E> {
    if !buffer.touches(start, end) {
        return read(start, end, limit);
    }
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut to = end.to_vec();
    while out.len() < limit && start < to.as_slice() {
        let want = limit - out.len();
        let mut rows = read(start, &to, want)?;
        // What the read covered: down to its last document when it is full.
        let covered = match rows.last() {
            Some((last, _)) if rows.len() >= want => last.clone(),
            _ => start.to_vec(),
        };
        rows.reverse();
        let mut part = buffer.over_rows((&covered, &to), rows);
        part.reverse();
        out.extend(part);
        to = covered;
    }
    out.truncate(limit);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    fn put(v: &str) -> IntentOp {
        IntentOp::Put { value_type: 1, value: k(v) }
    }

    fn history(v: &str) -> IntentOp {
        IntentOp::Put { value_type: teseladb_range::HISTORY_VALUE, value: k(v) }
    }

    fn stored() -> Vec<(Vec<u8>, Vec<u8>)> {
        ["a", "c", "e", "g"].iter().map(|x| (k(x), k(&x.to_uppercase()))).collect()
    }

    type Rows = Result<Vec<(Vec<u8>, Vec<u8>)>, std::convert::Infallible>;

    /// A store of `stored()` read forward.
    fn forward(from: &[u8], to: &[u8], n: usize) -> Rows {
        Ok(stored()
            .into_iter()
            .filter(|(key, _)| key.as_slice() >= from && key.as_slice() < to)
            .take(n)
            .collect())
    }

    fn backward(from: &[u8], to: &[u8], n: usize) -> Rows {
        let mut rows = forward(from, to, usize::MAX)?;
        rows.reverse();
        rows.truncate(n);
        Ok(rows)
    }

    fn shown(rows: &[(Vec<u8>, Vec<u8>)]) -> Vec<String> {
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        rows.iter().map(|(key, v)| format!("{}={}", text(key), text(v))).collect()
    }

    fn held() -> Buffer {
        let mut b = Buffer::default();
        b.hold(vec![(k("b"), put("new")), (k("c"), IntentOp::Delete), (k("e"), put("E2"))]);
        b.hold(vec![(k("f"), history("h"))]);
        b
    }

    #[test]
    fn a_later_write_of_a_key_replaces_the_earlier_one_and_counts_its_key_once() {
        let mut b = Buffer::default();
        b.hold(vec![(k("x"), put("1"))]);
        b.hold(vec![(k("x"), IntentOp::Delete), (k("yy"), put("2"))]);
        assert_eq!(b.bytes(), 2 + 4);
        assert_eq!(b.take_writes(), vec![(k("x"), IntentOp::Delete), (k("yy"), put("2"))]);
        assert!(b.is_empty());
        assert_eq!(b.bytes(), 0);
    }

    #[test]
    fn point_reads_see_the_held_writes() {
        let b = held();
        let keys = [k("a"), k("b"), k("c"), k("f")];
        let values = vec![Some((1, k("A"))), None, Some((1, k("C"))), None];
        let got = b.over_values(&keys, values);
        let h = teseladb_range::HISTORY_VALUE;
        assert_eq!(got, vec![Some((1, k("A"))), Some((1, k("new"))), None, Some((h, k("h")))]);
        let docs = vec![Some(k("A")), None, Some(k("C")), Some(k("was"))];
        assert_eq!(b.over_points(&keys, docs), vec![Some(k("A")), Some(k("new")), None, None]);
    }

    #[test]
    fn a_scan_gives_the_held_documents_in_their_place_and_reads_on_past_deletions() {
        let b = held();
        let all = scan(&b, (b"a", b"z"), usize::MAX, forward).unwrap();
        assert_eq!(shown(&all), ["a=A", "b=new", "e=E2", "g=G"]);
        // The stored `c` is deleted: a scan of two reads on to find its second document.
        let two = scan(&b, (b"a", b"z"), 2, forward).unwrap();
        assert_eq!(shown(&two), ["a=A", "b=new"]);
        let three = scan(&b, (b"b", b"z"), 3, forward).unwrap();
        assert_eq!(shown(&three), ["b=new", "e=E2", "g=G"]);
        // A span the transaction did not write is read as it is.
        let plain = scan(&b, (b"g", b"z"), 5, forward).unwrap();
        assert_eq!(shown(&plain), ["g=G"]);
        assert!(scan(&b, (b"c", b"d"), 5, forward).unwrap().is_empty());
    }

    #[test]
    fn a_scan_from_the_end_gives_the_same_documents_from_the_last_one_down() {
        let b = held();
        let all = scan_back(&b, (b"a", b"z"), usize::MAX, backward).unwrap();
        assert_eq!(shown(&all), ["g=G", "e=E2", "b=new", "a=A"]);
        let three = scan_back(&b, (b"a", b"z"), 3, backward).unwrap();
        assert_eq!(shown(&three), ["g=G", "e=E2", "b=new"]);
        let one = scan_back(&b, (b"a", b"f"), 1, backward).unwrap();
        assert_eq!(shown(&one), ["e=E2"]);
    }

    #[test]
    fn the_locks_are_noted_by_range_until_they_are_taken() {
        let mut b = Buffer::default();
        b.locked_in(GroupId(2), [k("x"), k("y")]);
        b.locked_in(GroupId(2), [k("x")]);
        b.locked_in(GroupId(5), [k("z")]);
        assert!(b.holds_lock(GroupId(2), b"y") && !b.holds_lock(GroupId(5), b"y"));
        assert_eq!(b.locks().len(), 2);
        let taken = b.take_locks();
        assert_eq!(taken, vec![(GroupId(2), vec![k("x"), k("y")]), (GroupId(5), vec![k("z")])]);
        assert!(b.locks().is_empty());
    }
}
