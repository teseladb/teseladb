//! The commands that read and write data and the catalog (FR-006 to FR-008, FR-016 to FR-018,
//! FR-026, FR-027), without I/O: they run against a [`Backend`] that reaches the catalog in
//! Group 0 and the ranges, and keep the node's cursors and sessions in [`NodeSessions`].
//!
//! - [`crud`]: `insert`, `update`, `delete`, `findAndModify` and `bulkWrite`.
//! - [`query`]: `find`, `getMore`, `killCursors`, `count` and `distinct`.
//! - [`catalog`]: `create`, `drop`, `renameCollection`, `collMod`, `listDatabases`,
//!   `listCollections` and `dropDatabase`.
//! - [`session`]: `endSessions`, `refreshSessions`, `killSessions`, `killAllSessions` and
//!   `startSession`.
//! - [`mutation`]: the writes as range mutations, evaluated on the range's leader.
//! - [`registry`]: the cluster's record of sessions, `config.system.sessions`.

pub mod admin;
pub mod aggregate;
pub mod args;
pub mod audit;
pub mod auth;
pub mod authorize;
pub mod authz_collections;
pub mod authz_writes;
pub mod capped;
pub mod catalog;
pub mod change_stream;
pub mod cluster_admin;
pub mod config_db;
pub mod config_settings;
pub mod cross_range;
pub mod crud;
pub mod diagnostics;
pub mod engines;
pub mod explain;
pub mod fcv;
pub mod hints;
pub mod index_build;
pub mod index_maintenance;
pub mod index_mod;
pub mod index_spec;
pub mod indexes;
pub mod kinds;
pub mod maintenance;
pub mod mutation;
pub mod names;
pub mod out_merge;
pub mod parameters;
pub mod partial_filter;
pub(crate) mod plan_cache;
pub(crate) mod plan_exec;
pub mod profile;
pub mod query;
pub mod registry;
pub mod roles;
pub mod search_indexes;
pub mod server_params;
pub mod session;
pub mod sort;
pub mod status;
pub mod tenants;
pub mod timeseries;
pub mod timeseries_filter;
pub mod ttl;
pub mod txn;
pub mod txn_concern;
pub mod txn_ddl;
pub mod txn_stats;
pub mod users;
pub mod users_info;
pub mod uuid;
pub mod views;
pub mod write_block;

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use teseladb_auth::privileges::Privilege;
use teseladb_auth::users::{RoleName, User, UserName};
use teseladb_bson::{Document, Value};
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::indexes::{IndexCommand, IndexEntry};
use teseladb_meta::security::{Principal, SecurityError, SecurityOp};
use teseladb_range::ack::WriteConcern;
use teseladb_range::read::ReadConcern;
use teseladb_session::cursor::{CURSOR_TIMEOUT_MS, CursorTable};
use teseladb_session::retry::StatementKey;
use teseladb_session::session::{SESSION_TIMEOUT_MS, SessionTable};

use crate::error::CommandError;
use crate::failpoint::FailPoint;
use crate::ops::OpTable;

/// A write for the range: its keys, the encoded [`mutation::Bound`], its statement and its write
/// concern.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteOp {
    /// Distinct keys the write latches.
    pub keys: Vec<Vec<u8>>,
    /// The encoded [`mutation::Bound`].
    pub mutation: Vec<u8>,
    /// The statement of a retryable write.
    pub dedup: Option<StatementKey>,
    /// Write concern.
    pub wc: WriteConcern,
}

/// What a read of a write's keys across ranges gives: the mark it read at and, for each key, its
/// value type and value (`None` without one).
pub type Versions = (teseladb_hlc::Hlc, Vec<Option<(u8, Vec<u8>)>>);

/// What the LDAP server answered to a PLAIN login (013 FR-025, FR-026).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LdapLogin {
    /// A wrong password, or a user the LDAP server does not know.
    Refused,
    /// Logged in; the roles are those of the user's document in `$external`.
    LoggedIn,
    /// Logged in with the roles of its LDAP groups (`security.ldap.authz.queryTemplate`): each
    /// group's DN, a role of `admin`.
    Authorized(Vec<RoleName>),
}

/// What a change stream read of an interval: the versions up to the resolved mark, and the mark.
pub type ChangeBatch = (Vec<teseladb_range::changes::Versioned>, teseladb_hlc::Hlc);

