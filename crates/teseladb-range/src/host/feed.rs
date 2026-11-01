//! What a change stream reads on the leader of a range (014 FR-003, FR-004): the versions of
//! `(from, to]` with `to` at or below the closed mark the range applied, at most
//! [`FEED_WINDOW_MS`] past `from`, and the oldest intent pending there ([`crate::changes`]). While
//! streams read the range its leader closes marks 200 ms behind its clock
//! ([`crate::closed_ts::STREAM_CLOSE_LAG_MS`]) for [`STREAM_HOLD`] after the last read.

use teseladb_hlc::Hlc;
use teseladb_multiraft::{MultiRaftCore, StateMachine};
use teseladb_types::{GroupId, Nanos, NodeId};

use super::{RangeError, RangeHost, Reply, Req, ReqId};
use crate::changes::change_rows;
use crate::closed_ts::follower_mark;
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
        let ChangesReq { g, start, end, from } = req;
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
        let window = Hlc { physical: from.physical.saturating_add(FEED_WINDOW_MS), logical: 0 };
        let to = match follower_mark(&known, applied, None) {
            Some(closed) if closed > from => closed.min(window.max(from)),
            _ => {
                self.reply(
                    id,
                    Ok(Reply::Changes { rows: Vec::new(), to: from, oldest_intent: None }),
                );
                return id;
            }
        };
        let f = FeedReq { g, start, end, from, to, hint: NodeId(0) };
        self.step_changes(id, f);
        id
    }

    /// Reads the versions of a stream's read; `true` once it is answered.
    pub(super) fn step_changes(&mut self, id: ReqId, f: FeedReq) -> bool {
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
