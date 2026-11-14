//! An in-memory node for the command tests: a catalog applied as Group 0 applies it, the
//! documents in a map, and every write evaluated as the range's leader evaluates it.

use std::collections::BTreeMap;

use teseladb_auth::users::{RoleName, UserName};
use teseladb_hlc::Hlc;
use teseladb_meta::catalog::{Catalog, CatalogCommand, CatalogError, CollectionEntry, Namespace};
use teseladb_meta::indexes::{IndexCommand, IndexEntry, Indexes};
use teseladb_meta::security::{Principal, Security, SecurityError, SecurityOp};
use teseladb_range::host::Mutation;
use teseladb_range::read::ReadConcern;
use teseladb_session::retry::StatementKey;
use teseladb_wire::commands::config_db::COLLECTIONS;
use teseladb_wire::commands::mutation::Bound;
use teseladb_wire::commands::session::KillScope;
use teseladb_wire::commands::sharding;
use teseladb_wire::commands::{Backend, Ctx, LdapLogin, Shared, WriteOp, run};
use teseladb_wire::error::{
    CommandError, DATABASE_DIFFER_CASE, INTERNAL_ERROR, NAMESPACE_EXISTS, NAMESPACE_NOT_FOUND,
    UNSATISFIABLE_WRITE_CONCERN,
};
use teseladb_bson::{Document, Value};

