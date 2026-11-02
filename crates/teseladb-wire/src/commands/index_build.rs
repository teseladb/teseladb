//! Online index builds (009 FR-006, T012, `spec/tla/OnlineIndexBuild.tla`). An index is created
//! delete-only, then made write-only (`building`); Group 0 answers each of those changes only once
//! no node still runs a write that read the catalog before it (the barrier, which the server
//! keeps with the authorization fence). Then the backfill writes, chunk by chunk, the entries of
//! the documents' current versions, each chunk in one range write that waits for the
//! `commitQuorum`; `currentOp` shows its progress and `killOp` stops it. Replies and errors as a
//! `refnode` and a `refrouter` 9.0.4 give them (measured).

use teseladb_bson::{Document, Value};
use teseladb_index::build::{BUILD_DESC, CommitQuorum, MAX_COMMIT_QUORUM, Progress};
use teseladb_meta::catalog::CollectionEntry;
use teseladb_meta::indexes::{IndexCommand, IndexEntry, IndexState};
use teseladb_range::ack::W;
use teseladb_range::read::ReadConcern;

use super::crud::{MODIFY_CHUNK_KEYS, Writer};
use super::mutation::{Bound, DocMutation, Rules};
use super::query::documents_of;
use super::{Backend, Shared, lock};
use crate::error::{
    CommandError, DUPLICATE_KEY, FAILED_TO_PARSE, INTERNAL_ERROR, INTERRUPTED,
    UNKNOWN_REPL_WRITE_CONCERN,
};
use crate::ops::{INTERRUPTED_MESSAGE, Op, op_type};

/// The named modes of `commitQuorum`.
const VOTING_MEMBERS: &str = "votingMembers";
const MAJORITY: &str = "majority";
/// The field of the command.
const COMMIT_QUORUM: &str = "commitQuorum";
/// `UnsatisfiableCommitQuorum`.
pub const UNSATISFIABLE_COMMIT_QUORUM: crate::error::Code =
    crate::error::Code { code: 278, name: "UnsatisfiableCommitQuorum" };
/// The message of a quorum above the voters (measured on 9.0.4).
const NOT_ENOUGH: &str = "Not enough data-bearing nodes to satisfy commit quorum";

/// A `commitQuorum`: what the reply shows and what the backfill waits for.
#[derive(Clone, Debug, PartialEq)]
pub struct Quorum {
    /// As the reply shows it.
    pub shown: Value,
    /// What it asks of each range.
    pub asked: CommitQuorum,
}

impl Default for Quorum {
    fn default() -> Quorum {
        Quorum { shown: Value::from(VOTING_MEMBERS), asked: CommitQuorum::VotingMembers }
    }
}

/// A number of `commitQuorum` as RefDb reads it: a double truncated, a decimal rounded half to
/// even, NaN as 0.
fn quorum_number(v: &Value) -> Option<i64> {
    match v {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) => Some(*n),
        // `as` truncates toward zero, saturates, and takes NaN to 0.
        Value::Double(x) => Some(*x as i64),
        Value::Decimal128(d) => Some(teseladb_mql::decimal_saturating(*d)),
        _ => None,
    }
}

/// The command's `commitQuorum` (measured on a `refnode` and a `refrouter` 9.0.4).
pub fn commit_quorum(cmd: &Document) -> Result<Quorum, CommandError> {
    let v = match cmd.get(COMMIT_QUORUM) {
        None | Some(Value::Null) => return Ok(Quorum::default()),
        Some(v) => v,
    };
    if let Some(n) = quorum_number(v) {
        let Some(n) = u32::try_from(n).ok().filter(|n| *n <= MAX_COMMIT_QUORUM) else {
            let msg = format!(
                "commitQuorum has to be a non-negative number and not greater than \
                 {MAX_COMMIT_QUORUM}"
            );
            return Err(CommandError::new(FAILED_TO_PARSE, msg));
        };
        // A quorum of 0 shows as 1 (measured).
        let shown = Value::Int32(i32::try_from(n.max(1)).unwrap_or(i32::MAX));
        return Ok(Quorum { shown, asked: CommitQuorum::Members(n) });
    }
    match v {
        Value::String(s) if s.is_empty() => {
            Err(CommandError::new(FAILED_TO_PARSE, "commitQuorum can't be an empty string"))
        }
        Value::String(s) if s == VOTING_MEMBERS => Ok(Quorum::default()),
        Value::String(s) if s == MAJORITY => {
            Ok(Quorum { shown: Value::from(MAJORITY), asked: CommitQuorum::Majority })
        }
        Value::String(s) => {
            let msg =
                format!("No write concern mode named '{s}' found in replica set configuration");
            Err(CommandError::new(UNKNOWN_REPL_WRITE_CONCERN, msg))
        }
        other => Err(crate::types::wrong_types(
            &format!("createIndexes.{COMMIT_QUORUM}"),
            other,
            &["long", "double", "decimal", "int", "string"],
        )),
    }
}

