//! A write outside a transaction that finds a transaction's intent (008 FR-002, T032): as in
//! RefDb 9.0, it waits for the owner to end instead of failing (measured: an update of a
//! document an open transaction holds waits, and with `maxTimeMS` fails with `MaxTimeMSExpired`).
//! Each round pushes the owner as abandoned: a finished owner (aborted by the push, or committed)
//! has the intent resolved at once and the write goes again; a live one makes it wait a pause.
//!
//! A read that finds one does not wait (008 FR-003, T013): it pushes the owner above its mark,
//! learns its state, resolves the intent of a finished one, and reads again at the same mark
//! with what it learned. The node's readers share their pushes (`PushCache`, SC-005): one push
//! goes for all of them, and those that meet the owner while it is on the way wait for it.

use std::sync::PoisonError;
use std::thread;
use std::time::Instant;

use teseladb_hlc::Hlc;
use teseladb_range::host::TxnOp;
use teseladb_range::intents::{IntentOwner, Push};
use teseladb_range::push::{Known, TxnView};
use teseladb_range::read::ReadConcern;
use teseladb_txn::push_cache::Lookup;
use teseladb_txn::read::{lazy_resolve, learn};
use teseladb_wire::commands::{lock, txn_stats};

use super::{RETRY_PAUSE, ServerBackend};
use crate::data_wire::{RemoteReply, RemoteReq};

/// `req` as it goes again after it met intents: a read at mark `at` with what it learned of their
/// transactions (`pinned`); any other request as it is.
pub(super) fn pin(req: RemoteReq, pinned: Option<&(Hlc, TxnView)>) -> RemoteReq {
    let Some((at, learned)) = pinned else { return req };
    let at = Some(*at);
    let with = |own: &TxnView| {
        let mut view = own.clone();
        view.known.extend(learned.known.iter().map(|(t, k)| (*t, *k)));
        view
    };
    match req {
        RemoteReq::Scan { start, end, limit, concern, backward } => {
            let view = with(&TxnView::default());
            let uncertain_to = None;
            RemoteReq::ReadWith {
                key: start,
                scan: Some((end, limit)),
                backward,
                at,
                concern,
                view,
                uncertain_to,
            }
        }
        // On the leader at the mark the follower read at: the intents it met are resolved there.
        RemoteReq::FollowerScan { start, end, limit, backward, .. } => {
            let (concern, view) = (ReadConcern::Majority, with(&TxnView::default()));
            let uncertain_to = None;
            RemoteReq::ReadWith {
                key: start,
                scan: Some((end, limit)),
                backward,
                at,
                concern,
                view,
                uncertain_to,
            }
        }
        RemoteReq::Read { key, .. } => {
            let (concern, view) = (ReadConcern::Majority, with(&TxnView::default()));
            let backward = false;
            RemoteReq::ReadWith { key, scan: None, backward, at, concern, view, uncertain_to: None }
        }
        // Again at its mark, with its window.
        RemoteReq::ReadWith { key, scan, backward, concern, view, uncertain_to, .. } => {
            let view = with(&view);
            RemoteReq::ReadWith { key, scan, backward, at, concern, view, uncertain_to }
        }
        other @ (RemoteReq::Write { .. }
        | RemoteReq::Txn { .. }
        | RemoteReq::BackupFetch { .. }
        | RemoteReq::RestorePiece { .. }
        | RemoteReq::RestoreIngest { .. }
        | RemoteReq::Changes { .. }
        | RemoteReq::Fence { .. }
        | RemoteReq::Live
        | RemoteReq::DocCount { .. }) => other,
    }
}

impl ServerBackend {
    /// One round of waiting for the owner of an intent a write ran into: the intent is resolved on
    /// the key the range found it on, which is the written document's or an entry the write read.
    pub(super) fn wait_for_intent(&mut self, owner: &IntentOwner) {
        let key = owner.key.as_slice();
        let push =
            TxnOp::PushTxn { txn: owner.txn, push: Push::AbortIfAbandoned { intent_ts: owner.ts } };
        let record = self.h.routes.lookup(&owner.record_key).map(|e| e.group);
        txn_stats::pushed(&mut lock(&self.h.sessions));
        let answer = record.map(|g| self.ask_range(g, push));
        // A pending owner has nothing to resolve: the write waits a pause.
        if let Some(Ok(RemoteReply::Record(rec))) = answer
            && let Some(op) = lazy_resolve(&rec, vec![key.to_vec()])
            && let Some(g) = self.h.routes.lookup(key).map(|e| e.group)
            && self.ask_range(g, op).is_ok()
        {
            return;
        }
        thread::sleep(RETRY_PAUSE);
    }

