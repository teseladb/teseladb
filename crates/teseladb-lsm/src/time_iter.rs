//! Iteration by time (FR-018, research R5): every version written between two timestamps, for
//! change streams (014).
//!
//! Each memtable and each file keeps its lowest and highest timestamps, so a time scan opens only
//! the sources that overlap `(from, to]`; the rest are never read. Versions come in key order and,
//! within a key, from the newest. A `Retract` and the intent it hides do not show: the intent was
//! resolved, and its commit, if any, is a version of its own. Range tombstones of the interval
//! are given apart ([`TimeScan::tombstones`]).

use std::sync::Arc;

use teseladb_keys::Hlc;

use crate::cursor::{Cursor, MemCursor, Pos};
use crate::entry::{self, Kind, RangeTombstone};
use crate::lsm::Lsm;
use crate::read::{Item, Step};
use crate::version::{Version, View};

/// A scan by time in progress: call [`TimeScan::next`] until [`Step::Done`].
pub struct TimeScan {
    version: Arc<Version>,
    start: Vec<u8>,
    end: Vec<u8>,
    from: Hlc,
    to: Hlc,
    cursors: Option<Vec<Cursor>>,
    tombstones: Vec<RangeTombstone>,
    last: Vec<u8>,
    finished: bool,
}

impl std::fmt::Debug for TimeScan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TimeScan").field("from", &self.from).field("to", &self.to).finish()
    }
}

impl View {
    /// Every version of `[start, end)` with a timestamp in `(from, to]` (FR-018).
    pub fn scan_time(&self, start: &[u8], end: &[u8], from: Hlc, to: Hlc) -> TimeScan {
        TimeScan {
            version: self.version.clone(),
            start: start.to_vec(),
            end: end.to_vec(),
            from,
            to,
            cursors: None,
            tombstones: Vec::new(),
            last: Vec::new(),
            finished: false,
        }
    }
}

impl TimeScan {
    fn in_time(&self, min: Hlc, max: Hlc) -> bool {
        max > self.from && min <= self.to
    }

    fn take_tombstones<'a>(&mut self, ts: impl IntoIterator<Item = &'a RangeTombstone>) {
        let (from, to) = (self.from, self.to);
        let (start, end) = (self.start.as_slice(), self.end.as_slice());
        self.tombstones.extend(
            ts.into_iter()
                .filter(|t| {
                    from < t.ts
                        && t.ts <= to
                        && t.start.as_slice() < end
                        && start < t.end.as_slice()
                })
                .cloned(),
        );
    }

    fn open(&mut self) -> Vec<Cursor> {
        let first = entry::first_version(&self.start);
        let v = self.version.clone();
        let mut cursors = Vec::new();
        for m in &v.memtables {
            let (Some(min), Some(max)) = m.times() else { continue };
            if !self.in_time(min, max) {
                continue;
            }
            self.take_tombstones(m.tombstones());
            let mut c = Cursor::Mem(MemCursor::new(m.clone()));
            c.seek(first.clone());
            cursors.push(c);
        }
        for t in v.files() {
            let p = &t.props;
            if !self.in_time(p.min_ts, p.max_ts)
                || p.hi.as_slice() < self.start.as_slice()
                || p.lo.as_slice() >= self.end.as_slice()
            {
                continue;
            }
            self.take_tombstones(&p.tombstones);
            cursors.push(Cursor::Table { table: t.clone(), pos: Pos::Seek(first.clone()) });
        }
        cursors
    }

    /// Memtables and files the scan reads (known once it has started).
    pub fn sources(&self) -> usize {
        self.cursors.as_ref().map_or(0, Vec::len)
    }

    /// Range tombstones over its range with a timestamp in `(from, to]` (known once it has
    /// started).
    pub fn tombstones(&self) -> &[RangeTombstone] {
        &self.tombstones
    }

    /// The next version.
    pub fn next(&mut self, lsm: &mut Lsm) -> Step {
        lsm.reader.begin();
        let step = self.step(lsm);
        if !matches!(step, Step::Miss(_)) {
            lsm.reader.finish();
        }
        step
    }

    fn step(&mut self, lsm: &mut Lsm) -> Step {
        if let Some(e) = &lsm.reader.failure {
            return Step::Failed(e.clone());
        }
        if self.finished {
            return Step::Done;
        }
        if self.cursors.is_none() {
            let c = self.open();
            self.cursors = Some(c);
        }
        let r = &mut lsm.reader;
        let cursors = self.cursors.as_mut().expect("opened");
        loop {
            let mut waiting = None;
            for c in cursors.iter_mut() {
                match c.ready(r) {
                    Err(e) => return Step::Failed(e),
                    Ok(Some(t)) => {
                        waiting.get_or_insert(t);
                    }
                    Ok(None) => {}
                }
            }
            if let Some(t) = waiting {
                return Step::Miss(t);
            }
            let mut min: Option<usize> = None;
            for (i, c) in cursors.iter().enumerate() {
                if let Some(k) = c.key()
                    && min.is_none_or(|m| {
                        entry::compare(k, cursors[m].key().expect("has a key")).is_lt()
                    })
                {
                    min = Some(i);
                }
            }
            let Some(i) = min else {
                self.finished = true;
                return Step::Done;
            };
            let key = cursors[i].key().expect("has a key").to_vec();
            let (logical, ts) = entry::split(&key);
            if logical >= self.end.as_slice() {
                self.finished = true;
                return Step::Done;
            }
            if key == self.last {
                // An older copy of the version just taken (or the intent of a `Retract`).
                if let Err(e) = cursors[i].advance() {
                    return Step::Failed(e);
                }
                continue;
            }
            if ts > self.to {
                cursors[i].seek(entry::internal_key(logical, self.to));
                continue;
            }
            if ts <= self.from {
                cursors[i].seek(entry::after_all_versions(logical));
                continue;
            }
            let (kind, value) = (cursors[i].kind(), cursors[i].value());
            let item = Item { key: logical.to_vec(), ts, kind, value };
            self.last = key;
            if let Err(e) = cursors[i].advance() {
                return Step::Failed(e);
            }
            if kind != Kind::Retract {
                return Step::Item(item);
            }
        }
    }
}
