//! Operations in progress (006 FR-020, FR-023): the `maxTimeMS` of each command, the default of
//! the cluster parameter `defaultMaxTimeMS` for reads, and the table of a node's running commands
//! that `currentOp` lists and `killOp` interrupts.
//!
//! A command runs against a [`Guarded`] backend: before each call to the node (the catalog, a
//! read or a write of the range) it checks whether its deadline passed or whether `killOp`
//! marked it, so an operation stops at its next step as RefDb's stops at its next interrupt
//! check.

use std::collections::BTreeMap;

use teseladb_auth::privileges::Privilege;
use teseladb_auth::users::{RoleName, UserName};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::indexes::{IndexCommand, IndexEntry};
use teseladb_meta::security::{Principal, SecurityError, SecurityOp};
use teseladb_range::read::ReadConcern;

use crate::commands::session::KillScope;
use crate::commands::tenants;
use crate::commands::{Backend, Shared, WriteOp, lock};
use crate::error::{BAD_VALUE, CommandError, FAILED_TO_PARSE, INTERRUPTED, MAX_TIME_MS_EXPIRED};
use crate::types::wrong_types;

/// The `maxTimeMS` field of every command.
pub const MAX_TIME_MS: &str = "maxTimeMS";
/// Largest `maxTimeMS` (`INT_MAX`).
pub const MAX_TIME_MS_LIMIT: i64 = i32::MAX as i64;
/// Message of an operation whose `maxTimeMS` expired.
pub const TIME_LIMIT_MESSAGE: &str = "operation exceeded time limit";
/// Message of an operation `killOp` interrupted.
pub const INTERRUPTED_MESSAGE: &str = "operation was interrupted";

/// The order in which a `refrouter` 9.0.4 lists the numeric types of `maxTimeMS` in its
/// `TypeMismatch` message, command by command (measured; the IDL of each command keeps its own
/// set, whose order is stable).
fn max_time_types(cmd: &str) -> [&'static str; 4] {
    const LONG_DOUBLE_INT_DECIMAL: [&str; 4] = ["long", "double", "int", "decimal"];
    const DECIMAL_LONG_DOUBLE_INT: [&str; 4] = ["decimal", "long", "double", "int"];
    const INT_DOUBLE_LONG_DECIMAL: [&str; 4] = ["int", "double", "long", "decimal"];
    const DOUBLE_LONG_DECIMAL_INT: [&str; 4] = ["double", "long", "decimal", "int"];
    const LONG_DECIMAL_INT_DOUBLE: [&str; 4] = ["long", "decimal", "int", "double"];
    const INT_DECIMAL_LONG_DOUBLE: [&str; 4] = ["int", "decimal", "long", "double"];
    match cmd {
        "insert" | "update" | "delete" | "findAndModify" | "findandmodify" | "buildInfo"
        | "buildinfo" => LONG_DOUBLE_INT_DECIMAL,
        "count" | "distinct" | "setClusterParameter" | "getClusterParameter" => {
            DECIMAL_LONG_DOUBLE_INT
        }
        "bulkWrite" | "ping" | "dropDatabase" => INT_DOUBLE_LONG_DECIMAL,
        "endSessions" | "refreshSessions" | "aggregate" => DOUBLE_LONG_DECIMAL_INT,
        "hostInfo" | "getCmdLineOpts" => LONG_DECIMAL_INT_DOUBLE,
        "getMore" | "connectionStatus" => INT_DECIMAL_LONG_DOUBLE,
        "find" => ["long", "decimal", "double", "int"],
        "killCursors" => ["decimal", "int", "double", "long"],
        "create" => ["double", "int", "long", "decimal"],
        "drop" => ["int", "double", "decimal", "long"],
        "renameCollection" => ["double", "decimal", "int", "long"],
        "listDatabases" => ["double", "decimal", "long", "int"],
        _ => ["decimal", "double", "int", "long"],
    }
}

