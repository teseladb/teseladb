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
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::indexes::{IndexCommand, IndexEntry};
use teseladb_meta::security::{Principal, SecurityError, SecurityOp};
use teseladb_range::read::ReadConcern;
use teseladb_bson::{Document, RawDoc, Value};

use crate::commands::session::KillScope;
use crate::commands::tenants;
use crate::commands::{Backend, Shared, WriteOp};
use crate::error::{BAD_VALUE, CommandError, FAILED_TO_PARSE, INTERRUPTED, MAX_TIME_MS_EXPIRED};
use crate::types::wrong_types;
use teseladb_range::ack::WriteConcern;

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

/// The index a data key belongs to (`01 · tenant · collection · index`): 0 for a document.
pub(crate) fn space_of(key: &[u8]) -> Option<u32> {
    match key {
        [teseladb_keys::space::DATA, _, _, _, _, _, _, _, _, a, b, c, d, ..] => {
            Some(u32::from_be_bytes([*a, *b, *c, *d]))
        }
        _ => None,
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
    /// The flag `killOp` also raises, for the work outside the backend's calls (JavaScript reads
    /// it every few thousand steps); the running thread's, set by [`Guarded`].
    pub interrupt: Option<Interrupt>,
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
    /// What `currentOp` shows of an operation of the node with metrics of its own, such as a
    /// resharding (018 FR-002): fields added to its document.
    pub metrics: Option<Document>,
}

/// A plan's report built only when something reads it: the profiler, the slow query log, a
/// sampled query's statistics or a cursor that stays open. RefDb builds a plan's summary from
/// its executor when it profiles or logs the operation, not for every read (H-035 of the native
/// tests).
#[derive(Clone)]
pub struct PendingPlan(std::sync::Arc<dyn Fn() -> Document + Send + Sync>);

impl PendingPlan {
    /// The report `make` builds.
    pub fn new(make: impl Fn() -> Document + Send + Sync + 'static) -> PendingPlan {
        PendingPlan(std::sync::Arc::new(make))
    }

    /// Builds the report.
    pub fn render(&self) -> Document {
        (self.0)()
    }
}

impl std::fmt::Debug for PendingPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PendingPlan")
    }
}

impl PartialEq for PendingPlan {
    fn eq(&self, other: &PendingPlan) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

/// What an operation read and how it planned its read, for the profiler (010 FR-015).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpReport {
    /// Index entries it read (`keysExamined`).
    pub keys: u64,
    /// Documents it read (`docsExamined`).
    pub docs: u64,
    /// The plan its read ran, as the planner reported it: `planSummary`, `execStats` and the
    /// hashes of its shape.
    pub plan: Option<Document>,
    /// Index keys its writes added and removed (`keysInserted`, `keysDeleted`).
    pub keys_written: (u64, u64),
    /// The indexes of its collection the winning plans of its writes scanned (`$indexStats`;
    /// a read counts its own uses).
    pub used: Vec<String>,
    /// The plan's report still to build, in place of [`OpReport::plan`].
    pub pending: Option<PendingPlan>,
}

impl OpReport {
    /// Adds `o`, a later part of the same operation's report: its plan, when it has one,
    /// replaces this one's.
    pub fn absorb(&mut self, o: OpReport) {
        self.keys += o.keys;
        self.docs += o.docs;
        self.keys_written.0 += o.keys_written.0;
        self.keys_written.1 += o.keys_written.1;
        self.used.extend(o.used);
        if o.plan.is_some() || o.pending.is_some() {
            self.plan = o.plan;
            self.pending = o.pending;
        }
    }

    /// Builds the plan's report if it is still pending.
    pub fn resolve_plan(&mut self) {
        if let Some(p) = self.pending.take() {
            self.plan = Some(p.render());
        }
    }
}

/// What an operation has examined so far (`$queryStats`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpReads {
    /// Index entries it read.
    pub keys: u64,
    /// Documents it read.
    pub docs: u64,
}

