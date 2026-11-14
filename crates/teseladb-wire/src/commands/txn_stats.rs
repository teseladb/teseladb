//! What the node counts of its client transactions (008 FR-019) for `serverStatus.transactions`
//! and the admin port's metrics. As the reference router 9.0.4 counts them (measured on
//! 31-10-2026): a transaction counts as aborted once, when the client gets the error that ends it,
//! whose name is its cause (`abort` for an `abortTransaction`); one the node aborted for its age
//! counts when its next command fails with `NoSuchTransaction`. Ranges stand for the reference
//! router's shards. A commit counts by its kind: `noShards` with no range contacted, `singleShard`
//! with one (whether it wrote or not, as the reference router counts a read-only transaction on one
//! shard), and with several, `readOnly` with nothing written, `singleWriteShard` with one range
//! written and `twoPhaseCommit` otherwise.

use std::collections::BTreeSet;

use teseladb_bson::{Document, Value};
use teseladb_types::GroupId;

use super::NodeSessions;
use super::status::StatusCounters;
use super::txn::Transactions;
use crate::error::{ALL, CommandError};

/// The cause of an abort asked with `abortTransaction`.
pub const EXPLICIT_ABORT: &str = "abort";

/// Indexes of [`super::status::COMMIT_TYPES`].
const NO_SHARDS: usize = 0;
const SINGLE_SHARD: usize = 1;
const SINGLE_WRITE_SHARD: usize = 2;
const READ_ONLY: usize = 3;
const TWO_PHASE_COMMIT: usize = 4;

/// The name of error code `code`, as `codeName` gives it.
pub fn code_name(code: i32) -> String {
    ALL.iter().find(|c| c.code == code).map_or_else(|| format!("Location{code}"), |c| c.name.into())
}

/// The cause of a statement's failure: its error, or its first write error.
pub fn failure_cause(result: &Result<Document, CommandError>) -> Option<String> {
    let reply = match result {
        // An error of the router's parse leaves the transaction running (measured on 9.0.4).
        Err(e) if e.origin == crate::error::ErrorOrigin::Router => return None,
        Err(e) => return Some(e.code.name.to_owned()),
        Ok(reply) => reply,
    };
    // An error answered as a reply (the index commands answer theirs so).
    if reply.get("ok") == Some(&Value::Double(0.0))
        && let Some(Value::Int32(c)) = reply.get("code")
    {
        return Some(code_name(*c));
    }
    let Some(Value::Array(errors)) = reply.get("writeErrors") else { return None };
    match errors.first() {
        Some(Value::Document(e)) => match e.get("code") {
            Some(Value::Int32(c)) => Some(code_name(*c)),
            _ => None,
        },
        _ => None,
    }
}

/// The index in [`super::status::COMMIT_TYPES`] of a commit that contacted `contacted` ranges and
/// wrote in `written`.
pub fn commit_kind(contacted: &BTreeSet<GroupId>, written: &BTreeSet<GroupId>) -> usize {
    let all: BTreeSet<&GroupId> = contacted.iter().chain(written).collect();
    match (all.len(), written.len()) {
        (0, _) => NO_SHARDS,
        (1, _) => SINGLE_SHARD,
        (_, 0) => READ_ONLY,
        (_, 1) => SINGLE_WRITE_SHARD,
        _ => TWO_PHASE_COMMIT,
    }
}

/// A transaction started.
pub fn started(g: &mut NodeSessions) {
    g.status.txn.started += 1;
}

/// A transaction ended aborted, by `cause`.
pub fn aborted(g: &mut NodeSessions, cause: &str) {
    let t = &mut g.status.txn;
    t.aborted += 1;
    *t.abort_causes.entry(cause.to_owned()).or_default() += 1;
}

/// A transaction that ended (`at_commit`: with its commit) after contacting `ranges` ranges with
/// `requests` requests.
pub fn ended(g: &mut NodeSessions, ranges: usize, requests: i64, at_commit: bool) {
    let t = &mut g.status.txn;
    let ranges = i64::try_from(ranges).unwrap_or(i64::MAX);
    t.participants += ranges;
    t.requests += requests;
    if at_commit {
        t.participants_at_commit += ranges;
    }
}