/// What the commands need from the node: Group 0's catalog, the ranges and the cluster time.
/// The server's implementation finds the leaders and forwards; errors come translated.
pub trait Backend {
    /// The collection of `ns`, as the node's copy of Group 0 has it.
    fn collection(&mut self, ns: &Namespace) -> Result<Option<CollectionEntry>, CommandError>;
    /// Every collection.
    fn collections(&mut self) -> Result<Vec<(Namespace, CollectionEntry)>, CommandError>;
    /// The databases of the catalog, with or without collections (D-077), in the order they were
    /// added.
    fn databases(&mut self) -> Result<Vec<String>, CommandError>;
    /// The mark of the Group 0 entry that added database `db`, if the node's copy knows it
    /// (008 FR-012).
    fn database_mark(&mut self, db: &str) -> Result<Option<teseladb_hlc::Hlc>, CommandError>;
    /// Runs a catalog command in Group 0 and waits until this node's copy applied it.
    fn catalog_write(
        &mut self,
        cmd: CatalogCommand,
    ) -> Result<Option<CollectionEntry>, CommandError>;
    /// The secondary indexes of collection `collection`, as the node's copy of Group 0 has them
    /// (009).
    fn indexes(&mut self, collection: u32) -> Result<Vec<IndexEntry>, CommandError>;
    /// The search indexes of collection `collection`, by name, as the node's copy of Group 0 has
    /// them (011).
    fn search_indexes(
        &mut self,
        collection: u32,
    ) -> Result<Vec<teseladb_meta::indexes::SearchIndexEntry>, CommandError>;
    /// Runs a change of the secondary indexes in Group 0 and waits until this node's copy
    /// applied it (009).
    fn index_write(&mut self, cmd: IndexCommand) -> Result<(), CommandError>;
    /// Runs a write; the mutation's encoded result.
    fn write(&mut self, op: WriteOp) -> Result<Vec<u8>, CommandError>;
    /// The newest committed versions of `keys` (value type and value, `None` without one) and a
    /// mark at which they are all current: what a write that spans ranges evaluates with
    /// (009 FR-003).
    fn read_versions(&mut self, keys: &[Vec<u8>]) -> Result<Versions, CommandError>;
    /// Writes `writes` as one internal transaction of 008 that reads at `read_ts`, as
    /// transaction `txn` or a new one: all or nothing. A version committed after `read_ts` on
    /// one of the keys is `WriteConflict`. Its commit mark.
    fn transact(
        &mut self,
        read_ts: teseladb_hlc::Hlc,
        writes: Vec<(Vec<u8>, teseladb_range::intents::IntentOp)>,
        txn: Option<teseladb_range::intents::TxnId>,
    ) -> Result<teseladb_hlc::Hlc, CommandError>;
    /// The range that holds `key`, for the coordinator of a client transaction (008), if the
    /// node knows one.
    fn txn_route(&self, key: &[u8]) -> Option<teseladb_types::GroupId>;
    /// What the state of a client transaction may take in its coordinator's memory before it
    /// fails with `TransactionTooLarge` (008 FR-010).
    fn txn_memory_budget(&self) -> usize;
    /// Waits a pause before the coordinator of a client transaction asks again (008): `false`
    /// once the operation's deadline passed.
    fn txn_pause(&mut self) -> bool;
    /// Waits until the wall clock ([`Backend::wall_ms`]) reaches `until_ms`, for a commit that
    /// answers once the clock is at its mark (008 FR-008): `false` once the operation's deadline
    /// passed.
    fn txn_wait_clock(&mut self, until_ms: u64) -> bool;
    /// Waits a pause before a command looks again for what it waits for (an `awaitData` cursor,
    /// 015): `false` once the operation's deadline passed.
    fn pause(&mut self) -> bool {
        self.txn_pause()
    }
    /// Sends request `op` of a client transaction to range `g` and waits for its answer, until
    /// the operation's deadline (008).
    fn txn_send(
        &mut self,
        g: teseladb_types::GroupId,
        op: teseladb_range::host::TxnOp,
    ) -> Result<teseladb_range::host::Reply, teseladb_range::host::RangeError>;
    /// Reads `key` at mark `at` with what the reader knows about transactions (`view`: its own
    /// transaction, whose intents it reads, and those it learned about): its value type and
    /// value, `None` without one (008 FR-003).
    fn txn_get(
        &mut self,
        key: &[u8],
        at: teseladb_hlc::Hlc,
        view: &teseladb_range::push::TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Option<(u8, Vec<u8>)>, CommandError>;
    /// Reads at most `limit` documents of `[start, end)` in key order at mark `at` with `view`,
    /// as [`Backend::txn_get`] reads one (008 FR-003).
    fn txn_scan(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        at: teseladb_hlc::Hlc,
        view: &teseladb_range::push::TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError>;
    /// The fewest voters of the ranges that hold `[start, end)`: what a `commitQuorum` may ask
    /// of them (009 FR-006).
    fn voters(&mut self, start: &[u8], end: &[u8]) -> Result<usize, CommandError>;
    /// The running operation ended: what it read of the catalog no longer holds back the
    /// changes of the indexes (009 FR-006). Every backend says what it does, so that a wrapper
    /// cannot drop the call.
    fn end_operation(&mut self);
    /// The plan a read of the running operation ran (`planSummary`, `execStats` and the hashes
    /// of its shape), for the profiler (010 FR-015). Every backend says what it does, so that a
    /// wrapper cannot drop the call.
    fn note_plan(&mut self, plan: Document);
    /// What a change stream reads of `[start, end)` after mark `from` (014 FR-003, FR-004): the
    /// versions of every range of the interval up to the lowest resolved mark of those ranges,
    /// and that mark, below which no version appears later.
    fn changes(
        &mut self,
        start: &[u8],
        end: &[u8],
        from: teseladb_hlc::Hlc,
    ) -> Result<ChangeBatch, CommandError>;
    /// Reads at most `limit` documents of `[start, end)` in key order: `(key, document)`.
    fn scan(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        concern: ReadConcern,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError>;
    /// Bytes of the keys and documents stored in `[start, end)`, read in chunks of
    /// [`query::SCAN_CHUNK`].
    fn stored_bytes(&mut self, start: &[u8], end: &[u8]) -> Result<u64, CommandError> {
        let mut next = start.to_vec();
        let mut total = 0u64;
        loop {
            let rows = self.scan(&next, end, query::SCAN_CHUNK, ReadConcern::Local)?;
            let short = rows.len() < query::SCAN_CHUNK;
            for (key, doc) in &rows {
                total += (key.len() + doc.len()) as u64;
            }
            match rows.last() {
                Some((key, _)) if !short => {
                    next = key.clone();
                    next.push(0);
                }
                _ => return Ok(total),
            }
        }
    }
    /// Kills the sessions of `scope` and their cursors on the other nodes of the cluster; a node
    /// alone has none.
    fn kill_everywhere(&mut self, _scope: &session::KillScope) -> Result<(), CommandError> {
        Ok(())
    }
    /// The operations in progress on the other nodes of the cluster, as `currentOp` describes
    /// them ([`admin::node_operations`], 010 T033); a node alone has no others.
    fn other_operations(&mut self) -> Result<Vec<Document>, CommandError> {
        Ok(Vec::new())
    }
    /// The documents of collection `coll` of `config` that the node generates from its copy of
    /// Group 0 (007 FR-017, [`config_db`]); a backend without a cluster has none.
    fn config_documents(&mut self, _coll: &str) -> Result<Vec<Document>, CommandError> {
        Ok(Vec::new())
    }
    /// Changes documents of `config.settings` in Group 0 together (007 FR-017,
    /// [`config_settings`]); `Ok(false)` if one of them is no longer as it was read. A backend
    /// without a cluster keeps no settings.
    fn config_settings_write(
        &mut self,
        _writes: Vec<config_settings::SettingsWrite>,
    ) -> Result<bool, CommandError> {
        Err(CommandError::new(
            crate::error::ILLEGAL_OPERATION,
            "this node keeps no cluster settings",
        ))
    }
    /// Asks the node to stop (`shutdown`, 007 FR-018); a backend that cannot stop refuses.
    fn request_shutdown(&mut self) -> Result<(), CommandError> {
        Err(CommandError::new(crate::error::ILLEGAL_OPERATION, "this node cannot be shut down"))
    }
    /// The cluster parameters as Group 0 keeps them: name and the BSON document of its value.
    fn cluster_parameters(&mut self) -> Result<BTreeMap<String, Vec<u8>>, CommandError>;
    /// The cluster's write gates (007 FR-029), from the cluster parameters unless the backend
    /// keeps them closer at hand.
    fn write_gate(&mut self) -> Result<write_block::WriteGate, CommandError> {
        Ok(write_block::gate_of(&self.cluster_parameters()?))
    }
    /// The tenant the running command works in (007 FR-025); the default one outside an
    /// operation, where a backend sees every tenant's databases with their qualified names.
    fn tenant(&self) -> u32 {
        teseladb_meta::tenants::DEFAULT_TENANT
    }
    /// The number of tenant `name` (007 FR-025); `None` if there is none.
    fn tenant_named(&mut self, _name: &str) -> Result<Option<u32>, CommandError> {
        Ok(None)
    }
    /// Whether every other live node of the cluster already applies write gates `want` (007
    /// FR-029: `fsync` and `setUserWriteBlockMode` answer once every node has them, as a `refrouter`
    /// answers once every shard did); a node alone has no other.
    fn gate_everywhere(&mut self, _want: write_block::WriteGate) -> Result<bool, CommandError> {
        Ok(true)
    }
    /// The highest feature compatibility version every node of the cluster runs (007 FR-027),
    /// if one runs less than this binary's latest.
    fn fcv_limit(&mut self) -> Result<Option<String>, CommandError> {
        Ok(None)
    }
    /// Sets a cluster parameter in Group 0 and waits until this node's copy applied it.
    fn set_cluster_parameter(&mut self, name: &str, value: Vec<u8>) -> Result<(), CommandError>;
    /// The time, on the monotonic clock of [`Backend::now_micros`] in milliseconds, at which the
    /// running command's `maxTimeMS` ends (`None`: no limit); the node waits for the catalog and
    /// the range no longer than that.
    fn set_deadline(&mut self, _at_ms: Option<u64>) {}

    /// The read preference of the command about to run (`$readPreference`, 007 FR-013); `None`:
    /// the leader. A backend without followers to read on ignores it.
    fn set_read_preference(&mut self, _pref: Option<teseladb_router::read_pref::ReadPreference>) {}
    /// The mark the reads of the command about to run are served at (`readConcern` `snapshot`
    /// or `afterClusterTime`, 008 FR-012, FR-013), and whether its reply gives it
    /// (`atClusterTime`); `None`: each read at its range's mark.
    fn set_read_mark(&mut self, at: Option<teseladb_txn::causal::ReadMark>);
    /// The mark [`Backend::set_read_mark`] set last.
    fn read_mark(&self) -> Option<teseladb_txn::causal::ReadMark>;
    /// Cluster time in milliseconds.
    fn now_ms(&self) -> u64;
    /// The node's wall clock in milliseconds, which no mark heard from another node raises: the
    /// clock a commit waits for before it answers (008 FR-008).
    fn wall_ms(&self) -> u64;
    /// Queues an audit event of the command for its client (013 FR-031); without an audit log,
    /// or outside a command, nothing.
    fn audit_event(&mut self, _atype: teseladb_auth::audit::AuditType, _param: Document) {}
    /// The node's clock as a mark: at or above every mark the node issued or received (008).
    fn cluster_mark(&self) -> teseladb_hlc::Hlc;
    /// A monotonic clock in microseconds, for the latencies of `serverStatus`.
    fn now_micros(&self) -> u64 {
        self.now_ms().saturating_mul(1000)
    }
    /// A random number (UUIDs, ObjectIds, cursor identifiers).
    fn random(&mut self) -> u64;
    /// This node's number.
    fn node(&self) -> u64;
    /// The document of user or role `name` of `db` as this node's copy of Group 0 has it
    /// (013 FR-013).
    fn principal(
        &mut self,
        kind: Principal,
        db: &str,
        name: &str,
    ) -> Result<Option<Vec<u8>>, CommandError>;
    /// The documents of the users or roles of `db` (of every database with `None`), in name
    /// order.
    fn principals(
        &mut self,
        kind: Principal,
        db: Option<&str>,
    ) -> Result<Vec<Vec<u8>>, CommandError>;
    /// Applies a batch of changes of the users and roles in Group 0, all or none, and waits
    /// until this node's copy applied it; the expectation that failed, if one did.
    fn security_write(
        &mut self,
        ops: Vec<SecurityOp>,
    ) -> Result<Result<(), SecurityError>, CommandError>;
    /// User `name`, decoded from [`Backend::principal`].
    fn user(&mut self, name: &UserName) -> Result<Option<User>, CommandError> {
        let Some(bytes) = self.principal(Principal::User, &name.db, &name.user)? else {
            return Ok(None);
        };
        let doc = Document::from(teseladb_bson::RawDoc::from_bytes(&bytes)?);
        match User::from_document(&doc) {
            Ok(u) => Ok(Some(u)),
            // A document whose `user` or `db` is not a string names no user (measured).
            Err(e) if e.code == teseladb_auth::users::USER_NOT_FOUND => Ok(None),
            Err(e) => Err(authz_collections::stored_error(e)),
        }
    }
    /// Fills `out` with random bytes of the operating system (the SCRAM nonces).
    fn secure_random(&mut self, out: &mut [u8]) -> Result<(), CommandError>;
    /// Whether `user` logs in to the node's LDAP server with `password` (PLAIN on `$external`,
    /// 013 FR-025), and its roles when the node authorizes by LDAP groups (FR-026); a node
    /// without `security.ldap.servers` logs in no one.
    fn ldap_login(&mut self, _user: &str, _password: &[u8]) -> Result<LdapLogin, CommandError> {
        Ok(LdapLogin::Refused)
    }
    /// The LDAP groups of `user`, who logged in another way (REFDB-X509), as roles of `admin`,
    /// when the node authorizes by LDAP groups (013 FR-026, FR-033); `None` when it does not.
    fn ldap_groups(&mut self, _user: &str) -> Result<Option<Vec<RoleName>>, CommandError> {
        Ok(None)
    }
    /// The roles of the running command's user when they come from LDAP groups (013 FR-026).
    fn authenticated_roles(&self) -> Option<Vec<RoleName>> {
        None
    }
    /// `invalidateUserCache`: forgets the LDAP groups kept of each user (013 FR-026).
    fn invalidate_user_cache(&mut self) {}
    /// The service keys of the node's keytab (GSSAPI, 013 FR-025); none by default.
    fn kerberos_keys(&self) -> Vec<teseladb_auth::kerberos::ServiceKey> {
        Vec::new()
    }
    /// `oidcIdentityProviders` (REFDB-OIDC, 013 FR-025); none by default.
    fn oidc_providers(&self) -> teseladb_auth::oidc::Providers {
        teseladb_auth::oidc::Providers::default()
    }
    /// The JWKS of the provider `issuer`; `refresh` reads it again (a key it does not know yet).
    fn oidc_keys(
        &mut self,
        _issuer: &str,
        _refresh: bool,
    ) -> Result<teseladb_auth::oidc::KeySet, CommandError> {
        Err(CommandError::new(crate::error::OPERATION_FAILED, "no JWKS"))
    }
    /// `awsSTSUrl`, where REFDB-AWS sends the client's request (013 FR-025).
    fn aws_sts_url(&self) -> String {
        teseladb_auth::aws::DEFAULT_STS_URL.to_owned()
    }
    /// Sends `req` to the STS: the answer's status and body. A node that cannot reach it fails
    /// the login.
    fn aws_sts(
        &mut self,
        _req: &teseladb_auth::aws::StsRequest,
    ) -> Result<(u16, Vec<u8>), CommandError> {
        Err(CommandError::new(crate::error::AUTHENTICATION_FAILED, "Authentication failed."))
    }
    /// The user the running command's connection authenticated as; none outside an operation.
    fn authenticated_user(&self) -> Option<UserName> {
        None
    }
    /// Whether the node holds an authorization lease (013 FR-012): without one its copy of the
    /// users and roles may be behind a revocation, and it authorizes nothing.
    fn authorization_lease(&self) -> bool;
    /// The privileges of the running command's user when the node authorizes commands (013);
    /// `None` without authorization or outside an operation.
    fn privileges(&self) -> Option<Vec<Privilege>> {
        None
    }
    /// Writes a warning line of the node's log, with RefDb's identifier `id`, its message and
    /// its attributes (the validation lines of `warn` and `errorAndLog`).
    fn log_warning(&mut self, id: i32, msg: &str, attr: &Document);
    /// Writes a «Slow query» line of the node's log (RefDb's identifier 51803, severity I) with
    /// attributes `attr` (010 FR-015).
    fn log_slow_query(&mut self, attr: &Document);
    /// Loads the node's certificates again (`rotateCertificates`, 013 FR-006): the new TLS
    /// settings serve the connections that come after, and on failure the old ones stay. A node
    /// without TLS has nothing to load. Every backend says what it does, so that a wrapper cannot
    /// drop the call.
    fn rotate_certificates(&mut self, message: Option<&str>) -> Result<(), CommandError>;
    /// What the diagnostic commands report of the process and its host; nothing by default (a
    /// node in tests).
    fn process(&mut self) -> diagnostics::ProcessFacts {
        diagnostics::ProcessFacts::default()
    }
    /// The CPU time the thread running the command has used, in nanoseconds (`cpuNanos` of the
    /// profiler, 010 FR-015); `None` where the node cannot tell (a node in tests).
    fn thread_cpu_nanos(&mut self) -> Option<u64> {
        None
    }
}

/// How long cursors and sessions live without use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    /// A cursor outside any session (`cursorTimeoutMillis`).
    pub cursor_ms: u64,
    /// A session (`localLogicalSessionTimeoutMinutes`).
    pub session_ms: u64,
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts { cursor_ms: CURSOR_TIMEOUT_MS, session_ms: SESSION_TIMEOUT_MS }
    }
}

/// Milliseconds between two reapings of a node's cursors and sessions
/// (`clientCursorMonitorFrequencySecs`, 4 on 9.0.4).
pub const REAP_INTERVAL_MS: u64 = 4_000;

/// The `uid` of a session without authentication, as RefDb writes it in messages: the
/// base64 of the SHA-256 of the empty user name.
pub const ANONYMOUS_UID: &str = "47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";

/// What a node's name in `config.shards` starts with, before its number.
pub const SHARD_PREFIX: &str = "node";

/// The name of node `node` as `config.shards` lists it (`_id`, 007 `config-db.md`): the shard a
/// client sees in a transaction's `recoveryToken` and in the errors a write gets as through a
/// `refrouter` (008).
pub fn shard_name(node: u64) -> String {
    format!("{SHARD_PREFIX}{node}")
}

/// The name a `refrouter` gives a shard's part of a reply (`raw`), here the node's:
/// `node<N>/host:port` (D-102).
pub fn raw_name<B: Backend + ?Sized>(backend: &mut B) -> String {
    let facts = backend.process();
    format!("{}/{}:{}", shard_name(backend.node()), facts.host.hostname, facts.port)
}

/// The host and port of the node, as a `refrouter` names the shard whose writes it could not
/// relay.
pub fn host_port<B: Backend + ?Sized>(backend: &mut B) -> String {
    let facts = backend.process();
    format!("{}:{}", facts.host.hostname, facts.port)
}

/// Error `e` of node `node` as a `refrouter` relays a shard's: «failed on: node<N> :: caused by ::»
/// before its message (D-102).
pub fn failed_on(node: u64, mut e: crate::error::CommandError) -> crate::error::CommandError {
    e.errmsg = format!("failed on: {} :: caused by :: {}", shard_name(node), e.errmsg);
    e
}

/// The cursors and sessions of a node, shared by its client workers.
#[derive(Debug, Default)]
pub struct NodeSessions {
    /// Open cursors.
    pub cursors: CursorTable<query::QueryCursor>,
    /// Sessions in use.
    pub sessions: SessionTable,
    /// The `failCommand` failpoint, with `enableTestCommands` only.
    pub failpoint: Option<FailPoint>,
    /// Commands in progress.
    pub ops: OpTable,
    /// The node's server parameters (`setParameter`).
    pub params: server_params::ServerParams,
    /// What the node counted of its commands (`serverStatus`).
    pub counters: diagnostics::OpCounters,
    /// The rest of what `serverStatus` reports (006 T046).
    pub status: status::StatusCounters,
    /// The audit log, on a node started with `--auditDestination` (013 FR-016).
    pub audit: Option<audit::Audit>,
    /// The sessions' client transactions (008 FR-009).
    pub txns: txn::Transactions,
    /// What the node's readers learned by pushing transactions (008 FR-003).
    pub pushes: teseladb_txn::push_cache::PushCache,
    /// Wakes the readers that wait for a push of [`NodeSessions::pushes`] on the way.
    pub push_settled: std::sync::Arc<std::sync::Condvar>,
    /// The collections the running transactions of the node created and have not committed
    /// (T019): the keeper drops the node's others.
    pub txn_scratch: std::collections::BTreeSet<Namespace>,
    /// Transactions that answered their commit or abort and whose intents are still to resolve
    /// (008 FR-005).
    pub txn_resolving: Vec<teseladb_txn::coordinator::Coordinator>,
    /// The bucket counters of each time series this node wrote, by collection (015 FR-010).
    pub timeseries: BTreeMap<u32, timeseries::TsStats>,
    /// The cluster's default concerns as this node last read them (007 FR-018).
    pub rw_defaults: cluster_admin::Defaults,
    /// The node's part of the ObjectIds it makes.
    pub object_ids: ObjectIds,
    /// The plans the node's reads chose, by shape (010 FR-002).
    pub plan_cache: plan_cache::PlanCache,
    /// The profiler's settings (010 FR-015).
    pub profiler: profile::Profiler,
}

/// What a node puts after the seconds of each ObjectId it makes (ObjectId specification): five
/// random bytes drawn once and a three-byte counter that grows from a random start. Both come
/// from the node's random source, so a simulated node with a seed makes the same ones.
#[derive(Clone, Copy, Debug, Default)]
pub struct ObjectIds {
    /// The random bytes and the next counter, once drawn.
    state: Option<([u8; 5], u32)>,
}

impl ObjectIds {
    /// The ObjectId made at `secs`; `random` draws the node's part the first time.
    pub fn next(&mut self, secs: u32, random: impl FnOnce() -> u64) -> [u8; 12] {
        let (bytes, counter) = *self.state.get_or_insert_with(|| {
            let r = random().to_be_bytes();
            ([r[0], r[1], r[2], r[3], r[4]], u32::from_be_bytes([0, r[5], r[6], r[7]]))
        });
        self.state = Some((bytes, (counter + 1) & 0x00ff_ffff));
        let mut id = [0u8; 12];
        id[..4].copy_from_slice(&secs.to_be_bytes());
        id[4..9].copy_from_slice(&bytes);
        id[9..].copy_from_slice(&counter.to_be_bytes()[1..]);
        id
    }
}

impl NodeSessions {
    /// The tables of a node started with `enableTestCommands`.
    pub fn with_test_commands(self) -> NodeSessions {
        NodeSessions { failpoint: Some(FailPoint::default()), ..self }
    }