/// The node's commands in progress, by operation id.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OpTable {
    last: u32,
    ops: BTreeMap<u32, Op>,
    reports: BTreeMap<u32, OpReport>,
    /// Each operation's own flag of `killOp`, which its calls read without this table's lock
    /// (H-013 of the native tests: every call of every worker took the node's lock to read
    /// `killed`; RefDb's `OperationContext` keeps its kill code in an atomic the same way).
    cancels: BTreeMap<u32, Interrupt>,
}

/// The flag `killOp` raises for one operation.
#[derive(Clone, Debug, Default)]
pub struct Interrupt(pub std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Interrupt {
    /// Whether `killOp` raised it.
    #[must_use]
    pub fn raised(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl PartialEq for Interrupt {
    /// Raised or not alike.
    fn eq(&self, other: &Interrupt) -> bool {
        self.raised() == other.raised()
    }
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
        self.insert_at(id, op);
        id
    }

    /// Records `op` under `id`, which no running operation has ([`Ops::begin`] chose it).
    fn insert_at(&mut self, id: u32, op: Op) {
        let cancel = Interrupt::default();
        cancel.0.store(op.killed, std::sync::atomic::Ordering::SeqCst);
        self.cancels.insert(id, cancel);
        self.ops.insert(id, op);
    }

    /// The operation ended.
    pub fn end(&mut self, id: u32) {
        self.ops.remove(&id);
        self.reports.remove(&id);
        self.cancels.remove(&id);
    }

    /// The flag `killOp` raises for operation `id`; a flag nobody raises for one not running.
    pub fn cancel(&self, id: u32) -> Interrupt {
        self.cancels.get(&id).cloned().unwrap_or_default()
    }

    /// What operation `id` has read and planned so far.
    pub fn report_mut(&mut self, id: u32) -> &mut OpReport {
        self.reports.entry(id).or_default()
    }

    /// What operation `id` read and planned, taken.
    pub fn take_report(&mut self, id: u32) -> OpReport {
        self.reports.remove(&id).unwrap_or_default()
    }

    /// Marks operation `id` for interruption; whether it was running.
    pub fn kill(&mut self, id: u32) -> bool {
        match self.ops.get_mut(&id) {
            Some(op) => {
                op.killed = true;
                if let Some(c) = self.cancels.get(&id) {
                    c.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                if let Some(f) = &op.interrupt {
                    f.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
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

/// Stripes of the node's operations: operation `id` lives in stripe `id % OP_STRIPES`.
const OP_STRIPES: usize = 16;

/// The node's commands in progress, in stripes that each take their own lock (H-013 of the native
/// tests): an operation begins and ends taking only its stripe's, not the node's, and with 8
/// connections the node's lock was where their threads queued. `currentOp` and `killOp` go
/// through the stripes; RefDb likewise registers each operation with its client, under that
/// client's own lock, and `currentOp` visits the clients one by one.
#[derive(Debug, Default)]
pub struct Ops {
    /// The last id given.
    last: std::sync::atomic::AtomicU32,
    stripes: [std::sync::Mutex<OpTable>; OP_STRIPES],
}

impl Ops {
    fn stripe(&self, id: u32) -> std::sync::MutexGuard<'_, OpTable> {
        let i = usize::try_from(id).unwrap_or(0) % OP_STRIPES;
        // A thread that panicked while holding a stripe left it as it was: still usable.
        self.stripes[i].lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Records `op` and gives its id: the next one after the last given, from 1, never 0, and
    /// never one still in use.
    pub fn begin(&self, op: Op) -> u32 {
        loop {
            let id = self.last.fetch_add(1, std::sync::atomic::Ordering::Relaxed).wrapping_add(1);
            if id == 0 {
                continue;
            }
            let mut t = self.stripe(id);
            if t.get(id).is_none() {
                t.insert_at(id, op);
                return id;
            }
        }
    }

    /// Records `op` with the `desc` its id gives it (an operation of the node itself) and
    /// gives its id.
    pub fn begin_named(&self, op: Op, desc: impl FnOnce(u32) -> String) -> u32 {
        let id = self.begin(op);
        if let Some(o) = self.stripe(id).get_mut(id) {
            o.desc = Some(desc(id));
        }
        id
    }

    /// Runs `f` on the stripe that holds operation `id`.
    pub fn with<R>(&self, id: u32, f: impl FnOnce(&mut OpTable) -> R) -> R {
        f(&mut self.stripe(id))
    }

    /// The operation ended.
    pub fn end(&self, id: u32) {
        self.stripe(id).end(id);
    }

    /// Marks operation `id` for interruption; whether it was running.
    pub fn kill(&self, id: u32) -> bool {
        self.stripe(id).kill(id)
    }

    /// The operations in progress, by id.
    pub fn snapshot(&self) -> Vec<(u32, Op)> {
        let mut all: Vec<(u32, Op)> = Vec::new();
        for i in 0..OP_STRIPES {
            let t = self.stripes[i].lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            all.extend(t.iter().map(|(id, op)| (id, op.clone())));
        }
        all.sort_unstable_by_key(|(id, _)| *id);
        all
    }
}

/// A backend whose every call first checks the operation's deadline and whether `killOp` marked
/// it.
pub struct Guarded<'a, B: Backend + ?Sized> {
    inner: &'a mut B,
    shared: &'a Shared,
    id: u32,
    deadline_ms: Option<u64>,
    /// The operation's flag of `killOp`, read by every call without the table's lock.
    cancel: Interrupt,
    /// The user its connection authenticated as, which does not change while it runs: read
    /// once, with the rest of its entry (H-013).
    user: Option<UserName>,
    /// What the operation read, planned and wrote so far: only it writes it, so it keeps it and
    /// hands it to the table once, when it ends, instead of taking the node's lock on every read
    /// (H-013).
    report: OpReport,
    /// The tenant the operation works in (007 FR-025): it sees its databases by their names,
    /// and the node by their qualified names.
    tenant: u32,
    /// The audit events of the catalog changes the operation made (013 FR-016).
    audit: crate::commands::audit::Pending,
    /// Whether the thread's interrupt flag was raised before this operation (an outer one's),
    /// put back at its end.
    outer_interrupt: bool,
}

impl<'a, B: Backend + ?Sized> Guarded<'a, B> {
    /// Operation `id` of `shared`'s table, over `inner`, with its deadline.
    pub fn new(inner: &'a mut B, shared: &'a Shared, id: u32) -> Guarded<'a, B> {
        Self::under(inner, shared, id)
    }

    /// Records `op` in `shared`'s table and runs it over `inner`, taking only the lock of its
    /// stripe of the table (H-013).
    pub fn begin(inner: &'a mut B, shared: &'a Shared, op: Op) -> Guarded<'a, B> {
        let id = crate::commands::hot::of(shared).ops.begin(op);
        Self::under(inner, shared, id)
    }

    /// The operation's id in its table.
    pub fn id(&self) -> u32 {
        self.id
    }

    fn under(inner: &'a mut B, shared: &'a Shared, id: u32) -> Guarded<'a, B> {
        let flag = crate::commands::js::thread_interrupt();
        let outer_interrupt = flag.raised();
        let ((deadline_ms, tenant, user), cancel) =
            crate::commands::hot::of(shared).ops.with(id, |t| {
                let entry = t
                    .get(id)
                    .map_or((None, 0, None), |o| (o.deadline_ms, o.tenant, o.user.clone()));
                let killed = t.get(id).is_some_and(|o| o.killed);
                flag.0.store(killed, std::sync::atomic::Ordering::SeqCst);
                if let Some(op) = t.get_mut(id) {
                    op.interrupt = Some(flag);
                }
                (entry, t.cancel(id))
            });
        inner.set_deadline(deadline_ms);
        Guarded {
            inner,
            shared,
            id,
            deadline_ms,
            cancel,
            user,
            report: OpReport::default(),
            tenant,
            audit: Vec::new(),
            outer_interrupt,
        }
    }

    /// What the operation read, planned and wrote so far, taken: the caller hands it to the
    /// table with the end of the operation, in one take of the node's lock.
    pub fn take_report(&mut self) -> OpReport {
        std::mem::take(&mut self.report)
    }

    /// The audit events of the catalog changes made so far, taken.
    pub fn take_audit(&mut self) -> crate::commands::audit::Pending {
        std::mem::take(&mut self.audit)
    }

    /// Counts the index entries and documents among `rows` for the operation's report: a
    /// document is a row of a collection's documents, an index entry a row of one of its indexes
    /// (also the `_id` entries of a capped collection), and the rest (multikey marks, guards of
    /// `prepareUnique`, the node's own tables) is neither.
    fn tally(&mut self, rows: &[(Vec<u8>, Vec<u8>)]) {
        let (mut keys, mut docs) = (0u64, 0u64);
        for (key, _) in rows {
            match space_of(key) {
                Some(teseladb_keys::space::DOCUMENTS) => docs += 1,
                Some(i)
                    if i < crate::commands::index_maintenance::MULTIKEY_MARKS
                        || i == teseladb_range::capped::ID_INDEX =>
                {
                    keys += 1;
                }
                _ => {}
            }
        }
        self.report.keys += keys;
        self.report.docs += docs;
    }

    /// The error that stops the operation now, if any.
    pub fn check(&self) -> Result<(), CommandError> {
        if self.cancel.raised() {
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
    /// What a write's index keys were, for the profiler (a result that does not read as a
    /// mutation's counts nothing).
    fn note_keys_written(&mut self, result: &[u8]) {
        if let Ok(r) = crate::commands::mutation::MutationResult::decode(result)
            && r.keys != (0, 0)
        {
            let k = &mut self.report.keys_written;
            k.0 += u64::try_from(r.keys.0).unwrap_or(0);
            k.1 += u64::try_from(r.keys.1).unwrap_or(0);
        }
    }

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

impl<B: Backend + ?Sized> Guarded<'_, B> {
    /// A collection or a database a tenant creates, explicitly or by writing, within its
    /// `maxCollections` and `maxDatabases` (007 FR-031): 14031 past them, since RefDb has no
    /// code of its own for it.
    fn check_tenant_quotas(&mut self, cmd: &CatalogCommand) -> Result<(), CommandError> {
        let CatalogCommand::Create { ns, .. } = cmd else { return Ok(()) };
        if self.tenant == 0 {
            return Ok(());
        }
        let Some(limits) = self.inner.tenant_state(self.tenant).map(|s| s.limits) else {
            return Ok(());
        };
        if limits.max_collections == 0 && limits.max_databases == 0 {
            return Ok(());
        }
        if self.collection(ns)?.is_some() {
            return Ok(());
        }
        let name = self.inner.tenant_name(self.tenant).unwrap_or_else(|| self.tenant.to_string());
        let over = |what: &str, n: usize, max: u32| {
            CommandError::new(
                crate::error::OUT_OF_DISK_SPACE,
                format!("tenant {name} is over its {what} quota: {n} of {max}"),
            )
        };
        let collections = self.collections()?;
        if limits.max_collections > 0 && collections.len() >= limits.max_collections as usize {
            return Err(over("collection", collections.len(), limits.max_collections));
        }
        let dbs = self.databases()?;
        if limits.max_databases > 0
            && !dbs.contains(&ns.db)
            && dbs.len() >= limits.max_databases as usize
        {
            return Err(over("database", dbs.len(), limits.max_databases));
        }
        Ok(())
    }
}

impl<B: Backend + ?Sized> Drop for Guarded<'_, B> {
    fn drop(&mut self) {
        if self.report != OpReport::default() {
            let report = std::mem::take(&mut self.report);
            let id = self.id;
            crate::commands::hot::of(self.shared).ops.with(id, |t| t.report_mut(id).absorb(report));
        }
        self.inner.set_deadline(None);
        self.inner.end_operation();
        let flag = crate::commands::js::thread_interrupt();
        flag.0.store(self.outer_interrupt, std::sync::atomic::Ordering::SeqCst);
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
        self.check_tenant_quotas(&cmd)?;
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
        let (at, versions) = self.call(|b| b.read_versions(keys))?;
        let found: Vec<(Vec<u8>, Vec<u8>)> = keys
            .iter()
            .zip(&versions)
            .filter(|(_, v)| v.is_some())
            .map(|(k, _)| (k.clone(), Vec::new()))
            .collect();
        self.tally(&found);
        Ok((at, versions))
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

    fn fence(
        &mut self,
        start: &[u8],
        end: &[u8],
        on: bool,
    ) -> Result<teseladb_hlc::Hlc, CommandError> {
        self.call(|b| b.fence(start, end, on))
    }

    fn changes(
        &mut self,
        start: &[u8],
        end: &[u8],
        from: teseladb_hlc::Hlc,
    ) -> Result<crate::commands::ChangeBatch, CommandError> {
        self.call(|b| b.changes(start, end, from))
    }

    fn catalog_changes(
        &mut self,
        from: teseladb_hlc::Hlc,
    ) -> Result<crate::commands::DdlBatch, CommandError> {
        let mut batch = self.call(|b| b.catalog_changes(from))?;
        batch.records = tenants::own_ddl(self.tenant, batch.records);
        Ok(batch)
    }

    fn range_changes(
        &mut self,
        start: &[u8],
        end: &[u8],
        from: teseladb_hlc::Hlc,
        at: Option<teseladb_hlc::Hlc>,
    ) -> Result<Vec<crate::commands::RangeBatch>, CommandError> {
        self.call(|b| b.range_changes(start, end, from, at))
    }

    fn set_read_preference(&mut self, pref: Option<teseladb_router::read_pref::ReadPreference>) {
        self.inner.set_read_preference(pref);
    }

    fn txn_route(&self, key: &[u8]) -> Option<teseladb_types::GroupId> {
        self.inner.txn_route(key)
    }

    fn txn_ranges(&self) -> Vec<(teseladb_types::GroupId, Vec<u8>)> {
        self.inner.txn_ranges()
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

    fn interrupted(&self) -> Result<(), CommandError> {
        self.check()
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
        let rows = self.call(|b| b.txn_scan(start, end, limit, at, view, uncertain_to))?;
        self.tally(&rows);
        Ok(rows)
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
        let rows = self.call(|b| b.txn_scan_back(start, end, limit, at, view, uncertain_to))?;
        self.tally(&rows);
        Ok(rows)
    }

    fn txn_gets(
        &mut self,
        keys: &[Vec<u8>],
        at: teseladb_hlc::Hlc,
        view: &teseladb_range::push::TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Vec<Option<(u8, Vec<u8>)>>, CommandError> {
        self.call(|b| b.txn_gets(keys, at, view, uncertain_to))
    }

    fn txn_points(
        &mut self,
        keys: &[Vec<u8>],
        at: teseladb_hlc::Hlc,
        view: &teseladb_range::push::TxnView,
        uncertain_to: Option<teseladb_hlc::Hlc>,
    ) -> Result<Vec<Option<Vec<u8>>>, CommandError> {
        let found = self.call(|b| b.txn_points(keys, at, view, uncertain_to))?;
        // Counted as the scans of each key would count them.
        let rows: Vec<(Vec<u8>, Vec<u8>)> = keys
            .iter()
            .zip(&found)
            .filter_map(|(k, v)| v.as_ref().map(|v| (k.clone(), v.clone())))
            .collect();
        self.tally(&rows);
        Ok(found)
    }

    fn not_examined(&mut self, docs: u64) {
        self.report.docs = self.report.docs.saturating_sub(docs);
        self.inner.not_examined(docs);
    }

    fn op_reads(&mut self) -> OpReads {
        OpReads { keys: self.report.keys, docs: self.report.docs }
    }

    fn op_client(&mut self) -> Option<Document> {
        let id = self.id;
        crate::commands::hot::of(self.shared)
            .ops
            .with(id, |t| t.get(id).and_then(|o| o.metadata.clone()))
    }

    fn op_plan(&mut self) -> Option<Document> {
        self.report.resolve_plan();
        self.report.plan.clone()
    }

    fn note_plan(&mut self, plan: Document) {
        let mut plan = plan;
        let report = &mut self.report;
        if let Some(Value::Document(used)) = plan.remove(crate::commands::plan_exec::USED_INDEXES)
            && let Some(Value::Array(names)) = used.get("names")
        {
            let names = names.iter().filter_map(|n| match n {
                Value::String(n) => Some(n.clone()),
                _ => None,
            });
            report.used.extend(names);
        }
        // A note of the indexes alone leaves the plan as it was.
        if plan.iter().next().is_some() {
            report.plan = Some(plan);
            report.pending = None;
        }
    }

    fn note_plan_lazy(&mut self, plan: PendingPlan) {
        self.report.plan = None;
        self.report.pending = Some(plan);
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
        let result = self.call(|b| b.write(op))?;
        self.note_keys_written(&result);
        Ok(result)
    }

    fn begin_statements(&mut self) -> bool {
        self.inner.begin_statements()
    }

    fn statement_barrier(&mut self, wc: WriteConcern) -> Result<(), CommandError> {
        // The statements written are confirmed whatever the time limit: a barrier with nothing
        // to confirm answers at once, and the backend's own deadline bounds the wait.
        self.inner.statement_barrier(wc)
    }

    fn write_unchanged(&mut self, op: &WriteOp) -> Option<Vec<u8>> {
        // An interrupted operation goes as a write, which reports the interruption.
        let result = self.call(|b| Ok(b.write_unchanged(op))).ok().flatten()?;
        self.note_keys_written(&result);
        Some(result)
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

    fn ordered_ids(&mut self) -> Result<bool, CommandError> {
        self.call(|b| b.ordered_ids())
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
        self.tally(&rows);
        Ok(rows)
    }

    fn scan_matching(
        &mut self,
        start: &[u8],
        end: &[u8],
        examine: usize,
        concern: ReadConcern,
        keep: &mut crate::commands::Keep<'_>,
        block: &dyn Fn(&dyn teseladb_keys::zone::Zones) -> bool,
        sent: Option<&[u8]>,
    ) -> Result<crate::commands::Matching, CommandError> {
        let m = self.call(|b| b.scan_matching(start, end, examine, concern, keep, block, sent))?;
        self.check()?;
        // Every document it examined counts, not only those its filter accepted.
        if space_of(start) == Some(teseladb_keys::space::DOCUMENTS) {
            self.report.docs += m.examined as u64;
        } else {
            self.tally(&m.rows);
        }
        Ok(m)
    }

    fn scatter_groups(
        &mut self,
        span: (&[u8], &[u8]),
        program: &std::sync::Arc<teseladb_exec::pipeline::Program>,
        concern: ReadConcern,
    ) -> Result<Option<crate::commands::Scattered>, CommandError> {
        let found = self.call(|b| b.scatter_groups(span, program, concern))?;
        self.check()?;
        // Every document the cores examined counts, as in a scan of the collection.
        if let Some(s) = &found {
            self.report.docs += s.examined as u64;
        }
        Ok(found)
    }

    fn doc_count(&mut self, tenant: u32, collection: u32) -> Result<Option<u64>, CommandError> {
        let n = self.call(|b| b.doc_count(tenant, collection))?;
        self.check()?;
        Ok(n)
    }

    fn points(
        &mut self,
        keys: &[Vec<u8>],
        concern: ReadConcern,
    ) -> Result<Vec<Option<Vec<u8>>>, CommandError> {
        // As `scan`: the backend underneath reads them as point reads where it can (H-013).
        let found = self.call(|b| b.points(keys, concern))?;
        self.check()?;
        let rows: Vec<(Vec<u8>, Vec<u8>)> = keys
            .iter()
            .zip(&found)
            .filter_map(|(k, v)| v.as_ref().map(|v| (k.clone(), v.clone())))
            .collect();
        self.tally(&rows);
        Ok(found)
    }

    fn scan_back(
        &mut self,
        start: &[u8],
        end: &[u8],
        limit: usize,
        concern: ReadConcern,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, CommandError> {
        // As `scan`: the deadline stops the operation once the read returns.
        let rows = self.call(|b| b.scan_back(start, end, limit, concern))?;
        self.check()?;
        self.tally(&rows);
        Ok(rows)
    }

    /// The spans go to the node together, as [`Backend::scan_spans`] of the backend underneath
    /// sends them (010 T053); the default would read them one after the other.
    fn scan_spans(
        &mut self,
        spans: &[crate::commands::Span],
        concern: ReadConcern,
    ) -> Result<Vec<crate::commands::Rows>, CommandError> {
        let all = self.call(|b| b.scan_spans(spans, concern))?;
        self.check()?;
        for rows in &all {
            self.tally(rows);
        }
        Ok(all)
    }

    fn catalog_barrier(&mut self) -> Result<(), CommandError> {
        self.call(|b| b.catalog_barrier())
    }

    fn kill_everywhere(&mut self, scope: &KillScope) -> Result<(), CommandError> {
        self.inner.kill_everywhere(scope)
    }

    fn other_operations(&mut self) -> Result<Vec<Document>, CommandError> {
        self.inner.other_operations()
    }

    fn route_entries(&mut self) -> Result<Vec<teseladb_router::table::RouteEntry>, CommandError> {
        self.call(|b| b.route_entries())
    }

    fn range_live_bytes(
        &mut self,
        e: &teseladb_router::table::RouteEntry,
    ) -> Result<u64, CommandError> {
        self.call(|b| b.range_live_bytes(e))
    }

    fn range_order(
        &mut self,
        cmd: teseladb_meta::command::Group0Command,
    ) -> Result<(), CommandError> {
        self.call(|b| b.range_order(cmd))
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

    fn group0_settings(
        &mut self,
        prefix: &[u8],
    ) -> Result<std::collections::BTreeMap<Vec<u8>, Vec<u8>>, CommandError> {
        self.call(|b| b.group0_settings(prefix))
    }

    fn group0_settings_write(
        &mut self,
        changes: Vec<teseladb_meta::command::SettingChange>,
    ) -> Result<bool, CommandError> {
        self.call(|b| b.group0_settings_write(changes))
    }

    fn request_shutdown(&mut self) -> Result<(), CommandError> {
        self.call(|b| b.request_shutdown())
    }

    fn compact(&mut self) -> Result<u64, CommandError> {
        self.call(|b| b.compact())
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

    fn thread_cpu_nanos(&mut self) -> Option<u64> {
        self.inner.thread_cpu_nanos()
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
        let tenant = self.tenant;
        let found = self.call(|b| b.principal(kind, db, name))?;
        Ok(found.filter(|d| tenants::of_principal(d) == tenant))
    }

    fn principals(
        &mut self,
        kind: Principal,
        db: Option<&str>,
    ) -> Result<Vec<Vec<u8>>, CommandError> {
        let tenant = self.tenant;
        let all = self.call(|b| b.principals(kind, db))?;
        Ok(all.into_iter().filter(|d| tenants::of_principal(d) == tenant).collect())
    }

    fn principal_by_id(
        &mut self,
        kind: Principal,
        id: &[u8],
    ) -> Result<Option<Vec<u8>>, CommandError> {
        self.call(|b| b.principal_by_id(kind, id))
    }

    fn security_write(
        &mut self,
        ops: Vec<SecurityOp>,
    ) -> Result<Result<(), SecurityError>, CommandError> {
        let Some(ops) = tenants::own_security_ops(self.tenant, ops)? else {
            return Ok(Err(SecurityError::Conflict));
        };
        self.call(|b| b.security_write(ops))
    }

    fn secure_random(&mut self, out: &mut [u8]) -> Result<(), CommandError> {
        self.inner.secure_random(out)
    }

    fn authenticated_user(&self) -> Option<UserName> {
        self.user.clone()
    }

    fn authorization_lease(&self) -> bool {
        self.inner.authorization_lease()
    }

    fn privileges(&self) -> Option<Vec<Privilege>> {
        let id = self.id;
        crate::commands::hot::of(self.shared)
            .ops
            .with(id, |t| t.get(id).and_then(|o| o.privileges.clone()))
    }

    fn invalidate_user_cache(&mut self) {
        self.inner.invalidate_user_cache();
    }

    fn authenticated_roles(&self) -> Option<Vec<RoleName>> {
        let id = self.id;
        crate::commands::hot::of(self.shared)
            .ops
            .with(id, |t| t.get(id).and_then(|o| o.roles.clone()))
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
            interrupt: None,
            user: None,
            tenant: 0,
            privileges: None,
            roles: None,
            desc: None,
            progress: None,
            metrics: None,
        };
        let mut t = OpTable { last: u32::MAX - 1, ..OpTable::default() };
        t.ops.insert(1, op.clone());
        assert_eq!(t.begin(op.clone()), u32::MAX);
        assert_eq!(t.begin(op), 2);
    }

    fn sample_op(conn: i32) -> Op {
        Op {
            conn,
            client: None,
            metadata: None,
            app_name: None,
            op: op_type("find"),
            ns: String::new(),
            command: Document::new(),
            started_ms: 0,
            deadline_ms: None,
            killed: false,
            interrupt: None,
            user: None,
            tenant: 0,
            privileges: None,
            roles: None,
            desc: None,
            progress: None,
            metrics: None,
        }
    }

    #[test]
    fn operations_begun_on_many_threads_get_distinct_ids_and_end_alone() {
        let ops = std::sync::Arc::new(Ops::default());
        let workers: Vec<_> = (0..8)
            .map(|w| {
                let ops = ops.clone();
                std::thread::spawn(move || {
                    let mut ids = Vec::new();
                    for _ in 0..500 {
                        let id = ops.begin(sample_op(w));
                        ops.with(id, |t| t.report_mut(id).docs += 1);
                        ids.push(id);
                    }
                    ids
                })
            })
            .collect();
        let mut all: Vec<u32> = workers.into_iter().flat_map(|w| w.join().unwrap()).collect();
        let n = all.len();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), n, "no id given twice");
        assert_eq!(ops.snapshot().len(), n);
        let id = all[17];
        assert!(ops.kill(id));
        assert!(ops.with(id, |t| t.get(id).is_some_and(|o| o.killed)));
        assert!(ops.with(id, |t| t.cancel(id)).raised());
        assert_eq!(ops.with(id, |t| t.take_report(id)).docs, 1);
        for id in &all {
            ops.end(*id);
        }
        assert!(ops.snapshot().is_empty());
        assert!(!ops.kill(id), "an operation that ended is not running");
    }

    #[test]
    fn the_snapshot_lists_every_operation_by_id() {
        let ops = Ops::default();
        let ids: Vec<u32> = (0..40).map(|c| ops.begin(sample_op(c))).collect();
        let named = ops.begin_named(sample_op(99), |id| format!("build {id}"));
        let listed: Vec<u32> = ops.snapshot().iter().map(|(id, _)| *id).collect();
        let mut want = ids.clone();
        want.push(named);
        want.sort_unstable();
        assert_eq!(listed, want);
        assert_eq!(
            ops.with(named, |t| t.get(named).and_then(|o| o.desc.clone())),
            Some(format!("build {named}"))
        );
    }
}
