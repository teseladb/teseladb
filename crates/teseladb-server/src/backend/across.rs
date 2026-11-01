//! Reads and transactions across ranges for the writes whose index entries are in another range
//! (009 FR-003, T007): a read of each key at one mark with its value type, and an internal
//! transaction of 008 that this client worker drives with [`Coordinator`] until its intents are
//! resolved.

use teseladb_hlc::Hlc;
use teseladb_range::host::{RangeError, Reply, TxnOp};
use teseladb_range::intents::{IntentOp, TxnId};
use teseladb_txn::coordinator::{Coordinator, Event, Outcome, Route, TxnError};
use teseladb_types::{GroupId, Nanos};
use teseladb_wire::commands::Backend;
use teseladb_wire::error::{
    CommandError, INTERNAL_ERROR, RangeContext, WRITE_CONFLICT, from_range,
};

use super::{RETRY_PAUSE, ServerBackend, process_start};
use crate::data::{DataReply, DataReq};
use crate::data_wire::{RemoteReply, RemoteReq};
use crate::logic::NodeMsg;
use crate::routes::SharedRoutes;
use std::thread;
use std::time::Instant;

/// The node's routing table as the coordinator asks it.
pub(super) struct Routes<'a>(pub(super) &'a SharedRoutes);

impl Route for Routes<'_> {
    fn range_of(&self, key: &[u8]) -> Option<GroupId> {
        self.0.lookup(key).map(|e| e.group)
    }
}

/// The time the coordinator counts its heartbeats with.
fn now() -> Nanos {
    Nanos(u64::try_from(process_start().elapsed().as_nanos()).unwrap_or(u64::MAX))
}

/// A leader's answer as the coordinator reads it.
pub(super) fn reply_of(r: RemoteReply) -> Result<Reply, RangeError> {
    match r {
        RemoteReply::Written { ts, result } => {
            Ok(Reply::Mutated { ts, change: Vec::new(), result })
        }
        RemoteReply::Value { ts, value } => Ok(Reply::Value { ts, value }),
        RemoteReply::Intents { ts } => Ok(Reply::Intents { ts }),
        RemoteReply::Record(record) => Ok(Reply::Record(record)),
        // What `FindIntents` found (the reaper, 008 record GC).
        RemoteReply::Rows(rows) => {
            Ok(Reply::Batch(rows.into_iter().map(|(k, v)| (k, (0, v))).collect()))
        }
        // A backup's or a restore's answer is never one of a transaction's.
        RemoteReply::Piece { .. } | RemoteReply::Restored { .. } => Err(RangeError::Unavailable),
    }
}

/// A conflict: after a pause, so that the transaction it met can end before the statement
/// starts again.
fn conflict() -> CommandError {
    thread::sleep(RETRY_PAUSE);
    CommandError::new(WRITE_CONFLICT, "the write conflicted with another one across ranges")
}

/// The answer to the client for how the transaction ended.
fn ended(o: Outcome) -> Result<Hlc, CommandError> {
    let ctx = RangeContext { retryable_write: false, max_time_expired: false };
    match o {
        Outcome::Committed(ts) => Ok(ts),
        // Aborted by a push or refused by a newer version: the statement starts again.
        Outcome::Aborted
        | Outcome::Failed(TxnError::WriteConflict | TxnError::NoSuchTransaction) => Err(conflict()),
        Outcome::Failed(TxnError::TooLarge) => Err(CommandError::new(
            teseladb_wire::error::TRANSACTION_TOO_LARGE,
            "the write's transaction passed the coordinator's memory budget",
        )),
        Outcome::Failed(TxnError::NoRange) => {
            Err(CommandError::new(INTERNAL_ERROR, "no range is known for a key of the write"))
        }
        Outcome::Failed(TxnError::Range(e)) | Outcome::Unknown(e) => Err(from_range(&e, ctx)),
    }
}