fn out_of_range(n: i64) -> Option<CommandError> {
    if n < 0 {
        let msg = format!("BSON field '{MAX_TIME_MS}' value must be >= 0, actual value '{n}'");
        return Some(CommandError::new(BAD_VALUE, msg));
    }
    if n > MAX_TIME_MS_LIMIT {
        let msg = format!(
            "BSON field '{MAX_TIME_MS}' value must be <= {MAX_TIME_MS_LIMIT}, actual value '{n}'"
        );
        return Some(CommandError::new(BAD_VALUE, msg));
    }
    None
}

/// The integer value of a `maxTimeMS` field, as RefDb's `parseMaxTimeMS` reads it.
fn integer(v: &Value, cmd: &str) -> Result<i64, CommandError> {
    exact_integer(v, MAX_TIME_MS).unwrap_or_else(|| {
        Err(wrong_types(&format!("{cmd}.{MAX_TIME_MS}"), v, &max_time_types(cmd)))
    })
}

/// A number that must be a whole 64-bit integer, read as RefDb's `exactInt64` reads it
/// (`maxTimeMS`, `count`'s `limit`), with its messages naming `field`; `None` when `v` is not a
/// number.
pub fn exact_integer(v: &Value, field: &str) -> Option<Result<i64, CommandError>> {
    if !matches!(v, Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_)) {
        return None;
    }
    let d = Document::new().with(field, v.clone());
    let bytes = d.to_bytes().ok()?;
    let raw = RawDoc::from_bytes(&bytes).ok()?.get(field)?;
    Some(teseladb_mql::exact_int64(raw, field).map_err(|m| CommandError::new(FAILED_TO_PARSE, m)))
}

/// The command's `maxTimeMS`: `None` without one (or `null`), `Some(0)` for an explicit "no
/// limit", which also overrides `defaultMaxTimeMS`.
pub fn max_time_ms(cmd: &Document, name: &str) -> Result<Option<u64>, CommandError> {
    let v = match cmd.get(MAX_TIME_MS) {
        None | Some(Value::Null) => return Ok(None),
        Some(v) => v,
    };
    let n = integer(v, name)?;
    if let Some(e) = out_of_range(n) {
        return Err(e);
    }
    Ok(u64::try_from(n).ok())
}

/// The read commands `defaultMaxTimeMS` applies to (its documentation's list).
pub const DEFAULT_MAX_TIME_COMMANDS: [&str; 5] =
    ["aggregate", "count", "dbHash", "distinct", "find"];

/// The time limit of a command: its own `maxTimeMS`, or for a read without one the cluster's
/// `defaultMaxTimeMS.readOperations` unless the client may bypass it (`bypassDefaultMaxTimeMS`).
/// `None` means no limit.
pub fn effective_limit(
    name: &str,
    own: Option<u64>,
    default_ms: u64,
    bypass_default: bool,
) -> Option<u64> {
    match own {
        Some(0) => None,
        Some(ms) => Some(ms),
        None if bypass_default || default_ms == 0 => None,
        None if DEFAULT_MAX_TIME_COMMANDS.contains(&name) => Some(default_ms),
        None => None,
    }
}

/// `currentOp`'s `op` of a command.
pub fn op_type(name: &str) -> &'static str {
    match name {
        "find" => "query",
        "insert" => "insert",
        "update" => "update",
        "delete" => "remove",
        "getMore" => "getmore",
        _ => "command",
    }
}

/// A command in progress on the node.
#[derive(Clone, Debug, PartialEq)]
pub struct Op {
    /// Its `connectionId`.
    pub conn: i32,
    /// The client's address, when the server knows it.
    pub client: Option<String>,
    /// The connection's client metadata, from its first `hello`.
    pub metadata: Option<Document>,
    /// `client.application.name`.
    pub app_name: Option<String>,
    /// `currentOp`'s `op`.
    pub op: &'static str,
    /// Namespace (`db.coll`, or `db.$cmd`).
    pub ns: String,
    /// The command.
    pub command: Document,
    /// Cluster time at its start, in milliseconds.
    pub started_ms: u64,
    /// When its time limit ends, on the monotonic clock ([`Backend::now_micros`] in
    /// milliseconds): a length of time, which the cluster time would stretch or cut.
    pub deadline_ms: Option<u64>,
    /// `killOp` marked it.
    pub killed: bool,
    /// The user its connection authenticated as (013).
    pub user: Option<UserName>,
    /// The tenant it runs in (007 FR-025): its user's, or the one `$tenant` named.
    pub tenant: u32,
    /// The user's privileges when the node authorizes commands; `None` without authorization.
    pub privileges: Option<Vec<Privilege>>,
    /// The user's roles when they come from LDAP groups (013 FR-026); `None` when they are
    /// those of its user document.
    pub roles: Option<Vec<RoleName>>,
    /// `desc` of an operation of the node itself, such as an index build (009 FR-006); `None`
    /// for a client's, which is `conn<id>`.
    pub desc: Option<String>,
    /// How far an index build went (009 FR-006).
    pub progress: Option<teseladb_index::build::Progress>,
}

