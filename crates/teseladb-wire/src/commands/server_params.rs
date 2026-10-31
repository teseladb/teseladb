//! Server parameters of a node (006 FR-017, FR-023): `getParameter` and `setParameter`, with the
//! forms, coercions and errors of a `refrouter` 9.0.4 (measured). Each node keeps its own, as each
//! `refrouter` does; `getParameter: "*"` lists TeselaDB's.

use teseladb_bson::{Document, Value};

use super::admin::admin_only;
use super::{Backend, Ctx, NodeSessions, lock};
use crate::error::{
    BAD_VALUE, CommandError, ILLEGAL_CLUSTER_AUTH_MODE_TRANSITION, ILLEGAL_OPERATION,
    INVALID_OPTIONS,
};

/// `getParameter`.
pub const GET_PARAMETER: &str = "getParameter";
/// `setParameter`.
pub const SET_PARAMETER: &str = "setParameter";
/// Idle time after which a cursor outside any session dies.
pub const CURSOR_TIMEOUT_MILLIS: &str = "cursorTimeoutMillis";
/// Period of the reaping of cursors and sessions.
pub const CLIENT_CURSOR_MONITOR_FREQUENCY_SECS: &str = "clientCursorMonitorFrequencySecs";
/// Idle time after which a session dies.
pub const LOCAL_LOGICAL_SESSION_TIMEOUT_MINUTES: &str = "localLogicalSessionTimeoutMinutes";
/// The test commands (`configureFailPoint`).
pub const ENABLE_TEST_COMMANDS: &str = "enableTestCommands";
/// The redaction of the clients' documents in the log (013 FR-027).
pub const REDACT_CLIENT_LOG_DATA: &str = "redactClientLogData";
/// Successful authorization checks go to the audit log too (013 FR-016).
pub const AUDIT_AUTHORIZATION_SUCCESS: &str = "auditAuthorizationSuccess";
/// The version of the users' and roles' schema, which `refdump` and `refrestore` read
/// (013 FR-030); it cannot change.
pub const AUTH_SCHEMA_VERSION: &str = "authSchemaVersion";
/// OCSP stapling of the server's certificate (013 FR-031): on by default, set at startup.
pub const OCSP_ENABLED: &str = "ocspEnabled";
/// The OCSP parameters a `refnode` 9.0.4 shows with their defaults (measured): TeselaDB refreshes
/// the staple halfway to its `nextUpdate` and waits five seconds for the responder.
pub const OCSP_STAPLING_REFRESH_PERIOD_SECS: &str = "ocspStaplingRefreshPeriodSecs";
/// See [`OCSP_STAPLING_REFRESH_PERIOD_SECS`].
pub const OCSP_VALIDATION_REFRESH_PERIOD_SECS: &str = "ocspValidationRefreshPeriodSecs";
/// See [`OCSP_STAPLING_REFRESH_PERIOD_SECS`].
pub const TLS_OCSP_STAPLING_TIMEOUT_SECS: &str = "tlsOCSPStaplingTimeoutSecs";
/// See [`OCSP_STAPLING_REFRESH_PERIOD_SECS`].
pub const TLS_OCSP_VERIFY_TIMEOUT_SECS: &str = "tlsOCSPVerifyTimeoutSecs";
/// `tlsOCSPVerifyTimeoutSecs` by default (measured).
const TLS_OCSP_VERIFY_TIMEOUT_DEFAULT: i32 = 4;
/// `transactionLifetimeLimitSeconds`: a transaction older than this is aborted (008 FR-010).
pub const TRANSACTION_LIFETIME_LIMIT_SECONDS: &str = "transactionLifetimeLimitSeconds";
/// Its default, as a `refnode` 9.0.4 has it (measured).
pub const TRANSACTION_LIFETIME_DEFAULT_SECS: i64 = 60;
/// Internal authorization for REFDB-X509 users under LDAP authorization (013 FR-033): kept and
/// shown, and, as on RefDb Enterprise 9.0.4 (measured), it changes nothing: those users keep
/// the roles of their LDAP groups.
pub const USE_INTERNAL_AUTHZ_FOR_X509: &str = "useInternalAuthzForX509";
/// `maxConcurrentMultiDocumentTransactions`: the transactions of clients a node keeps open at
/// once; 0 lifts the limit (008 FR-020).
pub const MAX_CONCURRENT_MULTI_DOCUMENT_TRANSACTIONS: &str =
    "maxConcurrentMultiDocumentTransactions";