    /// Empty tables with timeouts `t`.
    pub fn new(t: Timeouts) -> NodeSessions {
        NodeSessions {
            cursors: CursorTable::with_timeout(t.cursor_ms),
            sessions: SessionTable::with_timeout(t.session_ms),
            failpoint: None,
            profiler: profile::Profiler::default(),
            ops: OpTable::default(),
            counters: diagnostics::OpCounters::default(),
            status: status::StatusCounters::default(),
            audit: None,
            txns: txn::Transactions::new(),
            rw_defaults: cluster_admin::Defaults::default(),
            object_ids: ObjectIds::default(),
            plan_cache: plan_cache::PlanCache::default(),
            pushes: teseladb_txn::push_cache::PushCache::new(),
            push_settled: std::sync::Arc::default(),
            txn_scratch: std::collections::BTreeSet::new(),
            txn_resolving: Vec::new(),
            timeseries: BTreeMap::new(),
            params: server_params::ServerParams {
                cursor_timeout_ms: i64::try_from(t.cursor_ms).unwrap_or(i64::MAX),
                ..server_params::ServerParams::default()
            },
        }
    }
}

/// What one reaping ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reaped {
    /// Sessions that expired.
    pub sessions: Vec<u128>,
    /// Cursors killed: those of the expired sessions, then the idle ones outside any session.
    pub cursors: Vec<i64>,
}

