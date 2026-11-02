//! The write path of the host (FR-007, FR-008, FR-010, FR-011): latches, the current versions and
//! the idempotence record, evaluation inside the proposal at the write's own mark, the answer at
//! its write concern, and the latch kept while an entry's outcome is uncertain.

use super::*;
use teseladb_multiraft::ReadMode;

/// Why the evaluation inside a proposal stopped.
enum Eval {
    /// The layer above refused the mutation.
    Failed(MutationError),
    /// It has to see the current versions of these keys first (`Mutation::reads`).
    Read(Vec<Vec<u8>>),
    /// Its mark is past its deadline (009 FR-006).
    Expired,
}

impl From<MutationError> for Eval {
    fn from(e: MutationError) -> Eval {
        Eval::Failed(e)
    }
}

impl RangeHost {
    pub(super) fn mutation_done<S: StateMachine>(
        &mut self,
        core: &MultiRaftCore<S>,
        id: ReqId,
        mut m: MutReq,
        r: Result<bool, OpError>,
    ) {
        let p = m.state.proposal();
        let (latch, index, was_replied) = (p.latch, p.index, p.replied);
        if r.is_ok()
            && let Some(range) = self.ranges.get_mut(&m.g)
        {
            let took = self.now.0.saturating_sub(p.proposed_at.0);
            range.counters.propose_latency.record(took);
        }
        match &r {
            // Another leader's entry took its index: it will never be applied.
            Ok(_) | Err(OpError::ProposalDropped) => self.release(m.g, m.term, latch),
            // The entry may still be applied at its mark: a read above it must not go by first.
            Err(_) => self.uncertain.push((m.g, m.term, latch, index)),
        }
        if was_replied || m.ack == AckWhen::Never {
            return;
        }
        match r {
            Err(e) => {
                self.reply(id, Err(op_error(e)));
                return;
            }
            // A fence the range applied first refused it (018): nothing was written.
            Ok(true) => {
                self.reply(id, Err(RangeError::Fenced));
                return;
            }
            Ok(false) => {}
        }
        // Applied: `w: "majority"` answers now; `w: 1` and `w: <n>` once their copies are on disk.
        let reached = m.ack == AckWhen::Applied
            || core.status(m.g).is_some_and(|st| acked(m.ack, index, &st, true));
        let p = m.state.proposal();
        if reached {
            let r = p.answer();
            self.reply(id, Ok(r));
            return;
        }
        p.applied = true;
        self.reqs.insert(id, Req::Mut(m));
    }

