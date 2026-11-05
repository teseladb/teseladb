//! The state of a tenant on the administration port (007 FR-030, FR-034 to FR-036, contract
//! `admin-cli.md`): `GET` and `POST /admin/tenants/<name>/limits` and `/access`, and `POST
//! /admin/tenants/<name>/suspend` and `/resume`. A change becomes the entry of Group 0 that
//! carries it once every node reads those entries (FR-027); until then the port answers 503 and
//! proposes nothing.

use teseladb_auth::restrictions::cidr;
use teseladb_meta::command::Group0Command;
use teseladb_meta::tenants::{ServiceUser, TenantLimits, TenantState};
use teseladb_bson::{Document, Value};

use crate::admin::{AdminError, Answer, NodeAdmin};
use crate::admin_http::Method;

/// Last part of the path with the limits of a tenant.
pub const LIMITS: &str = "limits";
/// Last part of the path with the access list of a tenant.
pub const ACCESS: &str = "access";
/// Last part of the path that suspends a tenant.
pub const SUSPEND: &str = "suspend";
/// Last part of the path that reactivates a tenant.
pub const RESUME: &str = "resume";
/// Last part of the path with what a tenant uses (FR-033).
pub const USAGE: &str = "usage";
/// The most sources an access list holds (FR-035).
pub const MAX_SOURCES: usize = 256;
/// The database of a service user named without one.
pub const SERVICE_USER_DB: &str = "admin";

/// The fields of the state, as the port and `tenant list` name them.
pub mod field {
    /// Bytes of data and indexes.
    pub const STORAGE_BYTES: &str = "storageBytes";
    /// Connections on each node.
    pub const MAX_CONNECTIONS: &str = "maxConnections";
    /// Commands per second on each node.
    pub const OPS_PER_SECOND: &str = "opsPerSecond";
    /// API keys.
    pub const MAX_API_KEYS: &str = "maxApiKeys";
    /// The longest `maxTimeMS`.
    pub const MAX_TIME_MS: &str = "maxTimeMS";
    /// Operations at once on each node.
    pub const MAX_CONCURRENT_OPS: &str = "maxConcurrentOps";
    /// Bytes written per second on each node.
    pub const MAX_WRITE_BYTES_PER_SECOND: &str = "maxWriteBytesPerSecond";
    /// Databases.
    pub const MAX_DATABASES: &str = "maxDatabases";
    /// Collections and views.
    pub const MAX_COLLECTIONS: &str = "maxCollections";
    /// Search indexes.
    pub const MAX_SEARCH_INDEXES: &str = "maxSearchIndexes";
    /// The REST API switch.
    pub const HTTP_ENABLED: &str = "httpEnabled";
    /// The MCP switch.
    pub const MCP_ENABLED: &str = "mcpEnabled";
    /// The service user, `{user, db}`; `null` takes it away.
    pub const SERVICE_USER: &str = "serviceUser";
    /// The access list.
    pub const ALLOWED_SOURCES: &str = "allowedSources";
    /// Whether the tenant is suspended.
    pub const SUSPENDED: &str = "suspended";
}

/// What the port does with the state of a tenant.
#[derive(Clone, Debug, PartialEq)]
pub enum StateOp {
    /// Its limits; with a body, the limits it gives change first.
    Limits(Option<Document>),
    /// Its access list; with a body, `allowedSources` replaces it first.
    Access(Option<Document>),
    /// Suspends it (`true`) or reactivates it.
    Suspend(bool),
    /// What it uses in the cluster and on this node.
    Usage,
}