/// Ends the sessions and cursors idle at cluster time `now_ms`; a session's cursors die with it,
/// also those opened with `noCursorTimeout`.
pub fn reap(shared: &Shared, now_ms: u64) -> Reaped {
    let mut g = lock(shared);
    let sessions = g.sessions.expire(now_ms);
    let mut cursors = g.cursors.kill_sessions(&sessions);
    let idle = g.cursors.expire(now_ms);
    let at = i64::try_from(now_ms).unwrap_or(i64::MAX);
    let jobs = &mut g.status.sessions;
    jobs.reapings += 1;
    jobs.reap_at = at;
    jobs.sessions_reaped = i64::try_from(sessions.len()).unwrap_or(i64::MAX);
    jobs.cursors_closed = i64::try_from(cursors.len() + idle.len()).unwrap_or(i64::MAX);
    g.status.cursors_timed_out += i64::try_from(idle.len()).unwrap_or(i64::MAX);
    cursors.extend(idle);
    Reaped { sessions, cursors }
}

/// The node's cursors and sessions behind their lock.
pub type Shared = Mutex<NodeSessions>;

/// Locks the node's cursors and sessions; a command that panicked with the lock held left them
/// consistent, since every change is one call.
pub fn lock(shared: &Shared) -> MutexGuard<'_, NodeSessions> {
    shared.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One command: its database, its document and where it runs.
