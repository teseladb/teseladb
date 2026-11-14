//! The statement barrier (005 FR-036, D-163, model `StatementBarrier.tla`): a command of several
//! write statements has each one answered once its entry is in the leader's log, and at the end
//! asks the leader of each range it wrote in for a barrier with the last entry of each term. The
//! leader answers once it holds each of them with its term and they reach the command's write
//! concern by the rules of FR-010; it fails with [`RangeError::EntryLost`] when one of them is not
//! in its log with its term, and with `NotLeader` when it stops leading.

use super::*;
use crate::ack::EntryId;

/// A barrier: the range, the entries to confirm and the command's write concern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BarrierReq {
    /// Range.
    pub g: GroupId,
    /// The last entry of each term the command wrote in.
    pub entries: Vec<EntryId>,
    /// The command's write concern.
    pub wc: WriteConcern,
}

/// A barrier waiting for its entries.
pub(crate) struct BarrierWait {
    pub(super) g: GroupId,
    pub(super) entries: Vec<EntryId>,
    pub(super) ack: AckWhen,
}

/// Where an entry of a barrier stands on the leader.
enum Held {
    /// In its log with its term.
    Yes,
    /// Its term is not known yet: the entry is not on the leader's disk.
    Unknown,
    /// Not in its log with its term.
    No,
    /// Of a later term than the leader's: another replica leads now.
    Ahead,
}

impl RangeHost {
    /// A barrier of a command of several statements (FR-036).
    pub fn barrier<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        req: BarrierReq,
        now: Nanos,
    ) -> ReqId {
        self.now = now;
        let id = self.new_id();
        let BarrierReq { g, entries, wc } = req;
        if !self.ranges.contains_key(&g) {
            self.reply(id, Err(RangeError::OutOfRange));
            return id;
        }
        let replicas = core.conf(g).map_or(1, |c| c.voters.len() + c.learners.len());
        let Ok(ack) = ack_when(wc, replicas) else {
            self.reply(id, Err(RangeError::Unsatisfiable));
            return id;
        };
        self.refresh(core, g);
        self.reqs.insert(id, Req::Barrier(BarrierWait { g, entries, ack }));
        self.step(core, id, now);
        id
    }

    /// Answers barrier `b` if its entries are confirmed, fails it if the leader lacks one or
    /// stopped leading, and leaves it waiting otherwise.
    pub(super) fn step_barrier<S: StateMachine>(
        &mut self,
        core: &MultiRaftCore<S>,
        id: ReqId,
        b: BarrierWait,
    ) -> bool {
        // A command with `w: 0` waits for no one.
        if b.ack == AckWhen::Never {
            self.reply(id, Ok(Reply::Barrier));
            return true;
        }
        let Some(range) = self.ranges.get(&b.g) else {
            self.reply(id, Err(RangeError::OutOfRange));
            return true;
        };
        let (Some(l), Some(st)) = (range.leader.as_ref(), core.status(b.g)) else {
            let hint = core.status(b.g).map_or(NodeId(0), |s| s.leader);
            self.reply(id, Err(RangeError::NotLeader { hint }));
            return true;
        };
        let mut unknown = false;
        for e in &b.entries {
            match held(core, b.g, l.term, *e) {
                Held::Yes => {}
                Held::Unknown => unknown = true,
                Held::Ahead => {
                    self.reply(id, Err(RangeError::NotLeader { hint: st.leader }));
                    return true;
                }
                Held::No => {
                    self.reply(id, Err(RangeError::EntryLost));
                    return true;
                }
            }
        }
        let last = b.entries.iter().map(|e| e.index).max().unwrap_or(0);
        if unknown || !acked(b.ack, last, &st, st.applied >= last) {
            self.reqs.insert(id, Req::Barrier(b));
            return false;
        }
        self.reply(id, Ok(Reply::Barrier));
        true
    }
}

/// Whether the leader of `g` in `term` holds entry `e` with its term. An entry of its own term is
/// one it proposed: a leader never takes an entry of its term out of its log. One of an earlier
/// term is compared with the term at its index once that index is on its disk.
fn held<S: StateMachine>(core: &MultiRaftCore<S>, g: GroupId, term: u64, e: EntryId) -> Held {
    if e.term > term {
        return Held::Ahead;
    }
    let last = core.status(g).map_or(0, |st| st.last_index);
    if e.index > last {
        return Held::No;
    }
    if e.term == term {
        return Held::Yes;
    }
    match core.durable_term(g, e.index) {
        None => Held::Unknown,
        Some(t) if t == e.term => Held::Yes,
        Some(_) => Held::No,
    }
}