/// The operation of `method` on the last part `op` of a tenant's path, with the request's
/// `body`; `Err` with the status of a method the part does not take, `None` for no such part.
pub fn op_of(method: Method, op: &str, body: Option<Document>) -> Option<Result<StateOp, u16>> {
    Some(match (op, method) {
        (LIMITS, Method::Get) => Ok(StateOp::Limits(None)),
        (LIMITS, Method::Post) => Ok(StateOp::Limits(Some(body.unwrap_or_default()))),
        (ACCESS, Method::Get) => Ok(StateOp::Access(None)),
        (ACCESS, Method::Post) => Ok(StateOp::Access(Some(body.unwrap_or_default()))),
        (SUSPEND, Method::Post) => Ok(StateOp::Suspend(true)),
        (RESUME, Method::Post) => Ok(StateOp::Suspend(false)),
        (USAGE, Method::Get) => Ok(StateOp::Usage),
        (SUSPEND | RESUME, Method::Get) | (USAGE, Method::Post) => Err(405),
        _ => return None,
    })
}

fn as_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// The limits as the port gives them; 0 is no limit.
pub fn limits_doc(l: &TenantLimits) -> Document {
    let service = l.service_user.as_ref().map_or(Value::Null, |u| {
        Value::Document(Document::new().with("user", u.user.as_str()).with("db", u.db.as_str()))
    });
    Document::new()
        .with(field::STORAGE_BYTES, as_i64(l.storage_bytes))
        .with(field::MAX_CONNECTIONS, i64::from(l.max_connections))
        .with(field::OPS_PER_SECOND, i64::from(l.ops_per_second))
        .with(field::MAX_API_KEYS, i64::from(l.max_api_keys))
        .with(field::MAX_TIME_MS, i64::from(l.max_time_ms))
        .with(field::MAX_CONCURRENT_OPS, i64::from(l.max_concurrent_ops))
        .with(field::MAX_WRITE_BYTES_PER_SECOND, as_i64(l.max_write_bytes_per_second))
        .with(field::MAX_DATABASES, i64::from(l.max_databases))
        .with(field::MAX_COLLECTIONS, i64::from(l.max_collections))
        .with(field::MAX_SEARCH_INDEXES, i64::from(l.max_search_indexes))
        .with(field::HTTP_ENABLED, l.http_enabled)
        .with(field::MCP_ENABLED, l.mcp_enabled)
        .with(field::SERVICE_USER, service)
}

/// The whole state of a tenant: its limits, whether it is suspended and its access list.
pub fn state_doc(s: &TenantState) -> Document {
    limits_doc(&s.limits)
        .with(field::SUSPENDED, s.suspended)
        .with(field::ALLOWED_SOURCES, strings(&s.allowed_sources))
}

fn strings(all: &[String]) -> Value {
    Value::Array(all.iter().map(|c| Value::String(c.clone())).collect())
}

/// A whole number from 0 up to `max` in field `name`.
fn count(name: &str, v: &Value, max: u64) -> Result<u64, AdminError> {
    let n = match v {
        Value::Int32(n) => i128::from(*n),
        Value::Int64(n) => i128::from(*n),
        Value::Double(f) if f.fract() == 0.0 && f.is_finite() => *f as i128,
        _ => -1,
    };
    u64::try_from(n)
        .ok()
        .filter(|n| *n <= max)
        .ok_or_else(|| AdminError::bad(format!("{name}: a whole number from 0 to {max}")))
}

