//! What a change stream reads on the leader of a range (014 FR-003, FR-004): the versions of
//! `(from, to]` with `to` at or below the closed mark the range applied, at most
//! [`FEED_WINDOW_MS`] past `from` (unless `from` is zero), and the oldest intent pending there
//! ([`crate::changes`]). While streams read the range its leader closes marks 200 ms behind its
//! clock ([`crate::closed_ts::STREAM_CLOSE_LAG_MS`]) for [`STREAM_HOLD`] after the last read.
//!
//! A read that has to reach a mark (a search's, 011 FR-010) names it: when the closed marks are
//! below it, the leader serves the versions up to it as a read at that mark (FR-004, FR-005 of
//! 005): the core confirms the lease, the read waits for the writes in flight at or below the
//! mark and records the interval in the read cache, so no write lands at or below the mark
//! afterwards. A search then waits for its own mark instead of the closed marks, which trail the
//! clock by 200 ms or more.

use teseladb_hlc::Hlc;
use teseladb_multiraft::{MultiRaftCore, StateMachine};
use teseladb_types::{GroupId, Nanos, NodeId};

use super::{FUTURE_READ_MS, RangeError, RangeHost, Reply, Req, ReqId};
use crate::changes::change_rows;
use crate::closed_ts::follower_mark;
use crate::latch::ReadWait;
use crate::read::{ReadConcern, admit_read};
use crate::sm::RangeState;

/// How long after a stream's last read its range keeps the short lag of closed marks.
pub const STREAM_HOLD: Nanos = Nanos::from_secs(10);
/// The most a read of a stream goes past its start, so that its answer stays small.
pub const FEED_WINDOW_MS: u64 = 1_000;

/// A change stream's read of `[start, end)` of range `g` after mark `from`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangesReq {
    /// The range.
    pub g: GroupId,
    /// First key.
    pub start: Vec<u8>,
    /// End (excluded).
    pub end: Vec<u8>,
    /// The stream read the range up to this mark.
    pub from: Hlc,
    /// The mark the reader waits for, if any: past the closed marks, the read goes up to it
    /// (within the window) as a read at that mark.
    pub at: Option<Hlc>,
}

/// Where a read in progress is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FeedState {
    /// A read at a mark waits for the core to confirm the leadership.
    Core,
    /// It waits for the writes in flight at or below its mark and, for a mark above this
    /// leader's own, for the wall clock to reach it.
    Admit {
        /// Whether the mark is at or below the leader's own read mark.
        own: bool,
    },
    /// A mark above this leader's own and ahead of its wall clock: it waits for the clock to
    /// reach the mark, and the core confirms the leadership again then.
    Clock,
    /// It reads the versions.
    Read,
}

/// A read in progress: its interval and `(from, to]`.
pub(crate) struct FeedReq {
    pub(crate) g: GroupId,
    start: Vec<u8>,
    end: Vec<u8>,
    from: Hlc,
    to: Hlc,
    /// The leader to name if the range stops being led here.
    pub(crate) hint: NodeId,
    pub(crate) state: FeedState,
}

