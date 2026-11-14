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
use crate::ingress;

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
/// Index intersection by hashing (`AND_HASH` plans, 010 FR-001): off by default, as on a
/// `refrouter` 9.0.4 (measured).
pub const HASH_INTERSECTION: &str = "internalQueryPlannerEnableHashIntersection";
/// The memory one operation may hold in its stages (010 FR-010); past it a `$group` writes to
/// disk, or fails with 292 without `allowDiskUse` (measured on a `refnode` 9.0.4).
pub const MAX_OPERATION_MEMORY: &str = "internalQueryMaxMemoryUsageBytesPerOperation";
/// Its default: 1 GiB, the least RefDb gives it.
pub const MAX_OPERATION_MEMORY_DEFAULT: i64 = 1 << 30;
/// The memory one constant expression may hold when it is folded (010 FR-010).
pub const MAX_EXPRESSION_MEMORY: &str = "internalQueryMaxSingleExpressionMemoryUsageBytes";
/// How many queries a second `$queryStats` counts when it does not sample: -1 all, 0 none
/// (010 FR-023; 0 by default on a `refrouter` 9.0.4, measured).
pub const QUERY_STATS_RATE_LIMIT: &str = "internalQueryStatsRateLimit";
/// The probability that `$queryStats` counts a query; 0 leaves it to the rate limit.
pub const QUERY_STATS_SAMPLE_RATE: &str = "internalQueryStatsSampleRate";
/// [`QUERY_STATS_SAMPLE_RATE`] by default (measured on a `refrouter` 9.0.4).
pub const QUERY_STATS_SAMPLE_RATE_DEFAULT: f64 = 0.01;
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
    /// Whether the connections the node opens use TLS (`preferTLS` or `requireTLS`): the modes
    /// other than `keyFile` need it.
    pub tls_outgoing: bool,
    /// `internalQueryMaxMemoryUsageBytesPerOperation`.
    pub operation_memory: i64,
    /// `internalQueryStatsRateLimit`.
    pub query_stats_rate_limit: i64,
    /// The bits of `internalQueryStatsSampleRate` (an `f64`, kept as bits so the parameters
    /// compare exactly).
    pub query_stats_sample_bits: u64,
    /// `ocspStaplingRefreshPeriodSecs` as the start gave it; `-1`, the period of the response.
    pub ocsp_stapling_refresh_secs: i64,
    /// `ocspValidationRefreshPeriodSecs` as the start gave it; `-1`, the period of the response.
    pub ocsp_validation_refresh_secs: i64,
    /// `tlsOCSPStaplingTimeoutSecs` as the start gave it; `-1`, the default of the TLS stack.
    pub tls_ocsp_stapling_timeout_secs: i64,
    /// `tlsOCSPVerifyTimeoutSecs` as the start gave it.
    pub tls_ocsp_verify_timeout_secs: i64,
}

impl ServerParams {
    /// `internalQueryStatsSampleRate`.
    pub fn query_stats_sample_rate(&self) -> f64 {
        f64::from_bits(self.query_stats_sample_bits)
    }
}

impl Default for ServerParams {
    fn default() -> ServerParams {
        ServerParams {
            cursor_timeout_ms: i64::try_from(teseladb_session::cursor::CURSOR_TIMEOUT_MS)
                .unwrap_or(i64::MAX),
            monitor_ms: i64::try_from(super::REAP_INTERVAL_MS).unwrap_or(i64::MAX),
            transaction_lifetime_secs: TRANSACTION_LIFETIME_DEFAULT_SECS,
            max_concurrent_transactions: MAX_CONCURRENT_TRANSACTIONS_DEFAULT,
            audit_authorization_success: false,
            ocsp_enabled: true,
            use_internal_authz_for_x509: false,
            cluster_auth_mode: ClusterAuthMode::Undefined,
            tls_outgoing: true,
            operation_memory: MAX_OPERATION_MEMORY_DEFAULT,
            query_stats_rate_limit: 0,
            query_stats_sample_bits: QUERY_STATS_SAMPLE_RATE_DEFAULT.to_bits(),
            ocsp_stapling_refresh_secs: -1,
            ocsp_validation_refresh_secs: -1,
            tls_ocsp_stapling_timeout_secs: -1,
            tls_ocsp_verify_timeout_secs: i64::from(TLS_OCSP_VERIFY_TIMEOUT_DEFAULT),
        }
    }
}

