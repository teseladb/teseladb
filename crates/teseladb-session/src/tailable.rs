//! Tailable cursors of capped collections (015 FR-004, T008), as a `refrouter` 9.0.4 answers them
//! (measured on 31-10-2026): the cursor keeps the key of the last document it read and, on each
//! `getMore`, reads on from there. RefDb reads a document only when the batch needs it, so the
//! cursor's position is the last document it returned, or past the documents the filter left
//! out once it reached the newest one, never a document read ahead into its buffer; it stays open with an empty batch when nothing new came, and
//! with `awaitData` the `getMore` waits for new documents up to its `maxTimeMS` (one second
//! without it). When the document it last read is gone, the cursor dies with
//! `CappedPositionLost`.

/// How long a `getMore` of an `awaitData` cursor waits without `maxTimeMS`, in milliseconds
/// (measured: 1 007 ms).
pub const DEFAULT_AWAIT_MS: u64 = 1000;

use std::collections::VecDeque;

/// Where a tailable cursor is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tail {
    /// The key of the last document read, `None` before the first one.
    pub last: Option<Vec<u8>>,
    /// The keys read ahead of the position, oldest first, each with whether its document waits
    /// in the cursor's buffer (`false`: the filter or `skip` left it out).
    ahead: VecDeque<(Vec<u8>, bool)>,
    /// Opened with `awaitData`.
    pub await_data: bool,
    /// The read reached the newest document: the batch ends there.
    pub caught_up: bool,
}

impl Tail {
    /// A cursor that has read nothing yet.
    pub fn new(await_data: bool) -> Tail {
        Tail { last: None, ahead: VecDeque::new(), await_data, caught_up: false }
    }

    /// The read went past document `key`; `buffered` when it waits in the cursor's buffer.
    pub fn read(&mut self, key: Vec<u8>, buffered: bool) {
        self.ahead.push_back((key, buffered));
    }

    /// The cursor returned the next document of its buffer: the position moves to it, past the
    /// documents left out before it.
    pub fn took(&mut self) {
        while let Some((key, buffered)) = self.ahead.pop_front() {
            self.last = Some(key);
            if buffered {
                return;
            }
        }
    }

    /// The batch reached the newest document: the position moves past every document read.
    pub fn reached_newest(&mut self) {
        if let Some((key, _)) = self.ahead.drain(..).next_back() {
            self.last = Some(key);
        }
        self.caught_up = true;
    }

    /// Whether the first batch leaves the cursor open: a collection with no document at all
    /// closes it (measured: an empty collection gives cursor 0, a filter that matches nothing
    /// keeps it).
    pub fn stays_open(&self) -> bool {
        self.last.is_some() || !self.ahead.is_empty()
    }

    /// How long a `getMore` waits for new documents, in milliseconds: none without `awaitData`.
    pub fn wait_ms(&self, max_time_ms: Option<u64>) -> u64 {
        match self.await_data {
            true => max_time_ms.unwrap_or(DEFAULT_AWAIT_MS),
            false => 0,
        }
    }
}

/// The message of a `CappedPositionLost` for the document at record `record` (1 for the first
/// document of the collection, as RefDb numbers its records): `newest` when it was the newest
/// document (a deletion took it), otherwise an insert evicted it.
pub fn lost_message(newest: bool, record: u64) -> String {
    match newest {
        true => format!(
            "Executor error during getMore :: caused by :: CollectionScan died due to failure to restore tailable cursor position. Last seen record id: RecordId({record})"
        ),
        false => format!(
            "CollectionScan died due to position in capped collection being deleted. Last seen record id: RecordId({record})"
        ),
    }
}
