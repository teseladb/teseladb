//! Reads of several spans outside a transaction sent together (010 T053): each span's read goes to
//! the range that holds it at once and the answers are awaited after, so the reads take one round
//! instead of one round each. A span that more than one range holds, a read at a command's mark or
//! with a read preference, and a read that does not come back with rows (an intent, a range that
//! moved, a leader that changed) go on their own with every rule of [`Backend::scan`].

use std::sync::mpsc;
use std::time::Instant;

use teseladb_io::runtime::CoreInput;
use teseladb_range::read::ReadConcern;
use teseladb_router::read_pref::Mode;
use teseladb_wire::commands::{Backend, Rows, Span};
use teseladb_wire::error::CommandError;

use super::{ServerBackend, scan_req};
use crate::data::{DataReply, DataReq};
use crate::data_wire::RemoteReply;
use crate::logic::NodeMsg;

impl ServerBackend {
    /// Whether the reads of this command go to the leaders at the latest mark, which a read of
    /// several spans sends together.
    fn plain_reads(&self) -> bool {
        self.read_mark.is_none() && self.read_pref.as_ref().is_none_or(|p| p.mode == Mode::Primary)
    }

    /// Sends the read of each span that one range holds to that range, then waits for every
    /// answer: the rows of each span, `None` for one that goes again on its own.
    fn scan_together(&mut self, spans: &[Span], concern: ReadConcern) -> Vec<Option<Rows>> {
        let until = self.until();
        let prefer = (self.read_pref.clone(), self.h.node, self.h.routes.tags());
        let waiting: Vec<_> = spans
            .iter()
            .map(|s| {
                let entry = self.h.routes.lookup(&s.start)?;
                let within = entry.end.is_empty() || s.end.as_slice() <= entry.end.as_slice();
                let limit = u32::try_from(s.limit).ok().filter(|_| within)?;
                let req =
                    scan_req(&prefer, &entry, (s.start.clone(), s.end.clone(), limit), concern);
                let (tx, rx) = mpsc::channel();
                let msg = NodeMsg::Data(DataReq::Remote(entry.group, req), tx);
                self.cores.get(usize::from(entry.core))?.send(CoreInput::App(msg)).ok()?;
                Some(rx)
            })
            .collect();
        waiting
            .into_iter()
            .map(|rx| match rx?.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(Ok(DataReply::Remote(RemoteReply::Rows(rows)))) => Some(rows),
                _ => None,
            })
            .collect()
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
}