/// The write concern of a backfill write on ranges with `voters` voters: the range's majority,
/// or every replica the quorum asks beyond it.
fn concern(asked: CommitQuorum, voters: usize) -> Result<W, CommandError> {
    let replicas = asked
        .replicas(voters)
        .map_err(|_| CommandError::new(UNSATISFIABLE_COMMIT_QUORUM, NOT_ENOUGH))?;
    Ok(match replicas <= voters / 2 + 1 {
        true => W::Majority,
        false => W::Nodes(u32::try_from(replicas).unwrap_or(u32::MAX)),
    })
}

/// Refuses a quorum the ranges of `coll` cannot meet (`UnsatisfiableCommitQuorum`).
pub fn check_quorum<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
    asked: CommitQuorum,
) -> Result<(), CommandError> {
    let (start, end) = documents_of(coll);
    concern(asked, backend.voters(&start, &end)?).map(drop)
}

/// Creates the index `cmd` (a [`IndexCommand::Create`]) and makes it write-only: each change
/// answers once the barrier passed it. The index as it is then.
pub fn start<B: Backend + ?Sized>(
    backend: &mut B,
    collection: u32,
    cmd: IndexCommand,
) -> Result<IndexEntry, CommandError> {
    let IndexCommand::Create { name, .. } = &cmd else {
        return Err(CommandError::new(INTERNAL_ERROR, "an index build starts with its creation"));
    };
    let name = name.clone();
    backend.index_write(cmd)?;
    let entry = backend
        .indexes(collection)?
        .into_iter()
        .find(|e| e.name == name)
        .ok_or_else(|| CommandError::new(INTERNAL_ERROR, "the index was not created"))?;
    backend.index_write(IndexCommand::SetState { id: entry.id, state: IndexState::Building })?;
    Ok(IndexEntry { state: IndexState::Building, ..entry })
}

/// The operation of a build in the node's table, which `currentOp` lists and `killOp` marks.
pub struct BuildOp<'a> {
    shared: &'a Shared,
    id: u32,
}

impl<'a> BuildOp<'a> {
    /// Lists the build of `specs` (as stored) for `cmd` on namespace `ns`, from `now_ms`.
    pub fn begin(
        shared: &'a Shared,
        ns: String,
        cmd: &Document,
        specs: Vec<Document>,
        now_ms: u64,
    ) -> BuildOp<'a> {
        let mut command = Document::new();
        for (k, v) in cmd.iter() {
            match k {
                "indexes" => command
                    .push(k, Value::Array(specs.iter().cloned().map(Value::Document).collect())),
                _ => command.push(k, v.clone()),
            }
        }
        let op = Op {
            conn: 0,
            client: None,
            metadata: None,
            app_name: None,
            op: op_type(super::indexes::CREATE_INDEXES),
            ns,
            command,
            started_ms: now_ms,
            deadline_ms: None,
            killed: false,
            user: None,
            tenant: 0,
            privileges: None,
            roles: None,
            desc: None,
            // Known once the backfill counted the documents.
            progress: None,
            metrics: None,
        };
        let id = lock(shared).ops.begin_named(op, |id| format!("{BUILD_DESC}{id}"));
        BuildOp { shared, id }
    }

    /// Records how far the backfill went; whether `killOp` marked the build.
    fn report(&self, p: Progress) -> bool {
        lock(self.shared).ops.get_mut(self.id).is_some_and(|o| {
            o.progress = Some(p);
            o.killed
        })
    }
}

impl Drop for BuildOp<'_> {
    fn drop(&mut self) {
        lock(self.shared).ops.end(self.id);
    }
}