/// The parameters TeselaDB has, in name order: name, settable at runtime, settable at startup
/// with `--setParameter` (`redactClientLogData` starts with its own option, measured).
const KNOWN: [(&str, bool, bool); 26] = [
    (AUDIT_AUTHORIZATION_SUCCESS, true, true),
    (AUTH_SCHEMA_VERSION, false, false),
    (CLIENT_CURSOR_MONITOR_FREQUENCY_SECS, true, true),
    (CLUSTER_AUTH_MODE, true, false),
    (CURSOR_TIMEOUT_MILLIS, true, true),
    (ENABLE_TEST_COMMANDS, false, true),
    (ingress::BURST_CAPACITY_SECS, true, true),
    (ingress::MAX_QUEUE_DEPTH, true, true),
    (ingress::BYPASS, true, true),
    (ingress::ENABLED, true, true),
    (ingress::RATE_PER_SEC, true, true),
    (MAX_OPERATION_MEMORY, true, true),
    (MAX_EXPRESSION_MEMORY, true, true),
    (HASH_INTERSECTION, true, true),
    (QUERY_STATS_RATE_LIMIT, true, true),
    (QUERY_STATS_SAMPLE_RATE, true, true),
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
        OCSP_STAPLING_REFRESH_PERIOD_SECS => Value::Int64(s.params.ocsp_stapling_refresh_secs),
        OCSP_VALIDATION_REFRESH_PERIOD_SECS => Value::Int64(s.params.ocsp_validation_refresh_secs),
        TLS_OCSP_STAPLING_TIMEOUT_SECS => {
            Value::Int32(clamp_i32(s.params.tls_ocsp_stapling_timeout_secs))
        }
        TLS_OCSP_VERIFY_TIMEOUT_SECS => {
            Value::Int32(clamp_i32(s.params.tls_ocsp_verify_timeout_secs))
        }
        REDACT_CLIENT_LOG_DATA => {
            Value::Boolean(s.hot.redact_client_log_data.load(std::sync::atomic::Ordering::Relaxed))
        }
        USE_INTERNAL_AUTHZ_FOR_X509 => Value::Boolean(s.params.use_internal_authz_for_x509),
        HASH_INTERSECTION => {
            Value::Boolean(s.hot.hash_intersection.load(std::sync::atomic::Ordering::Relaxed))
        }
        MAX_OPERATION_MEMORY => Value::Int64(s.params.operation_memory),
        MAX_EXPRESSION_MEMORY => {
            Value::Int64(s.hot.expression_memory.load(std::sync::atomic::Ordering::Relaxed))
        }
        QUERY_STATS_RATE_LIMIT => Value::Int32(clamp_i32(s.params.query_stats_rate_limit)),
        QUERY_STATS_SAMPLE_RATE => rate_value(s.params.query_stats_sample_rate()),
        TRANSACTION_LIFETIME_LIMIT_SECONDS => {
            Value::Int32(clamp_i32(s.params.transaction_lifetime_secs))
        }
        MAX_CONCURRENT_MULTI_DOCUMENT_TRANSACTIONS => {
            Value::Int32(clamp_i32(s.params.max_concurrent_transactions))
        }
        n if ingress::NAMES.contains(&n) => s.ingress.value(n),
        _ => Value::Int32(clamp_i32(
            i64::try_from(s.sessions.timeout_ms() / 60_000).unwrap_or(i64::MAX),
        )),
    }
}

/// A rate as `getParameter` gives it: an integral one as an `int` (measured).
fn rate_value(x: f64) -> Value {
    if x.fract() == 0.0 { Value::Int32(x as i32) } else { Value::Double(x) }
}