pub struct Ctx<'a, B: Backend + ?Sized> {
    /// The node.
    pub backend: &'a mut B,
    /// Its cursors and sessions.
    pub shared: &'a Shared,
    /// `$db`.
    pub db: &'a str,
    /// The command.
    pub cmd: &'a Document,
}

impl<B: Backend + ?Sized> Ctx<'_, B> {
    /// The command's name.
    pub fn name(&self) -> &str {
        self.cmd.iter().next().map_or("", |(k, _)| k)
    }

    /// The value of the command's first field.
    pub fn first(&self) -> &Value {
        self.cmd.iter().next().map_or(&Value::Null, |(_, v)| v)
    }
}

/// The commands of this module.
const NAMES: [&str; 70] = [
    aggregate::AGGREGATE,
    plan_cache::PLAN_CACHE_CLEAR,
    explain::EXPLAIN,
    audit::LOG_APPLICATION_MESSAGE,
    authz_collections::MERGE_AUTHZ_COLLECTIONS,
    server_params::GET_PARAMETER,
    server_params::SET_PARAMETER,
    parameters::SET_CLUSTER_PARAMETER,
    parameters::GET_CLUSTER_PARAMETER,
    cluster_admin::GET_DEFAULT_RW_CONCERN,
    cluster_admin::SET_DEFAULT_RW_CONCERN,
    cluster_admin::FSYNC,
    write_block::FSYNC_UNLOCK,
    write_block::SET_USER_WRITE_BLOCK_MODE,
    fcv::SET_FCV,
    cluster_admin::COMPACT,
    cluster_admin::AUTO_COMPACT,
    cluster_admin::SHUTDOWN,
    admin::KILL_OP,
    admin::CURRENT_OP,
    admin::REPL_SET_GET_STATUS,
    crud::INSERT,
    crud::UPDATE,
    crud::DELETE,
    crud::FIND_AND_MODIFY,
    crud::FIND_AND_MODIFY_LEGACY,
    crud::BULK_WRITE,
    query::FIND,
    query::GET_MORE,
    query::KILL_CURSORS,
    query::COUNT,
    query::DISTINCT,
    catalog::CREATE,
    catalog::DROP,
    catalog::RENAME_COLLECTION,
    capped::CONVERT_TO_CAPPED,
    catalog::COLL_MOD,
    catalog::LIST_DATABASES,
    catalog::LIST_COLLECTIONS,
    catalog::LIST_INDEXES,
    indexes::CREATE_INDEXES,
    indexes::DROP_INDEXES,
    search_indexes::CREATE_SEARCH_INDEXES,
    search_indexes::LIST_SEARCH_INDEXES,
    search_indexes::UPDATE_SEARCH_INDEX,
    search_indexes::DROP_SEARCH_INDEX,
    catalog::DROP_DATABASE,
    session::END_SESSIONS,
    session::REFRESH_SESSIONS,
    session::KILL_SESSIONS,
    session::KILL_ALL_SESSIONS,
    session::START_SESSION,
    session::KILL_ALL_SESSIONS_BY_PATTERN,
    maintenance::DATA_SIZE,
    maintenance::LOG_ROTATE,
    maintenance::DROP_CONNECTIONS,
    maintenance::LOCK_INFO,
    maintenance::CONN_POOL_STATS,
    maintenance::IS_DB_GRID,
    maintenance::ROTATE_CERTIFICATES,
    diagnostics::BUILD_INFO,
    diagnostics::HOST_INFO,
    diagnostics::SERVER_STATUS,
    diagnostics::CONNECTION_STATUS,
    diagnostics::LIST_COMMANDS,
    diagnostics::GET_CMD_LINE_OPTS,
    diagnostics::GET_LOG,
    diagnostics::DB_STATS,
    diagnostics::COLL_STATS,
    profile::PROFILE,
];