/// The node's commands in progress, by operation id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpTable {
    last: u32,
    ops: BTreeMap<u32, Op>,
}

impl OpTable {
    /// Records `op` and gives its id: the next free one after the last given, from 1, never 0.
    pub fn begin(&mut self, op: Op) -> u32 {
        let mut id = self.last;
        loop {
            id = id.checked_add(1).unwrap_or(1);
            if !self.ops.contains_key(&id) {
                break;
            }
        }
        self.last = id;
        self.ops.insert(id, op);
        id
    }

    /// The operation ended.
    pub fn end(&mut self, id: u32) {
        self.ops.remove(&id);
    }

    /// Marks operation `id` for interruption; whether it was running.
    pub fn kill(&mut self, id: u32) -> bool {
        match self.ops.get_mut(&id) {
            Some(op) => {
                op.killed = true;
                true
            }
            None => false,
        }
    }

    /// Operation `id`, while it runs.
    pub fn get(&self, id: u32) -> Option<&Op> {
        self.ops.get(&id)
    }

    /// Operation `id`, to change, while it runs.
    pub fn get_mut(&mut self, id: u32) -> Option<&mut Op> {
        self.ops.get_mut(&id)
    }

    /// Records `op` with the `desc` its id gives it (an operation of the node itself) and
    /// gives its id.
    pub fn begin_named(&mut self, op: Op, desc: impl FnOnce(u32) -> String) -> u32 {
        let id = self.begin(op);
        self.ops.entry(id).and_modify(|o| o.desc = Some(desc(id)));
        id
    }

    /// The operations in progress, by id.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &Op)> {
        self.ops.iter().map(|(k, v)| (*k, v))
    }
}

/// A backend whose every call first checks the operation's deadline and whether `killOp` marked
/// it.
pub struct Guarded<'a, B: Backend + ?Sized> {
    inner: &'a mut B,
    shared: &'a Shared,
    id: u32,
    deadline_ms: Option<u64>,
    /// The tenant the operation works in (007 FR-025): it sees its databases by their names,
    /// and the node by their qualified names.
    tenant: u32,
    /// The audit events of the catalog changes the operation made (013 FR-016).
    audit: crate::commands::audit::Pending,
}

impl<'a, B: Backend + ?Sized> Guarded<'a, B> {
    /// Operation `id` of `shared`'s table, over `inner`, with its deadline.
    pub fn new(inner: &'a mut B, shared: &'a Shared, id: u32) -> Guarded<'a, B> {
        let (deadline_ms, tenant) =
            lock(shared).ops.get(id).map_or((None, 0), |o| (o.deadline_ms, o.tenant));
        inner.set_deadline(deadline_ms);
        Guarded { inner, shared, id, deadline_ms, tenant, audit: Vec::new() }
    }

    /// The audit events of the catalog changes made so far, taken.
    pub fn take_audit(&mut self) -> crate::commands::audit::Pending {
        std::mem::take(&mut self.audit)
    }

    /// The error that stops the operation now, if any.
    pub fn check(&self) -> Result<(), CommandError> {
        if lock(self.shared).ops.get(self.id).is_some_and(|o| o.killed) {
            return Err(CommandError::new(INTERRUPTED, INTERRUPTED_MESSAGE));
        }
        match self.deadline_ms {
            Some(d) if self.inner.now_micros() / 1000 >= d => {
                Err(CommandError::new(MAX_TIME_MS_EXPIRED, TIME_LIMIT_MESSAGE))
            }
            _ => Ok(()),
        }
    }

    /// Runs a call of the node: the check before, and after a failure the check again, since a
    /// call that waited until the deadline failed because of it.
    fn call<T>(
        &mut self,
        f: impl FnOnce(&mut B) -> Result<T, CommandError>,
    ) -> Result<T, CommandError> {
        self.check()?;
        let r = f(self.inner);
        if r.is_err() {
            self.check()?;
        }
        r
    }
}

