//! The write path of the host (FR-007, FR-008, FR-010, FR-011): latches, the current versions and
//! the idempotence record, evaluation inside the proposal at the write's own mark, the answer at
//! its write concern, and the latch kept while an entry's outcome is uncertain.

use super::*;
use crate::ack::AckWhen;
use teseladb_multiraft::ReadMode;
use teseladb_stages::Stage;

/// Why the evaluation inside a proposal stopped.
enum Eval {
    /// The layer above refused the mutation.
    Failed(MutationError),
    /// It has to see the current versions of these keys first (`Mutation::reads`).
    Read(Vec<Vec<u8>>),
    /// Its mark is past its deadline (009 FR-006).
    Expired,
    /// It changes nothing (005 FR-026): its answer, at the mark it was evaluated at, goes once
    /// the replica confirms it still leads, without an entry.
    NoChange(Vec<u8>, Hlc),
}

impl From<MutationError> for Eval {
    fn from(e: MutationError) -> Eval {
        Eval::Failed(e)
    }
}

/// The refusal of a retryable statement whose session has a newer `txnNumber` in the range, as
/// the reference server words it (`TransactionTooOld`, 225).
fn too_old(txn_number: i64, newest: i64) -> MutationError {
    MutationError(format!(
        "225\u{0}Retryable write with txnNumber {txn_number} is prohibited on session because a \
         newer retryable write with txnNumber {newest} has already started on this session."
    ))
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
            teseladb_stages::record(Stage::WriteProposeToApplied, took);
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
            teseladb_stages::record(Stage::WriteRange, self.now.0.saturating_sub(m.arrived.0));
            self.note_answered(m.g, m.term, index);
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
            // New writes go in at the rate the LSM's debt allows (FR-038), in order of arrival;
            // a decided transaction's resolution frees what others wait for and always goes in.
            if fenced_work && !m.admitted {
                if !self.throttle.admit(now) {
                    self.reqs.insert(id, Req::Mut(m));
                    return false;
                }
                m.admitted = true;
            }
            // A retry of the same statement waits for the original (FR-011).
            let record = m.dedup.map(|k| dedup_key(m.g, k.lsid, k.txn_number, k.stmt_id));
            let keys: Vec<Vec<u8>> = m.keys.iter().cloned().chain(record).collect();
            // A write without a transaction goes on the pending writes of its keys (FR-029); a
            // transaction's waits for them to be applied.
            let taken = match m.work {
                Work::Mutation(_) if !m.settled => l.latches.acquire_pipelined(&keys),
                Work::Mutation(_) | Work::Txn(_) => l.latches.acquire(&keys),
            };
            match taken {
                Ok(latch) => {
                    teseladb_stages::record(
                        Stage::WriteLatchWait,
                        now.0.saturating_sub(m.arrived.0),
                    );
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
            // The blocks it missed are not back: it would find them missing again (FR-037).
            // Every result of the disk steps the requests of the core, and a write of 191 keys
            // read them all again, 0,7 ms each time: behind the 250 file operations of a core
            // change its core spent 0,4 s in one turn (H-060 of the native tests).
            if !self.lsm.borrow().block_reads_back(m.read_due) {
                self.reqs.insert(id, Req::Mut(m));
                return moved;
            }
            if matches!(m.work, Work::Txn(_)) {
                return self.step_txn(core, id, m, (latch, moved), now);
            }
            if let Some(key) = m.dedup {
                match dedup::lookup(&self.lsm, m.g, key) {
                    None => {
                        self.wait_for_blocks(id, m);
                        return moved;
                    }
                    Some(Err(())) => {
                        self.release(m.g, m.term, latch);
                        self.reply(id, Err(RangeError::Unavailable));
                        return true;
                    }
                    Some(Ok(Some((ts, result)))) => {
                        self.release(m.g, m.term, latch);
                        self.reply(
                            id,
                            Ok(Reply::Mutated { ts, change: Vec::new(), result, entry: None }),
                        );
                        return true;
                    }
                    // Without its record, a statement of an older `txnNumber` than the session's
                    // newest in this range is too old: its record may have been purged.
                    Some(Ok(None)) => match dedup::superseded(&self.lsm, m.g, key) {
                        None => {
                            self.wait_for_blocks(id, m);
                            return moved;
                        }
                        Some(Err(())) => {
                            self.release(m.g, m.term, latch);
                            self.reply(id, Err(RangeError::Unavailable));
                            return true;
                        }
                        Some(Ok(Some(newest))) => {
                            self.release(m.g, m.term, latch);
                            self.reply(
                                id,
                                Err(RangeError::Mutation(too_old(key.txn_number, newest))),
                            );
                            return true;
                        }
                        Some(Ok(None)) => {}
                    },
                }
            }
            let view = TxnView::default();
            let leader = self.ranges.get(&m.g).and_then(|r| r.leader.as_ref());
            // Every key is read in one pass, the missing blocks asked for together (as a
            // transaction's keys are, `RangeHost::step_txn`); what was read stays read.
            let mut read = match std::mem::take(&mut m.read) {
                Partial::Visible(l, read) if l == latch && read.len() == m.keys.len() => read,
                _ => vec![None; m.keys.len()],
            };
            let mut missing = false;
            for (k, slot) in m.keys.iter().zip(read.iter_mut()) {
                if slot.is_some() {
                    continue;
                }
                // The value a pending write of the key leaves, which this one goes on (FR-029).
                if let Some(v) = leader.and_then(|l| l.latches.pending_value(k)) {
                    *slot = Some(v.clone());
                    continue;
                }
                // An intent of a transaction stops the write with its owner (008 FR-002): the
                // layer above waits for it, or pushes it if it is abandoned, and writes again.
                match read_visible(&self.lsm, k, (NEWEST, None), &view) {
                    Some(Ok(v)) => *slot = Some(v),
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
                    None => missing = true,
                }
            }
            if missing {
                m.read = Partial::Visible(latch, read);
                self.wait_for_blocks(id, m);
                return moved;
            }
            let current: Vec<crate::push::Visible> = read.into_iter().flatten().collect();
            // A split cut the range while the write read its documents (007): they are the new
            // range's, and the caller retries there.
            if !m.keys.iter().all(|k| self.covers(m.g, k)) {
                self.release(m.g, m.term, latch);
                self.reply(id, Err(RangeError::OutOfRange));
                return true;
            }
            // A transaction that holds its writes until the commit locked one of the keys (008
            // FR-022): the layer above waits for the lock and writes again.
            if self.locked_by_other(m.g, &m.keys, None) {
                self.release(m.g, m.term, latch);
                self.reply(id, Err(RangeError::KeyLocked));
                return true;
            }
            return self.propose(core, id, m, latch, &current, now);
        }
        // A refused write waits for the core to confirm the leadership.
        if matches!(m.state, MutState::Confirm(_) | MutState::Answer(_)) {
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
            teseladb_stages::record(Stage::WriteRange, now.0.saturating_sub(m.arrived.0));
            let applied = p.applied;
            let (latch, index) = (p.latch, p.index);
            // From now on a `local` read waits for it: it saw the write answered (DV-010).
            let leader = self.ranges.get_mut(&m.g).and_then(|r| r.leader.as_mut());
            if let Some(l) = leader.filter(|l| l.term == m.term) {
                l.latches.mark_answered(latch);
            }
            // An intent is no version a read sees, and its transaction commits once it checked
            // that its intents are applied (008 FR-021): no scan has to see it.
            let intents =
                matches!(&m.work, Work::Txn(op) if matches!(**op, TxnOp::PutIntents { .. }));
            if !intents {
                self.note_answered(m.g, m.term, index);
            }
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

    /// Leaves write `m` waiting for the blocks its reads just missed: it reads again once the
    /// block reads asked so far, its own among them, are back (FR-037).
    pub(super) fn wait_for_blocks(&mut self, id: ReqId, mut m: MutReq) {
        m.read_due = self.lsm.borrow().block_reads_asked();
        self.reqs.insert(id, Req::Mut(m));
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

    /// A write of a range on its way to another core is not proposed (H-025): it gives its latch
    /// back and is refused with `NotLeader` and no hint, which its client tries again.
    pub(super) fn refuse_moving(
        &mut self,
        id: ReqId,
        m: &MutReq,
        latch: LatchId,
        stopped: bool,
    ) -> bool {
        // A replica that applied `Relocate` to another core proposes nothing either, whoever
        // proposed it: its leader may not have known the proposal of an earlier term.
        if !self.is_moving(m.g) && !stopped {
            return false;
        }
        self.release(m.g, m.term, latch);
        self.reply(id, Err(RangeError::NotLeader { hint: NodeId(0) }));
        true
    }

    /// Notes that the write of entry `index`, which the leader of range `g` in `term` proposed,
    /// is answered to its client (005 FR-031), before the answer leaves: a scan outside the core
    /// that begins from now on reads a view that applied the entry.
    fn note_answered(&self, g: GroupId, term: u64, index: u64) {
        let leader = self.ranges.get(&g).and_then(|r| r.leader.as_ref());
        if let Some(l) = leader.filter(|l| l.term == term) {
            l.answered.fetch_max(index, Ordering::SeqCst);
        }
    }

    pub(super) fn release(&mut self, g: GroupId, term: u64, latch: LatchId) {
        // A latch of an older term is not this term's.
        let leader = self.ranges.get_mut(&g).and_then(|r| r.leader.as_mut());
        if let Some(l) = leader.filter(|l| l.term == term) {
            l.latches.release(latch);
        }
    }

    /// Proposes the write at a mark above the reads of its keys, evaluating it at that mark.
    pub(super) fn propose<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        mut m: MutReq,
        latch: LatchId,
        current: &[Option<(u8, Vec<u8>)>],
        now: Nanos,
    ) -> bool {
        let stopped = core.state_machine(m.g).is_some_and(StateMachine::halted);
        if self.refuse_moving(id, &m, latch, stopped) {
            return true;
        }
        let current: Vec<Option<(u8, &[u8])>> =
            current.iter().map(|c| c.as_ref().map(|(t, v)| (*t, v.as_slice()))).collect();
        // A mutation writes only keys of its range: an index entry of another range needs a
        // transaction across ranges (008). While a split runs the range ends at its cut: an
        // entry past it could go into the log after the cut, which the range could not apply
        // (found by the sustained load of 002 on 11-11-2026).
        let interval = self.ranges.get(&m.g).map(|r| {
            let cut = r.leader.as_ref().and_then(|l| l.split.gate());
            let end = match cut {
                Some(at) if at < r.end.as_slice() => at.to_vec(),
                _ => r.end.clone(),
            };
            (r.start.clone(), end)
        });
        let inside = |k: &[u8]| {
            interval.as_ref().is_some_and(|(s, e)| s.as_slice() <= k && k < e.as_slice())
        };
        // The numbers the range gave its documents so far (009 FR-023).
        let applied_next = core.state_machine(m.g).map_or(0, RangeState::next_local_id);
        let local_ids = self.local_ids;
        let l = self.ranges.get_mut(&m.g).and_then(|r| r.leader.as_mut()).expect("latched");
        // Evaluated on a pending write of its keys (FR-029): its answer waits for that write to
        // commit, so a write without change still goes through the log, behind it.
        let on_pending = m.keys.iter().any(|k| l.latches.pending_value(k).is_some());
        let floor = l.cache.floor_of(&m.keys);
        // Faulty variant (005 T017): the write ignores the reads of its keys and may go below one;
        // the core no longer raises the new leader's clock to the cache's minimum either
        // (`MultiRaftCore::raise_to_read_cache`), which covers the floor on its own (D-083).
        #[cfg(teseladb_bug = "write_below_read")]
        let floor = Hlc { physical: 0, logical: 0 }.min(floor);
        let mut floor = floor;
        let counted = self.counted_writes;
        let reaching = counted && self.reaching_records;
        let sized = counted && self.sized_counts;
        // Keys the mutation asked to read (`Mutation::reads`): they join its keys for the retry.
        let mut more = Vec::new();
        // A mutation without change, answered without an entry (005 FR-026).
        let mut no_change = None;
        let outcome = loop {
            let (mut change, mut result) = (Vec::new(), Vec::new());
            let Work::Mutation(mutation) = &m.work else { unreachable!("a mutation") };
            let (dedup, deadline) = (&m.dedup, m.deadline);
            // What the replica has applied is committed, so a write without change meets these
            // write concerns at once; a stored result of its statement was looked up before.
            let shortcut = m.ack.met_by_applied() && !on_pending;
            // What the write leaves under each key it writes, for the next writes of those keys
            // while it is pending (FR-029).
            let mut written: BTreeMap<Vec<u8>, crate::latch::Pending> = BTreeMap::new();
            // The new documents of the write get the range's next numbers (009 FR-023).
            let first_local = applied_next.max(l.next_local);
            let next_local = std::cell::Cell::new(first_local);
            let build = |ts| {
                let f = |c: &[Option<(u8, &[u8])>], t: Hlc| {
                    if deadline.is_some_and(|d| t.physical > d) {
                        return Err(Eval::Expired);
                    }
                    let mut o = mutation.eval_with(&m.keys, c, t)?;
                    match local_ids {
                        true => next_local.set(crate::local_ids::fill(&mut o.puts, first_local)),
                        false => crate::local_ids::settle(&mut o.puts),
                    }
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
                    if !unread.is_empty() {
                        return Err(Eval::Read(unread));
                    }
                    if shortcut && o.puts.is_empty() && o.deletes.is_empty() {
                        return Err(Eval::NoChange(o.result, t));
                    }
                    Ok(o)
                };
                let read = (m.keys.as_slice(), current.as_slice());
                let mut e = evaluate(&mut l.latches, &l.cache, latch, read, ts, f, *dedup)?;
                // With every node reading it, the write says how many documents of each
                // collection it adds or removes (007 FR-039).
                if let (true, Command::Write { puts, deletes, docs, .. }) =
                    (counted, &mut e.command)
                {
                    *docs =
                        Some(crate::doc_count::doc_delta(read.0, read.1, (puts, deletes), sized));
                }
                // With every node reading it, the record keeps the greatest key the write wrote
                // (007 FR-022).
                if let (true, Command::Write { dedup: Some(r), .. }) = (reaching, &mut e.command) {
                    r.reaching = true;
                }
                if let Command::Write { puts, deletes, .. } = &e.command {
                    written = deletes.iter().map(|k| (k.clone(), None)).collect();
                    written.extend(
                        puts.iter().map(|p| (p.key.clone(), Some((p.value_type, p.value.clone())))),
                    );
                }
                // Every replica raises the range's next number with the entry.
                if let (true, Command::Write { puts, .. }) =
                    (next_local.get() > first_local, &mut e.command)
                {
                    let value = next_local.get().to_be_bytes().to_vec();
                    puts.push(Put { key: crate::sm::next_local_key(m.g), value_type: 0, value });
                }
                (change, result) = (e.change, e.result);
                Ok::<_, Refused<Eval>>(e.command.encode())
            };
            match core.propose_at(m.g, floor, now, build) {
                Ok((op, ts)) => {
                    l.latches.set_mark(latch, ts);
                    l.next_local = l.next_local.max(next_local.get());
                    // In the leader's log: the next writes of its keys go on it (FR-029); the
                    // reads and the transactions still wait for it to be applied.
                    let records: Vec<Vec<u8>> = m
                        .dedup
                        .map(|k| dedup_key(m.g, k.lsid, k.txn_number, k.stmt_id))
                        .into_iter()
                        .collect();
                    // Its load, on every key it read or writes: a split by load does not cut
                    // through a document and its index entries (007 FR-006).
                    let keys = m.keys.iter().chain(written.keys());
                    if let (Some(first), Some(last)) = (keys.clone().min(), keys.max()) {
                        l.load.record_span(first, last, now);
                    }
                    l.latches.pipeline(latch, ts, &records, std::mem::take(&mut written));
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
                // Its latch was given back: the answer goes once the replica confirms it leads.
                Err(ProposeAt::Build(Refused::Failed(Eval::NoChange(result, ts)))) => {
                    no_change = Some((result, ts));
                    break Err(None);
                }
                Err(ProposeAt::Build(Refused::Failed(Eval::Expired))) => {
                    break Err(Some(RangeError::Timeout));
                }
                Err(ProposeAt::Op(e)) => {
                    // Faulty variant (005 T054): a refused proposal leaves its value pending, and
                    // the next write of its keys goes on a value that never was in the log.
                    #[cfg(teseladb_bug = "pending_on_refusal")]
                    {
                        let written = std::mem::take(&mut written);
                        l.latches.pipeline(latch, floor, &[], written);
                        break Err(Some(op_error(e)));
                    }
                    #[cfg(not(teseladb_bug = "pending_on_refusal"))]
                    {
                        l.latches.release(latch);
                        break Err(Some(op_error(e)));
                    }
                }
            }
        };
        if let Some((result, ts)) = no_change {
            // What the replica holds goes out only while it leads (005 FR-009), as a read.
            let answer = Reply::Mutated { ts, change: Vec::new(), result, entry: None };
            match core.read(m.g, ReadMode::Lease, now) {
                Ok(op) => {
                    m.state = MutState::Answer(Box::new(answer));
                    self.ops.insert(op, id);
                    self.reqs.insert(id, Req::Mut(m));
                }
                Err(oe) => self.reply(id, Err(op_error(oe))),
            }
            return true;
        }
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
                    term: m.term,
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
            // A failure evaluated on pending writes may stem from one that never commits: the
            // write waits for them to be applied and is evaluated again on what is committed
            // (FR-029).
            Err(Some(RangeError::Mutation(_))) if on_pending => {
                m.settled = true;
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
