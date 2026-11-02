//! The leader's side of write fences (018 FR-002, [`crate::fence`]). [`RangeHost::fence`]
//! proposes a `Fence` and answers with the entry's mark once the range applied it. From the
//! proposal on, the leader refuses the writes and intents of the span before proposing them
//! (`RangeError::Fenced`): every write it proposed before is earlier in the log, so the fence's
//! mark is at or above each of their marks. A new leader proposes nothing before it applied the
//! entries of earlier terms, the fence among them.

use super::*;
use crate::fence::Span;

/// A `Fence` proposed and not yet applied.
pub(crate) struct FenceReq {
    pub(crate) g: GroupId,
    span: Span,
    ts: Hlc,
}

/// Whether an applied entry's output says a fence refused it.
pub trait FenceOutcome {
    /// The range's output, if the entry was a range's.
    fn range_output(&self) -> Option<&crate::sm::RangeOutput>;
}

impl FenceOutcome for crate::sm::RangeOutput {
    fn range_output(&self) -> Option<&crate::sm::RangeOutput> {
        Some(self)
    }
}

/// Whether `out` is a refusal by a fence.
pub(crate) fn refused<O: FenceOutcome>(out: &O) -> bool {
    matches!(out.range_output(), Some(crate::sm::RangeOutput::Fenced))
}

impl RangeHost {
    /// Asks the leader of range `g` to add (`on`) or lift the fence of `[start, end)`; the reply
    /// is [`Reply::Fenced`] with the entry's mark once applied.
    pub fn fence<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        (g, start, end): (GroupId, Vec<u8>, Vec<u8>),
        on: bool,
        now: Nanos,
    ) -> ReqId {
        self.now = now;
        let id = self.new_id();
        let Some(range) = self.ranges.get_mut(&g) else {
            self.reply(id, Err(RangeError::OutOfRange));
            return id;
        };
        let ready = range.leader.as_ref().is_some_and(|l| l.caught_up(core, g));
        let Some(l) = range.leader.as_mut().filter(|_| ready) else {
            let hint = core.status(g).map_or(NodeId(0), |s| s.leader);
            self.reply(id, Err(RangeError::NotLeader { hint }));
            return id;
        };
        let cmd = Command::Fence { start: start.clone(), end: end.clone(), on };
        let payload = cmd.encode();
        let build = |_| Ok::<_, std::convert::Infallible>(payload);
        match core.propose_at(g, ZERO, now, build) {
            Ok((op, ts)) => {
                let span = (start, end);
                if on {
                    l.fencing.push(span.clone());
                }
                self.ops.insert(op, id);
                self.reqs.insert(id, Req::Fence(FenceReq { g, span, ts }));
            }
            Err(ProposeAt::Op(e)) => self.reply(id, Err(op_error(e))),
            Err(ProposeAt::Build(never)) => match never {},
        }
        id
    }

    /// The core's answer to a `Fence`.
    pub(super) fn fence_done<O>(&mut self, id: ReqId, f: FenceReq, r: Result<Done<O>, OpError>) {
        if let Some(l) = self.ranges.get_mut(&f.g).and_then(|r| r.leader.as_mut())
            && let Some(at) = l.fencing.iter().position(|s| *s == f.span)
        {
            l.fencing.remove(at);
        }
        match r {
            Ok(_) => self.reply(id, Ok(Reply::Fenced { ts: f.ts })),
            Err(e) => self.reply(id, Err(op_error(e))),
        }
    }

    /// Whether a fence, applied or proposed by this leader, holds one of `keys` of range `g`.
    pub(super) fn fence_holds<S: StateMachine + RangeState>(
        &self,
        core: &MultiRaftCore<S>,
        g: GroupId,
        keys: &[Vec<u8>],
    ) -> bool {
        // Faulty variant (018 T005): the leader proposes the writes of a fenced span.
        if cfg!(teseladb_bug = "reshard_without_fence") {
            return false;
        }
        let proposed = self
            .ranges
            .get(&g)
            .and_then(|r| r.leader.as_ref())
            .map_or(&[][..], |l| l.fencing.as_slice());
        let sm = core.state_machine(g);
        keys.iter().any(|k| {
            crate::fence::covers(proposed, k) || sm.is_some_and(|sm| RangeState::fenced(sm, k))
        })
    }
}
