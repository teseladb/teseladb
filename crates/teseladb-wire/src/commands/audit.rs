//! The audit log of the commands (013 FR-016): which events each command, catalog change and
//! authentication gives, built from what the command did and recorded through the node's
//! [`Audit`] when the node has one. The bytes go to a [`Sink`] the server provides.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard};

use teseladb_auth::audit::{AuditType, Auditor, Client, Endpoint, Event};
use teseladb_auth::audit_param::{self as param, IndexBuildState};
use teseladb_auth::users::{RoleName, UserName};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_meta::catalog::{CatalogCommand, CollectionEntry, Namespace};
use teseladb_meta::indexes::{IndexCommand, IndexState};

use super::{Backend, Ctx, Shared, lock};
use crate::connection::Connection;
use crate::error::CommandError;
use crate::error::{
    AUDITING_NOT_ENABLED, BAD_VALUE, Code, IDL_FAILED_TO_PARSE,
    RUNTIME_AUDIT_CONFIGURATION_NOT_ENABLED,
};
use crate::types::wrong_type;

/// Where the audit messages go: a file, the standard output or the system log.
pub trait Sink: Send {
    /// Writes one message as [`Auditor::render`] gave it.
    fn write(&mut self, bytes: &[u8]);
}

/// What a node records and how, shared by its workers and replaced as a whole when the
/// configuration changes.
pub type SharedAuditor = Arc<RwLock<Auditor>>;

/// The audit log of a node.
pub struct Audit {
    /// What is recorded and how.
    pub auditor: SharedAuditor,
    sink: Box<dyn Sink>,
    /// Indexes being built: their namespace, name and specification, for `IndexBuildSucceeded`.
    building: BTreeMap<u32, (String, String, Document)>,
    /// An `auditConfig` this node set, applied once the command's events before it are written:
    /// its filter, `auditAuthorizationSuccess` and time.
    pending: Option<(Document, bool, (u32, u32))>,
    /// When this worker last looked for an `auditConfig` set by another node.
    checked_ms: u64,
}

impl fmt::Debug for Audit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Audit").field("auditor", &self.auditor).finish()
    }
}

impl Audit {
    /// The audit log `auditor` describes, writing to `sink`.
    pub fn new(auditor: SharedAuditor, sink: Box<dyn Sink>) -> Audit {
        Audit { auditor, sink, building: BTreeMap::new(), pending: None, checked_ms: 0 }
    }

    /// The auditor as it is now.
    pub fn current(&self) -> RwLockReadGuard<'_, Auditor> {
        self.auditor.read().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `event` of `client` if the filter keeps it.
    pub fn write(&mut self, event: &Event, client: &Client) {
        let bytes = self.current().render(event, client);
        if let Some(bytes) = bytes {
            self.sink.write(&bytes);
        }
    }
}

/// Events waiting for their client: action type and parameters.
pub type Pending = Vec<(AuditType, Document)>;

/// Whether the node records an audit log.
pub fn enabled(shared: &Shared) -> bool {
    lock(shared).audit.is_some()
}

/// Whether an authorization check with `result` is recorded.
pub fn records_auth_check(shared: &Shared, result: i32) -> bool {
    lock(shared).audit.as_ref().is_some_and(|a| a.current().records_auth_check(result))
}

/// The connection's identifier, drawn the first time it is needed (a version 4 UUID).
fn connection_uuid<B: Backend + ?Sized>(conn: &mut Connection, backend: &mut B) -> [u8; 16] {
    if let Some(u) = conn.auth.audit_uuid {
        return u;
    }
    let mut u = [0u8; 16];
    u[..8].copy_from_slice(&backend.random().to_be_bytes());
    u[8..].copy_from_slice(&backend.random().to_be_bytes());
    u[6] = (u[6] & 0x0f) | 0x40;
    u[8] = (u[8] & 0x3f) | 0x80;
    conn.auth.audit_uuid = Some(u);
    u
}

fn endpoint(addr: Option<&str>) -> Endpoint {
    addr.and_then(Endpoint::parse).unwrap_or(Endpoint::SystemUser)
}

/// The client of the connection's events: its ends, its user and the roles granted to it.
pub fn client<B: Backend + ?Sized>(
    conn: &mut Connection,
    backend: &mut B,
    with_user: bool,
) -> Client {
    let uuid = connection_uuid(conn, backend);
    let (users, roles) = match conn.auth.user.as_ref().filter(|_| with_user) {
        None => (Vec::new(), Vec::new()),
        Some(u) => {
            let roles = u.direct_roles(&mut *backend).unwrap_or_default();
            (vec![u.name.clone()], roles)
        }
    };
    Client {
        uuid,
        local: endpoint(conn.auth.server_addr.as_deref()),
        remote: endpoint(conn.auth.client_addr.as_deref()),
        users,
        roles,
    }
}