impl<B: Backend + ?Sized> Drop for Guarded<'_, B> {
    fn drop(&mut self) {
        self.inner.set_deadline(None);
        self.inner.end_operation();
    }
}

impl<B: Backend + ?Sized> Backend for Guarded<'_, B> {
    // A collection a transaction created and has not committed is no client's (008 T019).
    fn collection(&mut self, ns: &Namespace) -> Result<Option<CollectionEntry>, CommandError> {
        if crate::commands::txn_ddl::is_scratch(ns) {
            return Ok(None);
        }
        let (tenant, ns) = (self.tenant, tenants::qualified(self.tenant, ns));
        Ok(self.call(|b| b.collection(&ns))?.map(|c| tenants::owned(tenant, c)))
    }

    fn collections(&mut self) -> Result<Vec<(Namespace, CollectionEntry)>, CommandError> {
        let all = self.call(|b| b.collections())?;
        let ours = tenants::own_collections(self.tenant, all);
        Ok(ours.into_iter().filter(|(ns, _)| !crate::commands::txn_ddl::is_scratch(ns)).collect())
    }

    fn databases(&mut self) -> Result<Vec<String>, CommandError> {
        let all = self.call(|b| b.databases())?;
        Ok(tenants::own_databases(self.tenant, all))
    }

    fn database_mark(&mut self, db: &str) -> Result<Option<teseladb_hlc::Hlc>, CommandError> {
        let db = teseladb_meta::tenants::qualified_db(self.tenant, db);
        self.call(|b| b.database_mark(&db))
    }

    fn catalog_write(
        &mut self,
        cmd: CatalogCommand,
    ) -> Result<Option<CollectionEntry>, CommandError> {
        use crate::commands::audit;
        let tenant = self.tenant;
        let qualified = tenants::qualified_command(tenant, &cmd);
        if !audit::enabled(self.shared) {
            let entry = self.call(|b| b.catalog_write(qualified))?;
            return Ok(entry.map(|c| tenants::owned(tenant, c)));
        }
        let before = audit::before_catalog(self, &cmd)?;
        let entry = self.call(|b| b.catalog_write(qualified))?.map(|c| tenants::owned(tenant, c));
        self.audit.extend(audit::after_catalog(before, entry.as_ref()));
        Ok(entry)
    }

    fn tenant(&self) -> u32 {
        self.tenant
    }

    fn tenant_named(&mut self, name: &str) -> Result<Option<u32>, CommandError> {
        self.call(|b| b.tenant_named(name))
    }

    fn indexes(&mut self, collection: u32) -> Result<Vec<IndexEntry>, CommandError> {
        self.call(|b| b.indexes(collection))
    }

    fn search_indexes(
        &mut self,
        collection: u32,
    ) -> Result<Vec<teseladb_meta::indexes::SearchIndexEntry>, CommandError> {
        self.call(|b| b.search_indexes(collection))
    }

    fn voters(&mut self, start: &[u8], end: &[u8]) -> Result<usize, CommandError> {
        self.call(|b| b.voters(start, end))
    }

    fn read_versions(
        &mut self,
        keys: &[Vec<u8>],
    ) -> Result<crate::commands::Versions, CommandError> {
        self.call(|b| b.read_versions(keys))
    }

    fn transact(
        &mut self,
        read_ts: teseladb_hlc::Hlc,
        writes: Vec<(Vec<u8>, teseladb_range::intents::IntentOp)>,
        txn: Option<teseladb_range::intents::TxnId>,
    ) -> Result<teseladb_hlc::Hlc, CommandError> {
        self.call(|b| b.transact(read_ts, writes, txn))
    }

    fn end_operation(&mut self) {
        self.inner.end_operation();
    }

    fn set_read_preference(&mut self, pref: Option<teseladb_router::read_pref::ReadPreference>) {
        self.inner.set_read_preference(pref);
    }

    fn txn_route(&self, key: &[u8]) -> Option<teseladb_types::GroupId> {
        self.inner.txn_route(key)
    }

    fn txn_memory_budget(&self) -> usize {
        self.inner.txn_memory_budget()
    }

    fn cluster_mark(&self) -> teseladb_hlc::Hlc {
        self.inner.cluster_mark()
    }

    fn set_read_mark(&mut self, at: Option<teseladb_txn::causal::ReadMark>) {
        self.inner.set_read_mark(at);
    }

    fn read_mark(&self) -> Option<teseladb_txn::causal::ReadMark> {
        self.inner.read_mark()
    }

    fn txn_pause(&mut self) -> bool {
        self.check().is_ok() && self.inner.txn_pause()
    }

    fn txn_wait_clock(&mut self, until_ms: u64) -> bool {
        self.check().is_ok() && self.inner.txn_wait_clock(until_ms)
    }

    fn txn_send(
        &mut self,
        g: teseladb_types::GroupId,
        op: teseladb_range::host::TxnOp,
    ) -> Result<teseladb_range::host::Reply, teseladb_range::host::RangeError> {
        // A killed or expired operation stops before its next request.
        if self.check().is_err() {
            return Err(teseladb_range::host::RangeError::Timeout);
        }
        self.inner.txn_send(g, op)
    }

    fn txn_get(
        &mut self,
        key: &[u8],
        at: teseladb_hlc::Hlc,
        view: &teseladb_range::push::TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Option<(u8, Vec<u8>)>, CommandError> {
        self.call(|b| b.txn_get(key, at, view, uncertain_to))
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
        self.call(|b| b.txn_scan(start, end, limit, at, view, uncertain_to))
    }

    fn audit_event(&mut self, atype: teseladb_auth::audit::AuditType, param: Document) {
        if crate::commands::audit::enabled(self.shared) {
            self.audit.push((atype, param));
        }
    }

    fn index_write(&mut self, cmd: IndexCommand) -> Result<(), CommandError> {
        use crate::commands::audit;
        if !audit::enabled(self.shared) {
            return self.call(|b| b.index_write(cmd));
        }
        // The inner backend also sees the collections a transaction created (008 T019), whose
        // indexes are recorded under the client's name (013).
        let before = self.call(|b| audit::before_index(b, &cmd))?;
        self.call(|b| b.index_write(cmd))?;
        let shared = self.shared;
        let events = audit::after_index(self, shared, before)?;
        self.audit.extend(events);
        Ok(())
    }

    fn write(&mut self, op: WriteOp) -> Result<Vec<u8>, CommandError> {
        self.call(|b| b.write(op))
    }

    fn write_gate(&mut self) -> Result<crate::commands::write_block::WriteGate, CommandError> {
        self.call(|b| b.write_gate())
    }

    fn gate_everywhere(
        &mut self,
        want: crate::commands::write_block::WriteGate,
    ) -> Result<bool, CommandError> {
        self.call(|b| b.gate_everywhere(want))
    }

    fn fcv_limit(&mut self) -> Result<Option<String>, CommandError> {
        self.call(|b| b.fcv_limit())
    }

    fn scan(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        concern: ReadConcern,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        // A read that ran past the deadline stops the operation once it returns; a write
        // that returned is done and keeps its result.
        let rows = self.call(|b| b.scan(start, end, limit, concern))?;
        self.check()?;
        Ok(rows)
    }

    fn kill_everywhere(&mut self, scope: &KillScope) -> Result<(), CommandError> {
        self.inner.kill_everywhere(scope)
    }

    fn config_documents(&mut self, coll: &str) -> Result<Vec<Document>, CommandError> {
        let all = self.call(|b| b.config_documents(coll))?;
        Ok(tenants::own_config_documents(self.tenant, all))
    }

    fn config_settings_write(
        &mut self,
        writes: Vec<crate::commands::config_settings::SettingsWrite>,
    ) -> Result<bool, CommandError> {
        self.call(|b| b.config_settings_write(writes))
    }

    fn request_shutdown(&mut self) -> Result<(), CommandError> {
        self.call(|b| b.request_shutdown())
    }

    fn cluster_parameters(&mut self) -> Result<BTreeMap<String, Vec<u8>>, CommandError> {
        self.call(|b| b.cluster_parameters())
    }

    fn set_cluster_parameter(&mut self, name: &str, value: Vec<u8>) -> Result<(), CommandError> {
        self.call(|b| b.set_cluster_parameter(name, value))
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
        self.call(|b| b.principal(kind, db, name))
    }

    fn principals(
        &mut self,
        kind: Principal,
        db: Option<&str>,
    ) -> Result<Vec<Vec<u8>>, CommandError> {
        self.call(|b| b.principals(kind, db))
    }

    fn security_write(
        &mut self,
        ops: Vec<SecurityOp>,
    ) -> Result<Result<(), SecurityError>, CommandError> {
        self.call(|b| b.security_write(ops))
    }

    fn secure_random(&mut self, out: &mut [u8]) -> Result<(), CommandError> {
        self.inner.secure_random(out)
    }

    fn authenticated_user(&self) -> Option<UserName> {
        lock(self.shared).ops.get(self.id).and_then(|o| o.user.clone())
    }

    fn authorization_lease(&self) -> bool {
        self.inner.authorization_lease()
    }

    fn privileges(&self) -> Option<Vec<Privilege>> {
        lock(self.shared).ops.get(self.id).and_then(|o| o.privileges.clone())
    }

    fn invalidate_user_cache(&mut self) {
        self.inner.invalidate_user_cache();
    }

    fn authenticated_roles(&self) -> Option<Vec<RoleName>> {
        lock(self.shared).ops.get(self.id).and_then(|o| o.roles.clone())
    }

    fn process(&mut self) -> crate::commands::diagnostics::ProcessFacts {
        self.inner.process()
    }

    fn log_warning(&mut self, id: i32, msg: &str, attr: &teseladb_bson::Document) {
        self.inner.log_warning(id, msg, attr);
    }

    fn rotate_certificates(&mut self, message: Option<&str>) -> Result<(), CommandError> {
        self.inner.rotate_certificates(message)
    }
}

