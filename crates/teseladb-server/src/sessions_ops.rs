//! `killSessions` and `killAllSessions` on every node (006 FR-016): core 0 of the node that runs
//! the command forwards the sessions to each other node, whose core 0 kills them in its node's
//! cursors and sessions and answers; the command returns once every node answered or the
//! forward of one ended without an answer.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::mpsc::Sender;

use teseladb_multiraft::OpId;

/// What the handle learns when a kill ended: whether every node answered.
pub type KillSender = Sender<bool>;

/// The kills in flight on core 0.
#[derive(Default)]
pub struct SessionOps {
    next: u64,
    /// The kill of each forward still without an answer.
    ops: BTreeMap<OpId, u64>,
    /// Kills with a forward that ended without an answer.
    failed: BTreeSet<u64>,
    replies: BTreeMap<u64, KillSender>,
}

impl SessionOps {
    /// A kill forwarded with operations `ops`; with none (a node alone) it ends now.
    pub fn start(&mut self, ops: Vec<OpId>, reply: KillSender) {
        if ops.is_empty() {
            let _ = reply.send(true);
            return;
        }
        let id = self.next;
        self.next += 1;
        for op in ops {
            self.ops.insert(op, id);
        }
        self.replies.insert(id, reply);
    }

    /// Whether operation `op` is one of this module's.
    pub fn owns(&self, op: OpId) -> bool {
        self.ops.contains_key(&op)
    }

    /// Forward `op` ended, answered or not; the last one of its kill answers the handle.
    pub fn on_done(&mut self, op: OpId, answered: bool) {
        let Some(id) = self.ops.remove(&op) else { return };
        if !answered {
            self.failed.insert(id);
        }
        if self.ops.values().any(|&k| k == id) {
            return;
        }
        let all = !self.failed.remove(&id);
        // A handle that gave up no longer listens: its send fails, and that is fine.
        let _ = self.replies.remove(&id).map(|r| r.send(all));
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn a_kill_ends_when_every_forward_ended() {
        let mut s = SessionOps::default();
        let (tx, rx) = mpsc::channel();
        s.start(Vec::new(), tx);
        assert_eq!(rx.try_recv(), Ok(true));
        let (tx, rx) = mpsc::channel();
        s.start(vec![OpId(1), OpId(2)], tx);
        assert!(s.owns(OpId(1)) && !s.owns(OpId(3)));
        s.on_done(OpId(3), true);
        s.on_done(OpId(1), true);
        assert!(rx.try_recv().is_err());
        s.on_done(OpId(2), false);
        assert_eq!(rx.try_recv(), Ok(false));
        assert!(!s.owns(OpId(2)));
    }
}
