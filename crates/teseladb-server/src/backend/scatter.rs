//! An aggregation over the ranges of several cores (010 FR-034, T064): the `$match`es and the
//! partial `$group` of its program go with the scan of each range to the core that owns it, all
//! the ranges at once through the cores' mailboxes; each core runs them over the part it read
//! and answers only its partial groups. A range goes on in bounded parts, as any scan on a core
//! (H-051 of the native tests), each part again at once for every range that has more. The
//! parts come back in key order, which the coordinator merges in.
//!
//! A range this node does not lead gets the program in bytes inside the request its core
//! forwards to the leader ([`RemoteReq::Pushed`]); the leader runs it on its core over its part
//! and answers the partial groups in bytes, which merge with those of this node's cores.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use teseladb_exec::pipeline::{PartialGroups, Program};
use teseladb_io::runtime::CoreInput;
use teseladb_range::host::RangeError;
use teseladb_range::read::ReadConcern;
use teseladb_range::view::RangeViews;
use teseladb_wire::commands::Scattered;

use super::{CORE_SCAN_PART, ServerBackend};
use crate::data::{DataReply, DataReq, Fold, Folded};
use crate::data_wire::{RemoteReply, RemoteReq};
use crate::logic::NodeMsg;
use crate::pushed::{Fallback, Work, Worked, note_fallback};

/// Aggregations whose partial groups the cores gave since the process started.
static SCATTERED: AtomicU64 = AtomicU64::new(0);
/// Parts of those aggregations whose program went to the leader of their range on another node.
static SENT_AWAY: AtomicU64 = AtomicU64::new(0);

/// Parts of split aggregations whose program this node sent to the leader of their range on
/// another node, since the process started (010 FR-034).
pub fn scattered_parts_sent_away() -> u64 {
    SENT_AWAY.load(Ordering::Relaxed)
}

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
    /// mark or with a read preference, a span of one range, and whenever a core or a leader
    /// declines, a range does not answer or its answer does not read; the caller then reads the
    /// span.
    pub(super) fn scatter_together(
        &self,
        (start, end): (&[u8], &[u8]),
        program: &Arc<Program>,
        concern: ReadConcern,
    ) -> Option<Scattered> {
        if !self.plain_reads() {
            return None;
        }
        let pending = self.parts_of(start, end)?;
        if pending.len() < 2 {
            return None;
        }
        let found = self.scatter_parts(pending, program, concern);
        if found.is_none() {
            note_fallback(Fallback::Fold);
        }
        found
    }

    /// Runs `program` over the ranges of `pending`, all at once and each in parts until its
    /// stop; `None` when a core or a leader declines, a range does not answer or its answer does
    /// not read.
    fn scatter_parts(
        &self,
        mut pending: Vec<Part>,
        program: &Arc<Program>,
        concern: ReadConcern,
    ) -> Option<Scattered> {
        let until = self.until();
        // In bytes once, for the ranges another node leads.
        let sent = std::cell::OnceCell::new();
        let mut groups: Vec<Vec<PartialGroups>> = vec![Vec::new(); pending.len()];
        let mut examined = 0usize;
        let mut index: Vec<usize> = (0..pending.len()).collect();
        while !pending.is_empty() {
            let mut waiting = Vec::with_capacity(pending.len());
            for p in &pending {
                waiting.push(self.send_part(p, (program, &sent), concern)?);
            }
            let mut next = Vec::new();
            let mut next_index = Vec::new();
            for ((p, i), rx) in pending.into_iter().zip(index).zip(waiting) {
                let wait = until.saturating_duration_since(Instant::now());
                let Ok(Ok(reply)) = rx.recv_timeout(wait) else { return None };
                let (found, n, more) = groups_of(reply)?;
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

    /// Sends the next part of the scan of range `p` with `program` to its core: to run there when
    /// this node leads the range, or in bytes (`sent`, encoded on the first need) for its core to
    /// forward to the leader.
    fn send_part(
        &self,
        p: &Part,
        (program, sent): (&Arc<Program>, &std::cell::OnceCell<Option<Vec<u8>>>),
        concern: ReadConcern,
    ) -> Option<mpsc::Receiver<Result<DataReply, RangeError>>> {
        let limit = u32::try_from(CORE_SCAN_PART).unwrap_or(u32::MAX);
        let (start, end) = (p.from.clone(), p.stop.clone());
        let (tx, rx) = mpsc::channel();
        let msg = if leads_here(&self.h.views, p) {
            let scan = RemoteReq::Scan { start, end, limit, concern, backward: false };
            NodeMsg::Data(DataReq::Partial(p.group, scan, Fold(program.clone())), tx)
        } else {
            let work = Work::Fold(sent.get_or_init(|| program.encode()).clone()?);
            SENT_AWAY.fetch_add(1, Ordering::Relaxed);
            let req = RemoteReq::Pushed { start, end, limit, concern, work };
            NodeMsg::Data(DataReq::Remote(p.group, req), tx)
        };
        self.cores.get(usize::from(p.core))?.send(CoreInput::App(msg)).ok()?;
        Some(rx)
    }
}

/// Whether this node publishes a view of the range of `p`, which only the range's leader does.
fn leads_here(views: &[RangeViews], p: &Part) -> bool {
    views.get(usize::from(p.core)).is_some_and(|v| v.get(p.group).is_some())
}

/// The partial groups of a part, the documents it read and where its range goes on, from the
/// answer of this node's core or of the leader on another node; `None` when either declined or
/// the groups do not read.
fn groups_of(reply: DataReply) -> Option<(PartialGroups, usize, Option<Vec<u8>>)> {
    match reply {
        DataReply::Partial(Folded { groups, examined, next }) => {
            Some((Arc::try_unwrap(groups?).unwrap_or_else(|a| (*a).clone()), examined, next))
        }
        DataReply::Remote(RemoteReply::Pushed(Worked::Folded { groups, examined, next })) => {
            let examined = usize::try_from(examined).ok()?;
            Some((PartialGroups::decode(&groups)?, examined, next))
        }
        _ => None,
    }
}
