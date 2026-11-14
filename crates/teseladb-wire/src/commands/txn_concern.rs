//! The read concern's marks (008 FR-011, FR-012, FR-013, DV-012): the mark a client transaction's
//! snapshot is read at, the times a read concern asks for checked against the cluster time, the
//! mark of a `snapshot` or causal read outside a transaction with the `atClusterTime` of its
//! reply, and the errors of a read below the history, as the reference router 9.0.4 answers them
//! (measured on 31-10-2026).

use teseladb_bson::{Document, Value};
use teseladb_hlc::{Hlc, from_timestamp, to_timestamp};
use teseladb_txn::causal;
use teseladb_txn::uncertainty::window_end;

use super::Backend;
use super::txn::{READ_CONCERN_LEVEL_IN_TRANSACTION, session_name};
use crate::concerns::{Level, ReadConcernArgs};
use crate::error::{
    CommandError, INVALID_OPTIONS, MIGRATION_CONFLICT, SNAPSHOT_TOO_OLD,
    TRANSIENT_TRANSACTION_ERROR,
};

/// `atClusterTime`.
const AT_CLUSTER_TIME: &str = "atClusterTime";
/// `afterClusterTime`.
const AFTER_CLUSTER_TIME: &str = "afterClusterTime";
/// The cursor of a reply.
const CURSOR: &str = "cursor";
/// The values of `distinct`'s reply.
const VALUES: &str = "values";

/// The mark a transaction reads at by default: the node's clock, or the cluster time the client
/// saw (`$clusterTime`) when that is ahead, so that it reads what the client saw. A range serves
/// a mark ahead of its clock once its clock gets there (D-081). A read at the node's own mark
/// leaves the rest of its millisecond to the writes, which a read at the millisecond's last
/// mark would take from them until the next one.
pub(super) fn now_mark<B: Backend + ?Sized>(backend: &B, name: &str, cmd: &Document) -> Hlc {
    causal::current(backend.cluster_mark(), gossip(cmd, name))
}

/// The cluster time the client saw (`$clusterTime`); a malformed one was refused before the
/// command ran.
fn gossip(cmd: &Document, name: &str) -> Option<Hlc> {
    match crate::cluster_time::parse(cmd, name) {
        Ok(Some(given)) => Some(from_timestamp((given.time.secs, given.time.inc))),
        Ok(None) | Err(_) => None,
    }
}

/// The reference server's error for a time ahead of the cluster time `current` (measured on 9.0.4):
/// the message names `atClusterTime` for a `snapshot` read, which the reference router reads at
/// that time.
fn ahead_error(word: &str, (t, i): (u32, u32), current: Hlc) -> CommandError {
    let (ct, ci) = to_timestamp(current);
    CommandError::new(
        INVALID_OPTIONS,
        format!(
            "readConcern {word} value must not be greater than the current clusterTime. Requested \
             clusterTime: {{ ts: Timestamp({t}, {i}) }}; current clusterTime: {{ ts: \
             Timestamp({ct}, {ci}) }}"
        ),
    )
}

/// The times command `name`'s read concern `rc` asks for, checked against the cluster time, and
/// the mark its reads are served at outside a transaction, with whether its reply gives it (FR-012,
/// FR-013; `None` in a transaction, which reads at its own mark).
pub fn causal_mark<B: Backend + ?Sized>(
    backend: &mut B,
    (name, db, cmd): (&str, &str, &Document),
    rc: Option<&ReadConcernArgs>,
    in_txn: bool,
) -> Result<Option<teseladb_txn::causal::ReadMark>, CommandError> {
    // A transaction's first statement checks its times once the transaction started
    // ([`begin_mark`]); the others cannot have a read concern.
    let (Some(rc), false) = (rc, in_txn) else { return Ok(None) };
    let current = now_mark(backend, name, cmd);
    let snapshot = rc.level == Some(Level::Snapshot);
    let at = rc.at_cluster_time.map(|t| (t, from_timestamp(t)));
    let after = rc.after_cluster_time.map(|t| (t, from_timestamp(t)));
    not_ahead(rc, current)?;
    // Before its database was added, the reference router 9.0.4 answers `MigrationConflict`
    // (measured). Only a read at a cluster time looks the database up: the lookup is a round trip
    // to the catalog that the other reads of the hot path do not pay (012 FR-008).
    if let Some((given, mark)) = at
        && let Some(added) = backend.database_mark(db)?
        && mark < added
    {
        let ((t, i), (at_t, at_i)) = (to_timestamp(added), given);
        return Err(CommandError::new(
            MIGRATION_CONFLICT,
            format!(
                "Database {db} has undergone a catalog change operation at time Timestamp({t}, {i}) \
                 and no longer satisfies the requirements for the current transaction which \
                 requires Timestamp({at_t}, {at_i}). Transaction will be aborted."
            ),
        ));
    }
    let asked = causal::Asked { snapshot, at: at.map(|(_, m)| m), after: after.map(|(_, m)| m) };
    Ok(causal::read_mark(asked, current))
}