    /// Pushes the owner of the intent a read at `at` met above that mark: what the read knows of
    /// it then, after resolving the intent of a finished one; `None` if the push failed or the
    /// operation's deadline `until` passed while it waited for another reader's push.
    pub(super) fn push_for_read(
        &mut self,
        owner: &IntentOwner,
        at: Hlc,
        until: Instant,
    ) -> Option<Known> {
        // The node's other readers wait for this push; it reads the record at the reader's own
        // mark (008 T043), which the leader's clock has reached or soon reaches.
        if let Ok(known) = self.push_turn(owner, at, until)? {
            return Some(known);
        }
        let push = teseladb_txn::read::push_for(owner, at);
        let record = self.h.routes.lookup(&owner.record_key).map(|e| e.group);
        let rec = match record.map(|g| self.ask_range(g, push)) {
            Some(Ok(RemoteReply::Record(rec))) => Some(rec),
            Some(Ok(_) | Err(_)) | None => None,
        };
        let cv = {
            let mut g = lock(&self.h.sessions);
            txn_stats::pushed(&mut g);
            g.pushes.settle(owner.txn, rec.as_ref());
            g.push_settled.clone()
        };
        cv.notify_all();
        let rec = rec?;
        // A finished owner's intent goes, without waiting for it: the read goes again knowing the
        // outcome, and the resolution is a write that would hold it for a sync of the disk. A
        // failure leaves it to the next reader or to the resolvers.
        if let Some(op) = lazy_resolve(&rec, vec![owner.key.clone()])
            && let Some(e) = self.h.routes.lookup(&owner.key)
        {
            self.tell_range(e.group, op);
        }
        Some(learn(&rec))
    }

    /// What a read at `at` that met `owner`'s intent knows from the node's pushes (`Ok`), or the
    /// mark it pushes the owner to (`Err`), after waiting for a push of another reader on the
    /// way; `None` once the operation's deadline `until` passed while it waited.
    fn push_turn(
        &mut self,
        owner: &IntentOwner,
        at: Hlc,
        until: Instant,
    ) -> Option<Result<Known, Hlc>> {
        let sessions = self.h.sessions.clone();
        let cv = lock(&sessions).push_settled.clone();
        let mut g = lock(&sessions);
        loop {
            match g.pushes.lookup(owner.txn, at) {
                Lookup::Known(known) => return Some(Ok(known)),
                Lookup::Push(to) => return Some(Err(to)),
                Lookup::Wait => {
                    let left = until.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    let waited = cv.wait_timeout(g, left.min(RETRY_PAUSE));
                    g = waited.unwrap_or_else(PoisonError::into_inner).0;
                }
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::data_wire::Ahead;
    use teseladb_range::ack::{W, WriteConcern};
    use teseladb_range::intents::TxnId;

    fn ts(p: u64) -> Hlc {
        Hlc { physical: p, logical: 0 }
    }

    #[test]
    fn a_read_that_met_intents_goes_again_at_its_mark_with_what_it_learned() {
        let learned = (ts(7), TxnView { own: None, known: [(TxnId(2), Known::Pushed)].into() });
        let scan = RemoteReq::Scan {
            start: vec![1],
            end: vec![2],
            limit: 5,
            concern: ReadConcern::Local,
            backward: false,
        };
        // Nothing met yet: it goes as it is.
        assert_eq!(pin(scan.clone(), None), scan);
        assert_eq!(
            pin(scan, Some(&learned)),
            RemoteReq::ReadWith {
                key: vec![1],
                scan: Some((vec![2], 5)),
                backward: false,
                at: Some(ts(7)),
                concern: ReadConcern::Local,
                view: learned.1.clone(),
                uncertain_to: None,
            }
        );
        // A follower's scan goes to the leader, at the mark the follower read at.
        let follower = RemoteReq::FollowerScan {
            start: vec![1],
            end: vec![2],
            limit: 5,
            max_stale_ms: Some(90_000),
            nodes: vec![(teseladb_types::NodeId(2), true)],
            backward: false,
        };
        assert_eq!(
            pin(follower, Some(&learned)),
            RemoteReq::ReadWith {
                key: vec![1],
                scan: Some((vec![2], 5)),
                backward: false,
                at: Some(ts(7)),
                concern: ReadConcern::Majority,
                view: learned.1.clone(),
                uncertain_to: None,
            }
        );
        assert_eq!(
            pin(RemoteReq::Read { key: vec![1], at: None }, Some(&learned)),
            RemoteReq::ReadWith {
                key: vec![1],
                scan: None,
                backward: false,
                at: Some(ts(7)),
                concern: ReadConcern::Majority,
                view: learned.1.clone(),
                uncertain_to: None,
            }
        );
        // A read of a transaction keeps its own and adds what it learned.
        let own = TxnView { own: Some(TxnId(1)), known: [(TxnId(3), Known::Aborted)].into() };
        let read = RemoteReq::ReadWith {
            key: vec![1],
            scan: None,
            backward: false,
            at: Some(ts(5)),
            concern: ReadConcern::Majority,
            view: own,
            uncertain_to: Some(ts(9)),
        };
        let RemoteReq::ReadWith { at, view, uncertain_to, .. } = pin(read, Some(&learned)) else {
            panic!("not a read")
        };
        assert_eq!((at, view.own, view.known.len()), (Some(ts(7)), Some(TxnId(1)), 2));
        // Its window goes with it.
        assert_eq!(uncertain_to, Some(ts(9)));
        // Writes go as they are.
        let txn = RemoteReq::Txn {
            op: TxnOp::PushTxn { txn: TxnId(4), push: Push::To(ts(1)) },
            deadline: None,
        };
        assert_eq!(pin(txn.clone(), Some(&learned)), txn);
        let write = RemoteReq::Write {
            keys: vec![],
            mutation: vec![],
            dedup: None,
            wc: WriteConcern { w: W::Majority, j: true },
            deadline: None,
            prepared: Ahead::default(),
        };
        assert_eq!(pin(write.clone(), Some(&learned)), write);
    }
}
