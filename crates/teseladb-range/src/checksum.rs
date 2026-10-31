//! Checksums of a range (FR-014, research R5): every [`CHECKSUM_EVERY_MS`] of cluster time, and
//! when [`RangeHost::check`] asks, the leader proposes `ComputeChecksum` at a mark of its own.
//! Applying it only records the request in the state machine (applying cannot wait for a block
//! from the disk); each replica's host then computes the SHA-256 of the range's versions at that
//! mark, its local keys and its data, a bounded number of versions per call of
//! [`RangeHost::follow_state`], and hands the [`Report`] to the embedder, which sends it to the range's leader. Entries after it carry
//! higher marks, so every replica sums the same versions whenever it gets to it.
//!
//! The leader keeps the [`Tally`] of its last checksum: once a sum is the majority's among the
//! voters, every replica whose sum differs, or that could not read its data, is marked for rebuild
//! ([`Rebuild`], [`RangeHost::take_rebuilds`]). The embedder rebuilds it: it replaces that replica,
//! which gets the range's data again from a healthy one as a snapshot (FR-012). The leader may be
//! the one marked.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

use teseladb_hash::Sha256;
use teseladb_hlc::Hlc;
use teseladb_lsm::{Lsm, Scan, Step};
use teseladb_multiraft::{MultiRaftCore, StateMachine};
use teseladb_types::{DecodeError, Decoder, Encoder, GroupId, NodeId};

use crate::gc::Round;
use crate::host::{RangeError, RangeHost};
use crate::sm::RangeState;
use crate::snapshot::local_interval;

/// How often the leader takes a checksum, in cluster time.
pub const CHECKSUM_EVERY_MS: u64 = 10 * 60_000;
/// Versions a replica sums in one call of [`RangeHost::follow_state`] at most, so the core goes on with its other work.
pub const SUM_STEP_VERSIONS: usize = 4096;

/// SHA-256 of a range's versions.
pub type Sum = [u8; 32];

/// The sum one replica computed for a checksum, on its way to the leader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Range.
    pub range: GroupId,
    /// Entry of the `ComputeChecksum`.
    pub index: u64,
    /// Mark of the checksum.
    pub at: Hlc,
    /// Replica that computed it.
    pub node: NodeId,
    /// The sum, or `None` if the replica could not read its data (a corrupt block).
    pub sum: Option<Sum>,
}

impl Report {
    /// Encodes it for a frame.
    pub fn encode(&self) -> Vec<u8> {
        let mut e = Encoder::new();
        e.u64(self.range.0).u64(self.index).u64(self.at.physical).u32(self.at.logical);
        e.u64(self.node.0).bool(self.sum.is_some());
        if let Some(s) = &self.sum {
            e.bytes(s);
        }
        e.finish()
    }

    /// Decodes a report written by [`Report::encode`].
    pub fn decode(b: &[u8]) -> Result<Report, DecodeError> {
        let mut d = Decoder::new(b);
        let range = GroupId(d.u64()?);
        let index = d.u64()?;
        let at = Hlc { physical: d.u64()?, logical: d.u32()? };
        let node = NodeId(d.u64()?);
        let sum = if d.bool()? {
            let offset = d.position();
            Some(Sum::try_from(d.bytes()?).map_err(|_| DecodeError::InvalidValue { offset })?)
        } else {
            None
        };
        d.finish()?;
        Ok(Report { range, index, at, node, sum })
    }
}

/// A replica the leader marked for rebuild: its sum for the checksum of entry `index` differs
/// from the majority's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rebuild {
    /// Range.
    pub range: GroupId,
    /// Replica.
    pub node: NodeId,
    /// Entry of the checksum that found it.
    pub index: u64,
}

/// The sums the leader received for one checksum.
#[derive(Debug)]
pub struct Tally {
    range: GroupId,
    index: u64,
    voters: Vec<NodeId>,
    sums: BTreeMap<NodeId, Option<Sum>>,
    marked: BTreeSet<NodeId>,
}

impl Tally {
    /// The tally of the checksum of entry `index` of `range`, whose majority counts `voters`.
    pub fn new(range: GroupId, index: u64, voters: Vec<NodeId>) -> Tally {
        Tally { range, index, voters, sums: BTreeMap::new(), marked: BTreeSet::new() }
    }

    /// Adds a report (one of another checksum is ignored) and returns the replicas it marks now.
    pub fn add(&mut self, r: &Report) -> Vec<Rebuild> {
        if r.range != self.range || r.index != self.index {
            return Vec::new();
        }
        self.sums.insert(r.node, r.sum);
        let Some(majority) = self.majority() else { return Vec::new() };
        let differ: Vec<NodeId> = self
            .sums
            .iter()
            .filter(|(n, s)| **s != Some(majority) && !self.marked.contains(n))
            .map(|(n, _)| *n)
            .collect();
        self.marked.extend(&differ);
        differ
            .into_iter()
            .map(|node| Rebuild { range: self.range, node, index: self.index })
            .collect()
    }

    /// The sum a majority of the voters reported, if any.
    fn majority(&self) -> Option<Sum> {
        let quorum = self.voters.len() / 2 + 1;
        let mut counts: BTreeMap<Sum, usize> = BTreeMap::new();
        for s in self.voters.iter().filter_map(|v| self.sums.get(v).copied().flatten()) {
            *counts.entry(s).or_default() += 1;
        }
        counts.into_iter().find(|(_, n)| *n >= quorum).map(|(s, _)| s)
    }
}

/// A replica's sum in progress: the range's intervals in order, at the checksum's mark.
struct SumScan {
    index: u64,
    at: Hlc,
    intervals: Vec<(Vec<u8>, Vec<u8>)>,
    cursor: Option<Scan>,
    hash: Sha256,
}