/// Its default, as a `refnode` 9.0.4 has it (measured).
pub const MAX_CONCURRENT_TRANSACTIONS_DEFAULT: i64 = 10_000;
/// How members of the cluster authenticate (013 FR-034): set at startup with
/// `--clusterAuthMode` and moved one step at a time at runtime.
pub const CLUSTER_AUTH_MODE: &str = "clusterAuthMode";
/// `getParameter: "*"`.
const ALL: &str = "*";

/// The values of `clusterAuthMode`, in the order of the steps a node may take at runtime
/// (measured on `refnode` 9.0.4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ClusterAuthMode {
    /// Neither `--keyFile` nor `--clusterAuthMode`.
    #[default]
    Undefined,
    /// The shared key of `--keyFile`.
    KeyFile,
    /// The shared key accepted, X.509 certificates sent.
    SendKeyFile,
    /// X.509 certificates sent, the shared key still accepted.
    SendX509,
    /// X.509 certificates only.
    X509,
}

impl ClusterAuthMode {
    /// Every value, in order.
    pub const ALL: [ClusterAuthMode; 5] = [
        ClusterAuthMode::Undefined,
        ClusterAuthMode::KeyFile,
        ClusterAuthMode::SendKeyFile,
        ClusterAuthMode::SendX509,
        ClusterAuthMode::X509,
    ];

    /// Its name.
    pub fn name(self) -> &'static str {
        match self {
            ClusterAuthMode::Undefined => "undefined",
            ClusterAuthMode::KeyFile => "keyFile",
            ClusterAuthMode::SendKeyFile => "sendKeyFile",
            ClusterAuthMode::SendX509 => "sendX509",
            ClusterAuthMode::X509 => "x509",
        }
    }

    /// The value named `name` that `--clusterAuthMode` and `setParameter` take (not
    /// `undefined`).
    pub fn parse(name: &str) -> Option<ClusterAuthMode> {
        ClusterAuthMode::ALL[1..].iter().copied().find(|m| m.name() == name)
    }

    /// Whether a client with a member's X.509 certificate logs in as `__system`.
    pub fn accepts_member_certificates(self) -> bool {
        matches!(self, ClusterAuthMode::SendX509 | ClusterAuthMode::X509)
    }

    /// Whether it uses the shared key, which relaxes the secure default (constitution IX).
    pub fn uses_shared_key(self) -> bool {
        matches!(self, ClusterAuthMode::KeyFile | ClusterAuthMode::SendKeyFile)
    }

    fn index(self) -> i64 {
        ClusterAuthMode::ALL.iter().position(|m| *m == self).map_or(0, |i| i as i64)
    }

    fn from_index(n: i64) -> ClusterAuthMode {
        usize::try_from(n)
            .ok()
            .and_then(|i| ClusterAuthMode::ALL.get(i).copied())
            .unwrap_or_default()
    }
}

/// The runtime values of a node's parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerParams {
    /// `cursorTimeoutMillis` as it was set (a negative value is kept and kills idle cursors at
    /// the next reaping).
    pub cursor_timeout_ms: i64,
    /// Period of the reaping, in milliseconds (`clientCursorMonitorFrequencySecs` × 1000).
    pub monitor_ms: i64,
    /// `redactClientLogData`: the documents of the log lines go redacted.
    pub redact_client_log_data: bool,
    /// `transactionLifetimeLimitSeconds`.
    pub transaction_lifetime_secs: i64,
    /// `maxConcurrentMultiDocumentTransactions`.
    pub max_concurrent_transactions: i64,
    /// `auditAuthorizationSuccess` of a node without an audit log (with one, the auditor keeps
    /// it).
    pub audit_authorization_success: bool,
    /// `ocspEnabled`.
    pub ocsp_enabled: bool,
    /// `useInternalAuthzForX509`.
    pub use_internal_authz_for_x509: bool,
    /// `clusterAuthMode`.
    pub cluster_auth_mode: ClusterAuthMode,
    /// Whether the node has TLS: the modes other than `keyFile` need it.
    pub tls_enabled: bool,
}

