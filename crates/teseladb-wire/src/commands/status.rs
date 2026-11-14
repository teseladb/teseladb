//! What the node counts for `serverStatus` beyond `opcounters` (FR-023, 006 T046): latencies by
//! kind of operation, each command's totals, cursors, write concern waits, operations stopped by
//! their time limit, collection scans, API versions by application, the jobs of the sessions'
//! cache, authentications by mechanism and the transactions of 008. Shapes and BSON types are
//! those of a `refrouter` 9.0.4 (measured on 30-10-2026); what a section cannot have in TeselaDB is in
//! DV-038.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_bson::{Document, Value};

use crate::commands::crud::{BULK_WRITE, DELETE, FIND_AND_MODIFY, INSERT, UPDATE};
use crate::commands::query::{COUNT, DISTINCT, FIND, GET_MORE};

/// `aggregate`, which 010 answers.
pub const AGGREGATE: &str = "aggregate";
/// What `metrics.apiVersions` calls a client that sent no `appName`.
pub const NO_APP_NAME: &str = "";
/// The API version of a command without `apiVersion`.
pub const DEFAULT_API_VERSION: &str = "default";
/// The key of `metrics.commands` for the commands the node does not know.
pub const UNKNOWN_COMMAND: &str = "<UNKNOWN>";

/// The kind of an operation in `opLatencies` (measured: each command counts in one kind only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    /// `find`, `getMore`, `count`, `distinct`, `aggregate`.
    Read,
    /// `insert`, `update`, `delete`, `findAndModify`, `bulkWrite`.
    Write,
    /// Every other command.
    Command,
    /// A whole transaction (008).
    Transaction,
}

impl OpKind {
    /// The kind of command `name`.
    pub fn of(name: &str) -> OpKind {
        match name {
            FIND | GET_MORE | COUNT | DISTINCT | AGGREGATE => OpKind::Read,
            INSERT | UPDATE | DELETE | FIND_AND_MODIFY | BULK_WRITE => OpKind::Write,
            _ => OpKind::Command,
        }
    }
}

/// Time and number of the operations of one kind.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Latency {
    /// Microseconds, summed.
    pub micros: i64,
    /// Operations.
    pub ops: i64,
}

/// What `$collStats` reports of one collection since the node started: latencies by
/// [`OpKind`] with their histograms, and the reads that scanned the whole collection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CollectionCounters {
    /// By [`OpKind`]: reads, writes, commands, transactions.
    pub latency: [Latency; 4],
    /// By [`OpKind`]: operations by the lower bound of their bucket, in microseconds.
    pub histograms: [BTreeMap<i64, i64>; 4],
    /// Reads that scanned the whole collection.
    pub collection_scans: i64,
}

/// The lower bound of the bucket of a latency of `micros`, as RefDb's histograms have them:
/// powers of two below 1024 µs, then each power of two split in two (`2^k` and `1.5 × 2^k`).
pub fn latency_bucket(micros: i64) -> i64 {
    if micros < 2 {
        return 0;
    }
    let p = 1i64 << (63 - micros.leading_zeros());
    if p < 1024 || micros < p + p / 2 { p } else { p + p / 2 }
}

/// Counts of one command in `metrics.commands`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommandCount {
    /// Times it ran.
    pub total: i64,
    /// Times it failed.
    pub failed: i64,
    /// Statements with `arrayFilters` (`update`, `findAndModify`, `bulkWrite`).
    pub array_filters: i64,
    /// Statements whose update is a pipeline.
    pub pipeline: i64,
    /// Commands with a `validator` (`create`, `collMod`), and those with `$jsonSchema`.
    pub validator: i64,
    /// See `validator`.
    pub json_schema: i64,
    /// `validationLevel` given, by level: `constraint`, `moderate`, `off`, `strict`.
    pub levels: BTreeMap<String, i64>,
}