    pub(super) fn step_mut<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        mut m: MutReq,
        now: Nanos,
    ) -> bool {
        let mut moved = false;
        if matches!(m.state, MutState::Latch) {
            // The bounds the state machine applied: a cut from an earlier leader's term may have
            // been applied since the embedder last had the host follow it.
            self.follow_bounds(core);
            // A split cut the range while the write waited (007): its keys are the new range's.
            if !m.keys.iter().all(|k| self.covers(m.g, k)) {
                self.reply(id, Err(RangeError::OutOfRange));
                return true;
            }
            let Some(l) = self.ranges.get_mut(&m.g).and_then(|r| r.leader.as_mut()) else {
                let hint = core.status(m.g).map_or(NodeId(0), |s| s.leader);
                self.reply(id, Err(RangeError::NotLeader { hint }));
                return true;
            };
            if !l.caught_up(core, m.g) {
                self.reqs.insert(id, Req::Mut(m));
                return false;
            }
            // A resharding's critical section refuses the writes of its collection (018 FR-002);
            // the resolution of a decided transaction's intents still goes through.
            let fenced_work = match &m.work {
                Work::Mutation(_) => true,
                Work::Txn(op) => matches!(**op, TxnOp::PutIntents { .. } | TxnOp::OnePhase { .. }),
            };
            if fenced_work && self.fence_holds(core, m.g, &m.keys) {
                self.reply(id, Err(RangeError::Fenced));
                return true;
            }
            let Some(l) = self.ranges.get_mut(&m.g).and_then(|r| r.leader.as_mut()) else {
                unreachable!("led above")
            };
            // Writes on the part being split off wait for the cut (007 FR-007), and all of them
            // for a merge's freeze (FR-009).
            let split_gate =
                l.split.gate().is_some_and(|at| m.keys.iter().any(|k| k.as_slice() >= at));
            if split_gate || l.merge.gates() {
                self.reqs.insert(id, Req::Mut(m));
                return false;
            }
            // A retry of the same statement waits for the original (FR-011).
            let record = m.dedup.map(|k| dedup_key(m.g, k.lsid, k.txn_number, k.stmt_id));
            let keys: Vec<Vec<u8>> = m.keys.iter().cloned().chain(record).collect();
            match l.latches.acquire(&keys) {
                Ok(latch) => {
                    m.state = MutState::Read(latch);
                    m.term = l.term;
                    moved = true;
                }
                Err(_) => {
                    if !m.waited {
                        m.waited = true;
                        self.count_wait(m.g);
                    }
                    self.reqs.insert(id, Req::Mut(m));
                    return false;
                }
            }
        }
        if let MutState::Read(latch) = m.state {
            if matches!(m.work, Work::Txn(_)) {
                return self.step_txn(core, id, m, (latch, moved), now);
            }
            if let Some(key) = m.dedup {
                match dedup::lookup(&self.lsm, m.g, key) {
                    None => {
                        self.reqs.insert(id, Req::Mut(m));
                        return moved;
                    }
                    Some(Err(())) => {
                        self.release(m.g, m.term, latch);
                        self.reply(id, Err(RangeError::Unavailable));
                        return true;
                    }
                    Some(Ok(Some((ts, result)))) => {
                        self.release(m.g, m.term, latch);
                        self.reply(id, Ok(Reply::Mutated { ts, change: Vec::new(), result }));
                        return true;
                    }
                    Some(Ok(None)) => {}
                }
            }
            let mut current = Vec::with_capacity(m.keys.len());
            let view = TxnView::default();
            for k in &m.keys {
                // An intent of a transaction stops the write with its owner (008 FR-002): the
                // layer above waits for it, or pushes it if it is abandoned, and writes again.
                match read_visible(&self.lsm, k, (NEWEST, None), &view) {
                    Some(Ok(v)) => current.push(v),
                    Some(Err(e)) => {
                        self.release(m.g, m.term, latch);
                        let e = read_error(e);
                        // Faulty variant (008 T032): the plain write fails with `WriteConflict`
                        // instead of naming the intent's owner to wait for.
                        #[cfg(teseladb_bug = "plain_write_fails_on_intent")]
                        let e = match e {
                            RangeError::IntentConflict(_) => RangeError::WriteConflict,
                            other => other,
                        };
                        self.reply(id, Err(e));
                        return true;
                    }
                    None => {
                        self.reqs.insert(id, Req::Mut(m));
                        return moved;
                    }
                }
            }
            // A split cut the range while the write read its documents (007): they are the new
            // range's, and the caller retries there.
            if !m.keys.iter().all(|k| self.covers(m.g, k)) {
                self.release(m.g, m.term, latch);
                self.reply(id, Err(RangeError::OutOfRange));
                return true;
            }
            return self.propose(core, id, m, latch, &current, now);
        }
        // A refused write waits for the core to confirm the leadership.
        if matches!(m.state, MutState::Confirm(_)) {
            self.reqs.insert(id, Req::Mut(m));
            return moved;
        }
        // `w: 1` and `w: <n>` answer before the entry is applied (D-034).
        let early =
            matches!(m.ack, AckWhen::LeaderLog | AckWhen::LeaderDisk | AckWhen::Replicas(_));
        let p = m.state.proposal();
        if early
            && !p.replied
            && core.status(m.g).is_some_and(|st| acked(m.ack, p.index, &st, p.applied))
        {
            p.replied = true;
            let r = p.answer();
            let applied = p.applied;
            self.reply(id, Ok(r));
            if applied {
                // Nothing else to wait for: its latch went when it was applied.
                return true;
            }
            moved = true;
        }
        self.reqs.insert(id, Req::Mut(m));
        moved
    }

    /// Frees the latches of uncertain writes whose entry was applied in their term or whose term
    /// ended; `true` if one went.
    pub(super) fn settle_uncertain<S: StateMachine>(&mut self, core: &MultiRaftCore<S>) -> bool {
        let (done, wait): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.uncertain).into_iter().partition(|&(g, term, _, index)| {
                let same_term = self
                    .ranges
                    .get(&g)
                    .and_then(|r| r.leader.as_ref())
                    .is_some_and(|l| l.term == term);
                !same_term || core.status(g).is_some_and(|st| st.applied >= index)
            });
        self.uncertain = wait;
        for &(g, term, latch, _) in &done {
            self.release(g, term, latch);
        }
        !done.is_empty()
    }

    pub(super) fn release(&mut self, g: GroupId, term: u64, latch: LatchId) {
        // A latch of an older term is not this term's.
        let leader = self.ranges.get_mut(&g).and_then(|r| r.leader.as_mut());
        if let Some(l) = leader.filter(|l| l.term == term) {
            l.latches.release(latch);
        }
    }

    /// Proposes the write at a mark above the reads of its keys, evaluating it at that mark.
    pub(super) fn propose<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        mut m: MutReq,
        latch: LatchId,
        current: &[Option<(u8, Vec<u8>)>],
        now: Nanos,
    ) -> bool {
        let current: Vec<Option<(u8, &[u8])>> =
            current.iter().map(|c| c.as_ref().map(|(t, v)| (*t, v.as_slice()))).collect();
        let interval = self.ranges.get(&m.g).map(|r| (r.start.clone(), r.end.clone()));
        // A mutation writes only keys of its range: an index entry of another range needs a
        // transaction across ranges (008).
        let inside = |k: &[u8]| {
            interval.as_ref().is_some_and(|(s, e)| s.as_slice() <= k && k < e.as_slice())
        };
        let l = self.ranges.get_mut(&m.g).and_then(|r| r.leader.as_mut()).expect("latched");
        let floor = l.cache.floor_of(&m.keys);
        // Faulty variant (005 T017): the write ignores the reads of its keys and may go below one;
        // the core no longer raises the new leader's clock to the cache's minimum either
        // (`MultiRaftCore::raise_to_read_cache`), which covers the floor on its own (D-083).
        #[cfg(teseladb_bug = "write_below_read")]
        let floor = Hlc { physical: 0, logical: 0 }.min(floor);
        let mut floor = floor;
        // Keys the mutation asked to read (`Mutation::reads`): they join its keys for the retry.
        let mut more = Vec::new();
        let outcome = loop {
            let (mut change, mut result) = (Vec::new(), Vec::new());
            let Work::Mutation(mutation) = &m.work else { unreachable!("a mutation") };
            let (dedup, deadline) = (&m.dedup, m.deadline);
            let build = |ts| {
                let f = |c: &[Option<(u8, &[u8])>], t: Hlc| {
                    if deadline.is_some_and(|d| t.physical > d) {
                        return Err(Eval::Expired);
                    }
                    let o = mutation.eval_with(&m.keys, c, t)?;
                    let keys = o.puts.iter().map(|p| p.key.as_slice());
                    if let Some(k) =
                        keys.chain(o.deletes.iter().map(Vec::as_slice)).find(|k| !inside(k))
                    {
                        return Err(Eval::Failed(MutationError(format!(
                            "{CROSSES_RANGES}: {k:02x?}, which needs a transaction across ranges"
                        ))));
                    }
                    let unread: Vec<Vec<u8>> =
                        mutation.reads(&o).into_iter().filter(|k| !m.keys.contains(k)).collect();
                    if unread.is_empty() { Ok(o) } else { Err(Eval::Read(unread)) }
                };
                let read = (m.keys.as_slice(), current.as_slice());
                let e = evaluate(&mut l.latches, &l.cache, latch, read, ts, f, *dedup)?;
                (change, result) = (e.change, e.result);
                Ok::<_, Refused<Eval>>(e.command.encode())
            };
            match core.propose_at(m.g, floor, now, build) {
                Ok((op, ts)) => {
                    l.latches.set_mark(latch, ts);
                    break Ok((op, ts, change, result));
                }
                Err(ProposeAt::Build(Refused::Below(f))) => floor = f,
                // Its latch was given back: it takes them again.
                Err(ProposeAt::Build(Refused::Busy(_))) => break Err(None),
                // Its latch was given back: it takes it again with the keys to read.
                Err(ProposeAt::Build(Refused::Failed(Eval::Read(keys)))) => {
                    more = keys;
                    break Err(None);
                }
                Err(ProposeAt::Build(Refused::Failed(Eval::Failed(e)))) => {
                    break Err(Some(RangeError::Mutation(e)));
                }
                Err(ProposeAt::Build(Refused::Failed(Eval::Expired))) => {
                    break Err(Some(RangeError::Timeout));
                }
                Err(ProposeAt::Op(e)) => {
                    l.latches.release(latch);
                    break Err(Some(op_error(e)));
                }
            }
        };
        match outcome {
            Ok((op, ts, change, result)) => {
                let index = core.status(m.g).map_or(0, |st| st.last_index);
                let (replied, applied) = (false, false);
                let proposed_at = self.now;
                let p = Proposal {
                    latch,
                    ts,
                    change,
                    result,
                    index,
                    replied,
                    applied,
                    proposed_at,
                    reply: None,
                };
                m.state = MutState::Proposed(p);
                self.ops.insert(op, id);
                self.reqs.insert(id, Req::Mut(m));
            }
            Err(None) => {
                m.keys.extend(more);
                m.state = MutState::Latch;
                self.reqs.insert(id, Req::Mut(m));
            }
            // What the replica holds goes out only while it leads: a deposed leader must not
            // answer from its stale copy (005 FR-009).
            Err(Some(e @ RangeError::Mutation(_))) => match core.read(m.g, ReadMode::Lease, now) {
                Ok(op) => {
                    m.state = MutState::Confirm(e);
                    self.ops.insert(op, id);
                    self.reqs.insert(id, Req::Mut(m));
                }
                Err(oe) => self.reply(id, Err(op_error(oe))),
            },
            Err(Some(e)) => self.reply(id, Err(e)),
        }
        true
    }
}