impl Default for ServerParams {
    fn default() -> ServerParams {
        ServerParams {
            cursor_timeout_ms: i64::try_from(teseladb_session::cursor::CURSOR_TIMEOUT_MS)
                .unwrap_or(i64::MAX),
            monitor_ms: i64::try_from(super::REAP_INTERVAL_MS).unwrap_or(i64::MAX),
            redact_client_log_data: false,
            transaction_lifetime_secs: TRANSACTION_LIFETIME_DEFAULT_SECS,
            max_concurrent_transactions: MAX_CONCURRENT_TRANSACTIONS_DEFAULT,
            audit_authorization_success: false,
            ocsp_enabled: true,
            use_internal_authz_for_x509: false,
            cluster_auth_mode: ClusterAuthMode::Undefined,
            tls_enabled: true,
        }
    }
}

/// The parameters TeselaDB has, in name order: name, settable at runtime, settable at startup
/// with `--setParameter` (`redactClientLogData` starts with its own option, measured).
const KNOWN: [(&str, bool, bool); 16] = [
    (AUDIT_AUTHORIZATION_SUCCESS, true, true),
    (AUTH_SCHEMA_VERSION, false, false),
    (CLIENT_CURSOR_MONITOR_FREQUENCY_SECS, true, true),
    (CLUSTER_AUTH_MODE, true, false),
    (CURSOR_TIMEOUT_MILLIS, true, true),
    (ENABLE_TEST_COMMANDS, false, true),
    (LOCAL_LOGICAL_SESSION_TIMEOUT_MINUTES, false, true),
    (MAX_CONCURRENT_MULTI_DOCUMENT_TRANSACTIONS, true, true),
    (OCSP_ENABLED, false, true),
    (OCSP_STAPLING_REFRESH_PERIOD_SECS, false, true),
    (OCSP_VALIDATION_REFRESH_PERIOD_SECS, false, true),
    (REDACT_CLIENT_LOG_DATA, true, false),
    (TLS_OCSP_STAPLING_TIMEOUT_SECS, false, true),
    (TLS_OCSP_VERIFY_TIMEOUT_SECS, false, true),
    (TRANSACTION_LIFETIME_LIMIT_SECONDS, true, true),
    (USE_INTERNAL_AUTHZ_FOR_X509, true, true),
];

/// The names of the parameters, in name order (`getParameter`'s help).
pub fn names() -> impl Iterator<Item = &'static str> {
    KNOWN.iter().map(|(name, _, _)| *name)
}

/// The names of the parameters settable at runtime (`setParameter`'s help).
pub fn runtime_names() -> impl Iterator<Item = &'static str> {
    KNOWN.iter().filter(|(_, runtime, _)| *runtime).map(|(name, _, _)| *name)
}

fn value_of(name: &str, s: &NodeSessions) -> Value {
    match name {
        AUDIT_AUTHORIZATION_SUCCESS => Value::Boolean(match &s.audit {
            Some(a) => a.current().config().authorization_success,
            None => s.params.audit_authorization_success,
        }),
        AUTH_SCHEMA_VERSION => Value::Int32(super::authz_collections::AUTH_SCHEMA_VERSION),
        CLIENT_CURSOR_MONITOR_FREQUENCY_SECS => Value::Int32(clamp_i32(s.params.monitor_ms / 1000)),
        CLUSTER_AUTH_MODE => Value::from(s.params.cluster_auth_mode.name()),
        CURSOR_TIMEOUT_MILLIS => Value::Int64(s.params.cursor_timeout_ms),
        ENABLE_TEST_COMMANDS => Value::Boolean(s.failpoint.is_some()),
        OCSP_ENABLED => Value::Boolean(s.params.ocsp_enabled),
        OCSP_STAPLING_REFRESH_PERIOD_SECS | OCSP_VALIDATION_REFRESH_PERIOD_SECS => Value::Int64(-1),
        TLS_OCSP_STAPLING_TIMEOUT_SECS => Value::Int32(-1),
        TLS_OCSP_VERIFY_TIMEOUT_SECS => Value::Int32(TLS_OCSP_VERIFY_TIMEOUT_DEFAULT),
        REDACT_CLIENT_LOG_DATA => Value::Boolean(s.params.redact_client_log_data),
        USE_INTERNAL_AUTHZ_FOR_X509 => Value::Boolean(s.params.use_internal_authz_for_x509),
        TRANSACTION_LIFETIME_LIMIT_SECONDS => {
            Value::Int32(clamp_i32(s.params.transaction_lifetime_secs))
        }
        MAX_CONCURRENT_MULTI_DOCUMENT_TRANSACTIONS => {
            Value::Int32(clamp_i32(s.params.max_concurrent_transactions))
        }
        _ => Value::Int32(clamp_i32(
            i64::try_from(s.sessions.timeout_ms() / 60_000).unwrap_or(i64::MAX),
        )),
    }
}

