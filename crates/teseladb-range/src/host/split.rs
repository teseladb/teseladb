//! The leader's side of a split (007 FR-007, FR-022). Group 0 recorded the intent and gave the
//! new range its number; the embedder asks the range's leader with [`RangeHost::split`]. From then
//! on no write on `[at, end)` takes its latches (it waits, and gets `OutOfRange` once the cut is
//! applied); once no latch of that interval is held, every statement on those keys has its
//! idempotence record applied, so the leader reads the whole table and proposes `Split` with it.
//! When the state machine applies the cut, every replica's host follows the new end
//! ([`RangeHost::follow_state`]) and this leader reports the split ([`RangeHost::take_splits`]) for
//! the embedder to complete it in Group 0. A change of term drops the job: the embedder asks the
//! new leader again, or completes the split if the cut is already applied.
//!
//! A range born from a split ([`RangeHost::add_born_range`]) has its first leader propose
//! `SplitGenesis`, which records its parent in its own log.

use super::*;
use crate::command::CopiedRecord;
use crate::local_keys::{LiveScan, RecordScan};

/// Where a leader's split is.
pub(crate) enum SplitJob {
    /// None asked.
    Idle,
    /// Writes on `[at, end)` wait; the leader waits for their latches to go.
    Draining { at: Vec<u8>, new_range: GroupId },
    /// Reading the idempotence table.
    Scanning { at: Vec<u8>, new_range: GroupId, scan: Box<RecordScan> },
    /// Counting the live bytes of `[at, end)`, with the records read.
    Counting { at: Vec<u8>, new_range: GroupId, records: Vec<CopiedRecord>, scan: Box<LiveScan> },
    /// `Split` is proposed; it ends when the state machine's end is `at`.
    Proposed { at: Vec<u8>, new_range: GroupId },
}

impl SplitJob {
    /// The first key of the writes that wait.
    pub(crate) fn gate(&self) -> Option<&[u8]> {
        match self {
            SplitJob::Idle => None,
            SplitJob::Draining { at, .. }
            | SplitJob::Scanning { at, .. }
            | SplitJob::Counting { at, .. }
            | SplitJob::Proposed { at, .. } => Some(at),
        }
    }
}

/// A split this host's leader saw applied: the embedder completes it in Group 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitDone {
    /// The range that was cut.
    pub g: GroupId,
    /// First key of the new range.
    pub at: Vec<u8>,
    /// The new range.
    pub new_range: GroupId,
}

impl RangeHost {
    /// Asks the leader of range `g` to split it at `at` into `new_range`, as Group 0 recorded.
    /// `false` if this host does not lead `g`, `at` is not strictly inside it, or another split
    /// is in progress.
    pub fn split(&mut self, g: GroupId, at: &[u8], new_range: GroupId) -> bool {
        let Some(r) = self.ranges.get_mut(&g) else { return false };
        let inside = r.start.as_slice() < at && at < r.end.as_slice();
        let Some(l) = r.leader.as_mut().filter(|l| inside && matches!(l.split, SplitJob::Idle))
        else {
            return false;
        };
        l.split = SplitJob::Draining { at: at.to_vec(), new_range };
        true
    }

    /// Range `g` was born from `parent`'s `Split` at index `at_index`: it covers `[start, end)`
    /// and its first leader proposes `SplitGenesis`.
    pub fn add_born_range(
        &mut self,
        g: GroupId,
        start: Vec<u8>,
        end: Vec<u8>,
        parent: GroupId,
        at_index: u64,
    ) {
        self.add_range(g, start, end);
        self.ranges.entry(g).and_modify(|r| r.genesis = Some((parent, at_index)));
    }

    /// The splits this host's leaders saw applied since the last call.
    pub fn take_splits(&mut self) -> Vec<SplitDone> {
        std::mem::take(&mut self.splits)
    }

