//! The leaders' side of a merge (007 FR-009, FR-022). Group 0 recorded the intent; the embedder
//! asks each range's leader for its part:
//!
//! - The right range's leader freezes it ([`RangeHost::freeze`]): from then on no write takes its
//!   latches (it waits, and gets `OutOfRange` once the freeze is applied), and once no latch is
//!   held every write proposed is in the log before `MergeFreeze`. A frozen range serves nothing
//!   and runs no purge or checksum. After an aborted merge it thaws ([`RangeHost::thaw`]).
//! - The left range's leader, once the embedder has seen the right range frozen and its data on
//!   the disk of every replica's node, reads the right range's idempotence table and proposes
//!   `Merge` with its frozen state ([`RangeHost::absorb`]), or gives the merge up
//!   ([`RangeHost::give_up`]).
//!
//! A proposal refused or a change of term drops the job: the embedder asks again, and applying
//! the same `MergeFreeze` or `Merge` twice changes nothing.

use super::*;
use crate::command::Absorbed;
use crate::local_keys::RecordScan;

/// What the left range's leader takes from the right range's frozen replica on its node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeSource {
    /// The right range.
    pub right: GroupId,
    /// The merge (the Group 0 index of its intent).
    pub merge: u64,
    /// First key past the right range.
    pub end: Vec<u8>,
    /// Its GC threshold.
    pub gc: Hlc,
    /// Its last mark.
    pub mark: Hlc,
    /// Its live bytes.
    pub live: u64,
}

/// Where a leader's part of a merge is.
pub(crate) enum MergeJob {
    /// None asked.
    Idle,
    /// Writes wait; the leader waits for their latches to go before proposing `MergeFreeze`.
    Draining { merge: u64 },
    /// `MergeFreeze` is proposed: writes keep waiting until the range applies it, or a write
    /// proposed meanwhile would land after it in the log.
    Freezing,
    /// Reading the right range's idempotence table before proposing `Merge`.
    Scanning { source: MergeSource, scan: Box<RecordScan> },
}

impl MergeJob {
    /// Whether the writes of the range wait.
    pub(crate) fn gates(&self) -> bool {
        matches!(self, MergeJob::Draining { .. } | MergeJob::Freezing)
    }
}

/// Proposes `cmd` on `g` at the leader's mark; `true` if the core took it.
fn propose<S: StateMachine>(
    core: &mut MultiRaftCore<S>,
    g: GroupId,
    cmd: &Command,
    now: Nanos,
) -> bool {
    let payload = cmd.encode();
    let zero = Hlc { physical: 0, logical: 0 };
    let build = |_| Ok::<_, std::convert::Infallible>(payload);
    core.propose_at(g, zero, now, build).is_ok()
}

impl RangeHost {
    /// The leader of `g` that has no merge job in progress.
    fn idle_merge_leader(&mut self, g: GroupId) -> Option<&mut Leader> {
        self.ranges
            .get_mut(&g)
            .and_then(|r| r.leader.as_mut())
            .filter(|l| matches!(l.merge, MergeJob::Idle))
    }

    /// Asks the leader of range `g` to freeze it for merge `merge`. `false` if this host does not
    /// lead `g` or another merge job is in progress.
    pub fn freeze(&mut self, g: GroupId, merge: u64) -> bool {
        let Some(l) = self.idle_merge_leader(g) else { return false };
        l.merge = MergeJob::Draining { merge };
        true
    }

    /// The leader of range `g` thaws it after an aborted merge; `false` if it does not lead `g`,
    /// has not applied the entries of earlier terms or the core refused the proposal.
    pub fn thaw<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        now: Nanos,
    ) -> bool {
        let ready = self.idle_merge_leader(g).is_some_and(|l| l.caught_up(core, g));
        ready && propose(core, g, &Command::MergeFreeze { merge: 0 }, now)
    }

    /// Asks the leader of range `g` to absorb the range `source` describes, whose replica on this
    /// node is frozen. `false` if this host does not lead `g` or another merge job is in progress.
    pub fn absorb(&mut self, g: GroupId, source: MergeSource) -> bool {
        let scan = Box::new(RecordScan::new(&self.lsm, source.right));
        let Some(l) = self.idle_merge_leader(g) else { return false };
        l.merge = MergeJob::Scanning { source, scan };
        true
    }

    /// The leader of range `g` gives merge `merge` of `right` up; `false` if it does not lead
    /// `g`, has not applied the entries of earlier terms or the core refused the proposal.
    pub fn give_up<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        (right, merge): (GroupId, u64),
        now: Nanos,
    ) -> bool {
        let ready = self.idle_merge_leader(g).is_some_and(|l| l.caught_up(core, g));
        ready && propose(core, g, &Command::Merge { right, merge, absorb: None }, now)
    }

    /// Moves the merge job of range `g` if this host leads it and its range applied the entries
    /// of earlier terms; `true` if it moved.
    pub(super) fn merge_step<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        now: Nanos,
    ) -> bool {
        let lsm = self.lsm.clone();
        let led = self.ranges.get_mut(&g).and_then(|r| {
            let (start, end, frozen) = (r.start.clone(), r.end.clone(), r.frozen);
            r.leader.as_mut().filter(|l| l.caught_up(core, g)).map(|l| (start, end, frozen, l))
        });
        let Some((start, end, frozen, l)) = led else { return false };
        let newest = Hlc { physical: u64::MAX, logical: u32::MAX };
        match &mut l.merge {
            MergeJob::Idle => false,
            MergeJob::Draining { merge } => {
                if l.latches.read_wait(&start, &end, newest, true) != ReadWait::Free {
                    return false;
                }
                let cmd = Command::MergeFreeze { merge: *merge };
                // A proposal refused (full queue, a handover) drops the job: the embedder asks
                // again.
                let proposed = propose(core, g, &cmd, now);
                l.merge = if proposed { MergeJob::Freezing } else { MergeJob::Idle };
                true
            }
            // Over once the range applied it (and with the leadership, by a new `Leader`).
            MergeJob::Freezing => {
                if !frozen {
                    return false;
                }
                l.merge = MergeJob::Idle;
                true
            }
            MergeJob::Scanning { source, scan } => match scan.step(&lsm) {
                None => false,
                Some(result) => {
                    let source = source.clone();
                    l.merge = MergeJob::Idle;
                    // A table that could not be read: the embedder asks again.
                    if let Ok(records) = result {
                        let MergeSource { right, merge, end, gc, mark, live } = source;
                        let absorb = Some(Absorbed { end, gc, mark, live, records });
                        propose(core, g, &Command::Merge { right, merge, absorb }, now);
                    }
                    true
                }
            },
        }
    }
}