impl ServerBackend {
    /// Asks range `g` for `op`, again while it has no leader or is overloaded, until the
    /// deadline, which the request carries (009 FR-006).
    pub(super) fn ask_range(&mut self, g: GroupId, op: TxnOp) -> Result<RemoteReply, RangeError> {
        let until = self.until();
        let req = RemoteReq::Txn { op, deadline: Some(self.deadline_of(until)) };
        loop {
            let e = match self.h.routes.entry(g) {
                Some(entry) => {
                    let msg = |tx| NodeMsg::Data(DataReq::Remote(g, req.clone()), tx);
                    match self.ask(usize::from(entry.core), msg, until) {
                        Some(Ok(DataReply::Remote(r))) => return Ok(r),
                        Some(Ok(_)) => return Err(RangeError::Unavailable),
                        Some(Err(e)) => {
                            if super::unsettled(&e) {
                                self.note_unsettled(until);
                            }
                            e
                        }
                        None => {
                            self.note_unsettled(until);
                            RangeError::Timeout
                        }
                    }
                }
                None => RangeError::OutOfRange,
            };
            let again = matches!(e, RangeError::NotLeader { .. } | RangeError::Overloaded);
            if !again || Instant::now() >= until {
                return Err(e);
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    /// The newest committed versions of `keys`, read at one mark: the first key's leader takes it
    /// under its clock rule (D-003, D-069) and the other keys are read at it.
    pub(super) fn versions(
        &mut self,
        keys: &[Vec<u8>],
    ) -> Result<teseladb_wire::commands::Versions, CommandError> {
        let mut mark: Option<Hlc> = None;
        let mut out = Vec::with_capacity(keys.len());
        for key in keys {
            let at = mark;
            let build = |_: &teseladb_router::table::RouteEntry, _| RemoteReq::Read {
                key: key.clone(),
                at,
            };
            match self.data(key, true, build)? {
                (_, RemoteReply::Value { ts, value }) => {
                    mark.get_or_insert(ts);
                    out.push(value);
                }
                _ => return Err(CommandError::new(INTERNAL_ERROR, "a read answered with a write")),
            }
        }
        let ts = mark.ok_or_else(|| CommandError::new(INTERNAL_ERROR, "a read of no keys"))?;
        Ok((ts, out))
    }

    /// Runs `writes` as one internal transaction reading at `read_ts`: its commit mark.
    pub(super) fn run_txn(
        &mut self,
        read_ts: Hlc,
        writes: Vec<(Vec<u8>, IntentOp)>,
        txn: Option<TxnId>,
    ) -> Result<Hlc, CommandError> {
        let id = txn.unwrap_or_else(|| {
            TxnId((u128::from(self.random()) << 64) | u128::from(self.random()))
        });
        let routes = self.h.routes.clone();
        let route = Routes(&routes);
        let mut c = Coordinator::new(id, (0, 0), read_ts, now());
        // The commit goes no lower than the read mark; its intents' marks lift it above.
        c.write_and_commit(&route, writes, read_ts);
        let until = self.until();
        let mut outcome = None;
        loop {
            for ev in c.take_events() {
                match ev {
                    // The first end is the answer; an abort after a failure is its cleanup.
                    Event::Ended(o) => {
                        outcome.get_or_insert(o);
                    }
                    // Its intents are resolved: nothing left to do.
                    Event::Resolved => {
                        if let Some(o) = outcome.take() {
                            return ended(o);
                        }
                    }
                    Event::Written { .. } => {}
                }
            }
            let requests = c.take_requests();
            if requests.is_empty() {
                // Ended without intents to resolve (one phase, or an abort before any).
                if let Some(o) = outcome.take() {
                    return ended(o);
                }
                if Instant::now() >= until {
                    return Err(from_range(
                        &RangeError::Timeout,
                        RangeContext { retryable_write: false, max_time_expired: false },
                    ));
                }
                // A commit above the clock answers once the clock is there (008 FR-008).
                match c.clock_wait() {
                    Some(ms) => {
                        super::wait_clock(ms, until);
                    }
                    None => thread::sleep(RETRY_PAUSE),
                }
                c.tick(&route, now(), crate::clock::wall_ms());
                continue;
            }
            for r in requests {
                let answer = self.ask_range(r.g, r.op).and_then(reply_of);
                c.on_reply(&route, r.id, answer);
            }
        }
    }
}
