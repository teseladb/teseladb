//! The merge of a cursor (014 FR-005, FR-010): the events of its ranges, each range in order, come
//! out in the merge order and only up to the lowest resolved mark of the ranges, so an event never
//! leaves before one that sorts below it and may still arrive.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_hlc::Hlc;

use crate::order::EventKey;

/// Identifier of a range's stream within one cursor.
pub type FeedId = u64;

/// Why the merge refuses an input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeError {
    /// The stream is not open in this merge.
    UnknownFeed(FeedId),
    /// The stream sent an event at or below its resolved mark or below its last event.
    OutOfOrder(FeedId),
}

#[derive(Debug)]
struct Feed {
    resolved: Hlc,
    last: Option<EventKey>,
}

/// The merge of one cursor; `T` is what an event carries besides its place.
#[derive(Debug)]
pub struct Merger<T> {
    feeds: BTreeMap<FeedId, Feed>,
    queue: BTreeMap<EventKey, T>,
    /// The mark of the last event out, or the start: nothing at or below it leaves again.
    emitted: Hlc,
}

impl<T> Merger<T> {
    /// A merge that starts after mark `after`.
    pub fn new(after: Hlc) -> Merger<T> {
        Merger { feeds: BTreeMap::new(), queue: BTreeMap::new(), emitted: after }
    }

    /// Opens the stream of a range, resolved up to `resolved` (where it starts).
    pub fn open(&mut self, feed: FeedId, resolved: Hlc) {
        self.feeds.insert(feed, Feed { resolved, last: None });
    }

    /// Closes a stream (its range split or merged away): its events already in stay.
    pub fn close(&mut self, feed: FeedId) -> Result<(), MergeError> {
        self.feeds.remove(&feed).map(|_| ()).ok_or(MergeError::UnknownFeed(feed))
    }

    /// The streams open.
    pub fn feeds(&self) -> BTreeSet<FeedId> {
        self.feeds.keys().copied().collect()
    }

    /// An event of a stream, above its resolved mark and its previous event.
    pub fn push(&mut self, feed: FeedId, key: EventKey, value: T) -> Result<(), MergeError> {
        let f = self.feeds.get_mut(&feed).ok_or(MergeError::UnknownFeed(feed))?;
        if key.ts <= f.resolved || f.last.as_ref().is_some_and(|l| key <= *l) {
            return Err(MergeError::OutOfOrder(feed));
        }
        f.last = Some(key.clone());
        if key.ts > self.emitted {
            self.queue.insert(key, value);
        }
        Ok(())
    }

    /// A stream is resolved up to `ts`: it sends nothing more at or below it.
    pub fn resolve(&mut self, feed: FeedId, ts: Hlc) -> Result<(), MergeError> {
        let f = self.feeds.get_mut(&feed).ok_or(MergeError::UnknownFeed(feed))?;
        f.resolved = f.resolved.max(ts);
        Ok(())
    }

    /// The resolved mark of the merge: the lowest of its streams (`None` without streams).
    pub fn resolved(&self) -> Option<Hlc> {
        self.feeds.values().map(|f| f.resolved).min()
    }

    /// The events that can leave, in order: those at or below the resolved mark of the merge.
    pub fn ready(&mut self) -> Vec<(EventKey, T)> {
        let Some(resolved) = self.resolved() else { return Vec::new() };
        let mut out = Vec::new();
        while let Some(entry) = self.queue.first_entry() {
            if !entry.key().emittable(resolved) {
                break;
            }
            out.push(entry.remove_entry());
        }
        if let Some((k, _)) = out.last() {
            self.emitted = self.emitted.max(k.ts);
        }
        out
    }

    /// Events waiting for the resolved mark.
    pub fn waiting(&self) -> usize {
        self.queue.len()
    }
}