/// Whether `name` is one of this module's commands.
pub fn is_data_command(name: &str) -> bool {
    NAMES.contains(&crate::concerns::canonical(name))
        || name == txn::COMMIT_TRANSACTION
        || name == txn::ABORT_TRANSACTION
        || users::NAMES.contains(&name)
        || roles::NAMES.contains(&name)
}

/// Runs the command of `ctx`; `None` if it is not one of this module's.
pub fn run<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Option<Result<Document, CommandError>> {
    if !is_data_command(ctx.name()) {
        return None;
    }
    Some(session_info(ctx).and_then(|()| {
        // The cluster's default concerns apply to the commands without their own (007 FR-018).
        let d = cluster_admin::current(ctx)?;
        crate::concerns::with_defaults(d.read, d.write, || command(ctx))
    }))
}

/// Reads the command's session fields, as every command does, and records the session's use.
fn session_info<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<(), CommandError> {
    let lsid = args::lsid(ctx.cmd)?;
    args::txn_number(ctx.cmd)?;
    if let Some(l) = lsid {
        let now = ctx.backend.now_ms();
        lock(ctx.shared).sessions.touch(l, now);
    }
    Ok(())
}

fn command<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    // A change stream refuses a buckets namespace as an internal one (014, measured).
    if let Value::String(coll) = ctx.first()
        && !change_stream::opens(ctx.cmd)
    {
        timeseries::legacy_buckets(ctx.db, coll, ctx.name())?;
    }
    match crate::concerns::canonical(ctx.name()) {
        audit::LOG_APPLICATION_MESSAGE => audit::log_application_message(ctx),
        authz_collections::MERGE_AUTHZ_COLLECTIONS => authz_collections::merge(ctx),
        parameters::SET_CLUSTER_PARAMETER => parameters::set_cluster_parameter(ctx),
        parameters::GET_CLUSTER_PARAMETER => parameters::get_cluster_parameter(ctx),
        cluster_admin::GET_DEFAULT_RW_CONCERN => cluster_admin::get_default_rw_concern(ctx),
        cluster_admin::SET_DEFAULT_RW_CONCERN => cluster_admin::set_default_rw_concern(ctx),
        cluster_admin::FSYNC => cluster_admin::fsync(ctx),
        write_block::FSYNC_UNLOCK => write_block::fsync_unlock(ctx),
        write_block::SET_USER_WRITE_BLOCK_MODE => write_block::set_user_write_block_mode(ctx),
        fcv::SET_FCV => fcv::set_fcv(ctx),
        cluster_admin::COMPACT => cluster_admin::compact(ctx),
        cluster_admin::AUTO_COMPACT => cluster_admin::auto_compact(ctx),
        cluster_admin::SHUTDOWN => cluster_admin::shutdown(ctx),
        server_params::GET_PARAMETER => server_params::get_parameter(ctx),
        server_params::SET_PARAMETER => server_params::set_parameter(ctx),
        admin::KILL_OP => admin::kill_op(ctx),
        diagnostics::BUILD_INFO => diagnostics::build_info(ctx),
        diagnostics::HOST_INFO => diagnostics::host_info(ctx),
        diagnostics::SERVER_STATUS => diagnostics::server_status(ctx),
        diagnostics::CONNECTION_STATUS => diagnostics::connection_status(ctx),
        diagnostics::LIST_COMMANDS => diagnostics::list_commands(ctx),
        diagnostics::GET_CMD_LINE_OPTS => diagnostics::get_cmd_line_opts(ctx),
        diagnostics::GET_LOG => diagnostics::get_log(ctx),
        diagnostics::DB_STATS => diagnostics::db_stats(ctx),
        diagnostics::COLL_STATS => diagnostics::coll_stats(ctx),
        profile::PROFILE => profile::profile(ctx),
        admin::CURRENT_OP => admin::current_op(ctx),
        admin::REPL_SET_GET_STATUS => admin::repl_set_get_status(ctx),
        crud::INSERT => crud::insert(ctx),
        crud::UPDATE => crud::update(ctx),
        crud::DELETE => crud::delete(ctx),
        crud::FIND_AND_MODIFY | crud::FIND_AND_MODIFY_LEGACY => crud::find_and_modify(ctx),
        crud::BULK_WRITE => crud::bulk_write(ctx),
        query::FIND => query::find(ctx),
        aggregate::AGGREGATE => aggregate::aggregate(ctx),
        plan_cache::PLAN_CACHE_CLEAR => plan_cache::plan_cache_clear(ctx),
        explain::EXPLAIN => explain::explain(ctx),
        query::GET_MORE => query::get_more(ctx),
        query::KILL_CURSORS => query::kill_cursors(ctx),
        query::COUNT => query::count(ctx),
        query::DISTINCT => query::distinct(ctx),
        catalog::CREATE => catalog::create(ctx),
        catalog::DROP => catalog::drop(ctx),
        catalog::RENAME_COLLECTION => catalog::rename(ctx),
        capped::CONVERT_TO_CAPPED => capped::convert_to_capped(ctx),
        catalog::COLL_MOD => catalog::coll_mod(ctx),
        catalog::LIST_DATABASES => catalog::list_databases(ctx),
        catalog::LIST_COLLECTIONS => catalog::list_collections(ctx),
        catalog::LIST_INDEXES => catalog::list_indexes(ctx),
        indexes::CREATE_INDEXES => indexes::create_indexes(ctx),
        indexes::DROP_INDEXES => indexes::drop_indexes(ctx),
        search_indexes::CREATE_SEARCH_INDEXES => search_indexes::create(ctx),
        search_indexes::LIST_SEARCH_INDEXES => search_indexes::list(ctx),
        search_indexes::UPDATE_SEARCH_INDEX => search_indexes::update(ctx),
        search_indexes::DROP_SEARCH_INDEX => search_indexes::drop(ctx),
        catalog::DROP_DATABASE => catalog::drop_database(ctx),
        session::END_SESSIONS => session::end_sessions(ctx),
        session::REFRESH_SESSIONS => session::refresh_sessions(ctx),
        session::KILL_SESSIONS => session::kill_sessions(ctx),
        session::KILL_ALL_SESSIONS => session::kill_all_sessions(ctx),
        session::START_SESSION => session::start_session(ctx),
        session::KILL_ALL_SESSIONS_BY_PATTERN => session::kill_all_sessions_by_pattern(ctx),
        maintenance::DATA_SIZE => maintenance::data_size(ctx),
        maintenance::LOG_ROTATE => maintenance::log_rotate(ctx),
        maintenance::DROP_CONNECTIONS => maintenance::drop_connections(ctx),
        maintenance::LOCK_INFO => maintenance::lock_info(ctx),
        maintenance::CONN_POOL_STATS => maintenance::conn_pool_stats(ctx),
        maintenance::IS_DB_GRID => maintenance::is_db_grid(ctx),
        maintenance::ROTATE_CERTIFICATES => maintenance::rotate_certificates(ctx),
        other if roles::NAMES.contains(&other) => {
            let name = other.to_owned();
            roles::run(ctx).unwrap_or_else(|| Err(not_found_error(&name)))
        }
        other if users::NAMES.contains(&other) => {
            let name = other.to_owned();
            users::run(ctx).unwrap_or_else(|| Err(not_found_error(&name)))
        }
        other => {
            Err(CommandError::new(crate::error::COMMAND_NOT_FOUND, format!("no such cmd: {other}")))
        }
    }
}

