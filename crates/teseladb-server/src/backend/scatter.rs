//! An aggregation over the ranges of several cores (010 FR-034, T064): the `$match`es and the
//! partial `$group` of its program go with the scan of each range to the core that owns it, all
//! the ranges at once through the cores' mailboxes; each core runs them over the part it read
//! and answers only its partial groups. A range goes on in bounded parts, as any scan on a core
//! (H-051 of the native tests), each part again at once for every range that has more. The
//! parts come back in key order, which the coordinator merges in.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use teseladb_exec::pipeline::{PartialGroups, Program};
use teseladb_io::runtime::CoreInput;
use teseladb_range::host::RangeError;
use teseladb_range::read::ReadConcern;
use teseladb_wire::commands::Scattered;

use super::{CORE_SCAN_PART, ServerBackend};
use crate::data::{DataReply, DataReq, Fold, Folded};
use crate::data_wire::RemoteReq;
use crate::logic::NodeMsg;

/// Aggregations whose partial groups the cores gave since the process started.
static SCATTERED: AtomicU64 = AtomicU64::new(0);

/// Aggregations whose `$match`es and partial `$group` ran on the cores that own the ranges, all
/// of whose parts came back, since the process started.
pub fn scattered_groups() -> u64 {
    SCATTERED.load(Ordering::Relaxed)
}

/// One range of the span: its group, its core, where its scan goes on and where it stops.
struct Part {
    group: teseladb_types::GroupId,
    core: u16,
    from: Vec<u8>,
    stop: Vec<u8>,
}

impl ServerBackend {
    /// [`teseladb_wire::commands::Backend::scatter_groups`]: `None` for a read at a command's
    /// mark or with a read preference, a span of one range, and whenever a core declines, a
    /// range does not answer or its leader is not this node; the caller then reads the span.
    pub(super) fn scatter_together(
        &self,
        (start, end): (&[u8], &[u8]),
        program: &Arc<Program>,
        concern: ReadConcern,
    ) -> Option<Scattered> {
        if !self.plain_reads() {
            return None;
        }
        let mut pending = self.parts_of(start, end)?;
        if pending.len() < 2 {
            return None;
        }
        let until = self.until();
        let mut groups: Vec<Vec<PartialGroups>> = vec![Vec::new(); pending.len()];
        let mut examined = 0usize;
        let mut index: Vec<usize> = (0..pending.len()).collect();
        while !pending.is_empty() {
            let mut waiting = Vec::with_capacity(pending.len());
            for p in &pending {
                waiting.push(self.send_part(p, program, concern)?);
            }
            let mut next = Vec::new();
            let mut next_index = Vec::new();
            for ((p, i), rx) in pending.into_iter().zip(index).zip(waiting) {
                let wait = until.saturating_duration_since(Instant::now());
                let Ok(Ok(DataReply::Partial(folded))) = rx.recv_timeout(wait) else {
                    return None;
                };
                let Folded { groups: found, examined: n, next: more } = folded;
                let found = Arc::try_unwrap(found?).unwrap_or_else(|a| (*a).clone());
                groups[i].push(found);
                examined += n;
                if let Some(from) = more.filter(|from| *from < p.stop) {
                    next.push(Part { from, ..p });
                    next_index.push(i);
                }
            }
            pending = next;
            index = next_index;
        }
        SCATTERED.fetch_add(1, Ordering::Relaxed);
        Some(Scattered { parts: groups.into_iter().flatten().collect(), examined })
    }

    /// The ranges of `[start, end)` in key order, as the node's routes know them; `None` when a
    /// key has no route.
    fn parts_of(&self, start: &[u8], end: &[u8]) -> Option<Vec<Part>> {
        let mut parts = Vec::new();
        let mut from = start.to_vec();
        while from.as_slice() < end {
            let e = self.h.routes.lookup(&from)?;
            let last = e.end.is_empty() || e.end.as_slice() >= end;
            let stop = if last { end.to_vec() } else { e.end.clone() };
            parts.push(Part { group: e.group, core: e.core, from, stop: stop.clone() });
            from = stop;
        }
        Some(parts)
    }

    /// Sends the next part of the scan of range `p` with `program` to its core.
    fn send_part(
        &self,
        p: &Part,
        program: &Arc<Program>,
        concern: ReadConcern,
    ) -> Option<mpsc::Receiver<Result<DataReply, RangeError>>> {
        let limit = u32::try_from(CORE_SCAN_PART).unwrap_or(u32::MAX);
        let (start, end) = (p.from.clone(), p.stop.clone());
        let scan = RemoteReq::Scan { start, end, limit, concern, backward: false };
        let (tx, rx) = mpsc::channel();
        let msg = NodeMsg::Data(DataReq::Partial(p.group, scan, Fold(program.clone())), tx);
        self.cores.get(usize::from(p.core))?.send(CoreInput::App(msg)).ok()?;
        Some(rx)
    }
}