/// That the times read concern `rc` asks for are not ahead of the cluster time `current`.
fn not_ahead(rc: &ReadConcernArgs, current: Hlc) -> Result<(), CommandError> {
    let snapshot = rc.level == Some(Level::Snapshot);
    for (given, word) in [
        rc.at_cluster_time.map(|t| (t, AT_CLUSTER_TIME)),
        rc.after_cluster_time
            .map(|t| (t, if snapshot { AT_CLUSTER_TIME } else { AFTER_CLUSTER_TIME })),
    ]
    .into_iter()
    .flatten()
    {
        if causal::ahead(from_timestamp(given), current) {
            return Err(ahead_error(word, given, current));
        }
    }
    Ok(())
}

/// The reply of a command whose reads were served at `mark`: a reply that gives it gets
/// `atClusterTime` (last in its cursor, or after `distinct`'s values, measured on 9.0.4), and a
/// mark below the history is a reference replica set member's `SnapshotTooOld`.
pub fn stamp(
    result: Result<Document, CommandError>,
    mark: Option<teseladb_txn::causal::ReadMark>,
) -> Result<Document, CommandError> {
    let Some(causal::ReadMark { at, learned, .. }) = mark else { return result };
    let (t, i) = to_timestamp(at);
    let reply = match result {
        Err(e) if e.code == SNAPSHOT_TOO_OLD => {
            return Err(CommandError::new(
                SNAPSHOT_TOO_OLD,
                format!(
                    "Read timestamp Timestamp({t}, {i}) is older than the oldest available timestamp."
                ),
            ));
        }
        Err(e) => return Err(e),
        Ok(reply) => reply,
    };
    if !learned {
        return Ok(reply);
    }
    let ts = Value::Timestamp { time: t, increment: i };
    let mut out = Document::new();
    for (k, v) in reply.iter() {
        match (k, v) {
            (CURSOR, Value::Document(c)) => {
                out.push(k, Value::Document(c.clone().with(AT_CLUSTER_TIME, ts.clone())))
            }
            (VALUES, _) => {
                out.push(k, v.clone());
                out.push(AT_CLUSTER_TIME, ts.clone());
            }
            _ => out.push(k, v.clone()),
        }
    }
    Ok(out)
}

/// How a transaction's first statement begins it, from its `readConcern`.
#[derive(Debug)]
pub(super) enum Begin {
    /// It reads at the mark, with the end of its uncertainty window if it has one.
    At(Hlc, Option<Hlc>),
    /// A time ahead of the cluster time: as on the reference router 9.0.4, the shard starts the
    /// transaction and the statement fails with this error, which aborts it (measured on
    /// 03-11-2026 with `atClusterTime` and `afterClusterTime`).
    Ahead(CommandError),
}

/// The mark a transaction reads at, from its first statement's `readConcern` (FR-011, DV-012):
/// `local`, `majority`, `snapshot` and none read at [`now_mark`], or at least at
/// `afterClusterTime`; `atClusterTime` reads at it. A time ahead of the cluster time is
/// [`Begin::Ahead`]. The other levels are refused before the transaction starts, as the reference
/// router 9.0.4 refuses them (measured).
pub(super) fn begin_mark<B: Backend + ?Sized>(
    backend: &B,
    name: &str,
    cmd: &Document,
) -> Result<Begin, CommandError> {
    let now = now_mark(backend, name, cmd);
    // A mark from the clock has an uncertainty window (FR-008); one the client gave has none.
    let clock = |at: Hlc| Ok(Begin::At(at, Some(window_end(at))));
    let Some(rc) = ReadConcernArgs::parse(cmd, name)? else { return clock(now) };
    if matches!(rc.level, Some(Level::Available | Level::Linearizable)) {
        return Err(CommandError::new(INVALID_OPTIONS, READ_CONCERN_LEVEL_IN_TRANSACTION));
    }
    if let Err(e) = not_ahead(&rc, now) {
        return Ok(Begin::Ahead(e));
    }
    if let Some(at) = rc.at_cluster_time {
        return Ok(Begin::At(from_timestamp(at), None));
    }
    clock(rc.after_cluster_time.map_or(now, |t| now.max(from_timestamp(t))))
}

/// A statement's read below the history (`SnapshotTooOld`) with the reference server's message and
/// label (measured on 9.0.4): the transaction is over and the driver repeats it.
pub(super) fn snapshot_error(
    e: CommandError,
    (lsid, txn_number): (u128, i64),
    statement: u32,
    at: Hlc,
) -> CommandError {
    if e.code != SNAPSHOT_TOO_OLD {
        return e;
    }
    let (t, i) = to_timestamp(at);
    CommandError::new(
        SNAPSHOT_TOO_OLD,
        format!(
            "Transaction {}:{} was aborted on statement {statement} due to: a non-retryable snapshot \
             error :: caused by :: Read timestamp Timestamp({t}, {i}) is older than the oldest \
             available timestamp.",
            session_name(lsid),
            txn_number
        ),
    )
    .with_label(TRANSIENT_TRANSACTION_ERROR)
}