fn not_found_error(name: &str) -> CommandError {
    CommandError::new(crate::error::COMMAND_NOT_FOUND, format!("no such cmd: {name}"))
}

/// A value as RefDb writes it in messages (`1`, `"a"`, `ObjectId('…')`).
pub fn display(v: &Value) -> String {
    let d = Document::new().with("v", v.clone());
    match d.to_bytes() {
        Ok(b) => match teseladb_bson::RawDoc::from_bytes(&b).ok().and_then(|r| r.get("v")) {
            Some(raw) => teseladb_mql::shell(raw),
            None => String::new(),
        },
        Err(_) => String::new(),
    }
}

/// A UUID as text (`xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx`).
pub fn uuid_text(u: &[u8; 16]) -> String {
    let h: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// A session as RefDb names it in messages: its id and its user's `uid`.
pub fn session_text(lsid: u128) -> String {
    format!("{} - {ANONYMOUS_UID} -  - ", uuid_text(&lsid.to_be_bytes()))
}

/// A fresh UUID (version 4) from the backend's random numbers.
pub fn new_uuid<B: Backend + ?Sized>(b: &mut B) -> [u8; 16] {
    let mut u = [0u8; 16];
    u[..8].copy_from_slice(&b.random().to_be_bytes());
    u[8..].copy_from_slice(&b.random().to_be_bytes());
    u[6] = (u[6] & 0x0f) | 0x40;
    u[8] = (u[8] & 0x3f) | 0x80;
    u
}

/// A fresh ObjectId of the node: seconds of the cluster time, then the node's part
/// ([`ObjectIds`]).
pub fn new_object_id<B: Backend + ?Sized>(b: &mut B, shared: &Shared) -> [u8; 12] {
    let secs = u32::try_from(b.now_ms() / 1000).unwrap_or(u32::MAX);
    lock(shared).object_ids.next(secs, || b.random())
}
