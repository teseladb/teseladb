//! Client transactions (008 FR-009, T016): `startTransaction` with `autocommit: false`, the
//! statements that continue it, `commitTransaction` and `abortTransaction`, with the session
//! rules and errors of [`SessionTxn`] as RefDb 9.0.4 gives them.
//!
//! The transaction's coordinator ([`Coordinator`]) lives in the session's entry of the node's
//! tables between commands. A command of the session waits while another one has it. A statement
//! takes it out, runs with a [`TxnBackend`] (reads at the
//! transaction's mark with its own intents; writes evaluated here as the range's leader would and
//! written as intents) and puts it back; a statement that fails aborts the transaction, as
//! RefDb does. `commitTransaction` answers as [`commit_answer`] says, and a commit asked again
//! gets the first one's answer. Between commands the node's keeper ticks the idle transactions
//! ([`tick_idle`]): their heartbeats keep the reaper away until the lifetime limit.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_auth::privileges::Privilege;
use teseladb_auth::users::{RoleName, UserName};
use teseladb_hlc::Hlc;
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::indexes::{IndexCommand, IndexEntry};
use teseladb_meta::security::{Principal, SecurityError, SecurityOp};
use teseladb_range::host::{RangeError, TxnOp};
use teseladb_range::intents::{IntentOp, TxnId};
use teseladb_range::push::TxnView;
use teseladb_range::read::ReadConcern;
use teseladb_txn::commit_retry::{CommitAnswer, commit_answer};
use teseladb_txn::coordinator::{Coordinator, Event, Outcome, Route, TxnError};
use teseladb_txn::recovery::{Decision, Recovery, client_txn_id};
use teseladb_txn::session_txn::{Ask, SessionError, SessionTxn, Verdict};
use teseladb_bson::extjson::base64;
use teseladb_bson::{Document, Value};
use teseladb_types::{GroupId, Nanos};

use super::cross_range;
use super::mutation::Bound;
use super::session::KillScope;
use super::txn_buffer::Buffer;
use super::txn_concern::{Begin, begin_mark, now_mark, snapshot_error};
use super::txn_ddl::{self, Created};
use super::txn_stats;
use super::{ANONYMOUS_UID, Backend, Shared, WriteOp, lock, uuid_text};
use crate::error::{
    CONFLICTING_OPERATION_IN_PROGRESS, CommandError, ErrorOrigin,
    FAILED_TO_SATISFY_READ_PREFERENCE, INTERNAL_ERROR, INVALID_OPTIONS, MAX_TIME_MS_EXPIRED,
    NO_SUCH_TRANSACTION, NOT_A_RETRYABLE_WRITE_COMMAND, OPERATION_NOT_SUPPORTED_IN_TRANSACTION,
    RECOVERY_TOKEN_REQUIRED, RangeContext, SHARD_NOT_FOUND, TOO_MANY_OPEN_TRANSACTIONS,
    TRANSACTION_COMMITTED, TRANSACTION_TOO_LARGE, TRANSACTION_TOO_OLD, TRANSIENT_TRANSACTION_ERROR,
    TXN_NUMBER_NOT_ALLOWED, UNAUTHORIZED, UNKNOWN_TRANSACTION_COMMIT_RESULT,
    UNSATISFIABLE_WRITE_CONCERN, WRITE_CONFLICT, WRITE_CONFLICT_MESSAGE, from_range,
};
use crate::ops::{MAX_TIME_MS, TIME_LIMIT_MESSAGE};
use teseladb_txn::uncertainty::{OnUncertain, on_uncertain, window_end};

/// `commitTransaction`.
pub const COMMIT_TRANSACTION: &str = "commitTransaction";
/// `abortTransaction`.
pub const ABORT_TRANSACTION: &str = "abortTransaction";
/// The field that marks a statement of a transaction.
pub(crate) const AUTOCOMMIT: &str = "autocommit";
/// The field of a transaction's first statement.
const START_TRANSACTION: &str = "startTransaction";
/// `count`, which a transaction cannot run.
const COUNT: &str = "count";
const WRITE_CONCERN: &str = "writeConcern";
const READ_CONCERN: &str = "readConcern";
/// `errmsg` of `count` in a transaction.
pub const COUNT_IN_TRANSACTION: &str = "Cannot run 'count' in a multi-document transaction. Please see http://dochub.refdb.org/core/transaction-count for a recommended alternative.";
/// `errmsg` of a statement with `writeConcern`.
pub const WRITE_CONCERN_IN_TRANSACTION: &str =
    "writeConcern is not allowed within a multi-statement transaction";
/// `errmsg` of a first statement whose `readConcern` level a transaction cannot read at.
pub const READ_CONCERN_LEVEL_IN_TRANSACTION: &str = "The first command in a transaction cannot specify a readConcern level other than local, majority, or snapshot";
/// `errmsg` of a later statement with `readConcern`.
pub const READ_CONCERN_AFTER_FIRST: &str =
    "Only the first command in a transaction may specify a readConcern";

/// A client transaction between its statements.
#[derive(Debug)]
struct Running {
    txn_number: i64,
    /// Cluster time at its start, in milliseconds.
    started_ms: u64,
    /// The node's monotonic time at its start, in microseconds (012 FR-015).
    started_us: u64,
    coord: Coordinator,
    view: TxnView,
    /// Statements run so far, which names the next one in RefDb's messages.
    statements: u32,
    /// A statement got past the router's parse: a `refrouter` 9.0.4 sent it to a participant
    /// (measured on 03-11-2026; until then the router answers the commit and the abort itself).
    sent: bool,
    /// The collections it created (T019).
    created: Vec<Created>,
    /// Its collections have their names: the commit is under way.
    published: bool,
    /// The end of its reads' uncertainty window (FR-008; `None` for a snapshot at a given time).
    uncertain_to: Option<Hlc>,
    /// The mark of a version a read of the running statement met in that window.
    uncertain: Option<Hlc>,
    /// The ranges its reads went to (FR-019).
    contacted: BTreeSet<GroupId>,
    /// Requests it sent to ranges.
    requests: i64,
    /// Documents its statements wrote: the position of the next one in its events (014).
    ops: u32,
    /// The writes held until the commit, which sends them with it (in one phase when one range
    /// has every key), and the keys locked for them (FR-022): `None` once each statement puts
    /// its intents.
    buffer: Option<Buffer>,
    /// Cluster time of its last lock, in milliseconds: an idle transaction locks again before
    /// its locks stop counting.
    locked_ms: u64,
}

impl Running {
    fn read_ts(&self) -> Hlc {
        self.coord.read_ts()
    }

    /// Notes the mark of a version in the uncertainty window that `e` carries.
    fn note_uncertain(&mut self, mut e: CommandError) -> CommandError {
        if let Some(ts) = e.take_uncertain() {
            self.uncertain = Some(self.uncertain.map_or(ts, |u| u.max(ts)));
        }
        e
    }
}

/// The transactions of one session.
#[derive(Debug, Default)]
pub struct SessionTxns {
    rules: SessionTxn,
    /// The running transaction, while no command has it.
    running: Option<Running>,
    /// A command has the running transaction.
    in_use: bool,
    /// The commit mark of the last committed transaction, for a commit asked again.
    committed: Option<(i64, Hlc)>,
    /// A transaction the node aborted for its age, until a command of it fails (FR-019).
    reaped: Option<i64>,
    /// The last transaction that started, and whether a write ran in it: its `recoveryToken`.
    token: Option<(i64, bool)>,
    /// A transaction a statement's error ended (008), until a command of it fails, and whether
    /// that statement was its first.
    failed: Option<(i64, bool)>,
}

impl SessionTxns {
    /// It has a transaction open, run by a command or between commands.
    pub fn is_open(&self) -> bool {
        self.in_use || self.running.is_some()
    }

    /// A command runs its transaction now.
    pub fn is_active(&self) -> bool {
        self.in_use
    }
}

/// The transactions of the node's sessions, by `lsid`.
pub type Transactions = BTreeMap<u128, SessionTxns>;

/// What a command says about transactions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TxnFields {
    lsid: u128,
    txn_number: i64,
    autocommit: Option<bool>,
    start: bool,
}

impl TxnFields {
    /// The fields of a transaction of the node itself (017: the writes with encrypted fields),
    /// on session `lsid`.
    pub(crate) fn internal(lsid: u128, txn_number: i64, start: bool) -> TxnFields {
        TxnFields { lsid, txn_number, autocommit: Some(false), start }
    }
}

fn flag(cmd: &Document, name: &str) -> Result<Option<bool>, CommandError> {
    match cmd.get(name) {
        None => Ok(None),
        Some(Value::Boolean(b)) => Ok(Some(*b)),
        Some(_) => {
            Err(CommandError::new(INVALID_OPTIONS, format!("'{name}' field must be a boolean")))
        }
    }
}

/// The transaction fields of `cmd`, `None` for a command outside a transaction.
///
/// The session fields are checked as a `refrouter` 9.0.4 checks them (measured): a `txnNumber`
/// needs a session, `autocommit` needs a `txnNumber`, and a commit or an abort needs both.
pub fn fields(name: &str, cmd: &Document) -> Result<Option<TxnFields>, CommandError> {
    let autocommit = flag(cmd, AUTOCOMMIT)?;
    let start = flag(cmd, START_TRANSACTION)?;
    let ends = ends(name);
    let (lsid, txn_number) = (super::args::lsid(cmd)?, super::args::txn_number(cmd)?);
    let invalid = |msg: String| Err(CommandError::new(INVALID_OPTIONS, msg));
    if txn_number.is_some() && lsid.is_none() {
        return invalid(NUMBER_WITHOUT_SESSION.to_owned());
    }
    if autocommit.is_some() && txn_number.is_none() {
        return invalid(AUTOCOMMIT_WITHOUT_NUMBER.to_owned());
    }
    if start.is_some() && autocommit.is_none() {
        return invalid(teseladb_txn::session_txn::START_WITHOUT_AUTOCOMMIT.to_owned());
    }
    let (Some(lsid), Some(txn_number)) = (lsid, txn_number) else {
        return match ends {
            true => invalid(format!("{name} can only be run within a session")),
            false => Ok(None),
        };
    };
    if autocommit.is_none() && !ends {
        return Ok(None);
    }
    let start = start.unwrap_or(false);
    Ok(Some(TxnFields { lsid, txn_number, autocommit, start }))
}

/// Whether command `name` ends a transaction: `commitTransaction` or `abortTransaction`.
pub fn ends(name: &str) -> bool {
    name == COMMIT_TRANSACTION || name == ABORT_TRANSACTION
}

/// `errmsg` of a `txnNumber` without a session (measured on a `refrouter` 9.0.4).
const NUMBER_WITHOUT_SESSION: &str =
    "Transaction number requires a session ID to also be specified";
/// `errmsg` of `autocommit` without a `txnNumber` (measured on a `refrouter` 9.0.4).
const AUTOCOMMIT_WITHOUT_NUMBER: &str =
    "'autocommit' field requires a transaction number to also be specified";

/// The error of a `txnNumber` sent without `autocommit: false` to command `name`, as a `refrouter`
/// 9.0.4 answers it (measured): only a retryable write takes it; the commands of
/// [`ROUTER_COMMANDS`] take none at all.
fn bare_txn_number(name: &str, cmd: &Document) -> Option<CommandError> {
    let bare = cmd.get("lsid").is_some()
        && cmd.get("txnNumber").is_some()
        && cmd.get(AUTOCOMMIT).is_none()
        && cmd.get(START_TRANSACTION).is_none();
    if !bare || WRITES.contains(&name) {
        return None;
    }
    if ROUTER_COMMANDS.contains(&name) {
        return Some(CommandError::new(
            TXN_NUMBER_NOT_ALLOWED,
            "It is illegal to provide a txnNumber for this command",
        ));
    }
    Some(CommandError::new(
        NOT_A_RETRYABLE_WRITE_COMMAND,
        format!(
            "txnNumber may only be provided for multi-document transactions and retryable write \
             commands. autocommit:false was not provided, and {name} is not a retryable write \
             command."
        ),
    ))
}