/// Records `events` of the connection with `result`. An `auditConfig` this node set applies
/// right before its `auditConfigure`, so the events before it go through the old filter, as on
/// the reference server 9.0.4 (measured); one another node set is picked up first, at most once a
/// second.
pub fn record<B: Backend + ?Sized>(
    shared: &Shared,
    conn: &mut Connection,
    backend: &mut B,
    events: Pending,
    result: i32,
) {
    if events.is_empty() || !enabled(shared) {
        return;
    }
    refresh(shared, backend);
    let c = client(conn, backend, true);
    let ts_ms = i64::try_from(backend.now_ms()).unwrap_or(i64::MAX);
    let mut g = lock(shared);
    g.audit.iter_mut().for_each(|a| {
        for (atype, param) in &events {
            if *atype == AuditType::AuditConfigure {
                a.apply_pending();
            }
            let event = Event { atype: *atype, ts_ms, param: param.clone(), result };
            a.write(&event, &c);
        }
        a.apply_pending();
    });
}

/// `logApplicationMessage`.
pub const LOG_APPLICATION_MESSAGE: &str = "logApplicationMessage";
/// The cluster parameter of the runtime audit configuration.
pub const AUDIT_CONFIG: &str = "auditConfig";
/// The name of its structure in the reference server's errors.
pub const AUDIT_CONFIG_IDL: &str = "auditConfigDocument";
/// Its filter.
const FILTER: &str = "filter";
/// Its `auditAuthorizationSuccess`.
const AUTHORIZATION_SUCCESS: &str = "auditAuthorizationSuccess";
/// How often a worker looks for an `auditConfig` another node set.
const REFRESH_MS: u64 = 1_000;

impl Audit {
    /// Applies the `auditConfig` this node set, if any.
    fn apply_pending(&mut self) {
        if let Some((filter, success, version)) = self.pending.take() {
            let mut a = self.auditor.write().unwrap_or_else(PoisonError::into_inner);
            // Checked when it was set; a filter that no longer parses leaves the old one.
            let _ = a.configure(&filter, success, version);
        }
    }
}

/// `logApplicationMessage`: an `applicationMessage` event with the text; without an audit log
/// the text goes nowhere (measured on 9.0.4).
pub fn log_application_message<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    let path = format!("{LOG_APPLICATION_MESSAGE}.{LOG_APPLICATION_MESSAGE}");
    let msg = match ctx.first() {
        Value::String(m) => m.clone(),
        Value::Null | Value::Undefined => {
            let why = format!("BSON field '{path}' is missing but a required field");
            return Err(CommandError::new(IDL_FAILED_TO_PARSE, why));
        }
        other => return Err(wrong_type(&path, other, "string")),
    };
    ctx.backend.audit_event(AuditType::ApplicationMessage, Document::new().with("msg", msg));
    Ok(Document::new().with("ok", 1.0))
}

/// Refuses a change of `auditConfig` on a node without an audit log or without its runtime
/// configuration (measured on 9.0.4).
pub fn runtime_enabled(shared: &Shared) -> Result<(), CommandError> {
    match lock(shared).audit.as_ref().map(|a| a.current().config().runtime) {
        None => Err(CommandError::new(AUDITING_NOT_ENABLED, "Auditing is not enabled")),
        Some(false) => Err(CommandError::new(
            RUNTIME_AUDIT_CONFIGURATION_NOT_ENABLED,
            "Runtime audit configuration has not been enabled",
        )),
        Some(true) => Ok(()),
    }
}

/// `auditConfig` when nobody set it.
pub fn audit_config_default() -> Document {
    Document::new().with(FILTER, Document::new()).with(AUTHORIZATION_SUCCESS, false)
}