/// The node.
#[derive(Default)]
pub struct Mem {
    pub catalog: Catalog,
    pub data: BTreeMap<Vec<u8>, Vec<u8>>,
    pub dedup: BTreeMap<StatementKey, Vec<u8>>,
    pub index: u64,
    pub now: u64,
    /// How far the node's clock (`cluster_mark`) runs ahead of `now`, as a node's clock may run
    /// ahead of the commit of a write a leader on another node made.
    pub mark_ahead: u64,
    /// The mark of the last write committed.
    pub committed: Option<Hlc>,
    /// The monotonic clock in milliseconds when it differs from the cluster time `now`.
    pub mono: Option<u64>,
    /// The last deadline an operation set.
    pub last_deadline: Option<u64>,
    pub rnd: u64,
    /// Writes the range ran (not counting answers from the idempotence records).
    pub writes: usize,
    /// Documents the filtered scans handed out (`Backend::scan_matching`).
    pub matched: usize,
    /// Replicas: a `w` above it is unsatisfiable.
    pub replicas: u32,
    /// The next write fails with this error.
    pub fail_next: Option<CommandError>,
    /// The write after this many more fails with this error (`(0, e)` is the next one).
    pub fail_write_at: Option<(usize, CommandError)>,
    pub shared: Shared,
    /// The kills sent to the other nodes; with `Some`, the next one fails with that error.
    pub killed: Vec<KillScope>,
    pub fail_kill: Option<CommandError>,
    /// The operations of the other nodes of the cluster, as each node describes its own.
    pub other_ops: Vec<Document>,
    /// Milliseconds the clock moves with each read of the range.
    pub tick: u64,
    /// The users and passwords the LDAP server of the node takes (013 PLAIN).
    pub ldap: Vec<(String, Vec<u8>)>,
    /// The groups of each LDAP user, as roles, when the node authorizes by LDAP (013 FR-026).
    pub ldap_groups: Option<Vec<(String, Vec<RoleName>)>>,
    /// `invalidateUserCache` calls.
    pub invalidations: usize,
    /// Lookups of the mark at which a database was added.
    pub database_marks: usize,
    /// The marks of the `$clusterTime`s clients sent (`Backend::observe_mark`).
    pub observed: Vec<Hlc>,
    /// What the STS answers to REFDB-AWS (status and body); `None`: unreachable.
    pub sts: Option<(u16, Vec<u8>)>,
    /// `oidcIdentityProviders` and the JWKS each provider gives, as JSON.
    pub oidc: Option<(String, Vec<u8>)>,
    /// The JWKS reads, with whether each was a refresh.
    pub jwks_reads: Vec<bool>,
    /// The requests sent to the STS.
    pub sts_requests: Vec<teseladb_auth::aws::StsRequest>,
    /// The service keys of the node's keytab (GSSAPI).
    pub kerberos: Vec<teseladb_auth::kerberos::ServiceKey>,
    /// Runs before each read of the range (a `killOp` from another connection).
    pub on_scan: Option<Box<dyn FnMut()>>,
    /// The cluster parameters.
    pub params: BTreeMap<String, Vec<u8>>,
    /// The last deadline the commands set.
    pub deadline: Option<u64>,
    /// What `process` reports.
    pub facts: teseladb_wire::commands::diagnostics::ProcessFacts,
    /// The users and roles of Group 0.
    pub security: Security,
    /// Whether the reads of users and roles see the copy of Group 0 as it was at the last
    /// [`Backend::catalog_barrier`], as a node behind the writes of another one does (013 T058).
    pub stale_security: bool,
    /// The copy those reads see, and the barriers taken.
    pub seen_security: Security,
    pub barriers: usize,
    /// The next this many security writes find a concurrent change (`usize::MAX`: all).
    pub fail_security_writes: usize,
    /// A user removed just before the next security write.
    pub drop_before_write: Option<UserName>,
    /// A role `(db, role)` removed just before the next security write.
    pub drop_role_before_write: Option<(String, String)>,
    /// The node lost its authorization lease (013 FR-012).
    pub lease_lost: bool,
    /// The user the running command's connection authenticated as.
    pub user: Option<UserName>,
    /// The privileges of that user when the node authorizes commands; `None` without
    /// authorization.
    pub user_privileges: Option<Vec<teseladb_auth::privileges::Privilege>>,
    /// The warning lines the commands logged: identifier, message, attributes.
    pub logged: Vec<(i32, String, Document)>,
    /// The `message` of each certificate rotation that ran.
    pub rotations: Vec<Option<String>>,
    /// The next certificate rotation fails with this error.
    pub fail_rotation: Option<CommandError>,
    /// The secondary indexes of Group 0 (009).
    pub indexes: Indexes,
    /// The changes of the indexes the commands made, in order (009 FR-006).
    pub index_log: Vec<IndexCommand>,
    /// The write concern of each write the range ran.
    pub concerns: Vec<teseladb_range::ack::WriteConcern>,
    /// Operations that ended.
    pub ended: usize,
    /// Reads of the indexes since the last operation ended: a server pins each until then (009 FR-006).
    pub open_index_reads: usize,
    /// Where a second range starts: a write whose outcome reaches the other side of it from its
    /// first key is refused as the range refuses it (009 FR-003).
    pub split: Option<Vec<u8>>,
    /// The transactions across ranges that committed: their identifier and writes.
    pub txns: Vec<(Option<teseladb_range::intents::TxnId>, Vec<Vec<u8>>)>,
    /// The next this many transactions find a conflict.
    pub conflicts: usize,
    /// Group 0's settings that `group0_settings` reads and writes (018 zones).
    pub group0: BTreeMap<Vec<u8>, Vec<u8>>,
    /// The next this many writes of Group 0's settings find a concurrent change.
    pub group0_conflicts: usize,
    /// The write fences of the ranges (018): a write or an intent with a key in one is refused.
    pub fences: Vec<(Vec<u8>, Vec<u8>)>,
    /// The next pause lifts every fence (the critical section ended meanwhile).
    pub lift_on_pause: bool,
    /// The intents of client transactions, by transaction (008).
    pub intents: BTreeMap<u128, Vec<(Vec<u8>, teseladb_range::intents::IntentOp)>>,
    /// The next `PutIntents` fail with this error.
    pub intent_errors: Vec<teseladb_range::host::RangeError>,
    /// How many `PutIntents` succeed before those of `intent_errors` fail.
    pub intents_before_error: usize,
    /// The next commits of a record fail with this error.
    pub commit_errors: Vec<teseladb_range::host::RangeError>,
    /// The coordinator's memory budget for a client transaction, when not the node's.
    pub txn_budget: Option<usize>,
    /// The oldest mark the history keeps: a transaction's read below it is `SnapshotTooOld`.
    pub history_from: Option<Hlc>,
    /// The mark of the last read of a client transaction.
    pub txn_read_at: Option<Hlc>,
    /// A version the next read of a client transaction meets in its uncertainty window.
    pub uncertain_once: Option<Hlc>,
    /// The mark the running command's reads are served at (008 FR-012).
    pub read_mark: Option<teseladb_txn::causal::ReadMark>,
    /// The mark of the last scan.
    pub scanned_at: Option<Option<Hlc>>,
    /// A version the next scan outside a transaction meets in its uncertainty window: the scan
    /// reads again at its mark and raises the command's, as a node's backend does (008 FR-008).
    pub uncertain_scan: Option<Hlc>,
    /// Value types other than a document's, by key (written by transactions).
    pub types: BTreeMap<Vec<u8>, u8>,
    /// How many times a command read a collection's catalog entry.
    pub catalog_reads: usize,
    /// Rounds of reads inside transactions: one per `txn_get` or `txn_scan`, one per batch of
    /// `txn_points` or `txn_gets`.
    pub txn_rounds: usize,
    /// How many `PutIntents` and one-phase commits the ranges took.
    pub intent_puts: usize,
    pub one_phase: usize,
    /// The one-phase commits of client transactions, by session and number, with their marks
    /// (008 FR-022), and those a recovery refused.
    pub decided: BTreeMap<(u128, i64), Hlc>,
    pub refused: std::collections::BTreeSet<(u128, i64)>,
    /// Lock requests of transactions that hold their writes.
    pub locks: usize,
    /// Requests to let go of a transaction's locks.
    pub unlocks: usize,
    /// The range refuses the locks as if the keys had moved: the statements put intents.
    pub lock_moved: bool,
    /// Every version written, at its mark: what a change stream reads (014).
    pub history: Vec<teseladb_range::changes::Versioned>,
    /// Runs before each read of the range with its first key and the data: another node's
    /// write meanwhile (009, `prepareUnique`).
    pub on_scan_data: Option<ScanHook>,
    /// The nodes `config.shards` lists, by name.
    pub shards: Vec<String>,
    /// The shards `config.shards` lists as draining.
    pub draining: Vec<String>,
    /// What `config.databases` lists, if set.
    pub databases_docs: Option<Vec<Document>>,
    /// The transactions' records, as their range keeps them (008): session, state and commit mark.
    pub records: BTreeMap<u128, (u128, teseladb_range::intents::TxnStatus, Hlc)>,
    /// The records a coordinator began (a recovery's own record was not).
    pub begun: std::collections::BTreeSet<u128>,
    /// The next pushes of a record fail with this error (its range cannot be reached).
    pub push_errors: Vec<teseladb_range::host::RangeError>,
    /// Versions the reads of client transactions meet in their windows after `uncertain_once`.
    pub uncertain_more: Vec<Hlc>,
    /// Pauses a waiting command takes before its time runs out (none: it runs out at once).
    pub pauses_left: usize,
    /// The node gets its authorization lease back during the next pause.
    pub lease_back_on_pause: bool,
    /// How many reads of the changes still give a resolved mark one millisecond behind the
    /// clock, as a range whose oldest intent is not resolved yet (011 FR-010).
    pub changes_lag: usize,
    /// The next change stream read fails with this error (014).
    pub fail_changes: Option<CommandError>,
    /// The next resolutions of intents fail with this error.
    pub resolve_errors: Vec<teseladb_range::host::RangeError>,
    /// A commit's wait for the clock runs out.
    pub wait_clock_fails: bool,
    /// The node knows no range for any key.
    pub no_route: bool,
    /// The highest feature compatibility version some node runs (007 FR-027).
    pub fcv_limit: Option<String>,
    /// Every node keeps the documents of a new collection in `_id` order (D-153).
    pub ordered_ids: bool,
    /// Answers of the other nodes that do not apply the write gates yet (007 FR-029).
    pub gate_lagging: usize,
    /// The tenants Group 0 knows, by number and name (007 FR-025).
    pub tenants: Vec<(u32, String)>,
    /// The tenants suspended (007 FR-034).
    pub suspended: Vec<u32>,
    /// The access list of every tenant (007 FR-035).
    pub sources: Vec<String>,
    /// The catalog changes, at the node's clock (014 FR-014).
    pub ddl: teseladb_meta::ddl_log::DdlLog,
    /// How far behind the clock the ranges' resolved mark goes, in milliseconds (014 FR-004).
    pub resolved_lag_ms: u64,
    /// The most a read of the changes goes past its start, in milliseconds: a range's leader
    /// answers part of what was asked when its budget of bytes runs out (014 FR-004,
    /// `teseladb_range::changes::ANSWER_BYTES`); zero: no limit. A read from zero has none.
    pub changes_window_ms: u64,
    /// Milliseconds the clock moves on at each pause of a waiting command.
    pub pause_ms: u64,
    /// The change stream reads of ranges so far (014 FR-012).
    pub change_reads: usize,
    /// The documents of `config.settings` Group 0 keeps (007 FR-017), if the node is part of a
    /// cluster that keeps them (`None`: it refuses, as a node alone does).
    pub settings: Option<Vec<Document>>,
    /// Changes of `config.settings` that find another change before them: each answers that a
    /// document is no longer as it was read.
    pub settings_conflicts: usize,
    /// The ranges the router knows (018 chunks); the chunk commands' orders apply to them at
    /// once.
    pub routes: Vec<teseladb_router::table::RouteEntry>,
    /// The range orders sent to Group 0, in order.
    pub range_orders: Vec<teseladb_meta::command::Group0Command>,
    /// Range orders Group 0 refuses, with this error.
    pub refuse_orders: Option<CommandError>,
    /// Range orders Group 0 takes but has not applied yet: they are kept in `range_orders` and the
    /// routes stay as they were.
    pub hold_orders: bool,
    /// The live bytes the state machines of these ranges keep; a range left out has those of
    /// the versions in its bounds.
    pub range_live: BTreeMap<teseladb_types::GroupId, u64>,
    /// Documents the scans gave.
    pub scanned_rows: usize,
    /// Reads of several spans at once ([`Backend::scan_spans`]), each a trip to the ranges.
    pub span_reads: usize,
    /// The spans those reads read, each also counted by `on_scan`.
    pub spans_batched: usize,
    /// Runs before each read of the changes (another client's writes while a resharding reads
    /// the history, 018).
    pub on_changes: Option<NodeHook>,
    /// Runs after each catalog write with its command (another client's DDL right after it).
    pub after_catalog_write: Option<CatalogHook>,
}

/// A change of the node by another client.
pub type NodeHook = Box<dyn FnMut(&mut Mem)>;
/// A change of the node by another client after a catalog write.
pub type CatalogHook = Box<dyn FnMut(&mut Mem, &CatalogCommand)>;

/// A change of the data before a read of `[start, …)`.
pub type ScanHook = Box<dyn FnMut(&[u8], &mut BTreeMap<Vec<u8>, Vec<u8>>)>;

impl Mem {
    pub fn new() -> Mem {
        Mem { now: 1_700_000_000_000, replicas: 3, ..Mem::default() }
    }

