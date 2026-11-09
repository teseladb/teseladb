//! The leader's closed marks and the reads of followers (007 FR-012, [`crate::closed_ts`]).

use teseladb_multiraft::{MultiRaftCore, StateMachine};
use teseladb_types::{GroupId, Nanos, NodeId};

use super::{GetReq, GetState, RangeError, RangeHost, ReadSpec, Req, ReqId};
use crate::closed_ts::{
    CLOSE_EVERY, CLOSED_LAG_MS, LOG_EVERY, STREAM_CLOSE_EVERY, STREAM_CLOSE_LAG_MS, closing,
    follower_mark,
};
use crate::command::Command;
use crate::push::ScanState;
use crate::sm::RangeState;

impl RangeHost {
    /// A read on this replica without its leader (007 FR-012): at the client's mark, or at the
    /// highest closed mark it can read at; `NotLeader` with the leader it knows when no closed
    /// mark covers it, or when the mark is more than `max_stale_ms` behind its wall clock
    /// (`maxStalenessSeconds`, FR-013). The replica applied every write at or below that mark: it reads the LSM at
    /// once, without latches nor read cache.
    pub fn read_follower<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        spec: ReadSpec,
        max_stale_ms: Option<u64>,
        now: Nanos,
        wall_ms: u64,
    ) -> ReqId {
        self.wall_ms = wall_ms;
        self.now = now;
        // A follower's mark is the client's or one its leader closed: no uncertainty window.
        let ReadSpec { g, key, scan, backward, at, concern, view, uncertain_to: _ } = spec;
        let id = self.new_id();
        let ends_inside = scan.as_ref().is_none_or(|(end, _)| {
            self.ranges.get(&g).is_some_and(|r| *end <= r.end) && key < *end
        });
        if !ends_inside || !self.covers(g, &key) {
            self.reply(id, Err(RangeError::OutOfRange));
            return id;
        }
        let sm = core.state_machine(g);
        let applied = sm.map_or(0, StateMachine::applied_index);
        let known = [core.closed(g), sm.and_then(RangeState::closed_mark)];
        // Faulty variant (007 T025): a follower reads at the client's mark or at its own clock,
        // whatever its leader closed, and misses the writes it has not applied yet.
        #[cfg(teseladb_bug = "follower_read_above_closed")]
        let known = [known[0].max(core.read_mark(now).map(|m| (m, 0))), known[1]];
        let fresh = |ts: &teseladb_hlc::Hlc| {
            max_stale_ms.is_none_or(|max| wall_ms.saturating_sub(ts.physical) <= max)
        };
        let Some(ts) = follower_mark(&known, applied, at).filter(fresh) else {
            let hint = core.status(g).map_or(NodeId(0), |s| s.leader);
            self.reply(id, Err(RangeError::NotLeader { hint }));
            return id;
        };
        // Below the range's GC threshold the compaction may have dropped what it needs (FR-013).
        let handed = self.gc_handed.iter().flatten().find(|(r, _)| *r == g).map(|(_, t)| *t);
        let gc_floor = sm.map_or(super::ZERO, RangeState::gc_threshold);
        if ts < handed.map_or(gc_floor, |t| t.max(gc_floor)) {
            self.reply(id, Err(RangeError::SnapshotTooOld));
            return id;
        }
        let state = match &scan {
            Some((end, _)) => GetState::Scanning(Box::new(ScanState::new(
                &self.lsm,
                (&key, end),
                (ts, None),
                &view,
                backward,
            ))),
            None => GetState::Read(ts),
        };
        let req = GetReq {
            g,
            key,
            scan,
            backward,
            at: Some(ts),
            gc_floor,
            concern,
            state,
            waited: false,
            view,
            uncertain_to: None,
        };
        self.reqs.insert(id, Req::Get(req));
        self.step(core, id, now);
        id
    }

    /// Closes a mark of `g`, which this replica leads, every `CLOSE_EVERY`, and writes it in the
    /// log every `LOG_EVERY`; `true` if it proposed.
    pub(super) fn close_step<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        g: GroupId,
        now: Nanos,
    ) -> bool {
        // `poll` calls it for the ranges this replica leads.
        let l = self.ranges.get_mut(&g).and_then(|r| r.leader.as_mut()).expect("a range it leads");
        if now < l.close_at || !l.caught_up(core, g) {
            return false;
        }
        // The clock the core's clock rule checked (005 FR-006).
        // With change streams reading the range, 200 ms behind and more often (014 FR-004,
        // SC-002); a mark never goes below one closed before with that lag.
        let streams = now < l.streams_until;
        l.close_at = now + if streams { STREAM_CLOSE_EVERY } else { CLOSE_EVERY };
        let lag = if streams { STREAM_CLOSE_LAG_MS } else { CLOSED_LAG_MS };
        let Some(c) = core.read_mark(now).and_then(|m| closing(m, lag)) else { return false };
        let c = c.max(l.last_closed.0);
        // The heartbeats carry it only within the lease (the core checks it): a deposed leader
        // could reach a follower with it. So do this replica's own reads (a change stream's,
        // 014 FR-003): out of its lease another leader may write below the mark. The log does not
        // need the lease: the entry commits only while this term holds a quorum, and comes after
        // every write at or below the mark. It also moves the mark of a quiet range, whose leader
        // sends no heartbeats, and of a leader without lease reads for the rest of its term (a
        // leadership transfer that did not finish), which writes it at every close instead.
        let leased = core.set_closed(g, c);
        if leased && c > l.last_closed.0 {
            l.last_closed = (c, core.status(g).map_or(0, |s| s.last_index));
        }
        if leased && now < l.log_close_at {
            return false;
        }
        l.log_close_at = now + LOG_EVERY;
        core.propose(g, &Command::SetClosedTs { closed: c }.encode(), now).is_ok()
    }
}