/// Reads a new `auditConfig`: the types first, in the order given, then
/// `auditAuthorizationSuccess` and `filter`, which are required, then the filter as an audit
/// filter; other fields are left out (measured on 9.0.4).
pub fn audit_config_parse(v: &Document) -> Result<Document, (Code, String)> {
    let (mut filter, mut success) = (None, None);
    let field = |name: &str| format!("{AUDIT_CONFIG_IDL}.{name}");
    for (k, x) in v.iter() {
        match (k, x) {
            (FILTER, Value::Document(d)) => filter = Some(d.clone()),
            (AUTHORIZATION_SUCCESS, Value::Boolean(b)) => success = Some(*b),
            (FILTER, other) => {
                let e = wrong_type(&field(FILTER), other, "object");
                return Err((e.code, e.errmsg));
            }
            (AUTHORIZATION_SUCCESS, other) => {
                let e = wrong_type(&field(AUTHORIZATION_SUCCESS), other, "bool");
                return Err((e.code, e.errmsg));
            }
            _ => {}
        }
    }
    let missing = |name: &str| {
        (
            IDL_FAILED_TO_PARSE,
            format!("BSON field '{}' is missing but a required field", field(name)),
        )
    };
    let success = success.ok_or_else(|| missing(AUTHORIZATION_SUCCESS))?;
    let filter = filter.ok_or_else(|| missing(FILTER))?;
    teseladb_auth::audit::check_filter(&filter).map_err(|e| (BAD_VALUE, e.0))?;
    Ok(Document::new().with(FILTER, filter).with(AUTHORIZATION_SUCCESS, success))
}

fn version_of(stored: &Document) -> (u32, u32) {
    match stored.get(super::parameters::PARAMETER_TIME) {
        Some(Value::Timestamp { time, increment }) => (*time, *increment),
        _ => (0, 0),
    }
}

/// The parameter `auditConfigure` of a change from `previous` to `stored` (both as stored: time,
/// filter, `auditAuthorizationSuccess`).
fn configure_param(previous: &Document, stored: &Document) -> Document {
    let time = stored.get(super::parameters::PARAMETER_TIME).cloned().unwrap_or(Value::Null);
    let mut config = Document::new().with("_id", AUDIT_CONFIG);
    for name in [FILTER, AUTHORIZATION_SUCCESS] {
        config.push(name, stored.get(name).cloned().unwrap_or(Value::Null));
    }
    Document::new()
        .with("previous", previous.clone())
        .with("config", config.with(super::parameters::PARAMETER_TIME, time))
}

/// Queues the `auditConfig` this node just stored, to apply with its `auditConfigure`, and gives
/// that event's parameter.
pub fn apply_audit_config(
    shared: &Shared,
    previous: &Document,
    stored: &Document,
) -> Result<Document, CommandError> {
    let filter = match stored.get(FILTER) {
        Some(Value::Document(d)) => d.clone(),
        _ => Document::new(),
    };
    let success = stored.get(AUTHORIZATION_SUCCESS) == Some(&Value::Boolean(true));
    if let Some(a) = lock(shared).audit.as_mut() {
        a.pending = Some((filter, success, version_of(stored)));
    }
    Ok(configure_param(previous, stored))
}

/// Picks up an `auditConfig` another node set: once a second at most per worker, and once per
/// node (the auditor is the node's), with `updateCachedClusterServerParameter` under the old
/// filter and then `auditConfigure`, as events of the node itself.
fn refresh<B: Backend + ?Sized>(shared: &Shared, backend: &mut B) {
    let now = backend.now_ms();
    let due = {
        let mut g = lock(shared);
        match g.audit.as_mut() {
            Some(a) if a.current().config().runtime && now >= a.checked_ms + REFRESH_MS => {
                a.checked_ms = now;
                true
            }
            _ => false,
        }
    };
    if !due {
        return;
    }
    let Ok(all) = backend.cluster_parameters() else { return };
    let Some(stored) = all.get(AUDIT_CONFIG).and_then(|b| RawDoc::from_bytes(b).ok()) else {
        return;
    };
    let stored = Document::from(stored);
    let version = version_of(&stored);
    let ts_ms = i64::try_from(now).unwrap_or(i64::MAX);
    let mut g = lock(shared);
    let Some(a) = g.audit.as_mut() else { return };
    // This node's own change, waiting for its `auditConfigure`.
    if a.pending.as_ref().is_some_and(|p| p.2 == version) {
        return;
    }
    let auditor = Arc::clone(&a.auditor);
    let mut w = auditor.write().unwrap_or_else(PoisonError::into_inner);
    if w.runtime_version() == Some(version) {
        return;
    }
    let filter = match stored.get(FILTER) {
        Some(Value::Document(d)) => d.clone(),
        _ => Document::new(),
    };
    let success = stored.get(AUTHORIZATION_SUCCESS) == Some(&Value::Boolean(true));
    let previous = Document::new()
        .with(super::parameters::PARAMETER_TIME, {
            let (t, i) = w.runtime_version().unwrap_or((0, 0));
            Value::Timestamp { time: t, increment: i }
        })
        .with(FILTER, w.config().filter_doc.clone().unwrap_or_default())
        .with(AUTHORIZATION_SUCCESS, w.config().authorization_success);
    let shown = super::parameters::shown_value(AUDIT_CONFIG, &all).unwrap_or_default();
    let mut nested = previous.clone();
    let time = nested.remove(super::parameters::PARAMETER_TIME).unwrap_or(Value::Null);
    let original = Document::new()
        .with("_id", AUDIT_CONFIG)
        .with(AUDIT_CONFIG, nested.with(super::parameters::PARAMETER_TIME, time));
    let cached = Document::new()
        .with("originalClusterServerParameter", original)
        .with("updatedClusterServerParameter", shown);
    let system = Client::system([0; 16]);
    let event = |atype, param| Event { atype, ts_ms, param, result: 0 };
    let line = w.render(&event(AuditType::UpdateCachedClusterServerParameter, cached), &system);
    if w.configure(&filter, success, version).is_err() {
        return;
    }
    let line2 =
        w.render(&event(AuditType::AuditConfigure, configure_param(&previous, &stored)), &system);
    drop(w);
    for bytes in [line, line2].into_iter().flatten() {
        a.sink.write(&bytes);
    }
}