    /// Refuses keys a fence holds, as the ranges do (018).
    pub fn check_fences(&self, keys: &[Vec<u8>]) -> Result<(), CommandError> {
        let held = |k: &Vec<u8>| self.fences.iter().any(|(s, e)| s <= k && k < e);
        match keys.iter().any(held) {
            true => Err(teseladb_wire::error::from_range(
                &teseladb_range::host::RangeError::Fenced,
                Default::default(),
            )),
            false => Ok(()),
        }
    }

    /// The node's mark now: its clock, as its writes and catalog changes get it.
    pub fn mark(&self) -> Hlc {
        Hlc { physical: self.now, logical: 0 }
    }

    /// Notes a version of `key` written at `ts` for the change streams.
    fn record(&mut self, key: &[u8], ts: Hlc, value: Option<(u8, Vec<u8>)>) {
        self.history.push(teseladb_range::changes::Versioned { key: key.to_vec(), ts, value });
    }

    /// Applies a committed write of a transaction at `ts`.
    fn apply_committed(&mut self, k: Vec<u8>, w: teseladb_range::intents::IntentOp, ts: Hlc) {
        match w {
            teseladb_range::intents::IntentOp::Put { value_type, value } => {
                self.record(&k, ts, Some((value_type, value.clone())));
                self.types.insert(k.clone(), value_type);
                self.data.insert(k, value);
            }
            teseladb_range::intents::IntentOp::Delete => {
                self.record(&k, ts, None);
                self.types.remove(&k);
                self.data.remove(&k);
            }
        }
    }

    /// The stored versions of the keys in `range` a range's scans return: all but those that only
    /// serve the history of a document (014).
    pub fn stored(
        &self,
        range: impl std::ops::RangeBounds<Vec<u8>>,
    ) -> BTreeMap<&Vec<u8>, &Vec<u8>> {
        let history = |k: &Vec<u8>| self.types.get(k) == Some(&teseladb_range::HISTORY_VALUE);
        self.data.range(range).filter(|(k, _)| !history(k)).collect()
    }

    /// Runs `cmd` on database `db`; the reply or the error document.
    pub fn run(&mut self, db: &str, cmd: Document) -> Document {
        let shared = std::mem::take(&mut self.shared);
        let mut ctx = Ctx { backend: self, shared: &shared, db, cmd: &cmd };
        let r = run(&mut ctx).expect("a command of the module");
        self.shared = shared;
        match r {
            Ok(d) => d,
            Err(e) => e.to_document(),
        }
    }
}

impl Mem {
    /// A read of a client transaction at `at`: what the range answers below the history.
    fn txn_read(&mut self, at: Hlc, window: Option<Hlc>) -> Result<(), CommandError> {
        self.txn_read_at = Some(at);
        if self.uncertain_once.is_none() && !self.uncertain_more.is_empty() {
            self.uncertain_once = Some(self.uncertain_more.remove(0));
        }
        if let (Some(ts), Some(end)) = (self.uncertain_once, window)
            && at < ts
            && ts <= end
        {
            self.uncertain_once = None;
            let e = teseladb_range::host::RangeError::ReadUncertain { ts };
            let ctx = teseladb_wire::error::RangeContext {
                retryable_write: false,
                max_time_expired: false,
            };
            return Err(teseladb_wire::error::from_range(&e, ctx));
        }
        if self.history_from.is_some_and(|from| at < from) {
            let e = teseladb_range::host::RangeError::SnapshotTooOld;
            let ctx = teseladb_wire::error::RangeContext {
                retryable_write: false,
                max_time_expired: false,
            };
            return Err(teseladb_wire::error::from_range(&e, ctx));
        }
        Ok(())
    }

    /// A scan in key order, or from the last key down (H-003), with the hooks of the tests.
    fn scan_dir(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        backward: bool,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        if let (Some(version), Some(mut m)) = (self.uncertain_scan.take(), self.read_mark) {
            m.at = m.at.max(version);
            self.read_mark = Some(m);
        }
        self.scanned_at = Some(self.read_mark.map(|m| m.at));
        if let Some(m) = self.read_mark {
            self.txn_read(m.at, m.uncertain_to)?;
        }
        self.now += self.tick;
        if let Some(f) = self.on_scan.as_mut() {
            f();
        }
        if let Some(f) = self.on_scan_data.as_mut() {
            f(start, &mut self.data);
        }
        // As a range's scan: the versions that only serve the history are no documents.
        if start >= end {
            return Ok(Vec::new());
        }
        let stored = self.stored(start.to_vec()..end.to_vec());
        let pairs: Box<dyn Iterator<Item = (&Vec<u8>, &Vec<u8>)>> = match backward {
            true => Box::new(stored.into_iter().rev()),
            false => Box::new(stored.into_iter()),
        };
        let rows: Vec<(Vec<u8>, Vec<u8>)> =
            pairs.take(limit).map(|(k, v)| (k.clone(), v.clone())).collect();
        self.scanned_rows += rows.len();
        Ok(rows)
    }
}

impl Backend for Mem {
    fn collection(&mut self, ns: &Namespace) -> Result<Option<CollectionEntry>, CommandError> {
        self.catalog_reads += 1;
        Ok(self.catalog.get(ns).cloned())
    }

    fn collections(&mut self) -> Result<Vec<(Namespace, CollectionEntry)>, CommandError> {
        Ok(self.catalog.collections.iter().map(|(n, c)| (n.clone(), c.clone())).collect())
    }

    fn databases(&mut self) -> Result<Vec<String>, CommandError> {
        Ok(self.catalog.databases_by_creation())
    }

    fn database_mark(&mut self, db: &str) -> Result<Option<Hlc>, CommandError> {
        self.database_marks += 1;
        Ok(self.catalog.db_marks.get(db).copied())
    }

    fn catalog_write(
        &mut self,
        cmd: CatalogCommand,
    ) -> Result<Option<CollectionEntry>, CommandError> {
        self.index += 1;
        let changes = teseladb_meta::ddl_log::catalog_changes(&self.catalog, &cmd);
        let r = self.catalog.apply(self.index, &cmd).map_err(|e| match e {
            CatalogError::NamespaceExists => CommandError::new(NAMESPACE_EXISTS, "exists"),
            CatalogError::NamespaceNotFound => CommandError::new(NAMESPACE_NOT_FOUND, "not found"),
            CatalogError::DatabaseDifferCase => CommandError::new(DATABASE_DIFFER_CASE, "case"),
            other => CommandError::new(INTERNAL_ERROR, format!("{other:?}")),
        })?;
        self.ddl.record_applied(self.mark(), changes, r.as_ref());
        // The hook is out while it runs: its own catalog writes do not call it again.
        if let Some(mut hook) = self.after_catalog_write.take() {
            hook(self, &cmd);
            self.after_catalog_write = Some(hook);
        }
        Ok(r)
    }

    fn indexes(&mut self, collection: u32) -> Result<Vec<IndexEntry>, CommandError> {
        self.open_index_reads += 1;
        Ok(self.indexes.of(collection).cloned().collect())
    }

    fn search_indexes(
        &mut self,
        collection: u32,
    ) -> Result<Vec<teseladb_meta::indexes::SearchIndexEntry>, CommandError> {
        Ok(self.indexes.search_of(collection).cloned().collect())
    }

    fn voters(&mut self, _: &[u8], _: &[u8]) -> Result<usize, CommandError> {
        Ok(self.replicas as usize)
    }

    fn note_plan(&mut self, _: Document) {}

    fn note_plan_lazy(&mut self, _: teseladb_wire::ops::PendingPlan) {}

    fn not_examined(&mut self, _docs: u64) {}

    fn op_reads(&mut self) -> teseladb_wire::ops::OpReads {
        teseladb_wire::ops::OpReads::default()
    }

    fn op_client(&mut self) -> Option<Document> {
        None
    }

    fn op_plan(&mut self) -> Option<Document> {
        None
    }

