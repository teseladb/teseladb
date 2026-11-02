//! The cursors of the `changestream` workload (014 T003, FR-010, FR-015): the `splits` workload,
//! whose ranges split while transactions write on both sides of the cuts, with change stream
//! cursors over the documents. Each transaction writes two documents and the meta of each write
//! (`teseladb_changestream::meta`), as the server's transactions do.
//!
//! A cursor reads `[01, 02)` range by range at each range's leader, after the mark it read up to,
//! with `IntervalRead` as the server's change reads do; the rows of the read become changes with
//! `changes`, which the cursor emits in the merge order through its `StreamPosition`. Its batches
//! hold one to three events: one that leaves events behind resumes the cursor after its last
//! event, as a driver resumes from the token of its last event, and the next read starts there.
//!
//! The `changestream_delivery` checker compares what each cursor emitted with the transactions
//! that committed: each committed change after its start once, in the merge order, never above the
//! resolved mark of its read, the changes of a transaction one after another and complete, and
//! every committed change at or below its last resolved mark emitted.

use teseladb_changestream::changes::{Versioned, changes};
use teseladb_changestream::cursor::StreamPosition;
use teseladb_changestream::meta::{ChangeMeta, TxnInfo, meta_key};
use teseladb_changestream::order::EventKey;
use teseladb_changestream::read::IntervalRead;
use teseladb_changestream::stage::Start;
use teseladb_changestream::token::{Position, ResumeToken};
use teseladb_hlc::Hlc;
use teseladb_range::host::{RangeError, Reply};

use crate::checkers::Violation;
use crate::checkers::changestream_delivery::{self, Change, CursorLog, Emitted};

/// The documents the cursors watch: the data keys of the transactions.
pub(crate) const START: &[u8] = b"\x01";
pub(crate) const END: &[u8] = b"\x02";
/// Cursors.
pub(crate) const CURSORS: u32 = 2;
/// Most events in a batch.
const MAX_BATCH: u64 = 3;
/// A collection's prefix in a document key after its first byte: tenant, collection, documents.
const DOC_TAIL: [u8; 11] = [0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];

/// The document key of transaction key `j`: in the data space, among the single documents.
pub(crate) fn doc_key(j: u64) -> Vec<u8> {
    let mut k = vec![1, (j * 32 + 4) as u8];
    k.extend_from_slice(&DOC_TAIL);
    k.push(b'k');
    k
}

/// The transaction identifier of tag `tag` in its metas.
pub(crate) fn txn_id(tag: u64) -> u128 {
    u128::from(tag)
}

/// The meta write of position `op` of tag `tag`'s transaction on `key`: its key and its bytes.
pub(crate) fn meta_write(key: &[u8], tag: u64, op: u32) -> (Vec<u8>, Vec<u8>) {
    let txn = TxnInfo { id: txn_id(tag), lsid: 1, uid: [0; 32], txn_number: tag as i64 };
    let meta = ChangeMeta {
        txn: Some(txn),
        op,
        update: None,
        replace: false,
        delete: false,
        doc: None,
        moved: None,
    };
    (meta_key(key, op), meta.encode().expect("a meta without documents encodes"))
}

/// The changes tag `tag`'s transaction committed at `ts` on `keys`, in the merge order.
pub(crate) fn committed(tag: u64, ts: Hlc, keys: &[Vec<u8>]) -> Vec<Change> {
    keys.iter()
        .enumerate()
        .map(|(op, key)| Change { ts, txn: Some(txn_id(tag)), op: op as u32, key: key.clone() })
        .collect()
}

fn change_of(e: &EventKey) -> Change {
    Change { ts: e.ts, txn: (e.txn != 0).then_some(e.txn), op: e.op, key: e.key.clone() }
}

/// A cursor.
pub(crate) struct Cursor {
    pos: StreamPosition,
    read: Option<IntervalRead<Versioned>>,
    /// Where the read in flight stops, while one is.
    stop: Option<Vec<u8>>,
    pub(crate) log: CursorLog,
}

impl Cursor {
    /// A cursor that starts after `after`.
    pub(crate) fn new(name: String, after: Hlc) -> Cursor {
        let pos = StreamPosition::new(&Start::ResumeAfter(Position::HighWater(after)), after);
        let log = CursorLog { name, after, emitted: Vec::new(), resolved: after };
        Cursor { pos, read: None, stop: None, log }
    }

    /// Whether a read is in flight.
    pub(crate) fn busy(&self) -> bool {
        self.stop.is_some()
    }

    /// The first key of the next range to read, starting a read if none is going on.
    pub(crate) fn next(&mut self) -> Option<Vec<u8>> {
        let from = self.pos.from();
        let read = self.read.get_or_insert_with(|| IntervalRead::new(START, END, from));
        read.next().map(<[u8]>::to_vec)
    }

    /// The read of the range that ends at `range_end` is sent.
    pub(crate) fn sent(&mut self, range_end: &[u8]) {
        self.stop = self.read.as_ref().map(|r| r.stop(range_end));
    }