/// Records an event of the node itself (`startup`, `shutdown`).
pub fn record_system(
    shared: &Shared,
    uuid: [u8; 16],
    ts_ms: u64,
    atype: AuditType,
    param: Document,
) {
    let ts_ms = i64::try_from(ts_ms).unwrap_or(i64::MAX);
    let event = Event { atype, ts_ms, param, result: 0 };
    lock(shared).audit.iter_mut().for_each(|a| a.write(&event, &Client::system(uuid)));
}

/// The `logout` of a connection that closes with a user logged in: the users before and after
/// and when it logged in, with no user on the message itself (measured).
pub fn on_close<B: Backend + ?Sized>(shared: &Shared, conn: &mut Connection, backend: &mut B) {
    let Some(user) = conn.auth.user.clone().filter(|_| enabled(shared)) else { return };
    let login_ms = i64::try_from(user.login_ms).unwrap_or(i64::MAX);
    let p = param::logout(param::DISCONNECTED, &[user.name], &[], login_ms);
    let c = client(conn, backend, false);
    let ts_ms = i64::try_from(backend.now_ms()).unwrap_or(i64::MAX);
    let event = Event { atype: AuditType::Logout, ts_ms, param: p, result: 0 };
    lock(shared).audit.iter_mut().for_each(|a| a.write(&event, &c));
}

/// The namespace as the client knows it: a collection a transaction created goes under the name
/// the client gave it, which ends its name until the commit (008 T019); the enterprise reference
/// server 9.0.4 records it with that name when the statement creates it (measured).
fn ns_text(ns: &Namespace) -> String {
    let coll = match ns.coll.strip_prefix(super::txn_ddl::SCRATCH_PREFIX) {
        // `<node>.<transaction>.<name>`; the name may have dots of its own.
        Some(rest) => rest.splitn(3, '.').nth(2).unwrap_or(rest),
        None => &ns.coll,
    };
    format!("{}.{}", ns.db, coll)
}

fn id_index_spec() -> Document {
    Document::new().with("v", 2).with("key", Document::new().with("_id", 1)).with("name", "_id_")
}

/// The `_id` index's name.
const ID_INDEX: &str = "_id_";

/// What a catalog command needs known before it runs to describe what it did.
#[derive(Debug)]
pub struct CatalogBefore {
    cmd: CatalogCommand,
    databases: Vec<String>,
    /// The collections it drops, with their secondary indexes.
    dropped: Vec<(String, Vec<String>)>,
}

fn indexes_of<B: Backend + ?Sized>(b: &mut B, id: u32) -> Result<Vec<String>, CommandError> {
    Ok(b.indexes(id)?.into_iter().map(|i| i.name).collect())
}