/// A new `internalQueryStatsSampleRate`: a number from 0 to 1 (measured on a `refrouter` 9.0.4).
fn sample_rate(v: &Value) -> Result<f64, CommandError> {
    let x = match v {
        Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_) => {
            crate::types::number(v)
        }
        _ => None,
    };
    let Some(x) = x.filter(|x| !x.is_nan()) else {
        let msg = format!(
            "Failed validating {QUERY_STATS_SAMPLE_RATE}: Unable to coerce value to correct type"
        );
        return Err(CommandError::new(BAD_VALUE, msg));
    };
    let bound = match x {
        x if x < 0.0 => Some("greater than or equal to 0"),
        x if x > 1.0 => Some("less than or equal to 1"),
        _ => None,
    };
    match bound {
        Some(b) => Err(CommandError::new(
            BAD_VALUE,
            format!("Invalid value for parameter {QUERY_STATS_SAMPLE_RATE}: {x} is not {b}"),
        )),
        None => Ok(x),
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
    // NaN and the infinities have their own message (measured on a `refnode` 9.0.4).
    if !x.is_finite() {
        return Err(failed("Unable to coerce NaN/Inf to integral type"));
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
/// any mode but `keyFile` needs TLS on the connections the node opens (not with `allowTLS`, as a
/// `refrouter` 9.0.4 answers); another step is refused.
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
    if to != ClusterAuthMode::KeyFile && !g.params.tls_outgoing {
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

/// A new value `setParameter` checked: a number (a boolean as 0 or 1, a rate as its bits, a mode
/// as its index), or one of the limiter's (006 FR-036).
#[derive(Clone, Debug, PartialEq, Eq)]
enum Change {
    Number(i64),
    Ingress(ingress::Setting),
}

/// A parameter given at startup with `--setParameter`, checked as `setParameter` checks it
/// (006 FR-037).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Startup {
    name: &'static str,
    change: Change,
}

/// The parameter `name` given at startup as `text`, with the messages of a `refnode` 9.0.4's
/// start (measured on 05-11-2026): one that does not exist, one that is not settable at
/// startup, and a value that does not read as the parameter's type or that `setParameter`
/// refuses, after «Bad value for parameter "<name>": ».
///
/// # Errors
///
/// The message the start fails with.
pub fn startup(name: &str, text: &str) -> Result<Startup, String> {
    let Some((known, _, at_start)) = KNOWN.iter().find(|(n, _, _)| *n == name) else {
        return Err(format!("Illegal --setParameter parameter: \"{name}\""));
    };
    if !at_start {
        return Err(format!("Cannot use --setParameter to set \"{name}\" at startup"));
    }
    let bad = |why: &str| format!("Bad value for parameter \"{name}\": {why}");
    let defaults = NodeSessions::new(super::Timeouts::default());
    let value = match value_of(known, &defaults) {
        Value::Boolean(_) => match text {
            "true" | "1" => Value::Boolean(true),
            "false" | "0" => Value::Boolean(false),
            _ => return Err(bad("Value is not a valid boolean")),
        },
        Value::Double(_) => Value::Double(text.parse::<f64>().map_err(|_| bad("Invalid number"))?),
        Value::Int32(_) | Value::Int64(_) => {
            Value::Int64(text.parse::<i64>().map_err(|_| bad("Did not consume whole string."))?)
        }
        _ => {
            let wrapped = format!("{{\"v\": {text}}}");
            let doc =
                teseladb_bson::extjson::parse(&wrapped).map_err(|e| bad(&format!("{e:?}")))?;
            doc.get("v").cloned().unwrap_or(Value::Null)
        }
    };
    let change = check(known, &value, None).map_err(|e| bad(&e.errmsg))?;
    Ok(Startup { name: known, change })
}

/// Gives the node's tables `startup`, before any client comes.
pub fn apply_startup(s: &mut NodeSessions, startup: &Startup) {
    apply(s, startup.name, startup.change.clone());
}

/// The change `v` asks of parameter `name`, checked as `setParameter` checks it; `shared` is the
/// node's at runtime and `None` at startup, when no audit configuration has been loaded yet.
fn check(
    name: &'static str,
    v: &Value,
    shared: Option<&super::Shared>,
) -> Result<Change, CommandError> {
    if ingress::NAMES.contains(&name) {
        return Ok(Change::Ingress(ingress::parse(name, v, integral)?));
    }
    let n = match name {
        REDACT_CLIENT_LOG_DATA | USE_INTERNAL_AUTHZ_FOR_X509 | HASH_INTERSECTION => {
            i64::from(crate::types::bson_true(v))
        }
        AUDIT_AUTHORIZATION_SUCCESS => match shared {
            Some(shared) => audit_authorization_success(shared, v)?,
            None => i64::from(crate::types::bson_true(v)),
        },
        CLUSTER_AUTH_MODE => match shared {
            Some(shared) => cluster_auth_mode_step(shared, v)?.index(),
            None => return Err(CommandError::new(ILLEGAL_OPERATION, "not at startup")),
        },
        QUERY_STATS_SAMPLE_RATE => i64::from_ne_bytes(sample_rate(v)?.to_bits().to_ne_bytes()),
        QUERY_STATS_RATE_LIMIT => {
            let n = integral(name, v)?;
            if i32::try_from(n).is_err() {
                let msg =
                    format!("Failed validating {name}: Out of bounds coercing to integral value");
                return Err(CommandError::new(BAD_VALUE, msg));
            }
            n
        }
        _ => integral(name, v)?,
    };
    let least = match name {
        TRANSACTION_LIFETIME_LIMIT_SECONDS => 1,
        QUERY_STATS_RATE_LIMIT => -1,
        MAX_CONCURRENT_MULTI_DOCUMENT_TRANSACTIONS => 0,
        _ => i64::MIN,
    };
    if (name == MAX_OPERATION_MEMORY || name == MAX_EXPRESSION_MEMORY) && n <= 0 {
        let msg = format!("Invalid value for parameter {name}: {n} is not greater than 0");
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    if n < least {
        let msg = format!(
            "Invalid value for parameter {name}: {n} is not greater than or equal to {least}"
        );
        return Err(CommandError::new(BAD_VALUE, msg));
    }
    Ok(Change::Number(n))
}

/// Sets parameter `name` of the node's tables `g` to `change`.
fn apply(g: &mut NodeSessions, name: &str, change: Change) {
    let n = match change {
        Change::Ingress(s) => {
            g.ingress.set(s);
            return;
        }
        Change::Number(n) => n,
    };
    match name {
        CURSOR_TIMEOUT_MILLIS => {
            g.params.cursor_timeout_ms = n;
            g.cursors.set_timeout_ms(u64::try_from(n).unwrap_or(0));
        }
        REDACT_CLIENT_LOG_DATA => {
            g.hot.redact_client_log_data.store(n != 0, std::sync::atomic::Ordering::Relaxed);
        }
        USE_INTERNAL_AUTHZ_FOR_X509 => g.params.use_internal_authz_for_x509 = n != 0,
        HASH_INTERSECTION => {
            g.hot.hash_intersection.store(n != 0, std::sync::atomic::Ordering::Relaxed)
        }
        MAX_OPERATION_MEMORY => g.params.operation_memory = n,
        MAX_EXPRESSION_MEMORY => {
            g.hot.expression_memory.store(n, std::sync::atomic::Ordering::Relaxed)
        }
        QUERY_STATS_RATE_LIMIT => g.params.query_stats_rate_limit = n,
        QUERY_STATS_SAMPLE_RATE => {
            g.params.query_stats_sample_bits = u64::from_ne_bytes(n.to_ne_bytes());
        }
        CLUSTER_AUTH_MODE => g.params.cluster_auth_mode = ClusterAuthMode::from_index(n),
        AUDIT_AUTHORIZATION_SUCCESS => match g.audit.as_ref() {
            Some(a) => {
                let mut w = a.auditor.write().unwrap_or_else(std::sync::PoisonError::into_inner);
                w.set_authorization_success(n != 0);
            }
            None => g.params.audit_authorization_success = n != 0,
        },
        TRANSACTION_LIFETIME_LIMIT_SECONDS => g.params.transaction_lifetime_secs = n,
        MAX_CONCURRENT_MULTI_DOCUMENT_TRANSACTIONS => g.params.max_concurrent_transactions = n,
        LOCAL_LOGICAL_SESSION_TIMEOUT_MINUTES => {
            let ms = u64::try_from(n).unwrap_or(0).saturating_mul(60_000);
            g.sessions = teseladb_session::session::SessionTable::with_timeout(ms);
        }
        OCSP_ENABLED => g.params.ocsp_enabled = n != 0,
        OCSP_STAPLING_REFRESH_PERIOD_SECS => g.params.ocsp_stapling_refresh_secs = n,
        OCSP_VALIDATION_REFRESH_PERIOD_SECS => g.params.ocsp_validation_refresh_secs = n,
        TLS_OCSP_STAPLING_TIMEOUT_SECS => g.params.tls_ocsp_stapling_timeout_secs = n,
        TLS_OCSP_VERIFY_TIMEOUT_SECS => g.params.tls_ocsp_verify_timeout_secs = n,
        CLIENT_CURSOR_MONITOR_FREQUENCY_SECS => g.params.monitor_ms = n.saturating_mul(1000),
        // The test commands come with the failpoint table, which the start builds (`--setParameter
        // enableTestCommands`); nothing else is left.
        _ => {}
    }
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
        changes.push((*name, check(name, v, Some(ctx.shared))?));
    }
    let Some((first, _)) = changes.first() else {
        let msg = "no option found to set, use help:true to see options ";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    };
    let mut g = lock(ctx.shared);
    let was = value_of(first, &g);
    for (name, change) in changes {
        apply(&mut g, name, change);
    }
    Ok(Document::new().with("was", was).with("ok", 1.0))
}
