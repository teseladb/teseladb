//! A scan with a filter over a range this node's view does not serve (010 FR-027): the filter
//! goes as a document with the request for each part of the range to its leader, here or on
//! another node, which evaluates it on its core and answers only the documents it accepts, with
//! how many it examined and where the range goes on ([`Work::Filter`]).
//!
//! From the second part of a scan on, the node asks for the next part of a range as soon as the
//! previous one comes, before it hands over its rows, and for the first part of the following
//! ranges of the span, up to [`AHEAD`] parts in flight. A part asked for ahead serves only the
//! scan that asked for it: the scan's next call has to start where the last one stopped, with the
//! same end, number of documents, read concern and filter. Any other call drops the parts in
//! flight, and so does the end of the operation.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::time::Instant;

use teseladb_io::runtime::CoreInput;
use teseladb_range::host::RangeError;
use teseladb_range::read::ReadConcern;
use teseladb_wire::commands::Matching;

use super::{CORE_SCAN_PART, ServerBackend};
use crate::data::{DataReply, DataReq};
use crate::data_wire::{RemoteReply, RemoteReq};
use crate::logic::NodeMsg;
use crate::pushed::{Fallback, Work, Worked, note_fallback};

/// The most parts of one scan in flight at once.
const AHEAD: usize = 4;

/// Parts of filtered scans sent with their filter to the leader of their range.
static SENT: AtomicU64 = AtomicU64::new(0);
/// Parts asked for ahead that the scan then took.
static TAKEN_AHEAD: AtomicU64 = AtomicU64::new(0);

/// Parts of filtered scans this node sent with their filter to the leader of their range since
/// the process started (010 FR-027).
pub fn filtered_parts_sent() -> u64 {
    SENT.load(Ordering::Relaxed)
}

/// Parts of filtered scans asked for ahead that their scan then took, since the process started.
pub fn parts_taken_ahead() -> u64 {
    TAKEN_AHEAD.load(Ordering::Relaxed)
}

/// What makes a scan the same one from a call to the next.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ScanOf {
    end: Vec<u8>,
    examine: usize,
    concern: ReadConcern,
    filter: Vec<u8>,
}

/// A part asked for: where it starts and stops, and its answer to come.
#[derive(Debug)]
struct InFlight {
    start: Vec<u8>,
    stop: Vec<u8>,
    answer: Receiver<Result<DataReply, RangeError>>,
}

/// The parts a worker asked for ahead, for the scan it serves.
#[derive(Debug, Default)]
pub(super) struct ReadAhead {
    /// The scan, and where its next call starts.
    scan: Option<(ScanOf, Option<Vec<u8>>)>,
    parts: Vec<InFlight>,
}

impl ReadAhead {
    /// Drops the parts in flight (the operation ended).
    pub(super) fn clear(&mut self) {
        *self = ReadAhead::default();
    }
}

impl ServerBackend {
    /// Examines at most `examine` documents of the part of `[start, end)` in the range of
    /// `start`, on its leader with `filter`, and gives those the filter accepts and where the
    /// scan goes on; `None` when the leader declined or did not answer, a range has no route or
    /// the answer does not read, and the caller reads the part itself.
    pub(super) fn scan_matching_sent(
        &mut self,
        (start, end): (&[u8], &[u8]),
        examine: usize,
        concern: ReadConcern,
        filter: &[u8],
    ) -> Option<Matching> {
        let scan = ScanOf { end: end.to_vec(), examine, concern, filter: filter.to_vec() };
        let goes_on = self
            .ahead
            .scan
            .as_ref()
            .is_some_and(|(s, next)| *s == scan && next.as_deref() == Some(start));
        if !goes_on {
            self.ahead.clear();
        }
        let asked = self.ahead.parts.iter().position(|p| p.start == start);
        let part = match asked {
            Some(i) => {
                TAKEN_AHEAD.fetch_add(1, Ordering::Relaxed);
                Some(self.ahead.parts.remove(i))
            }
            None => self.send_filtered(start, &scan),
        };
        let found = part.and_then(|part| self.matching_of(part, end));
        if found.is_none() {
            note_fallback(Fallback::Filter);
        }
        let next = found.as_ref().and_then(|m| m.next.clone());
        self.ahead.scan = found.as_ref().map(|_| (scan.clone(), next.clone()));
        if found.is_none() {
            self.ahead.parts.clear();
        }
        if goes_on && let Some(next) = next {
            self.read_ahead(next, &scan);
        }
        found
    }

    /// The answer to `part`, waited for until the operation's deadline.
    fn matching_of(&self, part: InFlight, end: &[u8]) -> Option<Matching> {
        let wait = self.until().saturating_duration_since(Instant::now());
        let reply = part.answer.recv_timeout(wait).ok()?.ok()?;
        let DataReply::Remote(RemoteReply::Pushed(Worked::Matched { rows, seen, examined, next })) =
            reply
        else {
            return None;
        };
        let seen = seen.into_iter().map(usize::try_from).collect::<Result<_, _>>().ok()?;
        // It stopped at its number of documents, or at the end of this range's part: the span
        // goes on in the next range unless that was its end.
        let next = next.or_else(|| (part.stop.as_slice() < end).then_some(part.stop));
        Some(Matching { rows, seen, examined: usize::try_from(examined).ok()?, next })
    }

    /// Asks for the part of the scan at `next`, and the first part of each following range of
    /// the span, while fewer than [`AHEAD`] are in flight and the range is led elsewhere: a
    /// range this node leads is read on the worker's thread from its view.
    fn read_ahead(&mut self, next: Vec<u8>, scan: &ScanOf) {
        let mut from = next;
        while self.ahead.parts.len() < AHEAD && from < scan.end {
            let Some(e) = self.h.routes.lookup(&from) else { return };
            let led_here =
                self.h.views.get(usize::from(e.core)).is_some_and(|v| v.get(e.group).is_some());
            let asked = self.ahead.parts.iter().any(|p| p.start == from);
            if !led_here && !asked {
                let Some(part) = self.send_filtered(&from, scan) else { return };
                self.ahead.parts.push(part);
            }
            if e.end.is_empty() {
                return;
            }
            from = e.end.clone();
        }
    }

    /// Sends the part of the scan from `start` in its range to the range's core, with the filter
    /// for the leader.
    fn send_filtered(&self, start: &[u8], scan: &ScanOf) -> Option<InFlight> {
        let e = self.h.routes.lookup(start)?;
        let stop = match e.end.is_empty() || scan.end <= e.end {
            true => scan.end.clone(),
            false => e.end.clone(),
        };
        let limit = u32::try_from(scan.examine.min(CORE_SCAN_PART)).unwrap_or(u32::MAX);
        let req = RemoteReq::Pushed {
            start: start.to_vec(),
            end: stop.clone(),
            limit,
            concern: scan.concern,
            work: Work::Filter(scan.filter.clone()),
        };
        let (tx, answer) = std::sync::mpsc::channel();
        let msg = NodeMsg::Data(DataReq::Remote(e.group, req), tx);
        self.cores.get(usize::from(e.core))?.send(CoreInput::App(msg)).ok()?;
        SENT.fetch_add(1, Ordering::Relaxed);
        Some(InFlight { start: start.to_vec(), stop, answer })
    }
}