    /// The answer of the read in flight. A refusal leaves the read where it was: the next turn
    /// asks again with the table as it is then.
    pub(crate) fn answered(&mut self, r: Result<Reply, RangeError>, batch: u64, now: Hlc) {
        let Some(stop) = self.stop.take() else { return };
        let Ok(Reply::Changes { rows, to, oldest_intent }) = r else { return };
        let Some(read) = self.read.as_mut() else { return };
        let rows = rows.into_iter().map(|v| Versioned { key: v.key, ts: v.ts, value: v.value });
        read.answered(stop, rows.collect(), to, oldest_intent);
        if read.next().is_none() {
            let done = self.read.take().expect("the read answered above");
            self.emit(done, batch.clamp(1, MAX_BATCH) as usize, now);
        }
    }

    /// Emits the changes of a finished read, at most `batch` of them.
    fn emit(&mut self, read: IntervalRead<Versioned>, batch: usize, now: Hlc) {
        let (rows, resolved) = read.finish(|v| v.ts);
        let Ok(found) = changes(rows) else { return };
        let wanted: Vec<_> = found.into_iter().filter(|c| self.pos.wants(&c.event)).collect();
        for c in wanted.iter().take(batch) {
            self.log.emitted.push(Emitted { change: change_of(&c.event), resolved });
        }
        if let Some(last) = wanted.get(batch.saturating_sub(1)).filter(|_| wanted.len() > batch) {
            // The batch left events behind: the cursor resumes after its last event.
            let token = ResumeToken::new(last.event.clone(), last.kind);
            self.pos = StreamPosition::new(&Start::ResumeAfter(Position::Event(token)), now);
            return;
        }
        self.pos.read_to(resolved);
        self.log.resolved = self.log.resolved.max(resolved);
    }

    /// Where the read in flight stops.
    pub(crate) fn stop_key(&self) -> Vec<u8> {
        self.stop.clone().unwrap_or_default()
    }

    /// The mark the cursor reads after.
    pub(crate) fn from(&self) -> Hlc {
        self.pos.from()
    }

    /// The cursor is dropped in the middle of a read (its node crashed).
    pub(crate) fn lost(&mut self) {
        self.stop = None;
    }
}

/// The `changestream_delivery` checker over the cursors, with the committed changes and those of
/// transactions whose end is unknown that some cursor emitted (they may have committed).
pub(crate) fn check(
    committed: &[Change],
    unknown: &[(u64, Vec<Vec<u8>>)],
    cursors: &[&CursorLog],
) -> Result<(), Violation> {
    let mut all = committed.to_vec();
    for c in cursors.iter().flat_map(|c| &c.emitted) {
        let maybe = unknown
            .iter()
            .any(|(tag, keys)| c.change.txn == Some(txn_id(*tag)) && keys.contains(&c.change.key));
        if maybe && !all.contains(&c.change) {
            all.push(c.change.clone());
        }
    }
    let logs: Vec<CursorLog> = cursors.iter().map(|c| (*c).clone()).collect();
    changestream_delivery::check(&all, &logs)?;
    let emitted = logs.iter().any(|c| !c.emitted.is_empty());
    if !emitted && !committed.is_empty() {
        return Err(Violation::liveness(changestream_delivery::CHECKER, "no cursor emitted"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(p: u64, txn: Option<u128>, key: &[u8]) -> Change {
        Change { ts: Hlc { physical: p, logical: 0 }, txn, op: 0, key: key.to_vec() }
    }

    fn log(emitted: Vec<Change>) -> CursorLog {
        let resolved = Hlc { physical: 100, logical: 0 };
        let emitted: Vec<Emitted> =
            emitted.into_iter().map(|change| Emitted { change, resolved }).collect();
        let after = Hlc { physical: 0, logical: 0 };
        let resolved = if emitted.is_empty() { after } else { resolved };
        CursorLog { name: "c".into(), after, emitted, resolved }
    }

    #[test]
    fn changes_of_unknown_transactions_count_once_some_cursor_emits_them() {
        let known = change(5, None, b"a");
        let maybe = change(6, Some(txn_id(7)), b"b");
        let other = change(6, Some(txn_id(7)), b"z");
        let unknown = [(7, vec![b"b".to_vec()]), (8, vec![b"c".to_vec()])];
        // Two cursors emit the change of the unknown transaction: it counts once.
        let (one, two) = (log(vec![known.clone(), maybe.clone()]), log(vec![known.clone(), maybe]));
        check(std::slice::from_ref(&known), &unknown, &[&one, &two]).expect("both saw it");
        // A key the unknown transaction did not write is no change of it.
        let three = log(vec![known.clone(), other]);
        check(std::slice::from_ref(&known), &unknown, &[&three]).expect_err("a change nobody made");
        // Nothing emitted while something committed.
        let v = check(&[known], &[], &[&log(vec![])]).expect_err("silent");
        assert_eq!(
            (v.checker, v.detail.as_str()),
            (changestream_delivery::CHECKER, "no cursor emitted")
        );
        check(&[], &[], &[&log(vec![])]).expect("nothing to emit");
    }
}