/// Reads what `cmd` will change.
pub fn before_catalog<B: Backend + ?Sized>(
    b: &mut B,
    cmd: &CatalogCommand,
) -> Result<CatalogBefore, CommandError> {
    let mut dropped = Vec::new();
    // A transaction's collection that goes away with its abort, or renamed at its commit, was
    // already recorded when it was created: nothing more is recorded for it.
    if transaction_own(cmd) {
        return Ok(CatalogBefore { cmd: cmd.clone(), databases: Vec::new(), dropped });
    }
    let targets: Vec<Namespace> = match cmd {
        CatalogCommand::Drop { ns } => vec![ns.clone()],
        CatalogCommand::Rename { to, drop_target: true, .. } => vec![to.clone()],
        CatalogCommand::DropDatabase { db } => {
            b.collections()?.into_iter().filter(|(ns, _)| &ns.db == db).map(|(ns, _)| ns).collect()
        }
        _ => Vec::new(),
    };
    for ns in targets {
        if let Some(entry) = b.collection(&ns)? {
            dropped.push((ns_text(&ns), indexes_of(b, entry.id)?));
        }
    }
    let databases = match cmd {
        CatalogCommand::Create { .. } => b.databases()?,
        _ => Vec::new(),
    };
    Ok(CatalogBefore { cmd: cmd.clone(), databases, dropped })
}

/// Whether `cmd` is the commit's rename, the abort's undo of it or a drop of a collection a
/// transaction created (008 T019).
fn transaction_own(cmd: &CatalogCommand) -> bool {
    let scratch = super::txn_ddl::is_scratch;
    match cmd {
        CatalogCommand::Rename { from, to, .. } => scratch(from) || scratch(to),
        CatalogCommand::Drop { ns } => scratch(ns),
        _ => false,
    }
}

fn drop_events(dropped: Vec<(String, Vec<String>)>, out: &mut Pending) {
    for (ns, indexes) in dropped {
        out.push((AuditType::DropCollection, param::namespace(&ns)));
        out.push((AuditType::DropIndex, param::drop_index(&ns, ID_INDEX)));
        for name in indexes {
            out.push((AuditType::DropIndex, param::drop_index(&ns, &name)));
        }
    }
}

/// The events of a catalog command that succeeded with `result`, in the reference server's order.
pub fn after_catalog(before: CatalogBefore, result: Option<&CollectionEntry>) -> Pending {
    let mut out = Pending::new();
    let new_db = |db: &str| !before.databases.iter().any(|d| d == db);
    match &before.cmd {
        CatalogCommand::Create { ns, uuid, .. } => {
            // An implicit creation of a collection that existed leaves it as it was.
            if result.is_some_and(|e| &e.uuid != uuid) {
                return out;
            }
            if new_db(&ns.db) {
                out.push((AuditType::CreateDatabase, param::namespace(&ns.db)));
            }
            let text = ns_text(ns);
            out.push((AuditType::CreateCollection, param::namespace(&text)));
            for state in [IndexBuildState::Started, IndexBuildState::Succeeded] {
                let p = param::create_index(&text, ID_INDEX, &id_index_spec(), state);
                out.push((AuditType::CreateIndex, p));
            }
        }
        // Written only for a database the catalog does not have (`ensure_database`).
        CatalogCommand::CreateDatabase { db } => {
            out.push((AuditType::CreateDatabase, param::namespace(db)));
        }
        _ if transaction_own(&before.cmd) => {}
        CatalogCommand::Drop { .. } => drop_events(before.dropped, &mut out),
        CatalogCommand::Rename { from, to, .. } => {
            drop_events(before.dropped, &mut out);
            let p = param::rename_collection(&ns_text(from), &ns_text(to));
            out.push((AuditType::RenameCollection, p));
        }
        CatalogCommand::DropDatabase { db } => {
            drop_events(before.dropped, &mut out);
            out.push((AuditType::DropDatabase, param::namespace(db)));
        }
        _ => {}
    }
    out
}

/// What an index command needs known before it runs: the namespace and name of the index.
#[derive(Debug)]
pub struct IndexBefore {
    cmd: IndexCommand,
    /// Namespace, name and specification of the index it names by identifier, or creates.
    index: Option<(String, String, Document)>,
}

fn spec_doc(bytes: &[u8]) -> Document {
    RawDoc::from_bytes(bytes).map(Document::from).unwrap_or_default()
}

/// Reads what `cmd` will change.
pub fn before_index<B: Backend + ?Sized>(
    b: &mut B,
    cmd: &IndexCommand,
) -> Result<IndexBefore, CommandError> {
    let index = match cmd {
        IndexCommand::Create { collection, name, spec, .. } => b
            .collections()?
            .into_iter()
            .find(|(_, e)| e.id == *collection)
            .map(|(ns, _)| (ns_text(&ns), name.clone(), spec_doc(spec))),
        IndexCommand::Drop { id } => {
            let mut found = None;
            for (ns, e) in b.collections()? {
                if let Some(i) = b.indexes(e.id)?.into_iter().find(|i| i.id == *id) {
                    found = Some((ns_text(&ns), i.name, spec_doc(&i.spec)));
                    break;
                }
            }
            found
        }
        _ => None,
    };
    Ok(IndexBefore { cmd: cmd.clone(), index })
}