/// How a backfill runs: the quorum its writes wait for and the operation that shows it.
#[derive(Default)]
pub struct Backfill<'a> {
    /// What each range must hold before the index is ready.
    pub asked: Option<CommitQuorum>,
    /// The build's operation, if `currentOp` lists it.
    pub op: Option<BuildOp<'a>>,
}

/// `cause` of a scan that stopped after `done` documents at the `at`-th (measured on a `refnode`
/// and a `refrouter` 9.0.4; the build's prefix comes from the caller).
fn scan_stopped(cause: CommandError, done: u64, at: u64, ms: u64) -> CommandError {
    let msg = format!(
        "collection scan stopped. totalRecords: {done}; durationMillis: {ms}ms; phase: \
         collection scan; collectionScanPosition: RecordId({at}); readSource: \
         kMajorityCommitted :: caused by :: {}",
        cause.errmsg
    );
    CommandError { errmsg: msg, ..cause }
}

/// Why a backfill stopped.
#[derive(Clone, Debug, PartialEq)]
pub enum Stopped {
    /// The build failed: a duplicate in a unique index, a document an index refuses, `killOp`.
    /// RefDb's message of a failed build goes around it.
    Build(CommandError),
    /// The node could not go on (no leader, a timeout).
    Node(CommandError),
}

impl Stopped {
    /// The error, whichever it is.
    pub fn error(self) -> CommandError {
        match self {
            Stopped::Build(e) | Stopped::Node(e) => e,
        }
    }
}

impl From<CommandError> for Stopped {
    fn from(e: CommandError) -> Stopped {
        Stopped::Node(e)
    }
}

/// The documents of `coll` now.
fn count<B: Backend + ?Sized>(
    backend: &mut B,
    coll: &CollectionEntry,
) -> Result<u64, CommandError> {
    let (mut from, end) = documents_of(coll);
    let mut total = 0u64;
    loop {
        let rows = backend.scan(&from, &end, MODIFY_CHUNK_KEYS, ReadConcern::Local)?;
        let Some((last, _)) = rows.last() else { return Ok(total) };
        from = [last.as_slice(), &[0]].concat();
        total += rows.len() as u64;
    }
}

impl Backfill<'_> {
    /// Writes the entries of the indexes of `rules` for every document of `coll`.
    pub fn run<B: Backend + ?Sized>(
        &self,
        backend: &mut B,
        coll: &CollectionEntry,
        rules: &Rules,
    ) -> Result<(), Stopped> {
        // How long the scan ran, in cluster time (the logic reads no clock of its own).
        let started = backend.now_ms();
        let took = |b: &B| b.now_ms().saturating_sub(started);
        let mut progress =
            Progress { done: 0, total: if self.op.is_some() { count(backend, coll)? } else { 0 } };
        let (mut from, end) = documents_of(coll);
        let mut w = Writer::internal();
        loop {
            if self.op.as_ref().is_some_and(|op| op.report(progress)) {
                let cause = CommandError::new(INTERRUPTED, INTERRUPTED_MESSAGE);
                let done = progress.done;
                return Err(Stopped::Build(scan_stopped(cause, done, done, took(backend))));
            }
            let rows = backend.scan(&from, &end, MODIFY_CHUNK_KEYS, ReadConcern::Local)?;
            let Some((last, _)) = rows.last() else { return Ok(()) };
            let next = [last.as_slice(), &[0]].concat();
            if let Some(asked) = self.asked {
                w.wc.w = concern(asked, backend.voters(&from, &next)?)?;
            }
            let keys: Vec<Vec<u8>> = rows.into_iter().map(|(k, _)| k).collect();
            let n = keys.len() as u64;
            let mutation = DocMutation::Backfill { rules: rules.clone() };
            let r = w.write(backend, &Bound { mutation, keys }, 0)?;
            if let Some(e) = r.errors.into_iter().next() {
                // A unique index finds its duplicates once the scan is over (measured).
                if e.error.code == DUPLICATE_KEY {
                    return Err(Stopped::Build(e.error));
                }
                let done = progress.done + e.index as u64;
                return Err(Stopped::Build(scan_stopped(e.error, done, done + 1, took(backend))));
            }
            progress.done += n;
            from = next;
        }
    }
}