/// The commands a statement of a transaction can be (measured on a `refrouter` 9.0.4; `count` has a
/// message of its own).
const IN_TRANSACTION: [&str; 15] = [
    "find",
    "getMore",
    KILL_CURSORS,
    "aggregate",
    "distinct",
    "insert",
    "update",
    "delete",
    "findAndModify",
    "findandmodify",
    "bulkWrite",
    "create",
    "createIndexes",
    COMMIT_TRANSACTION,
    ABORT_TRANSACTION,
];
/// `killCursors`, which cannot start a transaction.
const KILL_CURSORS: &str = "killCursors";
/// The commands a `refrouter` 9.0.4 runs itself and refuses in a transaction with
/// [`NOT_SUPPORTED_IN_TRANSACTION`] (measured).
const ROUTER_COMMANDS: [&str; 6] =
    ["hello", "isMaster", "ismaster", "ping", "connectionStatus", "listCommands"];
/// `errmsg` of a command of [`ROUTER_COMMANDS`] in a transaction.
pub const NOT_SUPPORTED_IN_TRANSACTION: &str = "This command is not supported in transactions";
/// `errmsg` of a transaction started with `killCursors`.
pub const KILL_CURSORS_FIRST: &str =
    "Cannot run killCursors as the first operation in a multi-document transaction.";

/// The commands refused before their arguments are read: those a `refrouter` 9.0.4 refuses at once
/// ([`ROUTER_COMMANDS`], and `killOp`, measured), and those that change the node's tables without
/// asking the node, which a refused command must not do while its arguments are read.
const REFUSED_AT_ONCE: [&str; 12] = [
    super::session::END_SESSIONS,
    super::session::REFRESH_SESSIONS,
    super::session::KILL_SESSIONS,
    super::session::KILL_ALL_SESSIONS,
    super::session::KILL_ALL_SESSIONS_BY_PATTERN,
    super::session::START_SESSION,
    "setParameter",
    "killOp",
    "invalidateUserCache",
    "rotateCertificates",
    "logRotate",
    KILL_CURSORS,
];

/// Whether a command [`refused`] in a transaction is refused before its arguments are read; the
/// others run first with every request to the node refused, so that their argument errors come
/// first, as on a `refrouter` 9.0.4 (measured), and nothing changes.
pub fn refused_at_once(name: &str) -> bool {
    ROUTER_COMMANDS.contains(&name) || REFUSED_AT_ONCE.contains(&name)
}

/// The error of command `name` sent as a statement of a transaction that a transaction cannot
/// run, as a `refrouter` 9.0.4 answers it (measured); `None` for a command that is no statement or
/// that a transaction runs.
pub fn refused(name: &str, cmd: &Document) -> Option<CommandError> {
    if let Some(e) = bare_txn_number(name, cmd) {
        return Some(e);
    }
    let f = fields(name, cmd).ok()??;
    let refuse = |msg: String| Some(CommandError::new(OPERATION_NOT_SUPPORTED_IN_TRANSACTION, msg));
    if name == KILL_CURSORS && f.start {
        return refuse(KILL_CURSORS_FIRST.to_owned());
    }
    if IN_TRANSACTION.contains(&name) || name == COUNT {
        return None;
    }
    // `configureFailPoint` as the router refuses its own commands (measured on a `refrouter` 9.0.4
    // with test commands; without them it does not exist, `dispatch`).
    if ROUTER_COMMANDS.contains(&name) || name == crate::failpoint::CONFIGURE_FAIL_POINT {
        return refuse(NOT_SUPPORTED_IN_TRANSACTION.to_owned());
    }
    refuse(format!("Cannot run '{name}' in a multi-document transaction."))
}

/// How a session's transaction is named in RefDb's messages.
pub(super) fn session_name(lsid: u128) -> String {
    format!("{} - {ANONYMOUS_UID} -  - ", uuid_text(&lsid.to_be_bytes()))
}

/// The `uid` of the session of `user` in RefDb's messages: the base64 of the SHA-256 of
/// `user@db`, [`ANONYMOUS_UID`] without a login (measured on a `refrouter` 9.0.4 on 03-11-2026).
pub(super) fn user_uid(user: Option<&UserName>) -> String {
    match user {
        Some(u) => base64::encode(&teseladb_hash::sha256(format!("{}@{}", u.user, u.db).as_bytes())),
        None => ANONYMOUS_UID.to_owned(),
    }
}

/// `e` with the session's user `uid` where [`session_name`] wrote the anonymous one: the
/// messages are made where the session's user is not at hand, and `run_in_txn` names it.
fn name_the_user(mut e: CommandError, uid: &str) -> CommandError {
    if uid != ANONYMOUS_UID {
        e.errmsg = e.errmsg.replace(ANONYMOUS_UID, uid);
    }
    e
}

fn session_error(e: SessionError, lsid: u128) -> CommandError {
    let session = session_name(lsid);
    match e {
        SessionError::InvalidOptions(msg) => CommandError::new(INVALID_OPTIONS, msg),
        SessionError::TooOld { asked, last } => CommandError::new(
            TRANSACTION_TOO_OLD,
            format!(
                "txnNumber {asked} is less than last txnNumber {last} seen in session {session}"
            ),
        ),
        SessionError::AlreadyStarted(n) => CommandError::new(
            CONFLICTING_OPERATION_IN_PROGRESS,
            format!("txnNumber {n} txnRetryCounter 0 for session {session} already started"),
        ),
        SessionError::NoSuchTransaction { last } => {
            let n = last.unwrap_or(-1);
            CommandError::new(
                NO_SUCH_TRANSACTION,
                format!("cannot continue txnId {n} for session {session} with txnRetryCounter 0"),
            )
            .with_label(TRANSIENT_TRANSACTION_ERROR)
        }
        // Answered with the command's `recoveryToken` (`unrecorded`).
        SessionError::Unrecorded { .. } => {
            CommandError::new(RECOVERY_TOKEN_REQUIRED, RECOVERY_TOKEN_MISSING)
        }
        SessionError::Committed(n) => CommandError::new(
            TRANSACTION_COMMITTED,
            format!("Transaction with {{ txnNumber: {n} }} has been committed."),
        ),
    }
}

/// The error of a statement or a commit of transaction `txn_number` that failed in the
/// coordinator.
fn txn_error(e: TxnError, txn_number: i64) -> CommandError {
    match e {
        TxnError::WriteConflict => CommandError::new(WRITE_CONFLICT, WRITE_CONFLICT_MESSAGE)
            .with_label(TRANSIENT_TRANSACTION_ERROR),
        // As the shard answers through a `refrouter` 9.0.4 (measured on 01-11-2026).
        TxnError::NoSuchTransaction => CommandError::new(
            NO_SUCH_TRANSACTION,
            format!("Transaction with {{ txnNumber: {txn_number} }} has been aborted."),
        )
        .with_label(TRANSIENT_TRANSACTION_ERROR),
        TxnError::TooLarge => CommandError::new(
            TRANSACTION_TOO_LARGE,
            "the transaction's state passed the coordinator's memory budget",
        ),
        TxnError::NoRange => {
            CommandError::new(INTERNAL_ERROR, "no range is known for a key of the transaction")
        }
        TxnError::Range(e) => range_error(&e),
    }
}

fn range_error(e: &RangeError) -> CommandError {
    from_range(e, RangeContext { retryable_write: false, max_time_expired: false })
}

/// The node's routing table, as the coordinator asks it.
struct Via<'a>(&'a dyn Backend);

impl Route for Via<'_> {
    fn range_of(&self, key: &[u8]) -> Option<GroupId> {
        self.0.txn_route(key)
    }

    fn ranges(&self) -> Vec<(GroupId, Vec<u8>)> {
        self.0.txn_ranges()
    }
}

/// How long an idle transaction keeps its locks before it asks for them again: well under the
/// time after which a range stops counting them (FR-022).
const LOCK_KEEP_MS: u64 = 2_000;

/// Lets go of `locks`, the keys transaction `txn` locked in each range (FR-022): it aborted, or
/// its commit failed. A lock that is not let go stops counting once the transaction is idle.
fn unlock(backend: &mut dyn Backend, txn: TxnId, locks: Vec<(GroupId, Vec<Vec<u8>>)>) {
    for (g, keys) in locks {
        let _ = send(backend, g, TxnOp::UnlockKeys { txn, keys });
    }
}

/// Lets go of the locks of the writes `run` holds.
fn release_locks(backend: &mut dyn Backend, run: &mut Running) {
    let txn = run.coord.txn();
    if let Some(buffer) = run.buffer.as_mut() {
        unlock(backend, txn, buffer.take_locks());
    }
}

/// Asks again for the locks of the writes `run` holds when it has been idle for
/// [`LOCK_KEEP_MS`], so that they keep counting in their ranges. A lock another transaction took
/// meanwhile is not an error here: the commit finds the conflict.
fn keep_locks(backend: &mut dyn Backend, run: &mut Running) {
    let now = backend.now_ms();
    let Some(buffer) = run.buffer.as_ref() else { return };
    let locks = buffer.locks();
    if locks.is_empty() || now.saturating_sub(run.locked_ms) < LOCK_KEEP_MS {
        return;
    }
    run.locked_ms = now;
    let (txn, read_ts) = (run.coord.txn(), run.read_ts());
    for (g, keys) in locks {
        let _ = send(backend, g, TxnOp::LockKeys { txn, read_ts, keys });
    }
}

/// Sends request `op` of a coordinator to range `g` and counts how long it took in its stage
/// (012 FR-015).
fn send(
    b: &mut dyn Backend,
    g: GroupId,
    op: teseladb_range::host::TxnOp,
) -> Result<teseladb_range::host::Reply, RangeError> {
    let stage = teseladb_txn::stages::stage_of(&op);
    let start = b.now_micros();
    let answer = b.txn_send(g, op);
    if let Some(stage) = stage {
        let took = b.now_micros().saturating_sub(start).saturating_mul(1_000);
        teseladb_stages::record(stage, took);
    }
    answer
}

fn nanos(b: &dyn Backend) -> Nanos {
    Nanos(b.now_micros().saturating_mul(1000))
}

/// Sends the coordinator's requests until one of its events makes `done` true: the events.
/// Without requests to send, it ticks the coordinator after a pause; past the operation's
/// deadline it stops with `Timeout`.
fn drive(
    b: &mut dyn Backend,
    coord: &mut Coordinator,
    done: impl Fn(&Event) -> bool,
) -> Result<Vec<Event>, RangeError> {
    let mut seen = Vec::new();
    loop {
        let events = coord.take_events();
        let finished = events.iter().any(&done);
        seen.extend(events);
        if finished {
            return Ok(seen);
        }
        let requests = coord.take_requests();
        if requests.is_empty() {
            // A commit above the clock answers once the clock is there (FR-008).
            let waited = match coord.clock_wait() {
                Some(ms) => {
                    let start = b.now_micros();
                    let waited = b.txn_wait_clock(ms);
                    let took = b.now_micros().saturating_sub(start).saturating_mul(1_000);
                    teseladb_stages::record(teseladb_stages::Stage::TxnClockWait, took);
                    waited
                }
                None => b.txn_pause(),
            };
            if !waited {
                return Err(RangeError::Timeout);
            }
            let (now, wall) = (nanos(b), b.wall_ms());
            coord.tick(&Via(&*b), now, wall);
            continue;
        }
        for r in requests {
            let answer = send(b, r.g, r.op);
            coord.on_reply(&Via(&*b), r.id, answer);
        }
    }
}