impl RangeHost {
    /// A change stream's read on this replica, which has to lead the range.
    pub fn changes<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        req: ChangesReq,
        now: Nanos,
    ) -> ReqId {
        self.now = now;
        let id = self.new_id();
        let ChangesReq { g, start, end, from, at } = req;
        let Some(range) = self.ranges.get_mut(&g) else {
            self.reply(id, Err(RangeError::OutOfRange));
            return id;
        };
        // Faulty variant (014 T020): a range a split shrank answers the part of the interval it
        // still holds, and the cursor takes the rest as read.
        #[cfg(teseladb_bug = "lose_events_on_split")]
        let end = match range.end.is_empty() || end <= range.end {
            true => end,
            false => range.end.clone(),
        };
        if start < range.start || (!range.end.is_empty() && end > range.end) || start >= end {
            self.reply(id, Err(RangeError::OutOfRange));
            return id;
        }
        let Some(leader) = range.leader.as_mut() else {
            let hint = core.status(g).map_or(NodeId(0), |s| s.leader);
            self.reply(id, Err(RangeError::NotLeader { hint }));
            return id;
        };
        leader.streams_until = now + STREAM_HOLD;
        let sm = core.state_machine(g);
        let applied = sm.map_or(0, StateMachine::applied_index);
        let own = Some(leader.last_closed).filter(|(c, _)| *c > super::ZERO);
        let known = [core.closed(g), sm.and_then(RangeState::closed_mark), own];
        // A read from the start of time (a search index's first build, 011 FR-014) reads the
        // range's whole history at once: a window would take it one second at a time from the
        // epoch.
        let window = match from == super::ZERO {
            true => Hlc { physical: u64::MAX, logical: u32::MAX },
            false => Hlc { physical: from.physical.saturating_add(FEED_WINDOW_MS), logical: 0 },
        };
        let reach = window.max(from);
        let closed =
            follower_mark(&known, applied, None).filter(|c| *c > from).map(|c| c.min(reach));
        let wanted = at.map(|t| t.min(reach)).filter(|t| *t > from);
        let f = |to, state| FeedReq { g, start, end, from, to, hint: NodeId(0), state };
        match (closed, wanted) {
            (Some(c), w) if w.is_none_or(|w| c >= w) => {
                self.step_changes(core, id, f(c, FeedState::Read));
            }
            (_, Some(w)) => match core.read(g, ReadConcern::Majority.mode(), now) {
                Ok(op) => {
                    self.ops.insert(op, id);
                    self.reqs.insert(id, Req::Changes(f(w, FeedState::Core)));
                }
                Err(e) => self.reply(id, Err(super::op_error(e))),
            },
            _ => self
                .reply(id, Ok(Reply::Changes { rows: Vec::new(), to: from, oldest_intent: None })),
        }
        id
    }

    /// The core confirmed the leadership for read `id` at a mark (or failed to).
    pub(super) fn changes_confirmed<S: StateMachine + RangeState>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        mut f: FeedReq,
        confirmed: bool,
    ) {
        if !confirmed {
            self.reply(id, Err(RangeError::NotLeader { hint: f.hint }));
            return;
        }
        let own = core.read_mark(self.now).is_some_and(|m| f.to <= m);
        f.state = FeedState::Admit { own };
        self.reqs.insert(id, Req::Changes(f));
        self.step(core, id, Nanos::ZERO);
    }

    /// A read waiting for this node's wall clock to reach its mark: once it does, the core
    /// confirms the leadership for it (a lease confirmed before may have ended meanwhile, and a
    /// leader elected since may have written below the mark); `true` if it moved.
    fn changes_clock<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        f: FeedReq,
    ) -> bool {
        if f.to.physical > self.wall_ms + FUTURE_READ_MS {
            self.reply(id, Err(RangeError::Timeout));
            return true;
        }
        if f.to.physical > self.wall_ms {
            self.reqs.insert(id, Req::Changes(f));
            return false;
        }
        match core.read(f.g, ReadConcern::Majority.mode(), self.now) {
            Ok(op) => {
                self.ops.insert(op, id);
                self.reqs.insert(id, Req::Changes(FeedReq { state: FeedState::Core, ..f }));
            }
            Err(e) => self.reply(id, Err(super::op_error(e))),
        }
        true
    }

    /// Reads the versions of a stream's read; `true` once it is answered.
    pub(super) fn step_changes<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        id: ReqId,
        mut f: FeedReq,
    ) -> bool {
        match f.state {
            FeedState::Core => {
                self.reqs.insert(id, Req::Changes(f));
                return false;
            }
            FeedState::Clock => return self.changes_clock(core, id, f),
            FeedState::Admit { own } => {
                // A mark at or below the leader's own is read as a read at its own mark would be;
                // one above, as a read at a mark the client gave (`step_get`): once this node's
                // wall clock reaches it, with the leadership confirmed again then, and the clock
                // takes it so that this node's later writes go above.
                if !own && f.to.physical > self.wall_ms {
                    f.state = FeedState::Clock;
                    return self.changes_clock(core, id, f);
                }
                self.clock.0.update(f.to);
                let Some(l) = self.ranges.get_mut(&f.g).and_then(|r| r.leader.as_mut()) else {
                    self.reply(id, Err(RangeError::NotLeader { hint: f.hint }));
                    return true;
                };
                let concern = ReadConcern::Majority;
                if admit_read(&l.latches, &mut l.cache, &f.start, &f.end, f.to, concern)
                    != ReadWait::Free
                {
                    self.reqs.insert(id, Req::Changes(f));
                    return false;
                }
                f.state = FeedState::Read;
            }
            FeedState::Read => {}
        }
        match change_rows(&self.lsm, (&f.start, &f.end), (f.from, f.to)) {
            None => {
                self.reqs.insert(id, Req::Changes(f));
                false
            }
            Some(Ok(c)) => {
                let reply =
                    Reply::Changes { rows: c.rows, to: f.to, oldest_intent: c.oldest_intent };
                self.reply(id, Ok(reply));
                true
            }
            Some(Err(_)) => {
                self.reply(id, Err(RangeError::Unavailable));
                true
            }
        }
    }
}
