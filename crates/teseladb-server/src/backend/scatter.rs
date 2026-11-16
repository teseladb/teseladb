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

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use teseladb_exec::pipeline::{PartialGroups, Program};
use teseladb_io::engine::CoreInput;
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
    /// mark or with a read preference, a span of one range unless the program takes columns (010
    /// FR-033), and whenever a core or a leader declines, a range does not answer or its answer
    /// does not read; the caller then reads the span.
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
        // One range goes to its core too when the program takes columns (010 FR-033).
        if pending.is_empty() || (pending.len() < 2 && program.columns().is_none()) {
            return None;
        }
        let found = self.scatter_parts(pending, program, concern);
        if found.is_none() {
            note_fallback(Fallback::Fold);
        }
        found
    }

    /// Runs `program` over the ranges of `pending`, all at once and each in parts until its
    /// stop, each range asking for its next part as soon as the one before is back (010 FR-034);
    /// `None` when a core or a leader declines, a range does not answer or its answer does not
    /// read.
    fn scatter_parts(
        &self,
        pending: Vec<Part>,
        program: &Arc<Program>,
        concern: ReadConcern,
    ) -> Option<Scattered> {
        // In bytes once, for the ranges another node leads.
        let sent = std::cell::OnceCell::new();
        let mut groups: Vec<Vec<PartialGroups>> = vec![Vec::new(); pending.len()];
        let mut examined = 0usize;
        let send = |p: &Part| self.send_part(p, (program, &sent), concern);
        let answer = |i: usize, p: Part, r: Result<DataReply, RangeError>| {
            let (found, n, more) = groups_of(r.ok()?)?;
            groups[i].push(found);
            examined += n;
            Some(more.filter(|from| *from < p.stop).map(|from| Part { from, ..p }))
        };
        drive(pending, self.until(), send, answer)?;
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

/// How long the coordinator waits for the oldest part in flight before it looks at the others
/// (010 FR-034).
const POLL: Duration = Duration::from_millis(1);

/// Drives the parts of several ranges to their ends (010 FR-034), handling each range's answer as
/// it comes: sends the `first` part of every range, then each
/// range's next part as soon as its answer is back, without waiting for the other ranges. The
/// oldest part in flight is waited for up to [`POLL`]; at each wake every part already back goes
/// on. `answer` takes a range's index, its part and the answer, and gives the range's next part,
/// none when the range ended, or `None` to give up. `None` when `send` or `answer` gives up, a
/// part's sender goes away or `until` passes.
fn drive<P, R>(
    first: Vec<P>,
    until: Instant,
    mut send: impl FnMut(&P) -> Option<mpsc::Receiver<R>>,
    mut answer: impl FnMut(usize, P, R) -> Option<Option<P>>,
) -> Option<()> {
    let mut flying: VecDeque<(usize, P, mpsc::Receiver<R>)> = VecDeque::with_capacity(first.len());
    for (i, p) in first.into_iter().enumerate() {
        let rx = send(&p)?;
        flying.push_back((i, p, rx));
    }
    while let Some((_, _, oldest)) = flying.front() {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        let mut back = Vec::new();
        match oldest.recv_timeout(left.min(POLL)) {
            Ok(r) => back.push((0, r)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return None,
        }
        for (at, (_, _, rx)) in flying.iter().enumerate().skip(1) {
            match rx.try_recv() {
                Ok(r) => back.push((at, r)),
                Err(mpsc::TryRecvError::Empty) => {}
                Err(mpsc::TryRecvError::Disconnected) => return None,
            }
        }
        // From the last answered position down, so that the positions before stay where they are.
        let mut taken: Vec<(usize, P, R)> = Vec::with_capacity(back.len());
        for (at, r) in back.into_iter().rev() {
            let (i, p, _) = flying.remove(at).expect("an answered part in flight");
            taken.push((i, p, r));
        }
        for (i, p, r) in taken.into_iter().rev() {
            if let Some(next) = answer(i, p, r)? {
                let rx = send(&next)?;
                flying.push_back((i, next, rx));
            }
        }
    }
    Some(())
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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A part of the fake ranges: its range and its number in that range.
    type Fake = (usize, u32);

    /// 010 FR-034: a range whose parts come back at once goes through all of them while another
    /// range still waits for its first one; the slow range answers only once the fast one ended,
    /// so a driver that waited for every range before the next parts would never get there.
    #[test]
    fn a_range_goes_on_without_waiting_for_the_slowest() {
        let slow: RefCell<Option<mpsc::Sender<u32>>> = RefCell::new(None);
        let mut order = Vec::new();
        let send = |p: &Fake| {
            let (tx, rx) = mpsc::channel();
            match p.0 {
                0 => tx.send(p.1).ok()?,
                _ => *slow.borrow_mut() = Some(tx),
            }
            Some(rx)
        };
        let answer = |i: usize, p: Fake, got: u32| {
            assert_eq!(got, p.1);
            order.push(i);
            match (i, p.1) {
                (0, n) if n < 3 => Some(Some((0, n + 1))),
                (0, _) => {
                    slow.borrow_mut().take()?.send(0).ok()?;
                    Some(None)
                }
                _ => Some(None),
            }
        };
        let until = Instant::now() + Duration::from_secs(5);
        assert_eq!(drive(vec![(0, 1), (1, 0)], until, send, answer), Some(()));
        assert_eq!(order, [0, 0, 0, 1]);
    }

    #[test]
    fn a_part_nobody_answers_gives_up_at_the_deadline() {
        let kept = RefCell::new(Vec::new());
        let send = |_: &Fake| {
            let (tx, rx) = mpsc::channel::<u32>();
            kept.borrow_mut().push(tx);
            Some(rx)
        };
        let until = Instant::now() + Duration::from_millis(20);
        assert_eq!(drive(vec![(0, 0)], until, send, |_, _, _| Some(None)), None);
    }

    #[test]
    fn a_part_whose_answer_went_away_or_was_refused_gives_up() {
        let gone = |_: &Fake| Some(mpsc::channel::<u32>().1);
        let until = Instant::now() + Duration::from_secs(5);
        assert_eq!(drive(vec![(0, 0)], until, gone, |_, _, _| Some(None)), None);
        let answered = |p: &Fake| {
            let (tx, rx) = mpsc::channel();
            tx.send(p.1).ok()?;
            Some(rx)
        };
        assert_eq!(drive(vec![(0, 0), (1, 0)], until, answered, |_, _, _| None), None);
        assert_eq!(drive(Vec::<Fake>::new(), until, answered, |_, _, _| None), Some(()));
    }
}