/// The jobs of the sessions' cache (`logicalSessionRecordCache`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionJobs {
    /// Refreshes of `config.system.sessions`.
    pub refreshes: i64,
    /// Duration of the last one, in milliseconds.
    pub refresh_ms: i64,
    /// When it started, in milliseconds since the Unix epoch.
    pub refresh_at: i64,
    /// When it ended.
    pub refresh_done: i64,
    /// Records written by it.
    pub refreshed: i64,
    /// Records it removed.
    pub ended: i64,
    /// Reapings of idle sessions and cursors.
    pub reapings: i64,
    /// When the last one ran.
    pub reap_at: i64,
    /// Cursors it closed.
    pub cursors_closed: i64,
    /// Sessions it ended.
    pub sessions_reaped: i64,
}

/// Authentications of one mechanism, as the server's side counts them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MechanismCount {
    /// `speculativeAuthenticate` in `hello`: tried and successful.
    pub speculative: (i64, i64),
    /// `authenticate`, `saslStart`: tried and successful.
    pub authenticate: (i64, i64),
}

/// The transactions of the node (008).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TxnCounts {
    /// Open now.
    pub open: i64,
    /// Running a command now.
    pub active: i64,
    /// Started.
    pub started: i64,
    /// Committed.
    pub committed: i64,
    /// Aborted.
    pub aborted: i64,
    /// Aborted, by cause: the name of the error that ended each one, `abort` for an
    /// `abortTransaction` (measured through a `refrouter` 9.0.4).
    pub abort_causes: BTreeMap<String, i64>,
    /// Transactions pushed by readers and writers that met their intents (008 FR-019).
    pub pushes: i64,
    /// Reads that met a version in their uncertainty window (008 FR-008, FR-019).
    pub uncertainty_restarts: i64,
    /// Intents of finished transactions that their coordinators have yet to resolve.
    pub unresolved_intents: i64,
    /// Ranges contacted, summed over the transactions.
    pub participants: i64,
    /// Ranges at commit, summed.
    pub participants_at_commit: i64,
    /// Requests sent to ranges.
    pub requests: i64,
    /// Commits by kind (`noShards`, `singleShard`, `singleWriteShard`, `readOnly`,
    /// `twoPhaseCommit`, `recoverWithToken`): started, successful, microseconds.
    pub commits: [(i64, i64, i64); 6],
    /// Internal transactions (005 `TooLarge`): started, retried, commits retried, succeeded.
    pub internal: [i64; 4],
}

/// The commit kinds of `transactions.commitTypes`, in their order.
pub const COMMIT_TYPES: [&str; 6] = [
    "noShards",
    "singleShard",
    "singleWriteShard",
    "readOnly",
    "twoPhaseCommit",
    "recoverWithToken",
];

/// The mechanisms of `security.authentication.mechanisms`, in their order, and whether the
/// node takes them from clients (the others only appear as `egress`).
pub const MECHANISMS: [(&str, bool); 7] = [
    ("GSSAPI", false),
    (teseladb_names::SASL_AWS, false),
    (teseladb_names::SASL_OIDC, false),
    (teseladb_names::SASL_X509, true),
    ("PLAIN", false),
    ("SCRAM-SHA-1", true),
    ("SCRAM-SHA-256", true),
];

/// The levels of `validationLevel` in `metrics.commands`, in their order.
const LEVELS: [&str; 5] = ["constraint", "default", "moderate", "off", "strict"];

/// Everything the node counts for `serverStatus` besides `opcounters`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StatusCounters {
    /// By [`OpKind`]: reads, writes, commands, transactions.
    pub latency: [Latency; 4],
    /// By command name.
    pub commands: BTreeMap<String, CommandCount>,
    /// Commands the node does not know.
    pub unknown: i64,
    /// Cursors closed for being idle.
    pub cursors_timed_out: i64,
    /// Waits for a write concern: number and milliseconds.
    pub wtime: (i64, i64),
    /// Write concerns that timed out.
    pub wtimeouts: i64,
    /// Operations stopped by their own `maxTimeMS`, and by `defaultMaxTimeMS`.
    pub killed_max_time: (i64, i64),
    /// Inserts that failed with a duplicate key.
    pub duplicate_inserts: i64,
    /// Reads that scanned a whole collection.
    pub collection_scans: i64,
    /// By collection (`db.coll`), for `$collStats`.
    pub collections: BTreeMap<String, CollectionCounters>,
    /// API versions used, by application name.
    pub api_versions: BTreeMap<String, BTreeSet<String>>,
    /// The sessions' cache.
    pub sessions: SessionJobs,
    /// `saslSupportedMechs` received in `hello`.
    pub sasl_supported_mechs: i64,
    /// Authentications by mechanism.
    pub mechanisms: BTreeMap<String, MechanismCount>,
    /// Microseconds spent authenticating clients.
    pub auth_micros: i64,
    /// Transactions (008).
    pub txn: TxnCounts,
}

