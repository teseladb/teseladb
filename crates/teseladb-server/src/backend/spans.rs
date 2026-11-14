//! Reads of several spans outside a transaction sent together (010 T053): each span's read goes to
//! the range that holds it at once and the answers are awaited after, so the reads take one round
//! instead of one round each. A span that more than one range holds, a read at a command's mark or
//! with a read preference, and a read that does not come back with rows (an intent, a range that
//! moved, a leader that changed) go on their own with every rule of [`Backend::scan`].

use std::sync::{Arc, mpsc};
use std::time::Instant;

use teseladb_io::runtime::CoreInput;
use teseladb_range::host::RangeError;
use teseladb_range::read::ReadConcern;
use teseladb_router::read_pref::{Mode, ReadPreference};
use teseladb_types::NodeId;
use teseladb_wire::commands::{Backend, Rows, Span};
use teseladb_wire::error::CommandError;

use super::{ServerBackend, scan_req};
use crate::data::{DataReply, DataReq};
use crate::data_wire::{RemoteReply, RemoteReq};
use crate::logic::NodeMsg;

/// The answer of one span of [`ServerBackend::scan_together`].
enum Answer {
    /// Read on this thread.
    Ready(Rows),
    /// Sent to its core, whose answer comes on the channel.
    Sent(mpsc::Receiver<Result<DataReply, RangeError>>),
    /// Read on its own after the others.
    Alone,
}

impl ServerBackend {
    /// Whether the reads of this command go to the leaders at the latest mark, which a read of
    /// several spans sends together.
    pub(super) fn plain_reads(&self) -> bool {
        self.read_mark.is_none() && self.read_pref.as_ref().is_none_or(|p| p.mode == Mode::Primary)
    }

    /// Sends the read of each span that one range holds to that range, then waits for every
    /// answer: the rows of each span, `None` for one that goes again on its own.
    fn scan_together(&mut self, spans: &[Span], concern: ReadConcern) -> Vec<Option<Rows>> {
        let until = self.until();
        let prefer = (self.read_pref.clone(), self.h.node, self.h.routes.tags());
        let mut waiting = Vec::with_capacity(spans.len());
        for s in spans {
            // On this thread when the span's range published a view that serves it (005
            // FR-028); the others go to their cores.
            if let Some((stop, rows)) =
                self.scan_outside((&s.start, &s.end), (s.limit, false), concern)
                && stop == s.end
            {
                waiting.push(Answer::Ready(rows));
                continue;
            }
            waiting.push(self.send_scan(s, &prefer, concern).map_or(Answer::Alone, Answer::Sent));
        }
        waiting
            .into_iter()
            .map(|a| match a {
                Answer::Ready(rows) => Some(rows),
                Answer::Alone => None,
                Answer::Sent(rx) => {
                    match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                        Ok(Ok(DataReply::Remote(RemoteReply::Rows(rows)))) => Some(rows),
                        _ => None,
                    }
                }
            })
            .collect()
    }

    /// Sends the read of span `s` to the range that holds it whole; `None` when no one range
    /// does or its core cannot take it.
    fn send_scan(
        &self,
        s: &Span,
        prefer: &(Option<ReadPreference>, NodeId, Arc<crate::routes::NodeTags>),
        concern: ReadConcern,
    ) -> Option<mpsc::Receiver<Result<DataReply, RangeError>>> {
        let entry = self.h.routes.lookup(&s.start)?;
        let within = entry.end.is_empty() || s.end.as_slice() <= entry.end.as_slice();
        let limit = u32::try_from(s.limit).ok().filter(|_| within)?;
        let req =
            scan_req(prefer, &entry, (s.start.clone(), s.end.clone(), limit), (concern, false));
        let (tx, rx) = mpsc::channel();
        let msg = NodeMsg::Data(DataReq::Remote(entry.group, req), tx);
        self.cores.get(usize::from(entry.core))?.send(CoreInput::App(msg)).ok()?;
        Some(rx)
    }

    /// [`Backend::scan_spans`]: the spans together, and on its own each one without rows.
    pub(super) fn spans_together(
        &mut self,
        spans: &[Span],
        concern: ReadConcern,
    ) -> Result<Vec<Rows>, CommandError> {
        if !self.plain_reads() || spans.len() < 2 {
            return spans.iter().map(|s| self.scan(&s.start, &s.end, s.limit, concern)).collect();
        }
        let answers = self.scan_together(spans, concern);
        spans
            .iter()
            .zip(answers)
            .map(|(s, rows)| match rows {
                Some(rows) => Ok(rows),
                None => self.scan(&s.start, &s.end, s.limit, concern),
            })
            .collect()
    }

    /// The documents of a collection from the counts of the ranges of `[start, end)` (007
    /// FR-039), asked of every range at once and added up (H-004 of the native tests: one
    /// round instead of one round each); `None` when a range does not answer with exact counts,
    /// its core cannot take the request or its leader moved, and the count goes range by range.
    pub(super) fn doc_count_together(
        &self,
        (tenant, collection): (u32, u32),
        (start, end): (&[u8], &[u8]),
    ) -> Option<u64> {
        let until = self.until();
        let mut waiting = Vec::new();
        let mut from = start.to_vec();
        loop {
            let entry = self.h.routes.lookup(&from)?;
            let (tx, rx) = mpsc::channel();
            let req = RemoteReq::DocCount { tenant, collection };
            let msg = NodeMsg::Data(DataReq::Remote(entry.group, req), tx);
            self.cores.get(usize::from(entry.core))?.send(CoreInput::App(msg)).ok()?;
            waiting.push(rx);
            if entry.end.is_empty() || entry.end.as_slice() >= end {
                break;
            }
            from = entry.end;
        }
        let mut total = 0u64;
        for rx in waiting {
            match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(Ok(DataReply::Remote(RemoteReply::DocCount { n: Some(n) }))) => {
                    total = total.saturating_add(n);
                }
                _ => return None,
            }
        }
        Some(total)
    }
}