    fn end_operation(&mut self) {
        self.ended += 1;
        self.open_index_reads = 0;
    }

    fn fence(&mut self, start: &[u8], end: &[u8], on: bool) -> Result<Hlc, CommandError> {
        let span = (start.to_vec(), end.to_vec());
        self.fences.retain(|f| *f != span);
        if on {
            self.fences.push(span);
        }
        self.now += 1;
        Ok(Hlc { physical: self.now, logical: 0 })
    }

    fn pause(&mut self) -> bool {
        if self.lease_back_on_pause {
            self.lease_lost = false;
        }
        // The critical section ends during the pause: the fences go.
        if self.lift_on_pause && !self.fences.is_empty() {
            self.fences.clear();
            self.lift_on_pause = false;
            return true;
        }
        self.txn_pause()
    }

    fn changes(
        &mut self,
        start: &[u8],
        end: &[u8],
        from: Hlc,
    ) -> Result<teseladb_wire::commands::ChangeBatch, CommandError> {
        self.change_reads += 1;
        if let Some(mut hook) = self.on_changes.take() {
            hook(self);
            self.on_changes = Some(hook);
        }
        if let Some(e) = self.fail_changes.take() {
            return Err(e);
        }
        // Every write of this node is applied at once: resolved up to its clock, less the lag
        // asked for (in milliseconds, or one millisecond for the next `changes_lag` reads).
        let lag = u64::from(self.changes_lag > 0);
        self.changes_lag = self.changes_lag.saturating_sub(1);
        let rows: Vec<teseladb_range::changes::Versioned> = self
            .history
            .iter()
            .filter(|v| v.ts > from && start <= v.key.as_slice() && v.key.as_slice() < end)
            .cloned()
            .collect();
        let behind = self.resolved_lag_ms.saturating_add(lag);
        let resolved = Hlc { physical: self.now.saturating_sub(behind), logical: 0 };
        let windowed = self.changes_window_ms > 0 && from.physical > 0;
        let resolved = match windowed {
            true => resolved.min(Hlc {
                physical: from.physical.saturating_add(self.changes_window_ms),
                logical: 0,
            }),
            false => resolved,
        };
        let rows = rows.into_iter().filter(|v| v.ts <= resolved).collect();
        Ok((rows, resolved))
    }

    // One range: up to its resolved mark, or with `at` up to that mark as a read at it (every
    // write of this node is applied at once), within the window of a read. An intent not yet
    // resolved (`changes_lag`) keeps the mark below it either way.
    fn range_changes(
        &mut self,
        start: &[u8],
        end: &[u8],
        from: Hlc,
        at: Option<Hlc>,
    ) -> Result<Vec<teseladb_wire::commands::RangeBatch>, CommandError> {
        let intent = self.changes_lag > 0;
        let (rows, resolved) = self.changes(start, end, from)?;
        let wanted = at.filter(|a| !intent && *a > resolved && a.physical <= self.now);
        let wanted = match (wanted, self.changes_window_ms > 0 && from.physical > 0) {
            (Some(a), true) => Some(a.min(Hlc {
                physical: from.physical.saturating_add(self.changes_window_ms),
                logical: 0,
            })),
            (w, _) => w,
        };
        let (rows, resolved) = match wanted {
            Some(a) => {
                let rows = self
                    .history
                    .iter()
                    .filter(|v| v.ts > from && v.ts <= a)
                    .filter(|v| start <= v.key.as_slice() && v.key.as_slice() < end)
                    .cloned()
                    .collect();
                (rows, a.max(resolved))
            }
            None => (rows, resolved),
        };
        Ok(vec![teseladb_wire::commands::RangeBatch {
            start: start.to_vec(),
            end: end.to_vec(),
            rows,
            resolved,
        }])
    }

    fn catalog_changes(
        &mut self,
        from: Hlc,
    ) -> Result<teseladb_wire::commands::DdlBatch, CommandError> {
        Ok(teseladb_wire::commands::DdlBatch {
            records: self.ddl.after(from),
            floor: self.ddl.floor,
            resolved: self.mark(),
        })
    }

    fn tenant_named(&mut self, name: &str) -> Result<Option<u32>, CommandError> {
        Ok(self.tenants.iter().find(|(_, n)| n == name).map(|(id, _)| *id))
    }

    fn tenant_state(&mut self, tenant: u32) -> Option<teseladb_meta::tenants::TenantState> {
        self.tenants.iter().any(|(id, _)| *id == tenant).then(|| {
            teseladb_meta::tenants::TenantState {
                suspended: self.suspended.contains(&tenant),
                allowed_sources: self.sources.clone(),
                ..Default::default()
            }
        })
    }

    fn gate_everywhere(
        &mut self,
        _want: teseladb_wire::commands::write_block::WriteGate,
    ) -> Result<bool, CommandError> {
        let behind = self.gate_lagging > 0;
        self.gate_lagging = self.gate_lagging.saturating_sub(1);
        Ok(!behind)
    }

    // One range, with no intents: a read sees the committed documents.
    fn txn_pause(&mut self) -> bool {
        let left = self.pauses_left > 0;
        self.pauses_left = self.pauses_left.saturating_sub(1);
        self.now += self.pause_ms;
        left
    }

    /// The clock jumps to the commit's mark: the wait is over at once.
    fn txn_wait_clock(&mut self, until_ms: u64) -> bool {
        self.now = self.now.max(until_ms);
        !self.wait_clock_fails
    }

    fn txn_route(&self, _key: &[u8]) -> Option<teseladb_types::GroupId> {
        (!self.no_route).then_some(teseladb_types::GroupId(1))
    }

    fn txn_ranges(&self) -> Vec<(teseladb_types::GroupId, Vec<u8>)> {
        match self.no_route {
            true => Vec::new(),
            false => vec![(teseladb_types::GroupId(1), Vec::new())],
        }
    }

    fn cluster_mark(&self) -> Hlc {
        Hlc { physical: self.now + self.mark_ahead, logical: 0 }
    }

    fn observe_mark(&mut self, mark: Hlc) {
        self.observed.push(mark);
    }

    fn last_commit(&self) -> Option<Hlc> {
        self.committed
    }

    fn txn_memory_budget(&self) -> usize {
        self.txn_budget.unwrap_or(teseladb_txn::coordinator::MEMORY_BUDGET)
    }