fn long(n: i64) -> Value {
    Value::Int64(n)
}

fn int(n: i64) -> Value {
    Value::Int32(i32::try_from(n).unwrap_or(i32::MAX))
}

fn date(ms: i64) -> Value {
    Value::DateTime(ms)
}

fn kind_index(k: OpKind) -> usize {
    match k {
        OpKind::Read => 0,
        OpKind::Write => 1,
        OpKind::Command => 2,
        OpKind::Transaction => 3,
    }
}

fn level_of(cmd: &Document) -> Option<String> {
    match cmd.get("validationLevel") {
        Some(Value::String(l)) => Some(l.clone()),
        _ => None,
    }
}

fn has_json_schema(v: &Value) -> bool {
    matches!(v, Value::Document(d) if d.get("$jsonSchema").is_some())
}

/// The statements of a write command whose `arrayFilters` or pipeline update counts.
fn statements<'a>(name: &str, cmd: &'a Document) -> Vec<&'a Document> {
    let docs = |field: &str| match cmd.get(field) {
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| match v {
                Value::Document(d) => Some(d),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    match name {
        UPDATE => docs("updates"),
        BULK_WRITE => docs("ops"),
        FIND_AND_MODIFY => vec![cmd],
        _ => Vec::new(),
    }
}

impl StatusCounters {
    /// Counts command `name` on collection `ns` that took `micros`.
    pub fn collection_op(&mut self, ns: &str, name: &str, micros: u64) {
        let micros = i64::try_from(micros).unwrap_or(i64::MAX);
        let k = kind_index(OpKind::of(name));
        let c = self.collections.entry(ns.to_owned()).or_default();
        c.latency[k].micros = c.latency[k].micros.saturating_add(micros);
        c.latency[k].ops += 1;
        *c.histograms[k].entry(latency_bucket(micros)).or_default() += 1;
    }

    /// The `latencyStats` of `$collStats` for collection `ns`: commands, reads, writes and
    /// transactions, each with its histogram when `histograms`.
    pub fn collection_latency(&self, ns: &str, histograms: bool) -> Document {
        let c = self.collections.get(ns).cloned().unwrap_or_default();
        let mut d = Document::new();
        for (name, kind) in [
            ("commands", OpKind::Command),
            ("reads", OpKind::Read),
            ("writes", OpKind::Write),
            ("transactions", OpKind::Transaction),
        ] {
            let k = kind_index(kind);
            let mut e = Document::new();
            if histograms {
                let buckets: Vec<Value> = c.histograms[k]
                    .iter()
                    .map(|(micros, count)| {
                        Value::Document(
                            Document::new()
                                .with("micros", long(*micros))
                                .with("count", long(*count)),
                        )
                    })
                    .collect();
                e.push("histogram", buckets);
            }
            e.push("latency", long(c.latency[k].micros));
            e.push("ops", long(c.latency[k].ops));
            e.push("queryableEncryptionLatencyMicros", long(0));
            d.push(name, e);
        }
        d
    }

    /// The reads of the whole of collection `ns` since the node started.
    pub fn collection_scans_of(&self, ns: &str) -> i64 {
        self.collections.get(ns).map_or(0, |c| c.collection_scans)
    }

    /// Counts a read of the whole of collection `ns`.
    pub fn collection_scan(&mut self, ns: &str) {
        self.collection_scans += 1;
        self.collections.entry(ns.to_owned()).or_default().collection_scans += 1;
    }

    /// Counts command `name` that took `micros` and failed or not; `known` is whether the node
    /// has it.
    pub fn command(&mut self, name: &str, cmd: &Document, micros: u64, failed: bool, known: bool) {
        let micros = i64::try_from(micros).unwrap_or(i64::MAX);
        let l = &mut self.latency[kind_index(OpKind::of(name))];
        l.micros = l.micros.saturating_add(micros);
        l.ops += 1;
        if !known {
            self.unknown += 1;
            return;
        }
        let c = self.commands.entry(name.to_owned()).or_default();
        c.total += 1;
        c.failed += i64::from(failed);
        for s in statements(name, cmd) {
            c.array_filters += i64::from(s.get("arrayFilters").is_some());
            let update = s.get(if name == BULK_WRITE { "updateMods" } else { "u" });
            let update = update.or_else(|| s.get("update"));
            c.pipeline += i64::from(matches!(update, Some(Value::Array(_))));
        }
        if let Some(v) = cmd.get("validator") {
            c.validator += 1;
            c.json_schema += i64::from(has_json_schema(v));
        }
        if let Some(l) = level_of(cmd) {
            *c.levels.entry(l).or_default() += 1;
        }
    }

    /// Records the API version a command of application `app` used.
    pub fn api_version(&mut self, app: Option<&str>, version: Option<&str>) {
        let app = app.unwrap_or(NO_APP_NAME).to_owned();
        let v = version.unwrap_or(DEFAULT_API_VERSION).to_owned();
        self.api_versions.entry(app).or_default().insert(v);
    }

    /// Counts an attempt to authenticate with `mechanism`, speculative or not.
    pub fn authentication_attempt(&mut self, mechanism: &str, speculative: bool) {
        let m = self.mechanisms.entry(mechanism.to_owned()).or_default();
        let pair = if speculative { &mut m.speculative } else { &mut m.authenticate };
        pair.0 += 1;
    }

    /// Adds the time an authentication command took.
    pub fn authentication_time(&mut self, micros: u64) {
        let micros = i64::try_from(micros).unwrap_or(i64::MAX);
        self.auth_micros = self.auth_micros.saturating_add(micros);
    }

    /// Counts a successful authentication with `mechanism`, speculative or not.
    pub fn authentication_succeeded(&mut self, mechanism: &str, speculative: bool) {
        let m = self.mechanisms.entry(mechanism.to_owned()).or_default();
        let pair = if speculative { &mut m.speculative } else { &mut m.authenticate };
        pair.1 += 1;
    }

    /// `opLatencies` (and `opWorkingTime`, the same measure: TeselaDB does not queue commands).
    pub fn latencies(&self) -> Document {
        let mut d = Document::new();
        for (name, k) in [
            ("commands", OpKind::Command),
            ("reads", OpKind::Read),
            ("writes", OpKind::Write),
            ("transactions", OpKind::Transaction),
        ] {
            let l = self.latency[kind_index(k)];
            d.push(
                name,
                Document::new()
                    .with("latency", long(l.micros))
                    .with("ops", long(l.ops))
                    .with("queryableEncryptionLatencyMicros", long(0)),
            );
        }
        d
    }

    /// `logicalSessionRecordCache`, with `active` sessions in the node's table.
    pub fn session_cache(&self, active: i64) -> Document {
        let s = &self.sessions;
        Document::new()
            .with("activeSessionsCount", int(active))
            .with("sessionsCollectionJobCount", int(s.refreshes))
            .with("lastSessionsCollectionJobDurationMillis", int(s.refresh_ms))
            .with("lastSessionsCollectionJobDurationMillisUpdatedTimestamp", date(s.refresh_done))
            .with("lastSessionsCollectionJobTimestamp", date(s.refresh_at))
            .with("lastSessionsCollectionJobEntriesFailedToRefresh", int(0))
            .with("lastSessionsCollectionJobEntriesRefreshed", int(s.refreshed))
            .with("lastSessionsCollectionJobEntriesRefreshedUpdatedTimestamp", date(s.refresh_done))
            .with("lastSessionsCollectionJobEntriesEnded", int(s.ended))
            .with("lastSessionsCollectionJobEntriesEndedUpdatedTimestamp", date(s.refresh_done))
            .with("lastSessionsCollectionJobCursorsClosed", int(s.cursors_closed))
            .with("lastSessionsCollectionJobCursorsClosedUpdatedTimestamp", date(s.reap_at))
            .with("transactionReaperJobCount", int(s.reapings))
            .with("lastTransactionReaperJobDurationMillis", int(0))
            .with("lastTransactionReaperJobDurationUpdatedTimestamp", date(s.reap_at))
            .with("lastTransactionReaperJobTimestamp", date(s.reap_at))
            .with("lastTransactionReaperJobEntriesCleanedUp", int(s.sessions_reaped))
            .with("lastTransactionReaperJobEntriesCleanedUpUpdatedTimestamp", date(s.reap_at))
            .with("sessionCatalogSize", int(active))
            .with("sessionsWithOutstandingKills", int(0))
    }

    /// `security`.
    pub fn security(&self) -> Document {
        let pair = |(total, ok): (i64, i64)| {
            Document::new().with("total", long(total)).with("successful", long(ok))
        };
        let mut mechs = Document::new();
        for (name, ingress) in MECHANISMS {
            let m = self.mechanisms.get(name).copied().unwrap_or_default();
            let mut d = Document::new();
            if ingress {
                d.push(
                    "ingress",
                    Document::new()
                        .with("speculativeAuthenticate", pair(m.speculative))
                        .with("clusterAuthenticate", pair((0, 0)))
                        .with("authenticate", pair(m.authenticate)),
                );
            }
            // The node never authenticates to another server with a client mechanism.
            d.push(
                "egress",
                Document::new()
                    .with("speculativeAuthenticate", pair((0, 0)))
                    .with("authenticate", pair((0, 0))),
            );
            mechs.push(name, d);
        }
        let auth = Document::new()
            .with("saslSupportedMechsReceived", long(self.sasl_supported_mechs))
            .with("mechanisms", mechs)
            .with("totalIngressAuthenticationTimeMicros", long(self.auth_micros))
            .with("totalEgressAuthenticationTimeMicros", long(0));
        Document::new().with("authentication", auth)
    }

    /// `transactions`.
    pub fn transactions(&self) -> Document {
        let t = &self.txn;
        let mut causes = Document::new();
        for (cause, n) in &t.abort_causes {
            causes.push(cause.as_str(), long(*n));
        }
        let mut kinds = Document::new();
        for (name, (initiated, ok, micros)) in COMMIT_TYPES.iter().zip(t.commits) {
            kinds.push(
                *name,
                Document::new()
                    .with("initiated", long(initiated))
                    .with("successful", long(ok))
                    .with("successfulDurationMicros", long(micros)),
            );
        }
        Document::new()
            .with("currentOpen", long(t.open))
            .with("currentActive", long(t.active))
            .with("currentInactive", long(t.open - t.active))
            .with("totalStarted", long(t.started))
            .with("totalCommitted", long(t.committed))
            .with("totalAborted", long(t.aborted))
            .with("abortCause", causes)
            .with("totalContactedParticipants", long(t.participants))
            .with("totalParticipantsAtCommit", long(t.participants_at_commit))
            .with("totalRequestsTargeted", long(t.requests))
            .with("commitTypes", kinds)
    }

    /// `internalTransactions`.
    pub fn internal_transactions(&self) -> Document {
        let [started, retried, retried_commits, succeeded] = self.txn.internal;
        Document::new()
            .with("started", long(started))
            .with("retriedTransactions", long(retried))
            .with("retriedCommits", long(retried_commits))
            .with("succeeded", long(succeeded))
    }

    /// `metrics.commands`: every command the node answers (`names`, in order) and the unknown
    /// ones.
    pub fn commands_section<'a>(&self, names: impl Iterator<Item = &'a str>) -> Document {
        let mut d = Document::new().with(UNKNOWN_COMMAND, long(self.unknown));
        let empty = CommandCount::default();
        let mut sorted: Vec<&str> = names.collect();
        sorted.sort_unstable();
        for name in sorted {
            let c = self.commands.get(name).unwrap_or(&empty);
            let mut e = Document::new();
            if matches!(name, UPDATE | FIND_AND_MODIFY | BULK_WRITE) {
                e.push("arrayFilters", long(c.array_filters));
            }
            e.push("failed", long(c.failed));
            if matches!(name, UPDATE | FIND_AND_MODIFY | BULK_WRITE) {
                e.push("pipeline", long(c.pipeline));
            }
            e.push("rejected", long(0));
            e.push("total", long(c.total));
            if matches!(name, "create" | "collMod") {
                let mut levels = Document::new();
                for l in LEVELS {
                    levels.push(l, long(c.levels.get(l).copied().unwrap_or(0)));
                }
                e.push("validationLevel", levels);
                e.push(
                    "validator",
                    Document::new()
                        .with("failed", long(0))
                        .with("jsonSchema", long(c.json_schema))
                        .with("total", long(c.validator)),
                );
            }
            if name == AGGREGATE {
                e.push(
                    "withExtension",
                    Document::new().with("failed", long(0)).with("succeeded", long(0)),
                );
            }
            d.push(name, e);
        }
        d
    }

    /// `metrics.apiVersions`.
    pub fn api_versions_section(&self) -> Document {
        let mut d = Document::new();
        for (app, versions) in &self.api_versions {
            let v: Vec<Value> = versions.iter().map(|s| Value::from(s.as_str())).collect();
            d.push(app.as_str(), v);
        }
        d
    }

    /// `metrics.cursor`, with the cursors open now (each reads one range at a time, so all are
    /// single-target).
    pub fn cursor_section(&self, open: i64) -> Document {
        let open = Document::new()
            .with("multiTarget", long(0))
            .with("singleTarget", long(open))
            .with("queuedData", long(0))
            .with("pinned", long(0))
            .with("total", long(open));
        Document::new().with("timedOut", long(self.cursors_timed_out)).with("open", open)
    }

    /// `metrics.getLastError`.
    pub fn write_concern_section(&self) -> Document {
        let default = Document::new().with("unsatisfiable", long(0)).with("wtimeouts", long(0));
        Document::new().with("wtimeouts", long(self.wtimeouts)).with("default", default).with(
            "wtime",
            Document::new().with("num", long(self.wtime.0)).with("totalMillis", long(self.wtime.1)),
        )
    }

    /// `metrics.operation`: the counters TeselaDB has, and zeros for what it never does
    /// (prepared transactions, RefEngine's write conflicts, range deletions, egress pools).
    pub fn operation_section(&self) -> Document {
        let mut d = Document::new()
            .with("insertFailedDueToDuplicateKeyError", long(self.duplicate_inserts))
            .with("killedDueToClientDisconnect", long(0))
            .with("killedDueToDefaultMaxTimeMSExpired", long(self.killed_max_time.1))
            .with("killedDueToInterruptedDueToOverload", long(0))
            .with("killedDueToMaxTimeMSExpired", long(self.killed_max_time.0));
        for name in [
            "killedDueToRangeDeletion",
            "numConnectionNetworkTimeouts",
            "numRequestsTimedOutBeforeConnectionAcquisitionAttempt",
            "numRequestsTimedOutBeforeSentToRemote",
            "prepareConflictWaitMicros",
            "prepareConflicts",
            "temporarilyUnavailableErrors",
            "temporarilyUnavailableErrorsConvertedToWriteConflict",
            "temporarilyUnavailableErrorsEscaped",
            "totalTimeWaitingBeforeConnectionTimeoutMillis",
            "transactionTooLargeForCacheErrors",
            "transactionTooLargeForCacheErrorsConvertedToWriteConflict",
            "unsendableCompletedResponses",
            "writeConflictRetryLimitHit",
            "writeConflictRetryWaiters",
        ] {
            d.push(name, long(0));
        }
        let mut interrupt = Document::new();
        for name in [
            "checksFromSample",
            "overdueChecksFromSample",
            "overdueInterruptApproxMaxMillisFromSample",
            "overdueInterruptTotalMillisFromSample",
            "overdueOpsFromSample",
            "sampledOps",
            "totalChecks",
        ] {
            interrupt.push(name, long(0));
        }
        d.push("interrupt", interrupt);
        d
    }

    /// `metrics.queryExecutor`.
    pub fn query_executor_section(&self) -> Document {
        let scans = Document::new()
            .with("nonTailable", long(self.collection_scans))
            .with("total", long(self.collection_scans));
        let profiler = Document::new().with(
            "collectionScans",
            Document::new()
                .with("nonTailable", long(0))
                .with("tailable", long(0))
                .with("total", long(0)),
        );
        Document::new().with("collectionScans", scans).with("profiler", profiler)
    }
}
