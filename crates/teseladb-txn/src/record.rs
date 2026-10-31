//! The coordinator's side of its transaction record (008 FR-001, FR-007): the intervals of keys
//! with intents that the record keeps, and when the next heartbeat is due so that no reader or
//! reaper takes the transaction for abandoned.

use std::collections::BTreeSet;

use teseladb_range::intents::ABANDONED_AFTER_MS;
use teseladb_types::Nanos;

/// A heartbeat goes this often: five per abandonment window, so a lost one or two do not let the
/// transaction be aborted.
pub const HEARTBEAT_EVERY_MS: u64 = ABANDONED_AFTER_MS / 5;

/// Longest key the record keeps whole: a longer one goes as the interval of the keys that start
/// with its first bytes, so that a document whose index entries carry megabytes (008 T036) does not
/// make a record too large for its Raft entry.
pub const SPAN_KEY_BYTES: usize = 1024;

/// The intervals of keys with a transaction's intents, `[start, end)`, merged when they touch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Spans {
    /// Each key, or the first [`SPAN_KEY_BYTES`] bytes of a longer one.
    keys: BTreeSet<Vec<u8>>,
    /// The cut keys among them: they stand for every key that starts with them.
    cut: BTreeSet<Vec<u8>>,
}

/// The first key after every key that starts with `p`, if `p` has a byte below `FF`.
fn after_prefix(p: &[u8]) -> Option<Vec<u8>> {
    let last = p.iter().rposition(|b| *b != u8::MAX)?;
    let mut end = p[..=last].to_vec();
    end[last] += 1;
    Some(end)
}

impl Spans {
    /// Adds a key with an intent.
    pub fn add(&mut self, key: &[u8]) {
        let cut = &key[..key.len().min(SPAN_KEY_BYTES)];
        if cut.len() < key.len() && after_prefix(cut).is_some() {
            self.cut.insert(cut.to_vec());
            self.keys.insert(cut.to_vec());
        } else {
            self.keys.insert(key.to_vec());
        }
    }

    /// Whether no key has an intent.
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The intervals: each key `k` covers `[k, k · 00)`, a cut one every key that starts with it,
    /// and intervals that touch or overlap merge.
    pub fn intervals(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        for k in &self.keys {
            let end = match self.cut.contains(k).then(|| after_prefix(k)).flatten() {
                Some(end) => end,
                None => [k.as_slice(), &[0]].concat(),
            };
            match out.last_mut() {
                Some(last) if *k <= last.1 => last.1 = end.max(last.1.clone()),
                _ => out.push((k.clone(), end)),
            }
        }
        out
    }
}

/// When the next heartbeat is due.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Heartbeat {
    last: Nanos,
}

impl Heartbeat {
    /// The record was written (created or beaten) at `now`.
    pub fn new(now: Nanos) -> Heartbeat {
        Heartbeat { last: now }
    }

    /// Whether a heartbeat is due at `now`.
    pub fn due(&self, now: Nanos) -> bool {
        now.0.saturating_sub(self.last.0) >= Nanos::from_millis(HEARTBEAT_EVERY_MS).0
    }

    /// A heartbeat went at `now`.
    pub fn sent(&mut self, now: Nanos) {
        self.last = now;
    }
}