    fn txn_send(
        &mut self,
        _g: teseladb_types::GroupId,
        op: teseladb_range::host::TxnOp,
    ) -> Result<teseladb_range::host::Reply, teseladb_range::host::RangeError> {
        use teseladb_range::host::{Reply, TxnOp};
        use teseladb_range::intents::{RecordOp, TxnRecord, TxnStatus};
        self.now += 1;
        let ts = Hlc { physical: self.now, logical: 0 };
        let record = |txn, status, commit_ts| TxnRecord {
            txn,
            lsid: 0,
            txn_number: 0,
            status,
            read_ts: ts,
            min_commit_ts: ts,
            commit_ts,
            heartbeat_ts: ts,
            spans: Vec::new(),
        };
        match op {
            TxnOp::PutIntents { txn, writes, .. } => {
                self.intent_puts += 1;
                if !self.intent_errors.is_empty() {
                    if self.intents_before_error == 0 {
                        return Err(self.intent_errors.remove(0));
                    }
                    self.intents_before_error -= 1;
                }
                let keys: Vec<Vec<u8>> = writes.iter().map(|(k, _)| k.clone()).collect();
                if self.check_fences(&keys).is_err() {
                    return Err(teseladb_range::host::RangeError::Fenced);
                }
                let mine = self.intents.entry(txn.0).or_default();
                for (k, w) in writes {
                    mine.retain(|(m, _)| *m != k);
                    mine.push((k, w));
                }
                Ok(Reply::Intents { ts })
            }
            // Before the commit (008 FR-021): every key holds an intent of the transaction.
            TxnOp::CheckIntents { txn, keys } => {
                let mine = self.intents.get(&txn.0);
                let held = keys.iter().all(|k| mine.is_some_and(|m| m.iter().any(|(w, _)| w == k)));
                match held {
                    true => Ok(Reply::Intents { ts }),
                    false => Err(teseladb_range::host::RangeError::NoSuchTransaction),
                }
            }
            TxnOp::EndTxn { txn, op: RecordOp::Commit { ts: at, .. } } => {
                if !self.commit_errors.is_empty() {
                    return Err(self.commit_errors.remove(0));
                }
                // A recovery aborted it first.
                let kept = self.records.get(&txn.0).copied();
                if kept.is_some_and(|(_, s, _)| s == TxnStatus::Aborted) {
                    return Err(teseladb_range::host::RangeError::NoSuchTransaction);
                }
                let lsid = kept.map_or(0, |(l, ..)| l);
                self.records.insert(txn.0, (lsid, TxnStatus::Committed, at.max(ts)));
                Ok(Reply::Record(record(txn, TxnStatus::Committed, at.max(ts))))
            }
            TxnOp::EndTxn { txn, op: RecordOp::Abort { .. } } => {
                let lsid = self.records.get(&txn.0).map_or(0, |(l, ..)| *l);
                self.records
                    .insert(txn.0, (lsid, TxnStatus::Aborted, Hlc { physical: 0, logical: 0 }));
                Ok(Reply::Record(record(txn, TxnStatus::Aborted, ts)))
            }
            TxnOp::EndTxn { txn, op: RecordOp::Begin { lsid, .. } } => {
                self.begun.insert(txn.0);
                self.records.entry(txn.0).or_insert((
                    lsid,
                    TxnStatus::Pending,
                    Hlc { physical: 0, logical: 0 },
                ));
                Ok(Reply::Record(record(txn, TxnStatus::Pending, ts)))
            }
            TxnOp::PushTxn { txn, push: teseladb_range::intents::Push::Abort { .. } } => {
                if !self.push_errors.is_empty() {
                    return Err(self.push_errors.remove(0));
                }
                let (lsid, status, at) =
                    self.records.get(&txn.0).copied().unwrap_or((0, TxnStatus::Pending, ts));
                let status = match status {
                    TxnStatus::Pending => TxnStatus::Aborted,
                    s => s,
                };
                self.records.insert(txn.0, (lsid, status, at));
                let read_ts = match self.begun.contains(&txn.0) {
                    true => ts,
                    false => Hlc { physical: 0, logical: 0 },
                };
                Ok(Reply::Record(TxnRecord { lsid, read_ts, ..record(txn, status, at) }))
            }
            TxnOp::EndTxn { txn, .. }
            | TxnOp::PushTxn { txn, .. }
            | TxnOp::PushByRead { txn, .. } => {
                Ok(Reply::Record(record(txn, TxnStatus::Pending, ts)))
            }
            TxnOp::ResolveIntents { txn, status, commit_ts, .. } => {
                if !self.resolve_errors.is_empty() {
                    return Err(self.resolve_errors.remove(0));
                }
                for (k, w) in self.intents.remove(&txn.0).unwrap_or_default() {
                    if status == TxnStatus::Committed {
                        self.apply_committed(k, w, commit_ts);
                    }
                }
                Ok(Reply::Intents { ts })
            }
            TxnOp::OnePhase { writes, session, .. } => {
                // Sent again, or refused by a recovery: answered from its record (008 FR-022).
                if let Some(at) = session.and_then(|s| self.decided.get(&s)) {
                    return Ok(Reply::Intents { ts: *at });
                }
                if session.is_some_and(|s| self.refused.contains(&s)) {
                    return Err(teseladb_range::host::RangeError::NoSuchTransaction);
                }
                self.one_phase += 1;
                // It is the write and the commit: their failures are its own.
                if !self.commit_errors.is_empty() {
                    return Err(self.commit_errors.remove(0));
                }
                if !self.intent_errors.is_empty() {
                    if self.intents_before_error == 0 {
                        return Err(self.intent_errors.remove(0));
                    }
                    self.intents_before_error -= 1;
                }
                let keys: Vec<Vec<u8>> = writes.iter().map(|(k, _)| k.clone()).collect();
                if self.check_fences(&keys).is_err() {
                    return Err(teseladb_range::host::RangeError::Fenced);
                }
                for (k, w) in writes {
                    self.apply_committed(k, w, ts);
                }
                if let Some(s) = session {
                    self.decided.insert(s, ts);
                }
                Ok(Reply::Intents { ts })
            }
            TxnOp::DecideOnePhase { lsid, txn_number, .. } => {
                match self.decided.get(&(lsid, txn_number)) {
                    Some(at) => Ok(Reply::Intents { ts: *at }),
                    None if self.refused.insert((lsid, txn_number)) => Ok(Reply::Batch(Vec::new())),
                    None => Err(teseladb_range::host::RangeError::NoSuchTransaction),
                }
            }
            // One range and no other transaction: a lock is granted, unless the test injects
            // the failure of the statement's write (008 FR-022).
            TxnOp::LockKeys { read_ts, .. } => {
                if self.lock_moved {
                    return Err(teseladb_range::host::RangeError::OutOfRange);
                }
                self.locks += 1;
                if !self.intent_errors.is_empty() {
                    if self.intents_before_error == 0 {
                        return Err(self.intent_errors.remove(0));
                    }
                    self.intents_before_error -= 1;
                }
                Ok(Reply::Intents { ts: read_ts })
            }
            TxnOp::UnlockKeys { .. } => {
                self.unlocks += 1;
                Ok(Reply::Intents { ts })
            }
            // Only the reaper sends these, and this node has none.
            TxnOp::FindIntents { .. } => Ok(Reply::Batch(Vec::new())),
            TxnOp::ForgetRecord { .. } => Err(teseladb_range::host::RangeError::WriteConflict),
            TxnOp::ReadRecord { txn } => match self.records.get(&txn.0) {
                Some((_, status, at)) => Ok(Reply::Record(record(txn, *status, *at))),
                None => Err(teseladb_range::host::RangeError::NoSuchTransaction),
            },
        }
    }