    /// Moves the split of range `g` if this host leads it and its range applied the entries of
    /// earlier terms; `true` if it moved.
    pub(super) fn split_step<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        now: Nanos,
    ) -> bool {
        let lsm = self.lsm.clone();
        let led = self.ranges.get_mut(&g).and_then(|r| {
            let (start, end) = (r.start.clone(), r.end.clone());
            r.leader.as_mut().filter(|l| l.caught_up(core, g)).map(|l| (start, end, l))
        });
        let Some((start, end, l)) = led else { return false };
        let newest = Hlc { physical: u64::MAX, logical: u32::MAX };
        match &mut l.split {
            SplitJob::Idle => false,
            // A cut an earlier leader proposed, applied since the split was asked for, already
            // put the key at the end (or past it): a second `Split` there would stop the range.
            SplitJob::Draining { at, .. } if *at <= start || *at >= end => {
                l.split = SplitJob::Idle;
                true
            }
            SplitJob::Draining { at, new_range } => {
                if l.latches.read_wait(at, &end, newest, WaitFor::Every) != ReadWait::Free {
                    return false;
                }
                let scan = Box::new(RecordScan::new(&lsm, g));
                l.split = SplitJob::Scanning { at: at.clone(), new_range: *new_range, scan };
                true
            }
            SplitJob::Scanning { at, new_range, scan } => match scan.step(&lsm) {
                None => false,
                Some(Err(())) => {
                    // The table could not be read: the embedder asks again.
                    l.split = SplitJob::Idle;
                    true
                }
                Some(Ok(records)) => {
                    let scan = Box::new(LiveScan::new(&lsm, at, &end));
                    let (at, new_range) = (at.clone(), *new_range);
                    l.split = SplitJob::Counting { at, new_range, records, scan };
                    true
                }
            },
            SplitJob::Counting { at, new_range, records, scan } => match scan.step(&lsm) {
                None => false,
                Some(Err(())) => {
                    // The count could not be read: the embedder asks again.
                    l.split = SplitJob::Idle;
                    true
                }
                Some(Ok(right_live)) => {
                    #[cfg(teseladb_bug = "split_loses_intents")]
                    drop_intents(core, g, &scan.intents, now);
                    let (at, new_range) = (at.clone(), *new_range);
                    let records = std::mem::take(records);
                    let split = Command::Split { at: at.clone(), new_range, records, right_live };
                    let payload = split.encode();
                    let zero = Hlc { physical: 0, logical: 0 };
                    let build = |_| Ok::<_, std::convert::Infallible>(payload);
                    // A proposal refused (full queue, a handover) drops the job: the embedder asks
                    // again.
                    let proposed = core.propose_at(g, zero, now, build);
                    l.split =
                        proposed.map_or(SplitJob::Idle, |_| SplitJob::Proposed { at, new_range });
                    true
                }
            },
            SplitJob::Proposed { at, new_range } => {
                if end != *at {
                    return false;
                }
                let done = SplitDone { g, at: at.clone(), new_range: *new_range };
                l.split = SplitJob::Idle;
                self.splits.push(done);
                true
            }
        }
    }

    /// The first leader of a range born from a split proposes `SplitGenesis`; `true` if it did.
    pub(super) fn genesis_step<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        now: Nanos,
    ) -> bool {
        let pending = self.ranges.get(&g).and_then(|r| {
            r.genesis.filter(|_| r.leader.as_ref().is_some_and(|l| l.caught_up(core, g)))
        });
        let Some((parent, at_index)) = pending else { return false };
        let end = self.ranges.get(&g).map(|r| r.end.clone()).unwrap_or_default();
        let payload = Command::SplitGenesis { parent, at_index, end }.encode();
        let zero = Hlc { physical: 0, logical: 0 };
        let build = |_| Ok::<_, std::convert::Infallible>(payload);
        let proposed = core.propose_at(g, zero, now, build).is_ok();
        self.ranges.entry(g).and_modify(|r| r.genesis = r.genesis.filter(|_| !proposed));
        proposed
    }

    /// Follows the end each state machine applied (a split's cut, a merge) and whether it is
    /// frozen for a merge; `true` if one moved.
    pub(crate) fn follow_bounds<S: StateMachine + RangeState>(
        &mut self,
        core: &MultiRaftCore<S>,
    ) -> bool {
        let mut moved = false;
        for (g, r) in &mut self.ranges {
            let sm = core.state_machine(*g);
            if let Some(end) = sm.and_then(RangeState::end).filter(|e| *e != r.end.as_slice()) {
                r.end = end.to_vec();
                moved = true;
            }
            let frozen = sm.is_some_and(RangeState::frozen);
            moved |= r.frozen != frozen;
            r.frozen = frozen;
        }
        moved
    }
}

/// Faulty variant (007 T017): the split retracts the intents of the half that moves, as if their
/// transactions had aborted, so a transaction that commits loses its write there.
#[cfg(teseladb_bug = "split_loses_intents")]
fn drop_intents<S: StateMachine>(
    core: &mut MultiRaftCore<S>,
    g: GroupId,
    intents: &[(Vec<u8>, Hlc, Vec<u8>)],
    now: Nanos,
) {
    use crate::command::Resolution;
    use crate::intents::{Intent, TxnStatus};
    let zero = Hlc { physical: 0, logical: 0 };
    for (key, intent_ts, payload) in intents {
        let Ok(intent) = Intent::decode(payload) else { continue };
        let resolutions =
            vec![Resolution { key: key.clone(), intent_ts: *intent_ts, commit: None }];
        let cmd = Command::ResolveIntents {
            txn: intent.txn,
            status: TxnStatus::Aborted,
            commit_ts: zero,
            resolutions,
            live_delta: 0,
        };
        let payload = cmd.encode();
        let _ = core.propose_at(g, zero, now, |_| Ok::<_, std::convert::Infallible>(payload));
    }
}