fn clamp_i32(n: i64) -> i32 {
    i32::try_from(n).unwrap_or(if n < 0 { i32::MIN } else { i32::MAX })
}

/// A value read as an integer as RefDb coerces a parameter's (`1.5` is 1, `true` is 1).
fn integral(name: &str, v: &Value) -> Result<i64, CommandError> {
    let failed =
        |why: &str| CommandError::new(BAD_VALUE, format!("Failed validating {name}: {why}"));
    let unable = || failed("Unable to coerce value to integral type");
    let x = match v {
        Value::Int32(n) => return Ok(i64::from(*n)),
        Value::Int64(n) => return Ok(*n),
        Value::Boolean(b) => return Ok(i64::from(*b)),
        Value::Double(_) | Value::Decimal128(_) => crate::types::number(v).ok_or_else(unable)?,
        _ => return Err(unable()),
    };
    if x.is_nan() {
        return Err(unable());
    }
    if !(-(2f64.powi(63))..2f64.powi(63)).contains(&x) {
        return Err(failed("Out of bounds coercing to integral value"));
    }
    Ok(x.trunc() as i64)
}

/// A new `auditAuthorizationSuccess`: a boolean or a number, not with the runtime audit
/// configuration (measured on RefDb Enterprise 9.0.4).
fn audit_authorization_success(shared: &super::Shared, v: &Value) -> Result<i64, CommandError> {
    let runtime = lock(shared).audit.as_ref().is_some_and(|a| a.current().config().runtime);
    if runtime {
        let msg = format!(
            "{AUDIT_AUTHORIZATION_SUCCESS} may not be changed via set parameter when runtime audit configuration is enabled"
        );
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    match v {
        Value::Boolean(b) => Ok(i64::from(*b)),
        Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_) => {
            Ok(i64::from(crate::types::bson_true(v)))
        }
        other => Err(CommandError::new(
            BAD_VALUE,
            format!(
                "{AUDIT_AUTHORIZATION_SUCCESS} expects bool, got {}",
                crate::types::type_name(other)
            ),
        )),
    }
}

/// `getParameter`: the named parameters, or all with `"*"`; `{showDetails: true}` gives each
/// with whether it can be set at startup and at runtime.
pub fn get_parameter<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(GET_PARAMETER, ctx.db)?;
    let details = matches!(ctx.first(), Value::Document(d)
        if d.get("showDetails").and_then(crate::types::truthy) == Some(true));
    let all = matches!(ctx.first(), Value::String(s) if s == ALL);
    let g = lock(ctx.shared);
    let mut out = Document::new();
    for (name, runtime, startup) in KNOWN {
        if !all && ctx.cmd.get(name).is_none() {
            continue;
        }
        let v = value_of(name, &g);
        let v = match details {
            true => Value::Document(
                Document::new()
                    .with("value", v)
                    .with("settableAtRuntime", runtime)
                    .with("settableAtStartup", startup),
            ),
            false => v,
        };
        out.insert(name, v);
    }
    if out.is_empty() {
        return Err(CommandError::new(INVALID_OPTIONS, "no option found to get"));
    }
    Ok(out.with("ok", 1.0))
}