/// The end of a commit or an abort: its first end, once its intents are resolved or nothing
/// more can be known.
fn ended(events: &[Event]) -> Option<Outcome> {
    events.iter().find_map(|e| match e {
        Event::Ended(o) => Some(o.clone()),
        Event::Written { .. } | Event::Resolved => None,
    })
}

fn finished(e: &Event) -> bool {
    matches!(e, Event::Resolved | Event::Ended(Outcome::Unknown(_)))
}

/// A statement of a client transaction: its reads at the transaction's mark with its own intents,
/// its writes as intents.
pub struct TxnBackend<'a> {
    inner: &'a mut dyn Backend,
    run: &'a mut Running,
    shared: &'a Shared,
}

impl TxnBackend<'_> {
    /// `values`, what the ranges hold at `keys`, with the writes the transaction holds over them
    /// (FR-022).
    fn over_values(
        &self,
        keys: &[Vec<u8>],
        values: Vec<Option<(u8, Vec<u8>)>>,
    ) -> Vec<Option<(u8, Vec<u8>)>> {
        match &self.run.buffer {
            Some(buffer) => buffer.over_values(keys, values),
            None => values,
        }
    }

    /// At most `limit` documents of `[start, end)` read at `at` with `view`, in key order or
    /// from the last one down (`back`), with the writes the transaction holds over them (FR-022).
    fn scan_over(
        &mut self,
        (start, end): (&[u8], &[u8]),
        limit: usize,
        (at, view, window): (Hlc, &TxnView, Option<Hlc>),
        back: bool,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        let inner = &mut *self.inner;
        let mut read = |from: &[u8], to: &[u8], n: usize| match back {
            false => inner.txn_scan(from, to, n, at, view, window),
            true => inner.txn_scan_back(from, to, n, at, view, window),
        };
        match (&self.run.buffer, back) {
            (Some(buffer), false) => super::txn_buffer::scan(buffer, (start, end), limit, read),
            (Some(buffer), true) => super::txn_buffer::scan_back(buffer, (start, end), limit, read),
            (None, _) => read(start, end, limit),
        }
    }

    /// Notes a read request to the range of `key` (FR-019).
    fn contact(&mut self, key: &[u8]) {
        self.run.requests += 1;
        if let Some(g) = self.inner.txn_route(key) {
            self.run.contacted.insert(g);
        }
    }

    /// `writes` with what the events of the documents they write need (014 FR-002, FR-005): each
    /// document's meta carries the transaction and the document's position in it, with the
    /// description of its update, if any.
    fn with_history(
        &mut self,
        writes: Vec<(Vec<u8>, IntentOp)>,
    ) -> Result<Vec<(Vec<u8>, IntentOp)>, CommandError> {
        use teseladb_changestream::meta::{ChangeMeta, DocWrite, TxnInfo, doc_of_meta, history};
        let is_history = |op: &IntentOp| matches!(op, IntentOp::Put { value_type, .. } if *value_type == teseladb_range::HISTORY_VALUE);
        let (mut updates, mut replaced, mut moved) = (Vec::new(), Vec::new(), Vec::new());
        for (key, op) in &writes {
            if let (IntentOp::Put { value, .. }, true) = (op, is_history(op)) {
                let meta = ChangeMeta::decode(value).map_err(|e| {
                    CommandError::new(INTERNAL_ERROR, format!("change description: {e:?}"))
                })?;
                let Some(doc) = doc_of_meta(key) else { continue };
                if let Some(u) = meta.update {
                    updates.push((doc.to_vec(), u));
                }
                if meta.replace {
                    replaced.push(doc.to_vec());
                }
                if let Some(m) = meta.moved {
                    moved.push((doc.to_vec(), m));
                }
            }
        }
        let mut out: Vec<(Vec<u8>, IntentOp)> =
            writes.into_iter().filter(|(_, op)| !is_history(op)).collect();
        let is_doc = |key: &[u8], op: &IntentOp| {
            let doc_key = matches!(
                teseladb_keys::KeyParts::of(key),
                Ok(teseladb_keys::KeyParts::Data { index: teseladb_keys::DOCUMENTS, .. })
            );
            doc_key
                && match op {
                    IntentOp::Put { value_type, .. } => *value_type == super::mutation::DOCUMENT,
                    IntentOp::Delete => true,
                }
        };
        // Each write keeps its type and the document it left: a later write of the transaction
        // replaces the document's intent, and its event still has to show this one.
        let docs: Vec<DocWrite> = out
            .iter()
            .filter(|(k, op)| is_doc(k, op))
            .map(|(key, op)| {
                let update = updates.iter().find(|(u, _)| u == key).map(|(_, d)| d.clone());
                let replace = update.is_none() && replaced.contains(key);
                // The meta keeps the document's BSON, without its table (003 FR-021).
                let doc = match op {
                    IntentOp::Put { value, .. } => {
                        Some(teseladb_keys::stored::strip(value.clone()))
                    }
                    IntentOp::Delete => None,
                };
                let delete = doc.is_none();
                let moved = moved.iter().find(|(k, _)| k == key).map(|(_, m)| m.clone());
                DocWrite { key: key.clone(), update, replace, delete, doc, moved }
            })
            .collect();
        let (lsid, txn_number) = self.run.coord.session();
        let uid = super::session::caller(&*self.inner).uid;
        let info = TxnInfo { id: self.run.coord.txn().0, lsid, uid, txn_number };
        for (key, meta) in history(&docs, Some((&info, self.run.ops))) {
            let value = meta.encode().map_err(|e| {
                CommandError::new(INTERNAL_ERROR, format!("change description: {e}"))
            })?;
            out.push((key, IntentOp::Put { value_type: teseladb_range::HISTORY_VALUE, value }));
        }
        self.run.ops = self.run.ops.saturating_add(u32::try_from(docs.len()).unwrap_or(u32::MAX));
        Ok(out)
    }

    /// Locks the keys of `writes` in their ranges for the transaction (FR-022): `true` once they
    /// are all locked, `false` when the statement has to put intents instead (an intent of
    /// another transaction is on a key, which the coordinator pushes; a key whose range moved; or
    /// the writes held pass the transaction's memory budget, which the coordinator refuses).
    /// Another transaction's lock or a newer version is the statement's `WriteConflict`.
    fn lock(&mut self, writes: &[(Vec<u8>, IntentOp)]) -> Result<bool, CommandError> {
        let (txn, read_ts, number) =
            (self.run.coord.txn(), self.run.read_ts(), self.run.txn_number);
        let budget = self.inner.txn_memory_budget();
        let Some(buffer) = self.run.buffer.as_mut() else { return Ok(false) };
        let added: usize = writes.iter().map(|(k, _)| 2 * k.len()).sum();
        if buffer.bytes().saturating_add(added) > budget {
            return Ok(false);
        }
        let mut by_range: BTreeMap<GroupId, Vec<Vec<u8>>> = BTreeMap::new();
        for (key, _) in writes {
            let Some(g) = self.inner.txn_route(key) else { return Ok(false) };
            if !buffer.holds_lock(g, key) {
                by_range.entry(g).or_default().push(key.clone());
            }
        }
        self.run.locked_ms = self.inner.now_ms();
        for (g, keys) in by_range {
            self.run.requests += 1;
            let op = TxnOp::LockKeys { txn, read_ts, keys: keys.clone() };
            match send(&mut *self.inner, g, op) {
                Ok(_) => buffer.locked_in(g, keys),
                Err(RangeError::IntentConflict(_) | RangeError::OutOfRange) => return Ok(false),
                Err(RangeError::WriteConflict) => {
                    return Err(txn_error(TxnError::WriteConflict, number));
                }
                Err(e) => return Err(range_error(&e)),
            }
        }
        Ok(true)
    }

    /// Stops holding the transaction's writes: those held so far become intents, and each
    /// statement puts its own from now on. The ranges let go of the transaction's locks as its
    /// intents reach them.
    fn unbuffer(&mut self) -> Result<(), CommandError> {
        let Some(mut buffer) = self.run.buffer.take() else { return Ok(()) };
        let held = buffer.take_writes();
        if held.is_empty() {
            unlock(&mut *self.inner, self.run.coord.txn(), buffer.take_locks());
            return Ok(());
        }
        self.put_intents(held)
    }

    /// Writes `writes` as one statement of the transaction.
    fn statement(&mut self, writes: Vec<(Vec<u8>, IntentOp)>) -> Result<(), CommandError> {
        let writes = self.with_history(writes)?;
        // A transaction that created a collection puts its intents, as its catalog writes do.
        if !self.run.created.is_empty() {
            self.unbuffer()?;
        }
        if self.run.buffer.is_some() {
            if self.lock(&writes)? {
                if let Some(buffer) = self.run.buffer.as_mut() {
                    buffer.hold(writes);
                }
                return Ok(());
            }
            self.unbuffer()?;
        }
        self.put_intents(writes)
    }

    /// Puts the intents of `writes` as one statement of the transaction.
    fn put_intents(&mut self, writes: Vec<(Vec<u8>, IntentOp)>) -> Result<(), CommandError> {
        let number = self.run.txn_number;
        let stmt =
            self.run.coord.write(&Via(&*self.inner), writes).map_err(|e| txn_error(e, number))?;
        let is_answer = |e: &Event| matches!(e, Event::Written { stmt: s, .. } if *s == stmt);
        let events =
            drive(&mut *self.inner, &mut self.run.coord, is_answer).map_err(|e| range_error(&e))?;
        let answer = events.into_iter().find_map(|e| match e {
            Event::Written { stmt: s, result } if s == stmt => Some(result),
            Event::Written { .. } | Event::Ended(_) | Event::Resolved => None,
        });
        match answer {
            Some(Ok(())) => Ok(()),
            Some(Err(e)) => Err(txn_error(e, number)),
            None => Err(CommandError::new(INTERNAL_ERROR, "the statement got no answer")),
        }
    }
}

/// The fields and commands a statement of a transaction cannot have (measured on 9.0.4).
fn statement_rules(name: &str, ask: Ask, cmd: &Document) -> Result<(), CommandError> {
    if name == COUNT {
        return Err(CommandError::new(
            OPERATION_NOT_SUPPORTED_IN_TRANSACTION,
            COUNT_IN_TRANSACTION,
        ));
    }
    if matches!(ask, Ask::Start | Ask::Continue) && cmd.get(WRITE_CONCERN).is_some() {
        return Err(CommandError::new(INVALID_OPTIONS, WRITE_CONCERN_IN_TRANSACTION));
    }
    if ask == Ask::Continue && cmd.get(READ_CONCERN).is_some() {
        return Err(CommandError::new(INVALID_OPTIONS, READ_CONCERN_AFTER_FIRST));
    }
    Ok(())
}