    // The committed documents, under the reader's own intents.
    fn txn_get(
        &mut self,
        key: &[u8],
        at: Hlc,
        view: &teseladb_range::push::TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Option<(u8, Vec<u8>)>, CommandError> {
        use teseladb_range::intents::IntentOp;
        self.txn_rounds += 1;
        self.txn_read(at, uncertain_to)?;
        let own = view.own.and_then(|t| self.intents.get(&t.0));
        if let Some((_, w)) = own.and_then(|o| o.iter().find(|(k, _)| k.as_slice() == key)) {
            return Ok(match w {
                IntentOp::Put { value_type, value } => Some((*value_type, value.clone())),
                IntentOp::Delete => None,
            });
        }
        let type_of = self.types.get(key).copied().unwrap_or(1);
        // A version written after `at` hides the current one: the read goes to the history.
        let newer = self.history.iter().any(|v| v.key.as_slice() == key && v.ts > at);
        if let Some(v) = self.data.get(key).filter(|_| !newer) {
            return Ok(Some((type_of, v.clone())));
        }
        // A document gone or changed since: its version at `at`, as a range's MVCC keeps it.
        Ok(self
            .history
            .iter()
            .rev()
            .find(|v| v.key.as_slice() == key && v.ts <= at)
            .and_then(|v| v.value.clone()))
    }

    fn txn_points(
        &mut self,
        keys: &[Vec<u8>],
        at: Hlc,
        view: &teseladb_range::push::TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Vec<Option<Vec<u8>>>, CommandError> {
        let rounds = self.txn_rounds;
        let mut out = Vec::new();
        for k in keys {
            let end = teseladb_wire::commands::point_end(k);
            out.push(self.txn_scan(k, &end, 1, at, view, uncertain_to)?.pop().map(|(_, v)| v));
        }
        self.txn_rounds = rounds + 1;
        Ok(out)
    }

    fn txn_gets(
        &mut self,
        keys: &[Vec<u8>],
        at: Hlc,
        view: &teseladb_range::push::TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Vec<Option<(u8, Vec<u8>)>>, CommandError> {
        let rounds = self.txn_rounds;
        let out = keys
            .iter()
            .map(|k| self.txn_get(k, at, view, uncertain_to))
            .collect::<Result<Vec<_>, _>>()?;
        self.txn_rounds = rounds + 1;
        Ok(out)
    }

    fn txn_scan(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        at: Hlc,
        view: &teseladb_range::push::TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        use teseladb_range::intents::IntentOp;
        self.txn_rounds += 1;
        self.txn_read(at, uncertain_to)?;
        // As a range's scan: the versions that only serve the history are no documents.
        let history = teseladb_range::HISTORY_VALUE;
        let mut rows: BTreeMap<Vec<u8>, Vec<u8>> = self
            .data
            .range(start.to_vec()..end.to_vec())
            .filter(|(k, _)| self.types.get(*k) != Some(&history))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let own = view.own.and_then(|t| self.intents.get(&t.0)).cloned().unwrap_or_default();
        for (k, w) in own.into_iter().filter(|(k, _)| start <= k.as_slice() && k.as_slice() < end) {
            match w {
                IntentOp::Put { value_type, .. } if value_type == history => None,
                IntentOp::Put { value, .. } => rows.insert(k, value),
                IntentOp::Delete => rows.remove(&k),
            };
        }
        Ok(rows.into_iter().take(limit).collect())
    }

    fn read_versions(
        &mut self,
        keys: &[Vec<u8>],
    ) -> Result<teseladb_wire::commands::Versions, CommandError> {
        self.now += 1;
        let ts = Hlc { physical: self.now, logical: 0 };
        let type_of = |k: &[u8]| self.types.get(k).copied().unwrap_or(1);
        Ok((ts, keys.iter().map(|k| self.data.get(k).map(|v| (type_of(k), v.clone()))).collect()))
    }

    fn transact(
        &mut self,
        _: Hlc,
        writes: Vec<(Vec<u8>, teseladb_range::intents::IntentOp)>,
        txn: Option<teseladb_range::intents::TxnId>,
    ) -> Result<Hlc, CommandError> {
        if self.conflicts > 0 {
            self.conflicts -= 1;
            return Err(CommandError::new(teseladb_wire::error::WRITE_CONFLICT, "conflict"));
        }
        self.check_fences(&writes.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>())?;
        self.now += 1;
        let ts = Hlc { physical: self.now, logical: 0 };
        let keys = writes.iter().map(|(k, _)| k.clone()).collect();
        for (k, op) in writes {
            match op {
                teseladb_range::intents::IntentOp::Put { value_type, value } => {
                    self.record(&k, ts, Some((value_type, value.clone())));
                    self.types.insert(k.clone(), value_type);
                    self.data.insert(k, value);
                }
                teseladb_range::intents::IntentOp::Delete => {
                    self.record(&k, ts, None);
                    self.types.remove(&k);
                    self.data.remove(&k);
                }
            }
        }
        self.txns.push((txn, keys));
        Ok(Hlc { physical: self.now, logical: 0 })
    }

    fn index_write(&mut self, cmd: IndexCommand) -> Result<(), CommandError> {
        self.index_log.push(cmd.clone());
        let changes = teseladb_meta::ddl_log::index_changes(&self.catalog, &self.indexes, &cmd);
        let live: Vec<u32> = self.catalog.collections.values().map(|c| c.id).collect();
        self.indexes.apply(&cmd, |c| live.contains(&c)).map(drop).map_err(|e| match e {
            CatalogError::NamespaceNotFound => CommandError::new(NAMESPACE_NOT_FOUND, "not found"),
            other => CommandError::new(INTERNAL_ERROR, format!("{other:?}")),
        })?;
        self.ddl.record_applied(self.mark(), changes, None);
        Ok(())
    }

    /// Every write goes through [`Backend::write`], where the test backend applies it.
    fn write_unchanged(&mut self, _op: &WriteOp) -> Option<Vec<u8>> {
        None
    }

    fn write(&mut self, op: WriteOp) -> Result<Vec<u8>, CommandError> {
        if let Some(e) = self.fail_next.take() {
            return Err(e);
        }
        match self.fail_write_at.take() {
            Some((0, e)) => return Err(e),
            Some((n, e)) => self.fail_write_at = Some((n - 1, e)),
            None => {}
        }
        if let teseladb_range::ack::W::Nodes(n) = op.wc.w
            && n > self.replicas
        {
            return Err(CommandError::new(UNSATISFIABLE_WRITE_CONCERN, "unsatisfiable"));
        }
        if let Some(k) = op.dedup
            && let Some(r) = self.dedup.get(&k)
        {
            return Ok(r.clone());
        }
        self.check_fences(&op.keys)?;
        let bound = Bound::decode(&op.mutation)?;
        assert_eq!(bound.latch_keys(), op.keys);
        self.concerns.push(op.wc);
        self.now += 1;
        let ts = Hlc { physical: self.now, logical: 0 };
        self.committed = Some(ts);
        // As the range host does: the keys the mutation asks to read join its keys, and it is
        // evaluated again with their current versions.
        let mut keys = op.keys.clone();
        let out = loop {
            let values: Vec<Option<Vec<u8>>> =
                keys.iter().map(|k| self.data.get(k).cloned()).collect();
            let current: Vec<Option<(u8, &[u8])>> =
                values.iter().map(|v| v.as_deref().map(|b| (1u8, b))).collect();
            let out = bound
                .eval_with(&keys, &current, ts)
                .map_err(|e| CommandError::from_mutation(&e))?;
            let unread: Vec<Vec<u8>> =
                bound.reads(&out).into_iter().filter(|k| !keys.contains(k)).collect();
            if unread.is_empty() {
                break out;
            }
            keys.extend(unread);
        };
        if let Some(split) = &self.split {
            let side = |k: &[u8]| k >= split.as_slice();
            let home = keys.first().is_some_and(|k| side(k));
            let reached = out
                .puts
                .iter()
                .map(|p| p.key.as_slice())
                .chain(out.deletes.iter().map(Vec::as_slice));
            if reached.into_iter().any(|k| side(k) != home) {
                let msg = format!("{}: past the split", teseladb_range::host::CROSSES_RANGES);
                return Err(CommandError::new(INTERNAL_ERROR, msg));
            }
        }
        self.writes += 1;
        for p in out.puts {
            self.record(&p.key, ts, Some((p.value_type, p.value.clone())));
            self.types.insert(p.key.clone(), p.value_type);
            self.data.insert(p.key, p.value);
        }
        for d in out.deletes {
            self.record(&d, ts, None);
            self.types.remove(&d);
            self.data.remove(&d);
        }
        if let Some(k) = op.dedup {
            self.dedup.insert(k, out.result.clone());
        }
        Ok(out.result)
    }

    fn scan_spans(
        &mut self,
        spans: &[teseladb_wire::commands::Span],
        concern: ReadConcern,
    ) -> Result<Vec<teseladb_wire::commands::Rows>, CommandError> {
        self.span_reads += 1;
        self.spans_batched += spans.len();
        spans
            .iter()
            .map(|s| match s.backward {
                true => self.scan_back(&s.start, &s.end, s.limit, concern),
                false => self.scan(&s.start, &s.end, s.limit, concern),
            })
            .collect()
    }

    fn scan(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        _: ReadConcern,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        self.scan_dir(start, end, limit, false)
    }

    fn scan_back(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        _: ReadConcern,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        self.scan_dir(start, end, limit, true)
    }

    fn scan_matching(
        &mut self,
        start: &[u8],
        end: &[u8],
        examine: usize,
        concern: ReadConcern,
        keep: &mut teseladb_wire::commands::Keep<'_>,
        _block: &dyn Fn(&dyn teseladb_keys::zone::Zones) -> bool,
    ) -> Result<teseladb_wire::commands::Matching, CommandError> {
        let range = (start, end);
        let out = teseladb_wire::commands::matching_by_scan(self, range, examine, concern, keep)?;
        self.matched += out.rows.len();
        Ok(out)
    }

    fn route_entries(&mut self) -> Result<Vec<teseladb_router::table::RouteEntry>, CommandError> {
        Ok(self.routes.clone())
    }

    fn range_live_bytes(
        &mut self,
        e: &teseladb_router::table::RouteEntry,
    ) -> Result<u64, CommandError> {
        // As a range's state machine counts them, without a scan.
        let live = |(k, v): (&Vec<u8>, &Vec<u8>)| (k.len() + 1 + v.len()) as u64;
        Ok(match (self.range_live.get(&e.group), e.end.is_empty()) {
            (Some(bytes), _) => *bytes,
            (None, true) => self.stored(e.start.clone()..).into_iter().map(live).sum(),
            (None, false) => {
                self.stored(e.start.clone()..e.end.clone()).into_iter().map(live).sum()
            }
        })
    }

    fn range_order(
        &mut self,
        cmd: teseladb_meta::command::Group0Command,
    ) -> Result<(), CommandError> {
        use teseladb_meta::command::Group0Command as C;
        if let Some(e) = self.refuse_orders.clone() {
            return Err(e);
        }
        self.range_orders.push(cmd.clone());
        if self.hold_orders {
            return Ok(());
        }
        let at = |routes: &[teseladb_router::table::RouteEntry], g| {
            routes.iter().position(|e| e.group == g)
        };
        match cmd {
            C::SplitRange { group, at: key } => {
                if let Some(i) = at(&self.routes, group) {
                    let next = self.routes.iter().map(|e| e.group.0).max().unwrap_or(0) + 1;
                    let mut right = self.routes[i].clone();
                    right.group = teseladb_types::GroupId(next);
                    right.start = key.clone();
                    right.moved_ms = self.now;
                    right.generation = 1;
                    self.routes[i].end = key;
                    self.routes[i].moved_ms = self.now;
                    self.routes[i].generation += 1;
                    self.routes.insert(i + 1, right);
                }
            }
            C::MergeRanges { left, right } => {
                if let (Some(l), Some(r)) = (at(&self.routes, left), at(&self.routes, right)) {
                    self.routes[l].end = self.routes[r].end.clone();
                    self.routes[l].moved_ms = self.routes[l].moved_ms.max(self.routes[r].moved_ms);
                    self.routes[l].generation += 1;
                    self.routes.remove(r);
                }
            }
            C::SetLeaderHint { group, node, .. } => {
                if let Some(i) = at(&self.routes, group) {
                    let e = &mut self.routes[i];
                    if e.leader_hint != Some(node) {
                        e.moved_ms = self.now;
                    }
                    e.leader_hint = Some(node);
                }
            }
            C::SetReplicas { group, epoch, add, remove } => {
                if let Some(i) = at(&self.routes, group) {
                    let e = &mut self.routes[i];
                    // Group 0 refuses a change named with another epoch (`RangeBusy`).
                    if e.replica_epoch != epoch {
                        return Err(CommandError::new(
                            teseladb_wire::error::CONFLICTING_OPERATION_IN_PROGRESS,
                            "Group 0 refused the change of the ranges: RangeBusy",
                        ));
                    }
                    e.replica_epoch += 1;
                    e.moved_ms = self.now;
                    e.voters.retain(|n| !remove.contains(n));
                    e.voters.extend(add);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn config_documents(&mut self, coll: &str) -> Result<Vec<Document>, CommandError> {
        Ok(match coll {
            teseladb_wire::commands::config_db::SHARDS => {
                // As Group 0 lists them: the name and the address of the node.
                let host = |s: &str| format!("{s}.cluster:27018");
                let shard = |s: &String| {
                    let d = Document::new().with("_id", s.as_str()).with("host", host(s));
                    if self.draining.contains(s) { d.with("draining", true) } else { d }
                };
                self.shards.iter().map(shard).collect()
            }
            teseladb_wire::commands::config_settings::SETTINGS => {
                self.settings.clone().unwrap_or_default()
            }
            "databases" => self.databases_docs.clone().unwrap_or_default(),
            teseladb_wire::commands::query_analysis::QUERY_ANALYZERS => {
                teseladb_wire::commands::query_analysis::of_settings(&self.group0)
            }
            COLLECTIONS => {
                let mut out = Vec::new();
                for (ns, c) in &self.catalog.collections {
                    out.extend(sharding::tracked_entry(ns, c)?);
                }
                out
            }
            _ => Vec::new(),
        })
    }

    fn group0_settings(
        &mut self,
        prefix: &[u8],
    ) -> Result<BTreeMap<Vec<u8>, Vec<u8>>, CommandError> {
        let all = self.group0.iter().filter(|(k, _)| k.starts_with(prefix));
        Ok(all.map(|(k, v)| (k.clone(), v.clone())).collect())
    }

    fn group0_settings_write(
        &mut self,
        changes: Vec<teseladb_meta::command::SettingChange>,
    ) -> Result<bool, CommandError> {
        if self.group0_conflicts > 0 {
            self.group0_conflicts -= 1;
            return Ok(false);
        }
        if changes.iter().any(|c| self.group0.get(&c.key) != c.expect.as_ref()) {
            return Ok(false);
        }
        for c in changes {
            match c.value {
                Some(v) => self.group0.insert(c.key, v),
                None => self.group0.remove(&c.key),
            };
        }
        Ok(true)
    }

    fn config_settings_write(
        &mut self,
        writes: Vec<teseladb_wire::commands::config_settings::SettingsWrite>,
    ) -> Result<bool, CommandError> {
        let Some(settings) = self.settings.as_mut() else {
            let msg = "this node keeps no cluster settings";
            return Err(CommandError::new(teseladb_wire::error::ILLEGAL_OPERATION, msg));
        };
        if self.settings_conflicts > 0 {
            self.settings_conflicts -= 1;
            return Ok(false);
        }
        let at = |settings: &[Document], id: &Value| {
            settings.iter().position(|d| d.get("_id") == Some(id))
        };
        // Every document as it was read, or none of the change goes.
        for w in &writes {
            if at(settings, &w.id).map(|i| &settings[i]) != w.read.as_ref() {
                return Ok(false);
            }
        }
        for w in writes {
            let i = at(settings, &w.id);
            match (i, w.now) {
                (Some(i), Some(d)) => settings[i] = d,
                (Some(i), None) => {
                    settings.remove(i);
                }
                (None, Some(d)) => settings.push(d),
                (None, None) => {}
            }
        }
        Ok(true)
    }

    fn kill_everywhere(&mut self, scope: &KillScope) -> Result<(), CommandError> {
        if let Some(e) = self.fail_kill.take() {
            return Err(e);
        }
        self.killed.push(scope.clone());
        Ok(())
    }

    fn other_operations(&mut self) -> Result<Vec<Document>, CommandError> {
        Ok(self.other_ops.clone())
    }

    fn cluster_parameters(&mut self) -> Result<BTreeMap<String, Vec<u8>>, CommandError> {
        Ok(self.params.clone())
    }

    fn set_cluster_parameter(&mut self, name: &str, value: Vec<u8>) -> Result<(), CommandError> {
        self.params.insert(name.to_owned(), value);
        Ok(())
    }

    fn fcv_limit(&mut self) -> Result<Option<String>, CommandError> {
        Ok(self.fcv_limit.clone())
    }

    fn ordered_ids(&mut self) -> Result<bool, CommandError> {
        Ok(self.ordered_ids)
    }

    fn set_read_mark(&mut self, at: Option<teseladb_txn::causal::ReadMark>) {
        self.read_mark = at;
    }

    fn read_mark(&self) -> Option<teseladb_txn::causal::ReadMark> {
        self.read_mark
    }

    fn set_deadline(&mut self, at_ms: Option<u64>) {
        self.deadline = at_ms;
        self.last_deadline = at_ms.or(self.last_deadline);
    }

    fn now_ms(&self) -> u64 {
        self.now
    }

    fn wall_ms(&self) -> u64 {
        self.now
    }

    fn now_micros(&self) -> u64 {
        self.mono.unwrap_or(self.now).saturating_mul(1000)
    }

    fn random(&mut self) -> u64 {
        self.rnd = self.rnd.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.rnd
    }

    fn node(&self) -> u64 {
        1
    }

    fn principal(
        &mut self,
        kind: Principal,
        db: &str,
        name: &str,
    ) -> Result<Option<Vec<u8>>, CommandError> {
        let table = if self.stale_security { &self.seen_security } else { &self.security };
        Ok(table.get(kind, db, name).map(<[u8]>::to_vec))
    }

    fn catalog_barrier(&mut self) -> Result<(), CommandError> {
        self.barriers += 1;
        self.seen_security = self.security.clone();
        Ok(())
    }

    fn principals(
        &mut self,
        kind: Principal,
        db: Option<&str>,
    ) -> Result<Vec<Vec<u8>>, CommandError> {
        let table = if self.stale_security { &self.seen_security } else { &self.security };
        Ok(table.list(kind, db).into_iter().map(<[u8]>::to_vec).collect())
    }

    fn security_write(
        &mut self,
        ops: Vec<SecurityOp>,
    ) -> Result<Result<(), SecurityError>, CommandError> {
        if self.fail_security_writes > 0 {
            if self.fail_security_writes != usize::MAX {
                self.fail_security_writes -= 1;
            }
            return Ok(Err(SecurityError::Conflict));
        }
        if let Some(n) = self.drop_before_write.take() {
            self.security.remove(Principal::User, &n.db, &n.user);
        }
        if let Some(k) = self.drop_role_before_write.take() {
            self.security.remove(Principal::Role, &k.0, &k.1);
        }
        Ok(self.security.apply(&ops))
    }

    fn authorization_lease(&self) -> bool {
        !self.lease_lost
    }

    fn authenticated_user(&self) -> Option<UserName> {
        self.user.clone()
    }

    fn privileges(&self) -> Option<Vec<teseladb_auth::privileges::Privilege>> {
        self.user_privileges.clone()
    }

    /// Deterministic bytes: the tests need nonces, not secrecy.
    fn invalidate_user_cache(&mut self) {
        self.invalidations += 1;
    }

    fn oidc_providers(&self) -> teseladb_auth::oidc::Providers {
        self.oidc
            .as_ref()
            .map(|(p, _)| teseladb_auth::oidc::Providers::parse(p).unwrap())
            .unwrap_or_default()
    }

    fn oidc_keys(
        &mut self,
        _issuer: &str,
        refresh: bool,
    ) -> Result<teseladb_auth::oidc::KeySet, CommandError> {
        self.jwks_reads.push(refresh);
        let (_, jwks) = self.oidc.as_ref().ok_or_else(|| {
            CommandError::new(teseladb_wire::error::OPERATION_FAILED, "no provider")
        })?;
        Ok(teseladb_auth::oidc::KeySet::parse(jwks).unwrap())
    }

    fn kerberos_keys(&self) -> Vec<teseladb_auth::kerberos::ServiceKey> {
        self.kerberos.clone()
    }

    fn aws_sts_url(&self) -> String {
        "https://sts.test:8443".to_owned()
    }

    fn aws_sts(
        &mut self,
        req: &teseladb_auth::aws::StsRequest,
    ) -> Result<(u16, Vec<u8>), CommandError> {
        self.sts_requests.push(req.clone());
        self.sts
            .clone()
            .ok_or_else(|| CommandError::new(teseladb_wire::error::OPERATION_FAILED, "unreachable"))
    }

    fn ldap_login(&mut self, user: &str, password: &[u8]) -> Result<LdapLogin, CommandError> {
        if !self.ldap.iter().any(|(u, p)| u == user && p == password) {
            return Ok(LdapLogin::Refused);
        }
        Ok(match &self.ldap_groups {
            None => LdapLogin::LoggedIn,
            Some(groups) => LdapLogin::Authorized(
                groups.iter().find(|(u, _)| u == user).map(|(_, g)| g.clone()).unwrap_or_default(),
            ),
        })
    }

    fn ldap_groups(&mut self, user: &str) -> Result<Option<Vec<RoleName>>, CommandError> {
        Ok(self.ldap_groups.as_ref().map(|groups| {
            groups.iter().find(|(u, _)| u == user).map(|(_, g)| g.clone()).unwrap_or_default()
        }))
    }

    fn secure_random(&mut self, out: &mut [u8]) -> Result<(), CommandError> {
        for b in out.iter_mut() {
            *b = self.random().to_le_bytes()[7];
        }
        Ok(())
    }

    fn log_warning(&mut self, id: i32, msg: &str, attr: &Document) {
        self.logged.push((id, msg.to_owned(), attr.clone()));
    }

    fn log_slow_query(&mut self, attr: &Document) {
        self.logged.push((
            teseladb_wire::commands::profile::SLOW_QUERY_ID,
            "Slow query".to_owned(),
            attr.clone(),
        ));
    }

    fn host_port(&mut self) -> String {
        format!("{}:{}", self.facts.host.hostname, self.facts.port)
    }

    fn process(&mut self) -> teseladb_wire::commands::diagnostics::ProcessFacts {
        self.facts.clone()
    }

    fn rotate_certificates(&mut self, message: Option<&str>) -> Result<(), CommandError> {
        if let Some(e) = self.fail_rotation.take() {
            return Err(e);
        }
        self.rotations.push(message.map(str::to_owned));
        Ok(())
    }
}

/// `{k: v, …}` from pairs.
pub fn doc(pairs: &[(&str, Value)]) -> Document {
    let mut d = Document::new();
    for (k, v) in pairs {
        d.push(*k, v.clone());
    }
    d
}

/// A field of a reply.
pub fn field<'a>(d: &'a Document, path: &str) -> &'a Value {
    let mut cur = d;
    let parts: Vec<&str> = path.split('.').collect();
    for (i, p) in parts.iter().enumerate() {
        let v = cur.get(p).unwrap_or_else(|| panic!("no {path} in {d:?}"));
        if i + 1 == parts.len() {
            return v;
        }
        match v {
            Value::Document(x) => cur = x,
            Value::Array(a) => {
                let idx: usize = parts[i + 1].parse().expect("an index");
                let rest = parts[i + 2..].join(".");
                let item = &a[idx];
                return match item {
                    Value::Document(x) if !rest.is_empty() => field(x, &rest),
                    other => other,
                };
            }
            _ => panic!("{path} crosses a scalar in {d:?}"),
        }
    }
    unreachable!("a path has a field")
}