/// The `clusterAuthMode` that `v` asks, one step after the node's (measured on `refnode` 9.0.4):
/// any mode but `keyFile` needs TLS; another step is refused.
fn cluster_auth_mode_step(
    shared: &super::Shared,
    v: &Value,
) -> Result<ClusterAuthMode, CommandError> {
    let text = match v {
        Value::String(s) => s.clone(),
        other => super::display(other),
    };
    let Some(to) = ClusterAuthMode::parse(&text) else {
        return Err(CommandError::new(BAD_VALUE, format!("Invalid clusterAuthMode '{text}'")));
    };
    let g = lock(shared);
    let from = g.params.cluster_auth_mode;
    if to != ClusterAuthMode::KeyFile && !g.params.tls_enabled {
        let msg = "Illegal state transition for clusterAuthMode, need to enable SSL for outgoing connections";
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    if to.index() != from.index() + 1 {
        let msg = format!(
            "Illegal state transition for clusterAuthMode from '{}' to '{}'",
            from.name(),
            to.name()
        );
        return Err(CommandError::new(ILLEGAL_CLUSTER_AUTH_MODE_TRANSITION, msg));
    }
    Ok(to)
}

/// `setParameter`: every parameter is checked before any changes; the answer has the old value
/// of the first.
pub fn set_parameter<B: Backend + ?Sized>(ctx: &mut Ctx<'_, B>) -> Result<Document, CommandError> {
    admin_only(SET_PARAMETER, ctx.db)?;
    let mut changes = Vec::new();
    for (k, v) in ctx.cmd.iter().skip(1) {
        if k.starts_with('$') || crate::generic::is_generic(k) {
            continue;
        }
        let Some((name, runtime, _)) = KNOWN.iter().find(|(n, _, _)| *n == k) else {
            let msg = format!(
                "attempted to set unrecognized parameter [{k}], use help:true to see options "
            );
            return Err(CommandError::new(INVALID_OPTIONS, msg));
        };
        if !runtime {
            let msg = format!("not allowed to change [{name}] at runtime");
            return Err(CommandError::new(ILLEGAL_OPERATION, msg));
        }
        let n = match *name {
            REDACT_CLIENT_LOG_DATA | USE_INTERNAL_AUTHZ_FOR_X509 => {
                i64::from(crate::types::bson_true(v))
            }
            AUDIT_AUTHORIZATION_SUCCESS => audit_authorization_success(ctx.shared, v)?,
            CLUSTER_AUTH_MODE => cluster_auth_mode_step(ctx.shared, v)?.index(),
            _ => integral(name, v)?,
        };
        let least = match *name {
            TRANSACTION_LIFETIME_LIMIT_SECONDS => 1,
            MAX_CONCURRENT_MULTI_DOCUMENT_TRANSACTIONS => 0,
            _ => i64::MIN,
        };
        if n < least {
            let msg = format!(
                "Invalid value for parameter {name}: {n} is not greater than or equal to {least}"
            );
            return Err(CommandError::new(BAD_VALUE, msg));
        }
        changes.push((*name, n));
    }
    let Some((first, _)) = changes.first() else {
        let msg = "no option found to set, use help:true to see options ";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    };
    let mut g = lock(ctx.shared);
    let was = value_of(first, &g);
    for (name, n) in changes {
        match name {
            CURSOR_TIMEOUT_MILLIS => {
                g.params.cursor_timeout_ms = n;
                g.cursors.set_timeout_ms(u64::try_from(n).unwrap_or(0));
            }
            REDACT_CLIENT_LOG_DATA => g.params.redact_client_log_data = n != 0,
            USE_INTERNAL_AUTHZ_FOR_X509 => g.params.use_internal_authz_for_x509 = n != 0,
            CLUSTER_AUTH_MODE => g.params.cluster_auth_mode = ClusterAuthMode::from_index(n),
            AUDIT_AUTHORIZATION_SUCCESS => match g.audit.as_ref() {
                Some(a) => {
                    let mut w =
                        a.auditor.write().unwrap_or_else(std::sync::PoisonError::into_inner);
                    w.set_authorization_success(n != 0);
                }
                None => g.params.audit_authorization_success = n != 0,
            },
            TRANSACTION_LIFETIME_LIMIT_SECONDS => g.params.transaction_lifetime_secs = n,
            MAX_CONCURRENT_MULTI_DOCUMENT_TRANSACTIONS => g.params.max_concurrent_transactions = n,
            _ => g.params.monitor_ms = n.saturating_mul(1000),
        }
    }
    Ok(Document::new().with("was", was).with("ok", 1.0))
}