fn small(name: &str, v: &Value) -> Result<u32, AdminError> {
    let n = count(name, v, u64::from(u32::MAX))?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

fn switch(name: &str, v: &Value) -> Result<bool, AdminError> {
    match v {
        Value::Boolean(b) => Ok(*b),
        _ => Err(AdminError::bad(format!("{name}: true or false"))),
    }
}

fn service_user(v: &Value) -> Result<Option<ServiceUser>, AdminError> {
    let bad = || AdminError::bad(format!("{}: {{user, db}} or null", field::SERVICE_USER));
    let Value::Document(d) = v else {
        return if *v == Value::Null { Ok(None) } else { Err(bad()) };
    };
    let text = |k: &str| match d.get(k) {
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err(bad()),
    };
    let db = if d.get("db").is_none() { SERVICE_USER_DB.to_owned() } else { text("db")? };
    Ok(Some(ServiceUser { user: text("user")?, db }))
}

/// `current` with the limits `body` gives; a field it does not give stays.
pub fn merge(current: &TenantLimits, body: &Document) -> Result<TenantLimits, AdminError> {
    let big = i64::MAX.unsigned_abs();
    let mut l = current.clone();
    for (k, v) in body.iter() {
        match k {
            field::STORAGE_BYTES => l.storage_bytes = count(k, v, big)?,
            field::MAX_CONNECTIONS => l.max_connections = small(k, v)?,
            field::OPS_PER_SECOND => l.ops_per_second = small(k, v)?,
            field::MAX_API_KEYS => l.max_api_keys = small(k, v)?,
            field::MAX_TIME_MS => l.max_time_ms = small(k, v)?,
            field::MAX_CONCURRENT_OPS => l.max_concurrent_ops = small(k, v)?,
            field::MAX_WRITE_BYTES_PER_SECOND => l.max_write_bytes_per_second = count(k, v, big)?,
            field::MAX_DATABASES => l.max_databases = small(k, v)?,
            field::MAX_COLLECTIONS => l.max_collections = small(k, v)?,
            field::MAX_SEARCH_INDEXES => l.max_search_indexes = small(k, v)?,
            field::HTTP_ENABLED => l.http_enabled = switch(k, v)?,
            field::MCP_ENABLED => l.mcp_enabled = switch(k, v)?,
            field::SERVICE_USER => l.service_user = service_user(v)?,
            other => return Err(AdminError::bad(format!("unknown limit {other}"))),
        }
    }
    Ok(l)
}

/// The access list `body` gives in `allowedSources`, each an address or a CIDR block; without
/// it, the list is emptied.
pub fn sources(body: &Document) -> Result<Vec<String>, AdminError> {
    if let Some((k, _)) = body.iter().find(|(k, _)| *k != field::ALLOWED_SOURCES) {
        return Err(AdminError::bad(format!("unknown field {k}")));
    }
    let list = match body.get(field::ALLOWED_SOURCES) {
        None => Vec::new(),
        Some(Value::Array(all)) => all.clone(),
        Some(_) => return Err(AdminError::bad(format!("{}: an array", field::ALLOWED_SOURCES))),
    };
    if list.len() > MAX_SOURCES {
        return Err(AdminError::bad(format!("an access list holds up to {MAX_SOURCES} sources")));
    }
    list.iter()
        .map(|v| match v {
            Value::String(s) if cidr(s).is_some() => Ok(s.clone()),
            other => Err(AdminError::bad(format!(
                "not an address or a CIDR block: {}",
                teseladb_wire::commands::display(other)
            ))),
        })
        .collect()
}

/// The command of `op` on tenant `id` with state `state`, or the answer of a read.
fn command(
    id: u32,
    state: &TenantState,
    op: StateOp,
) -> Result<Result<Group0Command, Document>, AdminError> {
    Ok(Ok(match op {
        StateOp::Limits(None) => return Ok(Err(limits_doc(&state.limits).with("ok", 1.0))),
        StateOp::Access(None) => return Ok(Err(access_doc(&state.allowed_sources))),
        StateOp::Limits(Some(body)) => {
            Group0Command::SetTenantLimits { tenant: id, limits: merge(&state.limits, &body)? }
        }
        StateOp::Access(Some(body)) => {
            Group0Command::SetTenantAccess { tenant: id, sources: sources(&body)? }
        }
        StateOp::Suspend(suspended) => Group0Command::SetTenantSuspended { tenant: id, suspended },
        // Read by the caller from the node's counters.
        StateOp::Usage => return Ok(Err(Document::new())),
    }))
}

/// What the port answers once `cmd` is applied.
fn answer_of(cmd: &Group0Command) -> Document {
    match cmd {
        Group0Command::SetTenantLimits { limits, .. } => limits_doc(limits).with("ok", 1.0),
        Group0Command::SetTenantAccess { sources, .. } => access_doc(sources),
        _ => Document::new().with("ok", 1.0),
    }
}

fn access_doc(sources: &[String]) -> Document {
    Document::new().with(field::ALLOWED_SOURCES, strings(sources)).with("ok", 1.0)
}

/// Does `op` on tenant `name` through node `a`, and answers its limits or access list after it.
pub fn apply(a: &NodeAdmin, name: &str, op: StateOp) -> Answer {
    let (all, ready) = crate::admin_tenants::listed(a)?;
    let (id, _, state) = all
        .into_iter()
        .find(|(_, n, _)| n == name)
        .ok_or_else(|| AdminError::not_found(format!("no tenant named '{name}'")))?;
    if op == StateOp::Usage {
        return Ok(crate::tenant_limits::usage_doc(&a.handle.routes, id));
    }
    let cmd = match command(id, &state, op)? {
        Ok(cmd) => cmd,
        Err(read) => return Ok(read),
    };
    if !ready {
        let waits = teseladb_meta::group0::Group0Error::NotEveryNodeRunsIt;
        return Err(crate::admin_tenants::refusal(name, waits));
    }
    let answer = answer_of(&cmd);
    crate::admin_tenants::change(a, name, cmd)?;
    Ok(answer)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_parts_of_the_path_and_their_methods() {
        assert_eq!(op_of(Method::Get, LIMITS, None), Some(Ok(StateOp::Limits(None))));
        let body = Document::new().with(field::MAX_CONNECTIONS, 5);
        assert_eq!(
            op_of(Method::Post, LIMITS, Some(body.clone())),
            Some(Ok(StateOp::Limits(Some(body))))
        );
        assert_eq!(
            op_of(Method::Post, ACCESS, None),
            Some(Ok(StateOp::Access(Some(Document::new()))))
        );
        assert_eq!(op_of(Method::Get, ACCESS, None), Some(Ok(StateOp::Access(None))));
        assert_eq!(op_of(Method::Post, SUSPEND, None), Some(Ok(StateOp::Suspend(true))));
        assert_eq!(op_of(Method::Post, RESUME, None), Some(Ok(StateOp::Suspend(false))));
        assert_eq!(op_of(Method::Get, SUSPEND, None), Some(Err(405)));
        assert_eq!(op_of(Method::Post, "usage-x", None), None);
    }

    #[test]
    fn limits_change_only_where_the_body_says_and_read_back() {
        let body = Document::new()
            .with(field::STORAGE_BYTES, 512_i64 << 20)
            .with(field::MAX_CONNECTIONS, 50)
            .with(field::OPS_PER_SECOND, 100.0)
            .with(field::MAX_API_KEYS, 5)
            .with(field::MAX_TIME_MS, 30_000)
            .with(field::MAX_CONCURRENT_OPS, 10)
            .with(field::MAX_WRITE_BYTES_PER_SECOND, 1_i64 << 20)
            .with(field::MAX_DATABASES, 20)
            .with(field::MAX_COLLECTIONS, 200)
            .with(field::MAX_SEARCH_INDEXES, 3)
            .with(field::HTTP_ENABLED, false)
            .with(field::MCP_ENABLED, true)
            .with(field::SERVICE_USER, Document::new().with("user", "svc.x"));
        let l = merge(&TenantLimits::default(), &body).unwrap();
        assert_eq!((l.storage_bytes, l.ops_per_second, l.http_enabled), (512 << 20, 100, false));
        let svc = l.service_user.clone().unwrap();
        assert_eq!((svc.user.as_str(), svc.db.as_str()), ("svc.x", SERVICE_USER_DB));
        assert_eq!(merge(&TenantLimits::default(), &limits_doc(&l)).unwrap(), l);
        // A body that gives one field leaves the others, and null takes the service user away.
        let one =
            Document::new().with(field::MAX_CONNECTIONS, 0).with(field::SERVICE_USER, Value::Null);
        let back = merge(&l, &one).unwrap();
        assert_eq!((back.max_connections, back.service_user, back.ops_per_second), (0, None, 100));
        for bad in [
            Document::new().with(field::MAX_CONNECTIONS, -1),
            Document::new().with(field::MAX_CONNECTIONS, 1_i64 << 40),
            Document::new().with(field::MAX_CONNECTIONS, 1.5),
            Document::new().with(field::MAX_CONNECTIONS, "5"),
            Document::new().with(field::HTTP_ENABLED, 1),
            Document::new().with(field::SERVICE_USER, "svc"),
            Document::new().with(field::SERVICE_USER, Document::new().with("user", "")),
            Document::new()
                .with(field::SERVICE_USER, Document::new().with("user", "u").with("db", 1)),
            Document::new().with("maxNothing", 1),
        ] {
            assert_eq!(merge(&l, &bad).map_err(|e| e.status), Err(400), "{bad:?}");
        }
    }

    #[test]
    fn an_access_list_holds_addresses_and_blocks() {
        let list = |v: Vec<&str>| {
            Document::new().with(
                field::ALLOWED_SOURCES,
                Value::Array(v.into_iter().map(|s| Value::String(s.into())).collect()),
            )
        };
        let ok = sources(&list(vec!["10.0.0.0/8", "2001:db8::/32", "203.0.113.9"])).unwrap();
        assert_eq!(ok.len(), 3);
        assert_eq!(sources(&Document::new()).unwrap(), Vec::<String>::new());
        let many: Vec<String> =
            (0..=MAX_SOURCES).map(|i| format!("10.0.{}.{}", i / 256, i % 256)).collect();
        let too_many = list(many.iter().map(String::as_str).collect());
        for bad in [
            list(vec!["10.0.0.0/33"]),
            list(vec!["nope"]),
            Document::new().with(field::ALLOWED_SOURCES, Value::Array(vec![Value::Int32(1)])),
            Document::new().with(field::ALLOWED_SOURCES, "10.0.0.0/8"),
            Document::new().with("other", 1),
            too_many,
        ] {
            assert_eq!(sources(&bad).map_err(|e| e.status), Err(400));
        }
    }

    #[test]
    fn a_read_answers_at_once_and_a_change_becomes_its_entry() {
        let state = TenantState {
            suspended: true,
            allowed_sources: vec!["10.0.0.0/8".into()],
            ..TenantState::default()
        };
        let read = command(3, &state, StateOp::Access(None)).unwrap().unwrap_err();
        assert_eq!(read.get(field::ALLOWED_SOURCES), Some(&strings(&state.allowed_sources)));
        let limits = command(3, &state, StateOp::Limits(None)).unwrap().unwrap_err();
        assert_eq!(limits.get(field::HTTP_ENABLED), Some(&Value::Boolean(true)));
        let cmd = command(3, &state, StateOp::Suspend(false)).unwrap().unwrap();
        assert_eq!(cmd, Group0Command::SetTenantSuspended { tenant: 3, suspended: false });
        assert_eq!(answer_of(&cmd), Document::new().with("ok", 1.0));
        let body = Document::new().with(field::MAX_DATABASES, 2);
        let cmd = command(3, &state, StateOp::Limits(Some(body))).unwrap().unwrap();
        assert_eq!(answer_of(&cmd).get(field::MAX_DATABASES), Some(&Value::Int64(2)));
        let body = Document::new().with(field::ALLOWED_SOURCES, Value::Array(Vec::new()));
        let cmd = command(3, &state, StateOp::Access(Some(body))).unwrap().unwrap();
        assert_eq!(cmd, Group0Command::SetTenantAccess { tenant: 3, sources: Vec::new() });
        assert_eq!(answer_of(&cmd).get(field::ALLOWED_SOURCES), Some(&Value::Array(Vec::new())));
        let state_view = state_doc(&state);
        assert_eq!(state_view.get(field::SUSPENDED), Some(&Value::Boolean(true)));
    }
}
