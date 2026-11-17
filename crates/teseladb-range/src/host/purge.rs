//! The leader's round that deletes the idempotence records 30 minutes old or older (FR-011).

use super::*;

impl RangeHost {
    /// Moves the purge round of range `g`, which this host leads, on (FR-011): every
    /// [`dedup::PURGE_EVERY_MS`] of cluster time it looks for the expired records, takes their
    /// latch so that no retry writes one meanwhile, and proposes their deletion. A round that
    /// found a full batch is followed at once by another that starts where it stopped, until a
    /// round reads every record.
    pub(super) fn purge_step<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        now: Nanos,
    ) -> bool {
        let now_ms = self.cluster_ms();
        let lsm = self.lsm.clone();
        let l = self.ranges.get_mut(&g).and_then(|r| r.leader.as_mut()).expect("a range it leads");
        let idle = Purge::Idle(now_ms + dedup::PURGE_EVERY_MS, None);
        if !l.caught_up(core, g) {
            return false;
        }
        match &mut l.purge {
            Purge::Idle(at, _) if now_ms < *at => false,
            // With its clock out of margin it could not propose the deletion: it waits.
            Purge::Idle(..) if core.read_mark(now).is_none() => false,
            Purge::Idle(_, from) => {
                let from = from.take();
                l.purge = match dedup::cutoff(now_ms) {
                    Some(cut) => {
                        let scan = dedup::PurgeScan::new(&lsm, g, cut, from.as_deref());
                        Purge::Scanning(Box::new(scan))
                    }
                    None => idle,
                };
                true
            }
            Purge::Scanning(scan) => match scan.step(&lsm) {
                None => false,
                Some(Ok(keys)) if !keys.is_empty() => {
                    let resume = scan.resume().map(<[u8]>::to_vec);
                    l.purge = Purge::Latching(keys, resume);
                    true
                }
                Some(_) => {
                    l.purge = idle;
                    true
                }
            },
            Purge::Latching(keys, resume) => {
                let Ok(latch) = l.latches.acquire(keys) else { return false };
                let payload = Command::PurgeDedup { keys: keys.clone() }.encode();
                let resume = resume.take();
                let zero = Hlc { physical: 0, logical: 0 };
                let build = |_| Ok::<_, std::convert::Infallible>(payload);
                match core.propose_at(g, zero, now, build) {
                    Ok((op, _)) => {
                        let index = core.status(g).map_or(0, |st| st.last_index);
                        let term = l.term;
                        l.purge = Purge::Proposed(resume);
                        let id = self.new_id();
                        self.reqs.insert(id, Req::Purge(PurgeReq { g, term, latch, index }));
                        self.ops.insert(op, id);
                    }
                    // Full queue, a handover in progress: it tries again on the next poll, having
                    // moved nothing.
                    Err(ProposeAt::Op(OpError::Overloaded)) => {
                        l.latches.release(latch);
                        if let Purge::Latching(_, r) = &mut l.purge {
                            *r = resume;
                        }
                        return false;
                    }
                    Err(_) => {
                        l.latches.release(latch);
                        l.purge = idle;
                    }
                }
                true
            }
            Purge::Proposed(_) => false,
        }
    }

    /// The core's answer to a `PurgeDedup`: its latch goes as a write's would, and the next round
    /// waits for its turn, or starts at once where this one stopped when it left records.
    pub(super) fn purge_done(&mut self, p: PurgeReq, r: Result<(), OpError>) {
        let applied = r.is_ok();
        match r {
            Ok(()) | Err(OpError::ProposalDropped) => self.release(p.g, p.term, p.latch),
            Err(_) => self.uncertain.push((p.g, p.term, p.latch, p.index)),
        }
        let now_ms = self.cluster_ms();
        let leader = self.ranges.get_mut(&p.g).and_then(|r| r.leader.as_mut());
        if let Some(l) = leader.filter(|l| l.term == p.term) {
            let resume = match std::mem::replace(&mut l.purge, Purge::Idle(0, None)) {
                Purge::Proposed(resume) if applied => resume,
                _ => None,
            };
            let at = if resume.is_some() { now_ms } else { now_ms + dedup::PURGE_EVERY_MS };
            l.purge = Purge::Idle(at, resume);
        }
    }
}