/// Runs command `name` of a transaction (`f`), the statements with `statement`.
pub fn run_in_txn(
    backend: &mut dyn Backend,
    shared: &Shared,
    command: (&str, &str, &Document),
    f: TxnFields,
    statement: impl FnMut(&mut TxnBackend<'_>) -> Option<Result<Document, CommandError>>,
) -> Option<Result<Document, CommandError>> {
    let arrived_us = backend.now_micros();
    let result = run_command(backend, shared, command, f, statement);
    if !ends(command.0) {
        let took = backend.now_micros().saturating_sub(arrived_us).saturating_mul(1_000);
        teseladb_stages::record(teseladb_stages::Stage::TxnStatement, took);
    }
    let result = with_token(backend, shared, command.0, f, result);
    let uid = user_uid(backend.authenticated_user().as_ref());
    let result = result.map(|r| r.map_err(|e| name_the_user(e, &uid)));
    // A transaction the node aborted for its age counts when a command of it fails (FR-019).
    if let Some(Err(e)) = &result
        && e.code == NO_SUCH_TRANSACTION
    {
        let mut g = lock(shared);
        let reaped = g.txns.get_mut(&f.lsid).and_then(|s| s.reaped.take_if(|n| *n == f.txn_number));
        if reaped.is_some() {
            txn_stats::aborted(&mut g, NO_SUCH_TRANSACTION.name);
        }
    }
    result
}

/// The commands that write, whose transaction then names its shard in `recoveryToken`.
const WRITES: [&str; 6] = [
    super::crud::INSERT,
    super::crud::UPDATE,
    super::crud::DELETE,
    super::crud::FIND_AND_MODIFY,
    super::crud::FIND_AND_MODIFY_LEGACY,
    super::crud::BULK_WRITE,
];

/// `result` with the `recoveryToken` a `refrouter` 9.0.4 adds inside a transaction (measured): `{}`
/// until a write or a `create` ran in it, then `recoveryShardId`, the node's name in
/// `config.shards`. Among the errors only those of `commitTransaction` and `abortTransaction` of a
/// transaction that started get it, with their `errorLabels` first; not the commit after a first
/// statement that failed, which the router answers: a statement's error is the shard's, which the
/// `refrouter` sends as it is (measured on 03-11-2026 with 48 `NamespaceExists` and 2 `BadValue`,
/// before and after a write, and with a time ahead of the cluster time).
fn with_token(
    backend: &mut dyn Backend,
    shared: &Shared,
    name: &str,
    f: TxnFields,
    result: Option<Result<Document, CommandError>>,
) -> Option<Result<Document, CommandError>> {
    let token = {
        let mut g = lock(shared);
        let s = g.txns.entry(f.lsid).or_default();
        // The router answers the commit after a failed first statement itself, without one.
        let router_commit = name == COMMIT_TRANSACTION && s.failed == Some((f.txn_number, true));
        // A write the transaction ran: not one refused as the transaction started, which never
        // ran (measured on 03-11-2026: an empty token after a time ahead of the cluster time).
        let ran = s.running.as_ref().is_some_and(|r| r.txn_number == f.txn_number);
        match s.token.as_mut() {
            Some((n, wrote)) if *n == f.txn_number && !router_commit => {
                let created = name == super::catalog::CREATE && matches!(result, Some(Ok(_)));
                *wrote |= ran && (WRITES.contains(&name) || created);
                Some(*wrote)
            }
            _ => None,
        }
    };
    let doc = |wrote: bool| match wrote {
        true => Document::new().with("recoveryShardId", super::shard_name(backend.node())),
        false => Document::new(),
    };
    result.map(|r| match (r, token) {
        (Ok(reply), t) => Ok(reply.with(RECOVERY_TOKEN, doc(t.unwrap_or(false)))),
        (Err(mut e), Some(wrote)) if ends(name) => {
            e.extra.push(RECOVERY_TOKEN, doc(wrote));
            Err(e.with_labels_first())
        }
        (Err(e), _) => Err(e),
    })
}

/// `recoveryToken`.
const RECOVERY_TOKEN: &str = "recoveryToken";

/// [`run_in_txn`] before its count.
fn run_command(
    backend: &mut dyn Backend,
    shared: &Shared,
    (name, db, cmd): (&str, &str, &Document),
    f: TxnFields,
    statement: impl FnMut(&mut TxnBackend<'_>) -> Option<Result<Document, CommandError>>,
) -> Option<Result<Document, CommandError>> {
    let ask = match name {
        COMMIT_TRANSACTION => Ask::Commit,
        ABORT_TRANSACTION => Ask::Abort,
        _ if f.start => Ask::Start,
        _ => Ask::Continue,
    };
    if ask == Ask::Commit && db != ADMIN {
        return Some(Err(CommandError::new(UNAUTHORIZED, COMMIT_OUTSIDE_ADMIN)));
    }
    if let Err(e) = statement_rules(name, ask, cmd) {
        return Some(Err(e));
    }
    // The first statement's read concern is checked before the transaction starts: a refused
    // one starts nothing.
    let begin = match ask {
        Ask::Start => match begin_mark(backend, name, cmd) {
            Ok(b) => Some(b),
            Err(e) => return Some(Err(e)),
        },
        Ask::Continue | Ask::Commit | Ask::Abort => None,
    };
    // Another command of the session has its transaction: this one waits for it, as a session's
    // commands wait for each other in RefDb 9.0.4 (measured through `refrouter`).
    while lock(shared).txns.get(&f.lsid).is_some_and(|s| s.in_use) {
        if !backend.txn_pause() {
            return Some(Err(CommandError::new(MAX_TIME_MS_EXPIRED, TIME_LIMIT_MESSAGE)));
        }
    }
    let (checked, before) = {
        let mut g = lock(shared);
        let s = g.txns.entry(f.lsid).or_default();
        let before = s.rules.clone();
        (s.rules.check(f.txn_number, f.autocommit, ask), before)
    };
    let verdict = match checked {
        Ok(v) => v,
        // A member of a replica set has no router that recovers a decision (measured on a replica
        // set of `ref:9.0.4`, 03-11-2026): the number stays unused.
        Err(SessionError::Unrecorded { last, .. }) if member(backend) => {
            restore(shared, f, before);
            return Some(Err(not_in_progress(f, last)));
        }
        Err(SessionError::Unrecorded { asked, last }) => {
            return Some(unrecorded(backend, shared, cmd, f.lsid, (asked, last)));
        }
        Err(SessionError::NoSuchTransaction { last }) if member(backend) => {
            return Some(Err(match before.is_aborted(f.txn_number) {
                true => txn_error(TxnError::NoSuchTransaction, f.txn_number),
                false => not_in_progress(f, last),
            }));
        }
        // As the shard answers through a `refrouter` 9.0.4 (measured on 01-11-2026).
        Err(SessionError::NoSuchTransaction { .. })
            if let Some(first) = failed_first(shared, f) =>
        {
            return Some(Err(failed_answer(backend, ask, f, first)));
        }
        Err(e) => return Some(Err(session_error(e, f.lsid))),
    };
    let ok = || Document::new().with("ok", 1.0);
    match verdict {
        Verdict::Begin => {
            if let Some(e) = too_many_open(shared, f) {
                // A member leaves the session as it was: the number can start later (measured on
                // a replica set of `ref:9.0.4`, 03-11-2026).
                if member(backend) {
                    restore(shared, f, before);
                }
                let shard = super::shard_name(backend.node());
                return Some(refused_write(&shard, name, e));
            }
            // A higher number ends the session's transaction still in progress, which a failed
            // commit leaves (RefDb aborts it: PyRef's `commit-transienttransactionerror`
            // retries on the same session and its first statement found the old intents).
            let superseded = lock(shared).txns.get_mut(&f.lsid).and_then(|s| s.running.take());
            if let Some(run) = superseded {
                end_abandoned(backend, shared, run);
            }
            lock(shared).txns.entry(f.lsid).or_default().token = Some((f.txn_number, false));
            // Any node finds its record from the session and the number (`Recovery`).
            let txn = client_txn_id(f.lsid, f.txn_number);
            let (read_ts, uncertain_to) = match begin {
                Some(Begin::At(at, to)) => (at, to),
                Some(Begin::Ahead(e)) => return Some(failed_at_start(backend, shared, name, f, e)),
                None => {
                    let now = now_mark(backend, name, cmd);
                    (now, Some(window_end(now)))
                }
            };
            let coord = Coordinator::new(txn, (f.lsid, f.txn_number), read_ts, nanos(backend))
                .with_budget(backend.txn_memory_budget());
            let view = TxnView { own: Some(txn), known: BTreeMap::new() };
            let started_ms = backend.now_ms();
            let run = Running {
                txn_number: f.txn_number,
                started_ms,
                started_us: backend.now_micros(),
                coord,
                view,
                statements: 0,
                sent: false,
                created: Vec::new(),
                published: false,
                uncertain_to,
                uncertain: None,
                contacted: BTreeSet::new(),
                requests: 0,
                ops: 0,
                buffer: Some(Buffer::default()),
                locked_ms: 0,
            };
            txn_stats::started(&mut lock(shared));
            store(shared, f.lsid, run);
            with_running(backend, shared, (name, db, cmd), f, statement)
        }
        Verdict::Statement => with_running(backend, shared, (name, db, cmd), f, statement),
        Verdict::RetryCommit => {
            let at = lock(shared).txns.get(&f.lsid).and_then(|s| s.committed);
            match at {
                Some((n, _)) if n == f.txn_number => Some(Ok(ok())),
                _ => Some(Err(CommandError::new(INTERNAL_ERROR, "no commit to answer again"))),
            }
        }
        Verdict::Commit | Verdict::Abort => {
            let commit = verdict == Verdict::Commit;
            let sent = lock(shared)
                .txns
                .get(&f.lsid)
                .and_then(|s| s.running.as_ref())
                .is_some_and(|run| run.sent);
            let ended = end(backend, shared, f, commit);
            Some(match (ended, sent) {
                (Err(e), _) => Err(e),
                (Ok(()), true) if commit => Ok(commit_reply(backend, cmd)),
                (Ok(()), true) => Ok(ok()),
                (Ok(()), false) => nothing_sent(shared, f, commit),
            })
        }
    }
}

/// The reply of a commit that committed: with a `w` above the voters of the node's ranges, the
/// transaction stays committed and the reply carries `UnsatisfiableWriteConcern` as a
/// `writeConcernError` before `ok`, with the concern asked (measured on a `refrouter` 9.0.4 on
/// 05-11-2026).
fn commit_reply(backend: &mut dyn Backend, cmd: &Document) -> Document {
    let ok = Document::new().with("ok", 1.0);
    let Some(Value::Document(wc)) = cmd.get(WRITE_CONCERN) else { return ok };
    let Some(w) = wc.get("w").and_then(crate::types::number) else { return ok };
    let voters = backend.voters(&[], &[u8::MAX; 8]).unwrap_or(usize::MAX);
    if w <= voters as f64 {
        return ok;
    }
    let mut asked = Document::new().with("w", wc.get("w").cloned().unwrap_or(Value::Int32(0)));
    asked.push("wtimeout", wc.get("wtimeout").cloned().unwrap_or(Value::Int32(0)));
    asked.push("provenance", "clientSupplied");
    let error = Document::new()
        .with("code", UNSATISFIABLE_WRITE_CONCERN.code)
        .with("codeName", UNSATISFIABLE_WRITE_CONCERN.name)
        .with("errmsg", "Not enough data-bearing nodes")
        .with("errInfo", Document::new().with("writeConcern", asked));
    Document::new().with("writeConcernError", error).with("ok", 1.0)
}

/// The start of the router's own `NoSuchTransaction` for a transaction that was aborted.
const ABORTED_PREFIX: &str = "Transaction with { txnNumber: ";

/// The reply of command `name` as a `refrouter` 9.0.4 gives it: its own `NoSuchTransaction` to a
/// commit or an abort of an aborted transaction carries `errorLabels` first, where an error
/// relayed from a shard has them last (measured on 03-11-2026 for the commit and the abort after
/// a failed statement and an abort repeated); a statement after the abort gets the shard's, with
/// them last (`txn-errors`).
pub fn router_reply(name: &str, reply: Document) -> Document {
    let aborted = ends(name)
        && reply.get("code") == Some(&Value::Int32(NO_SUCH_TRANSACTION.code))
        && matches!(reply.get("errmsg"), Some(Value::String(m)) if m.starts_with(ABORTED_PREFIX));
    match aborted {
        true => super::replset::labels_first(reply),
        false => reply,
    }
}

/// Whether the node presents itself as a member of a replica set (018), which answers as a
/// `refnode` and not as a `refrouter` where they differ; a presentation it cannot read is `refrouter`'s.
fn member(backend: &mut dyn Backend) -> bool {
    super::config_db::as_member(backend).unwrap_or(false)
}

/// Puts back the session's rules of transaction `f` as they were before its command.
fn restore(shared: &Shared, f: TxnFields, before: SessionTxn) {
    lock(shared).txns.entry(f.lsid).or_default().rules = before;
}

/// A member's `NoSuchTransaction` for a number of the session that no transaction runs with
/// (measured on a replica set of `ref:9.0.4`, 03-11-2026): it names the number asked and the
/// session's active one, -1 before the first.
fn not_in_progress(f: TxnFields, active: Option<i64>) -> CommandError {
    CommandError::new(
        NO_SUCH_TRANSACTION,
        format!(
            "Given transaction number {} on session {} using txnRetryCounter 0 does not match any \
             in-progress transactions. The active transaction number is {}",
            f.txn_number,
            session_name(f.lsid),
            active.unwrap_or(-1)
        ),
    )
    .with_label(TRANSIENT_TRANSACTION_ERROR)
}

/// `errmsg` of the abort of a transaction none of whose statements reached a participant.
const NOTHING_SENT: &str = "no known command has been sent by this router for this transaction";

/// The answer to the commit (`commit`) or the abort of transaction `f`, which ended, when none of
/// its statements got past the router's parse: a `refrouter` 9.0.4 answers both itself (measured on
/// 03-11-2026), the commit with an integer `ok` and the abort with `NoSuchTransaction` and no
/// `recoveryToken`.
fn nothing_sent(shared: &Shared, f: TxnFields, commit: bool) -> Result<Document, CommandError> {
    if commit {
        return Ok(Document::new().with("ok", 1));
    }
    if let Some(s) = lock(shared).txns.get_mut(&f.lsid) {
        s.token = None;
    }
    Err(CommandError::new(NO_SUCH_TRANSACTION, NOTHING_SENT)
        .with_label(TRANSIENT_TRANSACTION_ERROR))
}

/// The answer to the first statement of transaction `f`, which the shard started and whose
/// statement failed with `e` before it read anything (a time ahead of the cluster time): the
/// transaction is over, as after any first statement that failed, and a write answers with `e` as
/// its first write error from the node (measured on a `refrouter` 9.0.4 on 03-11-2026).
fn failed_at_start(
    backend: &mut dyn Backend,
    shared: &Shared,
    name: &str,
    f: TxnFields,
    e: CommandError,
) -> Result<Document, CommandError> {
    {
        let mut g = lock(shared);
        txn_stats::started(&mut g);
        txn_stats::aborted(&mut g, e.code.name);
        txn_stats::ended(&mut g, 0, 0, false);
        let s = g.txns.entry(f.lsid).or_default();
        s.rules.aborted(f.txn_number);
        s.failed = Some((f.txn_number, true));
    }
    refused_write(&super::host_port(backend), name, e)
}

/// Whether a statement's error ended transaction `f`, and if it was the first statement.
fn failed_first(shared: &Shared, f: TxnFields) -> Option<bool> {
    match lock(shared).txns.get(&f.lsid)?.failed {
        Some((n, first)) if n == f.txn_number => Some(first),
        _ => None,
    }
}

/// The answer to a command of a transaction a statement's error ended, as through a `refrouter`
/// 9.0.4 (measured on 03-11-2026): the shard's `NoSuchTransaction`, except for a commit after a
/// failed first statement, which the router answers itself naming the participant.
fn failed_answer(backend: &mut dyn Backend, ask: Ask, f: TxnFields, first: bool) -> CommandError {
    if !(first && ask == Ask::Commit) {
        return txn_error(TxnError::NoSuchTransaction, f.txn_number);
    }
    CommandError::new(
        NO_SUCH_TRANSACTION,
        format!(
            "{}:{} Failed to commit transaction because a previous statement on the transaction \
             participant {} was unsuccessful.",
            session_name(f.lsid),
            f.txn_number,
            super::shard_name(backend.node())
        ),
    )
    .with_label(TRANSIENT_TRANSACTION_ERROR)
}

/// The only database `commitTransaction` runs on, and its error elsewhere (measured on a
/// `refrouter` 9.0.4; `abortTransaction` runs on any).
const ADMIN: &str = "admin";
const COMMIT_OUTSIDE_ADMIN: &str = "commitTransaction may only be run against the admin database.";
/// `errmsg` of a commit of a transaction the node has no record of, without `recoveryToken`.
const RECOVERY_TOKEN_MISSING: &str =
    "Cannot recover the transaction decision without a recoveryToken";

/// The answer to a commit of transaction `asked` that session `lsid` has no record of, as a
/// `refrouter` 9.0.4 gives it from the command's `recoveryToken` (measured on 01-11-2026 through two
/// routers): without one it cannot recover the decision; an empty one says the transaction only
/// read; one that names a node of `config.shards` gets the decision from the transaction's
/// record, which Raft keeps in its range and any node finds from the session and the number
/// ([`Recovery`]): committed answers `ok`, a running one aborts, and a number the session never
/// began is stopped there. A name `config.shards` does not list is not found; a record that does
/// not answer by the deadline fails as a shard that cannot be reached. The session's running
/// transaction, of a lower number, is over.
fn unrecorded(
    backend: &mut dyn Backend,
    shared: &Shared,
    cmd: &Document,
    lsid: u128,
    (asked, last): (i64, Option<i64>),
) -> Result<Document, CommandError> {
    let superseded = lock(shared).txns.get_mut(&lsid).and_then(|s| s.running.take());
    if let Some(run) = superseded {
        end_abandoned(backend, shared, run);
    }
    let token = match cmd.get(RECOVERY_TOKEN) {
        Some(Value::Document(t)) => t,
        _ => return Err(CommandError::new(RECOVERY_TOKEN_REQUIRED, RECOVERY_TOKEN_MISSING)),
    };
    let unknown = |msg: String| {
        CommandError::new(NO_SUCH_TRANSACTION, msg).with_label(TRANSIENT_TRANSACTION_ERROR)
    };
    let shard = match token.get("recoveryShardId") {
        None => {
            return Err(unknown(
                "Recovery token is empty, meaning the transaction only performed reads and can \
                 be safely retried"
                    .to_owned(),
            ));
        }
        Some(Value::String(shard)) => shard.clone(),
        Some(other) => format!("{other:?}"),
    };
    check_shard(backend, &shard)?;
    let Some(decision) = recover(backend, lsid, asked) else {
        return Err(match cmd.get(MAX_TIME_MS) {
            Some(_) => CommandError::new(MAX_TIME_MS_EXPIRED, TIME_LIMIT_MESSAGE),
            None => CommandError::new(
                FAILED_TO_SATISFY_READ_PREFERENCE,
                format!(
                    "Could not find host matching read preference {{ mode: \"primary\" }} for \
                     set {shard}"
                ),
            ),
        });
    };
    let empty = Document::new();
    let mut e = match decision {
        Decision::Committed(_) => {
            return Ok(Document::new().with("ok", 1.0).with(RECOVERY_TOKEN, empty));
        }
        Decision::Aborted => {
            unknown("Recovering the transaction's outcome found the transaction aborted".into())
        }
        Decision::Unknown => unknown(format!(
            "Given transaction number {asked} on session {} using txnRetryCounter 0 does not \
             match any in-progress transactions. The active transaction number is {}",
            session_name(lsid),
            last.unwrap_or(-1)
        )),
    };
    e.extra.push(RECOVERY_TOKEN, empty);
    Err(e)
}

/// The decision on transaction `txn_number` of session `lsid` from its record (`None` if the
/// record's range did not answer before the operation's deadline).
fn recover(backend: &mut dyn Backend, lsid: u128, txn_number: i64) -> Option<Decision> {
    let mut r = Recovery::new(lsid, txn_number);
    loop {
        r.tick(&Via(&*backend));
        let requests = r.take_requests();
        // It pauses only when a step made no progress: nothing to send yet, or a range that
        // did not answer. Each step that got its answer is followed by the next at once.
        let mut stuck = requests.is_empty();
        for req in requests {
            let answer = send(backend, req.g, req.op);
            stuck |= matches!(&answer, Err(e) if *e != RangeError::NoSuchTransaction);
            r.on_reply(req.id, answer);
        }
        if let Some(d) = r.decision() {
            return Some(d);
        }
        if stuck && !backend.txn_pause() {
            return None;
        }
    }
}

/// That a `recoveryShardId` names this node or one `config.shards` lists.
fn check_shard(backend: &mut dyn Backend, shard: &str) -> Result<(), CommandError> {
    if shard == super::shard_name(backend.node()) {
        return Ok(());
    }
    let listed = backend.config_documents(super::config_db::SHARDS)?;
    match listed.iter().any(|d| matches!(d.get("_id"), Some(Value::String(s)) if s == shard)) {
        true => Ok(()),
        false => Err(CommandError::new(SHARD_NOT_FOUND, format!("Shard {shard} not found"))),
    }
}

/// Aborts transaction `run`, which no command will end any more (too old, or one of a higher
/// number replaced it): its intents resolved as far as one round goes, the reaper and the readers
/// resolve the rest, and the collections it created are dropped.
fn end_abandoned(backend: &mut dyn Backend, shared: &Shared, mut run: Running) {
    release_locks(backend, &mut run);
    run.coord.abort();
    for r in run.coord.take_requests() {
        let answer = send(backend, r.g, r.op);
        run.coord.on_reply(&Via(&*backend), r.id, answer);
    }
    if run.published {
        txn_ddl::unpublish(backend, &run.created);
    }
    txn_ddl::discard(backend, shared, &run.created);
}

/// `TooManyOpenTransactions` for a transaction that would pass the node's
/// `maxConcurrentMultiDocumentTransactions` (FR-020; 0 lifts the limit), with a `refnode` 9.0.4's
/// message; the refused one does not start.
fn too_many_open(shared: &Shared, f: TxnFields) -> Option<CommandError> {
    let mut g = lock(shared);
    let limit = g.params.max_concurrent_transactions;
    let open = g.txns.iter().filter(|(l, s)| **l != f.lsid && s.is_open()).count();
    let open = i64::try_from(open).unwrap_or(i64::MAX);
    if limit == 0 || open < limit {
        return None;
    }
    g.txns.entry(f.lsid).or_default().rules.aborted(f.txn_number);
    let msg = format!(
        "cannot start a new multi-document transaction; there are already {open} open \
         transactions, which meets or exceeds the limit of {limit}"
    );
    Some(CommandError::new(TOO_MANY_OPEN_TRANSACTIONS, msg))
}

/// The answer to a command whose transaction did not start, or failed as it started, because of
/// `e`: a write answers as a `refrouter` 9.0.4 does (measured), `ok` with nothing written and the
/// shard's error, from `from`, as its first write error; another command fails with `e`.
fn refused_write(from: &str, name: &str, e: CommandError) -> Result<Document, CommandError> {
    let counts: &[&str] = match name {
        super::crud::INSERT | super::crud::DELETE => &["n"],
        super::crud::UPDATE => &["nModified", "n"],
        _ => return Err(e),
    };
    let msg = format!("cluster write results unavailable from {from} :: caused by :: {}", e.errmsg);
    let error = Document::new().with("index", 0).with("code", e.code.code).with("errmsg", msg);
    let reply = counts.iter().fold(Document::new(), |d, n| d.with(*n, 0));
    Ok(reply.with("writeErrors", Value::Array(vec![Value::Document(error)])).with("ok", 1.0))
}

/// A statement's `WriteConflict` as a `refrouter` 9.0.4 relays it from the shard (measured, DV-031,
/// D-102): with the node's host and port where it names the shard's.
fn from_participant(backend: &mut dyn Backend, mut e: CommandError) -> CommandError {
    if e.code == WRITE_CONFLICT {
        let host = super::host_port(backend);
        e.errmsg = format!(
            "Encountered error from {host} during a transaction :: caused by :: cluster write results unavailable from {host} :: caused by :: {}",
            e.errmsg
        );
    }
    e
}

/// The ranges a transaction contacted: those it read and those it wrote.
fn participants(run: &Running) -> usize {
    let written = run.coord.written_ranges();
    run.contacted.union(&written).count()
}

fn store(shared: &Shared, lsid: u128, run: Running) {
    let mut g = lock(shared);
    let s = g.txns.entry(lsid).or_default();
    s.running = Some(run);
    s.in_use = false;
}

/// Takes the session's running transaction out of the tables.
fn take(shared: &Shared, f: TxnFields) -> Result<Running, CommandError> {
    let mut g = lock(shared);
    let s = g.txns.entry(f.lsid).or_default();
    match s.running.take() {
        Some(run) if run.txn_number == f.txn_number => {
            s.in_use = true;
            Ok(run)
        }
        other => {
            s.running = other;
            let last = s.rules.last();
            Err(session_error(SessionError::NoSuchTransaction { last }, f.lsid))
        }
    }
}

/// A reply whose first write error ends the transaction for the driver to repeat it: as RefDb
/// does in a transaction, the command fails with that error and `TransientTransactionError`
/// (measured on 9.0.4 for `WriteConflict`).
fn transient_on_top(reply: Document, txn_number: i64) -> Result<Document, CommandError> {
    let Some(Value::Array(errors)) = reply.get("writeErrors") else { return Ok(reply) };
    let code = match errors.first() {
        Some(Value::Document(e)) => e.get("code").cloned(),
        _ => None,
    };
    match code {
        Some(Value::Int32(c)) if c == WRITE_CONFLICT.code => {
            Err(txn_error(TxnError::WriteConflict, txn_number))
        }
        Some(Value::Int32(c)) if c == NO_SUCH_TRANSACTION.code => {
            Err(txn_error(TxnError::NoSuchTransaction, txn_number))
        }
        _ => Ok(reply),
    }
}

/// Runs a statement with the session's transaction; a failure, or a write error, aborts it.
fn with_running(
    backend: &mut dyn Backend,
    shared: &Shared,
    (name, db, cmd): (&str, &str, &Document),
    f: TxnFields,
    mut statement: impl FnMut(&mut TxnBackend<'_>) -> Option<Result<Document, CommandError>>,
) -> Option<Result<Document, CommandError>> {
    let mut run = match take(shared, f) {
        Ok(run) => run,
        Err(e) => return Some(Err(e)),
    };
    let number = run.statements;
    run.statements += 1;
    let mut restarts = 0;
    let result = loop {
        let mut tb = TxnBackend { inner: &mut *backend, run: &mut run, shared };
        let created = tb.run.created.clone();
        let result = match txn_ddl::precheck(&mut tb, &created, (name, db, cmd)) {
            Ok(()) => statement(&mut tb),
            // As `createIndexes` answers its shard's errors (`indexes::forwarded`).
            Err(e) if name == txn_ddl::CREATE_INDEXES => {
                Some(Ok(super::indexes::shard_error(&mut tb, e)))
            }
            Err(e) => Some(Err(e)),
        };
        // A read met a version in its uncertainty window (FR-008).
        let Some(ts) = run.uncertain.take() else { break result };
        txn_stats::uncertainty_restart(&mut lock(shared));
        let wrote = run.coord.written() || run.buffer.as_ref().is_some_and(|b| !b.is_empty());
        match on_uncertain(number, wrote, restarts, ts) {
            OnUncertain::Restart(at) => {
                restarts += 1;
                let (txn, now) = (run.coord.txn(), nanos(&*backend));
                run.coord = Coordinator::new(txn, (f.lsid, f.txn_number), at, now)
                    .with_budget(backend.txn_memory_budget());
                run.ops = 0;
            }
            OnUncertain::Fail => break result,
        }
    };
    let at = run.read_ts();
    let result = result
        .map(|r| {
            r.map(|mut reply| {
                txn_ddl::mark_created(&run.created, db, cmd, &mut reply);
                reply
            })
        })
        .map(|r| {
            r.and_then(|reply| transient_on_top(reply, f.txn_number))
                .map_err(|e| snapshot_error(e, (f.lsid, f.txn_number), number, at))
                .map_err(|e| from_participant(&mut *backend, e))
        });
    // The router's parse, and the shard's parse of the query of a statement that would join the
    // transaction, leave the statement short of a participant.
    let first = !run.sent;
    let short = match &result {
        Some(Err(e)) => match e.origin {
            ErrorOrigin::Router => true,
            ErrorOrigin::QueryParse => first,
            ErrorOrigin::Shard => false,
        },
        Some(Ok(_)) | None => false,
    };
    run.sent |= result.is_some() && !short;
    let failure = result.as_ref().filter(|_| !short).and_then(txn_stats::failure_cause);
    if let Some(cause) = failure {
        run.coord.abort();
        // The abort's answer only resolves the intents: the statement's error is the answer.
        let _ = drive(backend, &mut run.coord, finished);
        release_locks(backend, &mut run);
        txn_ddl::discard(backend, shared, &std::mem::take(&mut run.created));
        let mut g = lock(shared);
        txn_stats::aborted(&mut g, &cause);
        txn_stats::ended(&mut g, participants(&run), run.requests, false);
        let s = g.txns.entry(f.lsid).or_default();
        s.rules.aborted(f.txn_number);
        s.failed = Some((f.txn_number, first));
    }
    store(shared, f.lsid, run);
    result
}

/// Commits (`commit`) or aborts the session's transaction.
fn end(
    backend: &mut dyn Backend,
    shared: &Shared,
    f: TxnFields,
    commit: bool,
) -> Result<(), CommandError> {
    let mut run = take(shared, f)?;
    // The new collections get their names before the record commits (T019).
    if commit && !run.published {
        if let Err(e) = txn_ddl::publish(backend, &run.created) {
            run.coord.abort();
            let _ = drive(backend, &mut run.coord, finished);
            release_locks(backend, &mut run);
            txn_ddl::discard(backend, shared, &run.created);
            let mut g = lock(shared);
            txn_stats::aborted(&mut g, e.code.name);
            txn_stats::ended(&mut g, participants(&run), run.requests, false);
            let s = g.txns.entry(f.lsid).or_default();
            s.in_use = false;
            s.rules.aborted(f.txn_number);
            return Err(e);
        }
        run.published = true;
    }
    let mut buffer = run.buffer.take().unwrap_or_default();
    let (held, locks) = (buffer.take_writes(), buffer.take_locks());
    let mut written = run.coord.written_ranges();
    written.extend(held.iter().filter_map(|(k, _)| backend.txn_route(k)));
    let kind = txn_stats::commit_kind(&run.contacted, &written);
    let began = backend.now_micros();
    let clock = Hlc { physical: backend.wall_ms(), logical: 0 };
    match commit {
        true if !held.is_empty() => {
            txn_stats::commit_initiated(&mut lock(shared), kind);
            run.coord.write_and_commit(&Via(&*backend), held, clock);
        }
        true => {
            txn_stats::commit_initiated(&mut lock(shared), kind);
            run.coord.commit(clock);
        }
        false => run.coord.abort(),
    }
    // The answer goes at the end; the intents are resolved after it (FR-005, `resolve_queued`).
    let any_end = |e: &Event| matches!(e, Event::Ended(_) | Event::Resolved);
    let events = drive(backend, &mut run.coord, any_end);
    let resolved = events.as_ref().is_ok_and(|ev| ev.contains(&Event::Resolved));
    let answer = match events.map(|ev| ended(&ev)) {
        Ok(Some(o)) => commit_answer(&o),
        Ok(None) => CommitAnswer::Unknown(RangeError::Unavailable),
        Err(e) => CommitAnswer::Unknown(e),
    };
    if commit {
        let took = backend.now_micros().saturating_sub(run.started_us).saturating_mul(1_000);
        teseladb_stages::record(teseladb_stages::Stage::TxnTotal, took);
    }
    // A commit lets go of the locks in its ranges as its writes reach them; an abort, or a
    // commit that did not get there, lets go of them here (FR-022).
    if !matches!(answer, CommitAnswer::Committed(_)) {
        unlock(backend, run.coord.txn(), locks);
    }
    match &answer {
        CommitAnswer::Committed(_) => txn_ddl::forget(shared, &run.created),
        CommitAnswer::NoSuchTransaction | CommitAnswer::WriteConflict => {
            if run.published {
                txn_ddl::unpublish(backend, &run.created);
            }
            txn_ddl::discard(backend, shared, &run.created);
        }
        CommitAnswer::Unknown(_) => {}
    }
    let mut g = lock(shared);
    let (ranges, requests) = (participants(&run), run.requests);
    match (&answer, commit) {
        (CommitAnswer::Committed(_), true) => {
            let micros = backend.now_micros().saturating_sub(began);
            txn_stats::committed(&mut g, kind, micros);
            txn_stats::ended(&mut g, ranges, requests, true);
        }
        (CommitAnswer::NoSuchTransaction | CommitAnswer::WriteConflict, false) => {
            txn_stats::aborted(&mut g, txn_stats::EXPLICIT_ABORT);
            txn_stats::ended(&mut g, ranges, requests, false);
        }
        (CommitAnswer::NoSuchTransaction, true) => {
            txn_stats::aborted(&mut g, NO_SUCH_TRANSACTION.name);
            txn_stats::ended(&mut g, ranges, requests, false);
        }
        (CommitAnswer::WriteConflict, true) => {
            txn_stats::aborted(&mut g, WRITE_CONFLICT.name);
            txn_stats::ended(&mut g, ranges, requests, false);
        }
        (CommitAnswer::Committed(_), false) | (CommitAnswer::Unknown(_), _) => {}
    }
    let run = match !resolved && !matches!(answer, CommitAnswer::Unknown(_)) {
        true => {
            queue_resolution(&mut g, run.coord);
            None
        }
        false => Some(run),
    };
    let s = g.txns.entry(f.lsid).or_default();
    s.in_use = false;
    match (answer, commit) {
        (CommitAnswer::Committed(ts), true) => {
            s.rules.committed(f.txn_number);
            s.committed = Some((f.txn_number, ts));
            Ok(())
        }
        (CommitAnswer::Committed(_), false) => {
            Err(session_error(SessionError::Committed(f.txn_number), f.lsid))
        }
        // An abort that ends the transaction answers `ok`, whatever stopped it before.
        (CommitAnswer::NoSuchTransaction | CommitAnswer::WriteConflict, false) => {
            s.rules.aborted(f.txn_number);
            Ok(())
        }
        (CommitAnswer::NoSuchTransaction, true) => {
            s.rules.aborted(f.txn_number);
            Err(txn_error(TxnError::NoSuchTransaction, f.txn_number))
        }
        (CommitAnswer::WriteConflict, true) => {
            s.rules.aborted(f.txn_number);
            Err(txn_error(TxnError::WriteConflict, f.txn_number))
        }
        // The commit may have been applied: the driver asks again, and the coordinator with it.
        (CommitAnswer::Unknown(e), _) => {
            let max_time = matches!(e, RangeError::Timeout);
            let ctx = RangeContext { retryable_write: false, max_time_expired: max_time };
            s.running = run;
            Err(from_range(&e, ctx).with_label(UNKNOWN_TRANSACTION_COMMIT_RESULT))
        }
    }
}

/// Rounds of resolution a finished transaction gets after its answer before the keeper takes it.
const RESOLVE_ROUNDS: u32 = 4;

/// Resolves the intents of the transactions that answered their commit or abort (FR-005): the
/// worker that answered runs it after the answer went out when the node has no resolver threads,
/// and the keeper for those it left.
pub fn resolve_queued(backend: &mut dyn Backend, shared: &Shared) {
    // Every command ends here: only a queue with coordinators takes the node's lock (H-013).
    let flag = &super::hot::of(shared).txn_resolving;
    if !flag.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let queued = {
        let mut g = lock(shared);
        flag.store(false, std::sync::atomic::Ordering::SeqCst);
        std::mem::take(&mut g.txn_resolving)
    };
    for coord in queued {
        resolve_coordinator(backend, shared, coord);
    }
}

/// Hands coordinator `coord` of a transaction that ended to the threads that resolve the intents
/// (008 FR-005): the caller answers its client without waiting for the resolution.
pub fn hand_to_resolvers(shared: &Shared, coord: teseladb_txn::coordinator::Coordinator) {
    queue_resolution(&mut lock(shared), coord);
}

fn queue_resolution(g: &mut super::NodeSessions, coord: teseladb_txn::coordinator::Coordinator) {
    g.txn_resolving.push(coord);
    g.hot.txn_resolving.store(true, std::sync::atomic::Ordering::SeqCst);
    g.hot.wake_resolvers();
}

/// Resolves one transaction that answered its commit or abort, if one waits (FR-005): what each
/// of the node's resolver threads runs, so that the next command of the connection that ended it
/// does not wait for its resolution (H-050 of the native tests). `false` when none waited.
pub fn resolve_one(backend: &mut dyn Backend, shared: &Shared) -> bool {
    let flag = &super::hot::of(shared).txn_resolving;
    if !flag.load(std::sync::atomic::Ordering::SeqCst) {
        return false;
    }
    let coord = {
        let mut g = lock(shared);
        let coord = g.txn_resolving.pop();
        if g.txn_resolving.is_empty() {
            flag.store(false, std::sync::atomic::Ordering::SeqCst);
        }
        coord
    };
    let Some(coord) = coord else { return false };
    resolve_coordinator(backend, shared, coord);
    true
}

/// A few rounds of resolution of `coord`; one that is not done goes back to the queue, for the
/// keeper or a resolver.
fn resolve_coordinator(
    backend: &mut dyn Backend,
    shared: &Shared,
    mut coord: teseladb_txn::coordinator::Coordinator,
) {
    for _ in 0..RESOLVE_ROUNDS {
        let mut requests = coord.take_requests();
        if requests.is_empty() {
            coord.tick(&Via(&*backend), nanos(backend), backend.wall_ms());
            requests = coord.take_requests();
        }
        for r in requests {
            let answer = send(backend, r.g, r.op);
            coord.on_reply(&Via(&*backend), r.id, answer);
        }
        if coord.take_events().contains(&Event::Resolved) {
            return;
        }
    }
    let mut g = lock(shared);
    g.txn_resolving.push(coord);
    g.hot.txn_resolving.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Ticks the transactions no command has, so that their heartbeats go (008 FR-007), and aborts
/// those older than `transactionLifetimeLimitSeconds` (FR-010); the node's keeper calls it every
/// second.
pub fn tick_idle(backend: &mut dyn Backend, shared: &Shared) {
    let idle: Vec<u128> = lock(shared)
        .txns
        .iter()
        .filter(|(_, s)| !s.in_use && s.running.is_some())
        .map(|(l, _)| *l)
        .collect();
    for lsid in idle {
        let taken = {
            let mut g = lock(shared);
            let s = g.txns.entry(lsid).or_default();
            s.in_use = s.running.is_some();
            s.running.take()
        };
        let Some(mut run) = taken else { continue };
        let limit_ms = lock(shared).params.transaction_lifetime_secs.saturating_mul(1000);
        let age = backend.now_ms().saturating_sub(run.started_ms);
        if age > u64::try_from(limit_ms).unwrap_or(u64::MAX) {
            // Too old: aborted; its next statement finds no transaction.
            let number = run.txn_number;
            end_abandoned(backend, shared, run);
            let mut g = lock(shared);
            let s = g.txns.entry(lsid).or_default();
            s.rules.aborted(number);
            s.reaped = Some(number);
            s.in_use = false;
            continue;
        }
        let (now, wall) = (nanos(backend), backend.wall_ms());
        keep_locks(backend, &mut run);
        run.coord.tick(&Via(&*backend), now, wall);
        for r in run.coord.take_requests() {
            let answer = send(backend, r.g, r.op);
            run.coord.on_reply(&Via(&*backend), r.id, answer);
        }
        store(shared, lsid, run);
    }
    resolve_queued(backend, shared);
    txn_ddl::sweep(backend, shared);
}

impl Backend for TxnBackend<'_> {
    fn collection(&mut self, ns: &Namespace) -> Result<Option<CollectionEntry>, CommandError> {
        match txn_ddl::find(&self.run.created, ns) {
            Some(c) => Ok(Some(c.entry.clone())),
            None => self.inner.collection(ns),
        }
    }

    fn collections(&mut self) -> Result<Vec<(Namespace, CollectionEntry)>, CommandError> {
        self.inner.collections()
    }

    fn databases(&mut self) -> Result<Vec<String>, CommandError> {
        self.inner.databases()
    }

    fn database_mark(&mut self, db: &str) -> Result<Option<Hlc>, CommandError> {
        self.inner.database_mark(db)
    }

    fn catalog_write(
        &mut self,
        cmd: CatalogCommand,
    ) -> Result<Option<CollectionEntry>, CommandError> {
        let txn = self.run.coord.txn();
        txn_ddl::catalog_write(self.inner, self.shared, (txn, &mut self.run.created), cmd)
    }

    fn indexes(&mut self, collection: u32) -> Result<Vec<IndexEntry>, CommandError> {
        self.inner.indexes(collection)
    }

    fn search_indexes(
        &mut self,
        collection: u32,
    ) -> Result<Vec<teseladb_meta::indexes::SearchIndexEntry>, CommandError> {
        self.inner.search_indexes(collection)
    }

    fn voters(&mut self, start: &[u8], end: &[u8]) -> Result<usize, CommandError> {
        self.inner.voters(start, end)
    }

    fn read_versions(
        &mut self,
        keys: &[Vec<u8>],
    ) -> Result<crate::commands::Versions, CommandError> {
        let (at, view, window) = (self.run.read_ts(), self.run.view.clone(), self.run.uncertain_to);
        for k in keys {
            self.contact(k);
        }
        let values =
            self.inner.txn_gets(keys, at, &view, window).map_err(|e| self.run.note_uncertain(e))?;
        Ok((at, self.over_values(keys, values)))
    }

    fn transact(
        &mut self,
        read_ts: teseladb_hlc::Hlc,
        writes: Vec<(Vec<u8>, teseladb_range::intents::IntentOp)>,
        _txn: Option<teseladb_range::intents::TxnId>,
    ) -> Result<teseladb_hlc::Hlc, CommandError> {
        // The writes go in the client's transaction, which reads at its own mark.
        self.statement(writes)?;
        Ok(read_ts)
    }

    fn note_plan(&mut self, plan: Document) {
        self.inner.note_plan(plan);
    }

    fn note_plan_lazy(&mut self, plan: crate::ops::PendingPlan) {
        self.inner.note_plan_lazy(plan);
    }

    fn not_examined(&mut self, docs: u64) {
        self.inner.not_examined(docs);
    }

    fn op_reads(&mut self) -> crate::ops::OpReads {
        self.inner.op_reads()
    }

    fn op_client(&mut self) -> Option<Document> {
        self.inner.op_client()
    }

    fn op_plan(&mut self) -> Option<Document> {
        self.inner.op_plan()
    }

    fn thread_cpu_nanos(&mut self) -> Option<u64> {
        self.inner.thread_cpu_nanos()
    }

    fn end_operation(&mut self) {
        self.inner.end_operation();
    }

    fn fence(&mut self, start: &[u8], end: &[u8], on: bool) -> Result<Hlc, CommandError> {
        self.inner.fence(start, end, on)
    }

    fn changes(
        &mut self,
        start: &[u8],
        end: &[u8],
        from: Hlc,
    ) -> Result<super::ChangeBatch, CommandError> {
        self.inner.changes(start, end, from)
    }

    fn catalog_changes(&mut self, from: Hlc) -> Result<super::DdlBatch, CommandError> {
        self.inner.catalog_changes(from)
    }

    fn range_changes(
        &mut self,
        start: &[u8],
        end: &[u8],
        from: Hlc,
        at: Option<Hlc>,
    ) -> Result<Vec<super::RangeBatch>, CommandError> {
        self.inner.range_changes(start, end, from, at)
    }

    fn txn_route(&self, key: &[u8]) -> Option<teseladb_types::GroupId> {
        self.inner.txn_route(key)
    }

    fn txn_ranges(&self) -> Vec<(teseladb_types::GroupId, Vec<u8>)> {
        self.inner.txn_ranges()
    }

    fn set_read_preference(&mut self, pref: Option<teseladb_router::read_pref::ReadPreference>) {
        self.inner.set_read_preference(pref);
    }

    fn txn_memory_budget(&self) -> usize {
        self.inner.txn_memory_budget()
    }

    fn cluster_mark(&self) -> Hlc {
        self.inner.cluster_mark()
    }

    fn set_read_mark(&mut self, at: Option<teseladb_txn::causal::ReadMark>) {
        self.inner.set_read_mark(at);
    }

    fn read_mark(&self) -> Option<teseladb_txn::causal::ReadMark> {
        self.inner.read_mark()
    }

    fn txn_pause(&mut self) -> bool {
        self.inner.txn_pause()
    }

    fn txn_wait_clock(&mut self, until_ms: u64) -> bool {
        self.inner.txn_wait_clock(until_ms)
    }

    fn txn_send(
        &mut self,
        g: teseladb_types::GroupId,
        op: teseladb_range::host::TxnOp,
    ) -> Result<teseladb_range::host::Reply, teseladb_range::host::RangeError> {
        self.run.requests += 1;
        self.inner.txn_send(g, op)
    }

    fn txn_get(
        &mut self,
        key: &[u8],
        at: teseladb_hlc::Hlc,
        view: &teseladb_range::push::TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Option<(u8, Vec<u8>)>, CommandError> {
        self.contact(key);
        let value = self.inner.txn_get(key, at, view, uncertain_to)?;
        let value = self.over_values(&[key.to_vec()], vec![value]).pop().flatten();
        // Its own intents are stored documents too: read as their BSON (003 FR-021).
        Ok(value.map(|(t, v)| match t == super::mutation::DOCUMENT {
            true => (t, teseladb_keys::stored::document_value(key, v)),
            false => (t, v),
        }))
    }

    fn txn_scan(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        at: teseladb_hlc::Hlc,
        view: &teseladb_range::push::TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        self.contact(start);
        self.scan_over((start, end), limit, (at, view, uncertain_to), false)
            .map(teseladb_keys::stored::document_rows)
    }

    fn txn_scan_back(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        at: teseladb_hlc::Hlc,
        view: &teseladb_range::push::TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        self.contact(start);
        self.scan_over((start, end), limit, (at, view, uncertain_to), true)
            .map(teseladb_keys::stored::document_rows)
    }

    fn index_write(&mut self, cmd: IndexCommand) -> Result<(), CommandError> {
        self.inner.index_write(cmd)
    }

    fn write_gate(&mut self) -> Result<super::write_block::WriteGate, CommandError> {
        self.inner.write_gate()
    }

    fn gate_everywhere(
        &mut self,
        want: super::write_block::WriteGate,
    ) -> Result<bool, CommandError> {
        self.inner.gate_everywhere(want)
    }

    fn tenant(&self) -> u32 {
        self.inner.tenant()
    }

    fn tenant_named(&mut self, name: &str) -> Result<Option<u32>, CommandError> {
        self.inner.tenant_named(name)
    }

    fn tenant_state(&mut self, tenant: u32) -> Option<teseladb_meta::tenants::TenantState> {
        self.inner.tenant_state(tenant)
    }

    fn tenant_begin(
        &mut self,
        tenant: u32,
        user: &teseladb_auth::users::UserName,
        bytes: u64,
        grows: bool,
    ) -> Result<Option<u64>, CommandError> {
        self.inner.tenant_begin(tenant, user, bytes, grows)
    }

    fn tenant_end(&mut self, tenant: u32) {
        self.inner.tenant_end(tenant);
    }

    fn tenant_name(&mut self, tenant: u32) -> Option<String> {
        self.inner.tenant_name(tenant)
    }

    /// A transaction's writes are its own intents: none is answered outside it.
    fn write_unchanged(&mut self, _op: &WriteOp) -> Option<Vec<u8>> {
        None
    }

    fn write(&mut self, op: WriteOp) -> Result<Vec<u8>, CommandError> {
        let bound = Bound::decode(&op.mutation)?;
        if bound.rules().capped.is_some() {
            return Err(super::capped::in_transaction(&bound));
        }
        let (_, writes, result) = cross_range::evaluate(self, &bound)?;
        self.statement(writes)?;
        Ok(result)
    }

    fn scan(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        _concern: ReadConcern,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        // Inside the transaction: at its read mark, with its own intents and held writes.
        self.contact(start);
        let (at, view, window) = (self.run.read_ts(), self.run.view.clone(), self.run.uncertain_to);
        self.scan_over((start, end), limit, (at, &view, window), false)
            .map(teseladb_keys::stored::document_rows)
            .map_err(|e| self.run.note_uncertain(e))
    }

    fn scan_back(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        _concern: ReadConcern,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        // As `scan`, from the last key down (H-003).
        self.contact(start);
        let (at, view, window) = (self.run.read_ts(), self.run.view.clone(), self.run.uncertain_to);
        self.scan_over((start, end), limit, (at, &view, window), true)
            .map(teseladb_keys::stored::document_rows)
            .map_err(|e| self.run.note_uncertain(e))
    }

    fn points(
        &mut self,
        keys: &[Vec<u8>],
        _concern: ReadConcern,
    ) -> Result<Vec<Option<Vec<u8>>>, CommandError> {
        // As `scan` reads each one, at the transaction's mark, all of them together.
        for k in keys {
            self.contact(k);
        }
        let (at, view, window) = (self.run.read_ts(), self.run.view.clone(), self.run.uncertain_to);
        let docs = self
            .inner
            .txn_points(keys, at, &view, window)
            .map_err(|e| self.run.note_uncertain(e))?;
        let docs = match &self.run.buffer {
            Some(buffer) => buffer.over_points(keys, docs),
            None => docs,
        };
        Ok(keys
            .iter()
            .zip(docs)
            .map(|(k, v)| v.map(|v| teseladb_keys::stored::document_value(k, v)))
            .collect())
    }

    fn kill_everywhere(&mut self, scope: &KillScope) -> Result<(), CommandError> {
        self.inner.kill_everywhere(scope)
    }

    fn other_operations(&mut self) -> Result<Vec<Document>, CommandError> {
        self.inner.other_operations()
    }

    fn config_documents(&mut self, coll: &str) -> Result<Vec<Document>, CommandError> {
        self.inner.config_documents(coll)
    }

    fn route_entries(&mut self) -> Result<Vec<teseladb_router::table::RouteEntry>, CommandError> {
        self.inner.route_entries()
    }

    fn range_live_bytes(
        &mut self,
        e: &teseladb_router::table::RouteEntry,
    ) -> Result<u64, CommandError> {
        self.inner.range_live_bytes(e)
    }

    fn range_order(
        &mut self,
        cmd: teseladb_meta::command::Group0Command,
    ) -> Result<(), CommandError> {
        self.inner.range_order(cmd)
    }

    fn config_settings_write(
        &mut self,
        writes: Vec<super::config_settings::SettingsWrite>,
    ) -> Result<bool, CommandError> {
        self.inner.config_settings_write(writes)
    }

    fn group0_settings(
        &mut self,
        prefix: &[u8],
    ) -> Result<std::collections::BTreeMap<Vec<u8>, Vec<u8>>, CommandError> {
        self.inner.group0_settings(prefix)
    }

    fn group0_settings_write(
        &mut self,
        changes: Vec<teseladb_meta::command::SettingChange>,
    ) -> Result<bool, CommandError> {
        self.inner.group0_settings_write(changes)
    }

    fn request_shutdown(&mut self) -> Result<(), CommandError> {
        self.inner.request_shutdown()
    }

    fn compact(&mut self) -> Result<u64, CommandError> {
        self.inner.compact()
    }

    fn cluster_parameters(&mut self) -> Result<BTreeMap<String, Vec<u8>>, CommandError> {
        self.inner.cluster_parameters()
    }

    fn set_cluster_parameter(&mut self, name: &str, value: Vec<u8>) -> Result<(), CommandError> {
        self.inner.set_cluster_parameter(name, value)
    }

    fn now_ms(&self) -> u64 {
        self.inner.now_ms()
    }

    fn wall_ms(&self) -> u64 {
        self.inner.wall_ms()
    }

    fn now_micros(&self) -> u64 {
        self.inner.now_micros()
    }

    fn random(&mut self) -> u64 {
        self.inner.random()
    }

    fn node(&self) -> u64 {
        self.inner.node()
    }

    fn principal(
        &mut self,
        kind: Principal,
        db: &str,
        name: &str,
    ) -> Result<Option<Vec<u8>>, CommandError> {
        self.inner.principal(kind, db, name)
    }

    fn principals(
        &mut self,
        kind: Principal,
        db: Option<&str>,
    ) -> Result<Vec<Vec<u8>>, CommandError> {
        self.inner.principals(kind, db)
    }

    fn principal_by_id(
        &mut self,
        kind: Principal,
        id: &[u8],
    ) -> Result<Option<Vec<u8>>, CommandError> {
        self.inner.principal_by_id(kind, id)
    }

    fn security_write(
        &mut self,
        ops: Vec<SecurityOp>,
    ) -> Result<Result<(), SecurityError>, CommandError> {
        self.inner.security_write(ops)
    }

    fn secure_random(&mut self, out: &mut [u8]) -> Result<(), CommandError> {
        self.inner.secure_random(out)
    }

    fn authenticated_user(&self) -> Option<UserName> {
        self.inner.authenticated_user()
    }

    fn authorization_lease(&self) -> bool {
        self.inner.authorization_lease()
    }

    fn privileges(&self) -> Option<Vec<Privilege>> {
        self.inner.privileges()
    }

    fn invalidate_user_cache(&mut self) {
        self.inner.invalidate_user_cache();
    }

    fn authenticated_roles(&self) -> Option<Vec<RoleName>> {
        self.inner.authenticated_roles()
    }

    fn host_port(&mut self) -> String {
        self.inner.host_port()
    }

    fn process(&mut self) -> crate::commands::diagnostics::ProcessFacts {
        self.inner.process()
    }

    fn log_slow_query(&mut self, attr: &teseladb_bson::Document) {
        self.inner.log_slow_query(attr)
    }

    fn log_warning(&mut self, id: i32, msg: &str, attr: &teseladb_bson::Document) {
        self.inner.log_warning(id, msg, attr);
    }

    fn rotate_certificates(&mut self, message: Option<&str>) -> Result<(), CommandError> {
        self.inner.rotate_certificates(message)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::commands::{NodeSessions, Timeouts, txn_stats};
    use teseladb_range::intents::TxnId;

    /// A session's messages name its user as `refrouter` 9.0.4 does: the base64 of the SHA-256 of
    /// `user@db` (measured on 03-11-2026 for `compat@admin`), the empty name without a login.
    #[test]
    fn session_messages_name_the_logged_in_user() {
        let user = UserName { user: "compat".into(), db: "admin".into() };
        let uid = user_uid(Some(&user));
        assert_eq!(uid, "DHddVc+utV+nsjzG+R/hGWwKoCO1ATGo0Gttjtu7a+Q=");
        assert_eq!(user_uid(None), ANONYMOUS_UID);
        let e = CommandError::new(NO_SUCH_TRANSACTION, format!("txnId 1 for {}", session_name(7)));
        let named = name_the_user(e, &uid);
        assert!(named.errmsg.contains(&uid) && !named.errmsg.contains(ANONYMOUS_UID), "{named:?}");
        let anonymous = CommandError::new(NO_SUCH_TRANSACTION, session_name(7));
        assert_eq!(name_the_user(anonymous.clone(), ANONYMOUS_UID).errmsg, anonymous.errmsg);
    }

    /// A session whose transaction a command runs counts as open and active; the coordinators that
    /// still resolve count their intents (FR-019).
    #[test]
    fn the_gauges_count_sessions_and_unresolved_intents() {
        let mut g = NodeSessions::new(Timeouts::default());
        g.txns.insert(1, SessionTxns { in_use: true, ..SessionTxns::default() });
        g.txns.insert(2, SessionTxns::default());
        let route = |_: &[u8]| Some(GroupId(1));
        struct One<F: Fn(&[u8]) -> Option<GroupId>>(F);
        impl<F: Fn(&[u8]) -> Option<GroupId>> Route for One<F> {
            fn range_of(&self, key: &[u8]) -> Option<GroupId> {
                (self.0)(key)
            }
        }
        let mut c =
            Coordinator::new(TxnId(3), (3, 1), Hlc { physical: 1, logical: 0 }, Nanos::ZERO);
        let put = IntentOp::Put { value_type: 1, value: vec![1] };
        assert!(c.write(&One(route), vec![(b"a".to_vec(), put)]).is_ok());
        g.txn_resolving.push(c);
        txn_stats::pushed(&mut g);
        let s = txn_stats::status(&g);
        assert_eq!((s.txn.open, s.txn.active, s.txn.pushes), (1, 1, 1));
        assert!(SessionTxns::default().is_open().eq(&false));
        assert_eq!(s.txn.unresolved_intents, 0, "nothing to resolve before it ended");
    }
}