/// The events of an index command that succeeded. A created index waits in the node's audit log
/// until it becomes ready; `id_of` finds the identifier the catalog gave it.
pub fn after_index<B: Backend + ?Sized>(
    b: &mut B,
    shared: &Shared,
    before: IndexBefore,
) -> Result<Pending, CommandError> {
    let mut out = Pending::new();
    match (&before.cmd, before.index) {
        (IndexCommand::Create { collection, name, .. }, Some((ns, n, spec))) => {
            let id = b.indexes(*collection)?.into_iter().find(|i| &i.name == name).map(|i| i.id);
            let p = param::create_index(&ns, &n, &spec, IndexBuildState::Started);
            out.push((AuditType::CreateIndex, p));
            let entry = id.map(|id| (id, (ns, n, spec)));
            lock(shared).audit.iter_mut().zip(entry).for_each(|(a, (id, e))| {
                a.building.insert(id, e);
            });
        }
        (IndexCommand::SetState { id, state: IndexState::Ready }, _) => {
            let built = lock(shared).audit.as_mut().and_then(|a| a.building.remove(id));
            out.extend(built.map(|(ns, n, spec)| {
                let p = param::create_index(&ns, &n, &spec, IndexBuildState::Succeeded);
                (AuditType::CreateIndex, p)
            }));
        }
        (IndexCommand::Drop { id }, Some((ns, n, _))) => {
            lock(shared).audit.iter_mut().for_each(|a| {
                a.building.remove(id);
            });
            out.push((AuditType::DropIndex, param::drop_index(&ns, &n)));
        }
        _ => {}
    }
    Ok(out)
}

fn role_list(v: Option<&Value>, db: &str) -> Vec<RoleName> {
    let Some(Value::Array(items)) = v else { return Vec::new() };
    items
        .iter()
        .filter_map(|r| match r {
            Value::String(name) => Some(RoleName::new(name.as_str(), db)),
            other => RoleName::from_value(other),
        })
        .collect()
}

fn privilege_list(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(items)) => items.clone(),
        _ => Vec::new(),
    }
}

/// The event of a user or role command that succeeded, with what it changed (never a password).
pub fn user_role_event(db: &str, cmd: &Document) -> Option<(AuditType, Document)> {
    let (name, first) = cmd.iter().next()?;
    let atype = AuditType::from_name(name)?;
    let target = match first {
        Value::String(s) => s.as_str(),
        _ => "",
    };
    let user = UserName::new(target, db);
    let role = RoleName::new(target, db);
    let roles = || role_list(cmd.get("roles"), db);
    let privileges = || privilege_list(cmd.get("privileges"));
    let p = match atype {
        AuditType::CreateUser => param::create_user(&user, &roles()),
        AuditType::DropUser => param::drop_user(&user),
        AuditType::UpdateUser => {
            let custom = match cmd.get("customData") {
                Some(Value::Document(d)) => Some(d),
                _ => None,
            };
            let r = cmd.get("roles").map(|_| roles());
            param::update_user(&user, cmd.get("pwd").is_some(), custom, r.as_deref())
        }
        AuditType::GrantRolesToUser | AuditType::RevokeRolesFromUser => {
            param::user_roles(&user, &roles())
        }
        AuditType::CreateRole => param::create_role(&role, &roles(), &privileges()),
        AuditType::UpdateRole => {
            let r = cmd.get("roles").map(|_| roles());
            let p = cmd.get("privileges").map(|_| privileges());
            param::update_role(&role, r.as_deref(), p.as_deref())
        }
        AuditType::DropRole => param::drop_role(&role),
        AuditType::GrantRolesToRole | AuditType::RevokeRolesFromRole => {
            param::role_roles(&role, &roles())
        }
        AuditType::GrantPrivilegesToRole | AuditType::RevokePrivilegesFromRole => {
            param::role_privileges(&role, &privileges())
        }
        AuditType::DropAllUsersFromDatabase | AuditType::DropAllRolesFromDatabase => {
            param::database(db)
        }
        _ => return None,
    };
    Some((atype, p))
}