/// What a step of a sum gives: nothing yet, or the sum (`None`: the data could not be read).
enum SumStep {
    Pending { moved: bool },
    Done(Option<Sum>),
}

impl SumScan {
    fn new(g: GroupId, start: &[u8], end: &[u8], index: u64, at: Hlc) -> SumScan {
        // Taken from the back: the local keys first, then the data.
        let intervals = vec![(start.to_vec(), end.to_vec()), local_interval(g)];
        SumScan { index, at, intervals, cursor: None, hash: Sha256::new() }
    }

    /// Sums up to [`SUM_STEP_VERSIONS`] versions; stops early while a block is being read.
    fn step(&mut self, lsm: &RefCell<Lsm>) -> SumStep {
        for n in 0..SUM_STEP_VERSIONS {
            let mut cursor = match self.cursor.take() {
                Some(c) => c,
                None => match self.intervals.pop() {
                    Some((start, end)) => lsm.borrow().view().scan(&start, &end, self.at),
                    None => {
                        let hash = std::mem::replace(&mut self.hash, Sha256::new());
                        return SumStep::Done(Some(hash.finalize()));
                    }
                },
            };
            let step = cursor.next(&mut lsm.borrow_mut());
            match step {
                Step::Item(i) => {
                    self.hash.update(&(i.key.len() as u64).to_be_bytes());
                    self.hash.update(&i.key);
                    self.hash.update(&i.ts.physical.to_be_bytes());
                    self.hash.update(&i.ts.logical.to_be_bytes());
                    self.hash.update(&[i.kind as u8]);
                    self.hash.update(&(i.value.len() as u64).to_be_bytes());
                    self.hash.update(&i.value);
                    self.cursor = Some(cursor);
                }
                Step::Done => {}
                Step::Miss(_) => {
                    self.cursor = Some(cursor);
                    return SumStep::Pending { moved: n > 0 };
                }
                Step::Failed(_) => return SumStep::Done(None),
            }
        }
        SumStep::Pending { moved: true }
    }
}

/// A replica's checksums of one range: the last one it finished and the one in progress.
#[derive(Default)]
pub(crate) struct Computer {
    done: u64,
    scan: Option<SumScan>,
}

impl RangeHost {
    /// Asks the leader of `g` for a checksum now (FR-014): it is proposed on the next poll unless
    /// one is already proposed.
    pub fn check<S: StateMachine>(
        &mut self,
        core: &MultiRaftCore<S>,
        g: GroupId,
    ) -> Result<(), RangeError> {
        let range = self.ranges.get_mut(&g).ok_or(RangeError::OutOfRange)?;
        let Some(l) = range.leader.as_mut() else {
            let hint = core.status(g).map_or(NodeId(0), |s| s.leader);
            return Err(RangeError::NotLeader { hint });
        };
        if let Round::Idle(_) = l.check {
            l.check = Round::Idle(0);
        }
        Ok(())
    }

    /// The sums this replica finished since the last call: the embedder sends each to the leader
    /// of its range, which passes it to [`RangeHost::on_checksum_report`].
    pub fn take_checksum_reports(&mut self) -> Vec<Report> {
        std::mem::take(&mut self.reports)
    }

    /// A replica's sum reaching the leader; `true` if it belongs to this leader's last checksum.
    pub fn on_checksum_report(&mut self, r: &Report) -> bool {
        let leader = self.ranges.get_mut(&r.range).and_then(|range| range.leader.as_mut());
        let Some(tally) = leader.and_then(|l| l.tally.as_mut()).filter(|t| t.index == r.index)
        else {
            return false;
        };
        let marked = tally.add(r);
        self.rebuilds.extend(marked);
        true
    }

    /// The replicas this leader marked for rebuild since the last call.
    pub fn take_rebuilds(&mut self) -> Vec<Rebuild> {
        std::mem::take(&mut self.rebuilds)
    }

    /// Follows what the state machines of this core's ranges applied, after [`RangeHost::poll`]:
    /// hands their GC thresholds to the LSM when one changed (FR-013), takes the end a split cut
    /// (007) and moves this replica's checksums (FR-014); `true` if something moved.
    pub fn follow_state<S: StateMachine + RangeState>(&mut self, core: &MultiRaftCore<S>) -> bool {
        let mut moved = self.hand_gc_thresholds(core);
        moved |= self.follow_bounds(core);
        let groups: Vec<GroupId> = self.ranges.keys().copied().collect();
        for g in groups {
            moved |= self.sum_step(core, g);
        }
        moved
    }

    /// Moves this replica's sum of range `g`, one of its ranges: it starts on a checksum the state
    /// machine applied (a newer one replaces the one in progress); `true` if it moved.
    fn sum_step<S: StateMachine + RangeState>(
        &mut self,
        core: &MultiRaftCore<S>,
        g: GroupId,
    ) -> bool {
        let request = core.state_machine(g).and_then(|sm| sm.checksum_request());
        let node = core.status(g).map_or(NodeId(0), |st| st.id);
        let lsm = self.lsm.clone();
        let range = self.ranges.get_mut(&g).expect("a range of this host");
        let c = &mut range.sum;
        let mut moved = false;
        if let Some((index, at)) = request
            && index > c.done
            && c.scan.as_ref().is_none_or(|s| s.index != index)
        {
            c.scan = Some(SumScan::new(g, &range.start, &range.end, index, at));
            moved = true;
        }
        let Some(scan) = c.scan.as_mut() else { return moved };
        match scan.step(&lsm) {
            SumStep::Pending { moved: m } => moved || m,
            SumStep::Done(sum) => {
                let (index, at) = (scan.index, scan.at);
                c.done = index;
                c.scan = None;
                self.reports.push(Report { range: g, index, at, node, sum });
                true
            }
        }
    }
}
