//! The leader's replication state towards each follower (data-model §2): probing, pipelined
//! replication with a maximum of messages in flight, or snapshot sending.

use std::collections::VecDeque;

use teseladb_types::Nanos;

/// Replication mode towards a follower.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressState {
    /// Looking for the point where the logs match: one message at a time.
    Probe,
    /// Pipelined: sends without waiting for a response, up to the in-flight maximum.
    Replicate,
    /// A snapshot up to this index is being sent.
    Snapshot(u64),
}

/// Progress of a follower.
#[derive(Clone, Debug)]
pub struct Progress {
    /// Last index known to match.
    pub matched: u64,
    /// Next index to send.
    pub next: u64,
    /// Mode.
    pub state: ProgressState,
    /// While probing: a message has been sent without a response.
    pub probe_sent: bool,
    /// Last index of each `AppendReq` in flight.
    pub inflight: VecDeque<u64>,
    /// It has responded something since the last quorum check.
    pub recent_active: bool,
    /// It is a learner.
    pub learner: bool,
    /// Round trip of its latest answer to a heartbeat round still remembered.
    pub rtt: Option<Nanos>,
}

impl Progress {
    /// New progress of a follower whose log is assumed to reach `last`.
    pub fn new(last: u64, learner: bool) -> Progress {
        Progress {
            matched: 0,
            next: last + 1,
            state: ProgressState::Probe,
            probe_sent: false,
            inflight: VecDeque::new(),
            recent_active: false,
            learner,
            rtt: None,
        }
    }

    /// Tells whether nothing more can be sent to it now.
    pub fn paused(&self, max_inflight: usize) -> bool {
        match self.state {
            ProgressState::Probe => self.probe_sent,
            ProgressState::Replicate => self.inflight.len() >= max_inflight,
            ProgressState::Snapshot(_) => true,
        }
    }

    /// Switches to probing from `matched + 1`.
    pub fn become_probe(&mut self) {
        if let ProgressState::Snapshot(idx) = self.state {
            self.next = (self.matched + 1).max(idx + 1);
        } else {
            self.next = self.matched + 1;
        }
        self.state = ProgressState::Probe;
        self.probe_sent = false;
        self.inflight.clear();
    }

    /// Switches to pipelined replication.
    pub fn become_replicate(&mut self) {
        self.state = ProgressState::Replicate;
        self.next = self.matched + 1;
        self.probe_sent = false;
        self.inflight.clear();
    }

    /// Positive response up to `index`. Returns whether it advanced.
    pub fn maybe_update(&mut self, index: u64) -> bool {
        let advanced = index > self.matched;
        if advanced {
            self.matched = index;
            self.probe_sent = false;
        }
        self.next = self.next.max(index + 1);
        while self.inflight.front().is_some_and(|&i| i <= index) {
            self.inflight.pop_front();
        }
        advanced
    }

    /// Rejection of `rejected` with the follower's last index `hint`: steps back. Returns whether
    /// the rejection was the expected one (not an old one).
    pub fn maybe_decr(&mut self, rejected: u64, hint: u64) -> bool {
        if self.state == ProgressState::Replicate {
            if rejected <= self.matched {
                return false;
            }
            self.next = self.matched + 1;
            return true;
        }
        if self.next.checked_sub(1) != Some(rejected) {
            return false;
        }
        self.next = (rejected.min(hint + 1)).max(1);
        self.probe_sent = false;
        true
    }

    /// An `AppendReq` up to `last` was sent.
    pub fn sent(&mut self, last: u64) {
        match self.state {
            ProgressState::Replicate => {
                if last >= self.next {
                    self.next = last + 1;
                }
                self.inflight.push_back(last);
            }
            ProgressState::Probe => self.probe_sent = true,
            ProgressState::Snapshot(_) => {}
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn old_rejections_are_ignored() {
        // Replicating: a rejection at or below `matched` is from before the last acknowledgement.
        let mut p = Progress::new(10, false);
        p.maybe_update(8);
        p.become_replicate();
        assert!(!p.maybe_decr(8, 3));
        assert!(!p.maybe_decr(5, 3));
        assert_eq!(p.next, 9);
        assert!(p.maybe_decr(9, 8));
        // Probing: only a rejection of the probe just sent (`next - 1`) counts.
        p.become_probe();
        assert!(!p.maybe_decr(4, 3));
        assert!(p.maybe_decr(p.next - 1, 3));
        assert_eq!(p.next, 4);
    }
}