/// Name of the cluster parameter with the default time limit of reads.
pub const DEFAULT_MAX_TIME_MS: &str = "defaultMaxTimeMS";
/// Its field with the limit, in milliseconds.
pub const READ_OPERATIONS: &str = "readOperations";

/// `defaultMaxTimeMS.readOperations` of the cluster parameters (0: no default).
pub fn default_read_ms(params: &BTreeMap<String, Vec<u8>>) -> u64 {
    params
        .get(DEFAULT_MAX_TIME_MS)
        .and_then(|b| RawDoc::from_bytes(b).ok().map(Document::from))
        .and_then(|d| d.get(READ_OPERATIONS).and_then(crate::types::number))
        .map_or(0, |ms| ms.max(0.0) as u64)
}

/// The namespace `currentOp` shows for a command: `db.coll` when its first field names a
/// collection, `db.$cmd.aggregate` for an `aggregate: 1`, `db.$cmd` otherwise.
pub fn op_ns(db: &str, cmd: &Document) -> String {
    match cmd.iter().next() {
        Some((_, Value::String(coll))) if !coll.is_empty() => format!("{db}.{coll}"),
        // An `aggregate: 1` runs on the database's own namespace (measured on a `refrouter` 9.0.4).
        Some(("aggregate", _)) => format!("{db}.$cmd.aggregate"),
        _ => format!("{db}.$cmd"),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn ids_wrap_past_the_last_and_skip_those_in_use() {
        let op = Op {
            conn: 1,
            client: None,
            metadata: None,
            app_name: None,
            op: op_type("find"),
            ns: String::new(),
            command: Document::new(),
            started_ms: 0,
            deadline_ms: None,
            killed: false,
            user: None,
            tenant: 0,
            privileges: None,
            roles: None,
            desc: None,
            progress: None,
        };
        let mut t = OpTable { last: u32::MAX - 1, ops: BTreeMap::new() };
        t.ops.insert(1, op.clone());
        assert_eq!(t.begin(op.clone()), u32::MAX);
        assert_eq!(t.begin(op), 2);
    }
}
