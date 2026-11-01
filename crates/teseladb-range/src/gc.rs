//! GC threshold of a range (FR-013, D-020, research R6): every [`GC_EVERY_MS`] of cluster time the
//! leader proposes `SetGcThreshold` with «now − gc_ttl», [`GC_TTL_MS`] being 24 hours. Every
//! replica applies it (the state machine keeps the highest) and its host, in
//! [`RangeHost::follow_state`], hands the LSM the threshold of each range that went up, over the
//! range's local keys and data and nothing else (a clearing of another range keeps its own); the
//! compaction then drops the versions no read at or above the threshold sees.
//!
//! The leader's periodic proposals (the threshold here, the checksum in [`crate::checksum`])
//! follow a [`Round`]: due at a cluster time, proposed, and due again once the core answers.

use teseladb_hlc::Hlc;
use teseladb_lsm::GcTable;
use teseladb_multiraft::{MultiRaftCore, OpError, ProposeAt, StateMachine};
use teseladb_types::{GroupId, Nanos};

use crate::checksum::Tally;
use crate::command::Command;
use crate::host::{RangeHost, Req, RoundReq};
use crate::sm::RangeState;
use crate::snapshot::local_interval;

/// History kept by default (`gc_ttl`, D-020), in cluster time.
pub const GC_TTL_MS: u64 = 24 * 60 * 60_000;
/// How often the leader raises the threshold, in cluster time.
pub const GC_EVERY_MS: u64 = 60_000;

/// The threshold at cluster time `now_ms`: «now − gc_ttl», `None` within the clock's first day.
pub fn threshold(now_ms: u64) -> Option<Hlc> {
    threshold_with(now_ms, GC_TTL_MS)
}

/// The threshold at cluster time `now_ms` with `ttl_ms` of history (the cluster parameter
/// `teselaHistoryRetention`, 007 FR-019): `None` while the clock is below it.
pub fn threshold_with(now_ms: u64, ttl_ms: u64) -> Option<Hlc> {
    now_ms.checked_sub(ttl_ms).map(|physical| Hlc { physical, logical: 0 })
}

/// The intervals a range's threshold covers: its local keys and its data `[start, end)`.
pub fn intervals(g: GroupId, start: &[u8], end: &[u8]) -> [(Vec<u8>, Vec<u8>); 2] {
    [local_interval(g), (start.to_vec(), end.to_vec())]
}

/// The LSM's table from the thresholds of a core's ranges `(range, start, end, threshold)`: each
/// threshold covers the range's [`intervals`].
pub fn table(ranges: &[(GroupId, Vec<u8>, Vec<u8>, Hlc)]) -> GcTable {
    let entries = ranges
        .iter()
        .flat_map(|(g, start, end, t)| intervals(*g, start, end).map(|(s, e)| (s, e, *t)))
        .collect();
    GcTable::new(entries)
}

/// Where a leader's periodic proposal is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Round {
    /// Due at this cluster time (ms).
    Idle(u64),
    /// Proposed; due again once the core answers.
    Proposed,
}

impl Round {
    /// Whether it is due at cluster time `now_ms`.
    pub fn due(&self, now_ms: u64) -> bool {
        matches!(self, Round::Idle(at) if now_ms >= *at)
    }
}

/// A leader's periodic proposal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Periodic {
    /// `SetGcThreshold`.
    Gc,
    /// `ComputeChecksum`.
    Checksum,
}

impl Periodic {
    /// Its period in cluster time.
    pub fn every_ms(self) -> u64 {
        match self {
            Periodic::Gc => GC_EVERY_MS,
            Periodic::Checksum => crate::checksum::CHECKSUM_EVERY_MS,
        }
    }
}

impl RangeHost {
    /// Hands the LSM the threshold of each of this core's ranges that changed since the last time
    /// (a `SetGcThreshold` applied, a snapshot restored, the first poll), one range at a time so
    /// that the thresholds of the other keys stay; `true` if it handed any.
    pub(crate) fn hand_gc_thresholds<S: StateMachine + RangeState>(
        &mut self,
        core: &MultiRaftCore<S>,
    ) -> bool {
        let marks: Vec<(GroupId, Hlc)> = self
            .ranges
            .keys()
            .filter_map(|g| Some((*g, core.state_machine(*g)?.gc_threshold())))
            .collect();
        let handed = self.gc_handed.take().unwrap_or_default();
        let mut lsm = self.lsm.borrow_mut();
        let mut moved = false;
        for (g, t) in marks.iter().filter(|m| !handed.contains(m)) {
            let r = &self.ranges[g];
            lsm.set_range_gc_threshold(&intervals(*g, &r.start, &r.end), *t);
            moved = true;
        }
        drop(lsm);
        self.gc_handed = Some(marks);
        moved
    }

    /// Moves the round `kind` of range `g`, which this host leads: when it is due it proposes its
    /// command at a mark of its own (a checksum is taken at that mark and the leader opens its
    /// tally), and it is due again [`Periodic::every_ms`] after the core answers. Before the range
    /// applied the entries of earlier terms, with its clock out of margin, or with a full queue, it
    /// tries again on a later poll, having moved nothing.
    pub(crate) fn round_step<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        kind: Periodic,
        now: Nanos,
    ) -> bool {
        let now_ms = self.cluster_ms();
        let voters = core.conf(g).map(|c| c.voters.clone()).unwrap_or_default();
        let l = self.ranges.get_mut(&g).and_then(|r| r.leader.as_mut()).expect("a range it leads");
        if !l.caught_up(core, g) || !l.round(kind).due(now_ms) || core.read_mark(now).is_none() {
            return false;
        }
        let gc = threshold_with(now_ms, self.gc_ttl_ms);
        let build = |ts| {
            let command = match (kind, gc) {
                (Periodic::Gc, Some(t)) => Command::SetGcThreshold { ts: t },
                (Periodic::Gc, None) => return Err(()),
                (Periodic::Checksum, _) => Command::ComputeChecksum { at: ts },
            };
            Ok(command.encode())
        };
        let zero = Hlc { physical: 0, logical: 0 };
        match core.propose_at(g, zero, now, build) {
            Ok((op, _)) => {
                let index = core.status(g).map_or(0, |st| st.last_index);
                *l.round(kind) = Round::Proposed;
                if kind == Periodic::Checksum {
                    l.tally = Some(Tally::new(g, index, voters));
                }
                let term = l.term;
                let id = self.new_id();
                self.reqs.insert(id, Req::Round(RoundReq { g, term, kind }));
                self.ops.insert(op, id);
            }
            Err(ProposeAt::Op(OpError::Overloaded)) => return false,
            Err(_) => *l.round(kind) = Round::Idle(now_ms + kind.every_ms()),
        }
        true
    }

    /// The core's answer to a periodic proposal: the round is due again a period later.
    pub(crate) fn round_done(&mut self, r: RoundReq) {
        let now_ms = self.cluster_ms();
        let leader = self.ranges.get_mut(&r.g).and_then(|range| range.leader.as_mut());
        if let Some(l) = leader.filter(|l| l.term == r.term) {
            *l.round(r.kind) = Round::Idle(now_ms + r.kind.every_ms());
        }
    }
}
