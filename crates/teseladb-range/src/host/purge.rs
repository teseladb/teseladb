//! The leader's round that deletes the idempotence records 30 minutes old or older (FR-011).

use super::*;

impl RangeHost {
    /// Moves the purge round of range `g`, which this host leads, on (FR-011): every
    /// [`dedup::PURGE_EVERY_MS`] of cluster time it looks for the expired records, takes their
    /// latch so that no retry writes one meanwhile, and proposes their deletion.
    pub(super) fn purge_step<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        now: Nanos,
    ) -> bool {
        let now_ms = self.cluster_ms();
        let lsm = self.lsm.clone();
        let l = self.ranges.get_mut(&g).and_then(|r| r.leader.as_mut()).expect("a range it leads");
        let idle = Purge::Idle(now_ms + dedup::PURGE_EVERY_MS);
        if !l.caught_up(core, g) {
            return false;
        }
        match &mut l.purge {
            Purge::Idle(at) if now_ms < *at => false,
            // With its clock out of margin it could not propose the deletion: it waits.
            Purge::Idle(_) if core.read_mark(now).is_none() => false,
            Purge::Idle(_) => {
                l.purge = match dedup::cutoff(now_ms) {
                    Some(cut) => Purge::Scanning(Box::new(dedup::PurgeScan::new(&lsm, g, cut))),
                    None => idle,
                };
                true
            }
            Purge::Scanning(scan) => match scan.step(&lsm) {
                None => false,
                Some(Ok(keys)) if !keys.is_empty() => {
                    l.purge = Purge::Latching(keys);
                    true
                }
                Some(_) => {
                    l.purge = idle;
                    true
                }
            },
            Purge::Latching(keys) => {
                let Ok(latch) = l.latches.acquire(keys) else { return false };
                let payload = Command::PurgeDedup { keys: keys.clone() }.encode();
                let zero = Hlc { physical: 0, logical: 0 };
                let build = |_| Ok::<_, std::convert::Infallible>(payload);
                match core.propose_at(g, zero, now, build) {
                    Ok((op, _)) => {
                        let index = core.status(g).map_or(0, |st| st.last_index);
                        let term = l.term;
                        l.purge = Purge::Proposed;
                        let id = self.new_id();
                        self.reqs.insert(id, Req::Purge(PurgeReq { g, term, latch, index }));
                        self.ops.insert(op, id);
                    }
                    // Full queue, a handover in progress: it tries again on the next poll, having
                    // moved nothing.
                    Err(ProposeAt::Op(OpError::Overloaded)) => {
                        l.latches.release(latch);
                        return false;
                    }
                    Err(_) => {
                        l.latches.release(latch);
                        l.purge = idle;
                    }
                }
                true
            }
            Purge::Proposed => false,
        }
    }

    /// The core's answer to a `PurgeDedup`: its latch goes as a write's would, and the next round
    /// waits for its turn.
    pub(super) fn purge_done(&mut self, p: PurgeReq, r: Result<(), OpError>) {
        match r {
            Ok(()) | Err(OpError::ProposalDropped) => self.release(p.g, p.term, p.latch),
            Err(_) => self.uncertain.push((p.g, p.term, p.latch, p.index)),
        }
        let now_ms = self.cluster_ms();
        let leader = self.ranges.get_mut(&p.g).and_then(|r| r.leader.as_mut());
        if let Some(l) = leader.filter(|l| l.term == p.term) {
            l.purge = Purge::Idle(now_ms + dedup::PURGE_EVERY_MS);
        }
    }
}