/// A commit of kind `kind` began.
pub fn commit_initiated(g: &mut NodeSessions, kind: usize) {
    g.status.txn.commits[kind].0 += 1;
}

/// A commit of kind `kind` succeeded after `micros` microseconds.
pub fn committed(g: &mut NodeSessions, kind: usize, micros: u64) {
    let t = &mut g.status.txn;
    t.committed += 1;
    t.commits[kind].1 += 1;
    t.commits[kind].2 += i64::try_from(micros).unwrap_or(i64::MAX);
}

/// A read met a version in its uncertainty window.
pub fn uncertainty_restart(g: &mut NodeSessions) {
    g.status.txn.uncertainty_restarts += 1;
}

/// A reader or a writer pushed a transaction whose intent it met.
pub fn pushed(g: &mut NodeSessions) {
    g.status.txn.pushes += 1;
}

/// The node's counters for `serverStatus`, with the transactions' gauges as they are now: open,
/// running a command, and the intents their coordinators have yet to resolve.
pub fn status(g: &NodeSessions) -> StatusCounters {
    let mut status = g.status.clone();
    g.hot.stats.add_to(&mut status);
    status.txn.open = count(&g.txns, |s| s.is_open());
    status.txn.active = count(&g.txns, |s| s.is_active());
    let unresolved: usize = g.txn_resolving.iter().map(|c| c.unresolved()).sum();
    status.txn.unresolved_intents = i64::try_from(unresolved).unwrap_or(i64::MAX);
    status
}

fn count(txns: &Transactions, f: impl Fn(&super::txn::SessionTxns) -> bool) -> i64 {
    i64::try_from(txns.values().filter(|s| f(s)).count()).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::error::{DUPLICATE_KEY, WRITE_CONFLICT};

    fn set(gs: &[u64]) -> BTreeSet<GroupId> {
        gs.iter().map(|g| GroupId(*g)).collect()
    }

    #[test]
    fn commits_take_the_kind_the_reference_router_gives_them() {
        assert_eq!(commit_kind(&set(&[]), &set(&[])), NO_SHARDS);
        assert_eq!(commit_kind(&set(&[1]), &set(&[])), SINGLE_SHARD);
        assert_eq!(commit_kind(&set(&[1]), &set(&[1])), SINGLE_SHARD);
        assert_eq!(commit_kind(&set(&[]), &set(&[2])), SINGLE_SHARD);
        assert_eq!(commit_kind(&set(&[1, 2]), &set(&[])), READ_ONLY);
        assert_eq!(commit_kind(&set(&[1, 2]), &set(&[2])), SINGLE_WRITE_SHARD);
        assert_eq!(commit_kind(&set(&[1]), &set(&[2, 3])), TWO_PHASE_COMMIT);
    }

    #[test]
    fn causes_are_named_by_their_error() {
        assert_eq!(code_name(DUPLICATE_KEY.code), DUPLICATE_KEY.name);
        assert_eq!(code_name(50940), "Location50940");
        let e = CommandError::new(WRITE_CONFLICT, "x");
        assert_eq!(failure_cause(&Err(e)).as_deref(), Some(WRITE_CONFLICT.name));
        let write_error =
            |e: Value| Document::new().with("ok", 1.0).with("writeErrors", Value::Array(vec![e]));
        let dup = Document::new().with("code", DUPLICATE_KEY.code);
        let reply = write_error(Value::Document(dup));
        assert_eq!(failure_cause(&Ok(reply)).as_deref(), Some(DUPLICATE_KEY.name));
        assert_eq!(failure_cause(&Ok(Document::new().with("ok", 1.0))), None);
        assert_eq!(failure_cause(&Ok(write_error(Value::Int32(1)))), None);
        let no_code = Document::new().with("errmsg", "x");
        assert_eq!(failure_cause(&Ok(write_error(Value::Document(no_code)))), None);
    }
}
