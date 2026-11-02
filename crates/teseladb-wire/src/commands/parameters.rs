//! Cluster parameters (006 FR-020): `setClusterParameter` and `getClusterParameter`, kept in
//! Group 0 so every node reads the same value, with the forms and errors of a `refrouter` 9.0.4
//! (measured). Each specification adds its parameters to [`KNOWN`]; 006 has `defaultMaxTimeMS`,
//! 013 `auditConfig` (FR-031) and 007 TeselaDB's own `teselaHistoryRetention` (FR-019, DV-016)
//! and `teselaLoadSplitOpsPerSecond` (D-075), and 019 `httpOptions` and `mcpOptions` (DV-016). Each read and change goes to the audit log as on RefDb
//! Enterprise 9.0.4: `getClusterParameter` with the names asked for, `setClusterParameter` and
//! `updateCachedClusterServerParameter` with the value before and the one given.

use std::collections::BTreeMap;

use teseladb_bson::{Document, RawDoc, Value};

use teseladb_auth::audit::AuditType;

use super::admin::admin_only;
use super::audit as audit_log;
use super::http_parameters;
use super::{Backend, Ctx};
use crate::error::{
    BAD_VALUE, Code, CommandError, IDL_DUPLICATE_FIELD, IDL_UNKNOWN_FIELD, INVALID_OPTIONS,
    NO_SUCH_KEY,
};
use crate::ops::{DEFAULT_MAX_TIME_MS, READ_OPERATIONS};
use crate::types::{number, wrong_type, wrong_types};

/// `setClusterParameter`.
pub const SET_CLUSTER_PARAMETER: &str = "setClusterParameter";
/// `getClusterParameter`.
pub const GET_CLUSTER_PARAMETER: &str = "getClusterParameter";
/// Field with the time of the last change of a parameter.
pub const PARAMETER_TIME: &str = "clusterParameterTime";
/// `getClusterParameter: "*"` lists every parameter.
pub const ALL_PARAMETERS: &str = "*";

/// A cluster parameter: its name, its value when nobody set it, how a new value is read and
/// how its errors are written.
struct Known {
    name: &'static str,
    default: fn() -> Document,
    parse: fn(&Document) -> Result<Document, (Code, String)>,
    /// The name of its structure in RefDb's errors.
    idl: &'static str,
    /// Whether its errors go after `Failed parsing ClusterServerParameter '<name>' :: caused by ::`.
    prefixed: bool,
    /// Whether `getClusterParameter` shows its fields inside a field with its name.
    nested: bool,
}

/// The cluster parameters TeselaDB has, in name order.
const KNOWN: [Known; 7] = [
    Known {
        name: audit_log::AUDIT_CONFIG,
        default: audit_log::audit_config_default,
        parse: audit_log::audit_config_parse,
        idl: audit_log::AUDIT_CONFIG_IDL,
        prefixed: false,
        nested: true,
    },
    Known {
        name: DEFAULT_MAX_TIME_MS,
        default: default_max_time_default,
        parse: default_max_time_parse,
        idl: "ClusterServerParameter",
        prefixed: true,
        nested: false,
    },
    Known {
        name: http_parameters::HTTP_OPTIONS,
        default: http_parameters::http_default,
        parse: http_parameters::http_parse,
        idl: "ClusterServerParameter",
        prefixed: true,
        nested: false,
    },
    Known {
        name: http_parameters::MCP_OPTIONS,
        default: http_parameters::mcp_default,
        parse: http_parameters::mcp_parse,
        idl: "ClusterServerParameter",
        prefixed: true,
        nested: false,
    },
    Known {
        name: HISTORY_RETENTION,
        default: history_default,
        parse: history_parse,
        idl: "ClusterServerParameter",
        prefixed: true,
        nested: false,
    },
    Known {
        name: super::presentation::TOPOLOGY_PRESENTATION,
        default: super::presentation::default_value,
        parse: super::presentation::parse,
        idl: "ClusterServerParameter",
        prefixed: true,
        nested: false,
    },
    Known {
        name: LOAD_SPLIT_OPS,
        default: load_split_default,
        parse: load_split_parse,
        idl: "ClusterServerParameter",
        prefixed: true,
        nested: false,
    },
];

/// `teselaHistoryRetention` (007 FR-019, DV-016): `{seconds}` of history every range keeps for
/// reads at an earlier mark; 24 hours without a value.
pub const HISTORY_RETENTION: &str = "teselaHistoryRetention";
/// `teselaLoadSplitOpsPerSecond` (007 FR-006, D-075): `{opsPerSecond}` a range has to serve for
/// 10 seconds in a row to split by load; 2,500 without a value.
pub const LOAD_SPLIT_OPS: &str = "teselaLoadSplitOpsPerSecond";
const SECONDS: &str = "seconds";
const OPS_PER_SECOND: &str = "opsPerSecond";
/// History kept without a value, in seconds.
pub const DEFAULT_HISTORY_SECONDS: i64 = 24 * 60 * 60;
/// Load split threshold without a value.
pub const DEFAULT_LOAD_SPLIT_OPS: i64 = 2_500;

fn history_default() -> Document {
    Document::new().with(SECONDS, DEFAULT_HISTORY_SECONDS)
}

fn load_split_default() -> Document {
    Document::new().with(OPS_PER_SECOND, DEFAULT_LOAD_SPLIT_OPS)
}

/// A document with one field `field`, a whole number of at least 1 (a double is truncated).
fn positive_field(v: &Document, field: &str) -> Result<Document, (Code, String)> {
    let mut n = None;
    for (k, x) in v.iter() {
        if k != field {
            let why = format!("BSON field 'ClusterServerParameter.{k}' is an unknown field.");
            return Err((IDL_UNKNOWN_FIELD, why));
        }
        let Some(x) = number(x) else {
            let path = format!("ClusterServerParameter.{field}");
            let e = wrong_types(&path, x, &["long", "int", "double", "decimal"]);
            return Err((e.code, e.errmsg));
        };
        if x.trunc() < 1.0 {
            let why =
                format!("BSON field '{field}' value must be >= 1, actual value '{}'", x.trunc());
            return Err((BAD_VALUE, why));
        }
        n = Some(x.trunc() as i64);
    }
    let n = n.ok_or_else(|| {
        (
            crate::error::IDL_FAILED_TO_PARSE,
            format!("BSON field 'ClusterServerParameter.{field}' is missing but a required field"),
        )
    })?;
    Ok(Document::new().with(field, n))
}

fn history_parse(v: &Document) -> Result<Document, (Code, String)> {
    positive_field(v, SECONDS)
}

fn load_split_parse(v: &Document) -> Result<Document, (Code, String)> {
    positive_field(v, OPS_PER_SECOND)
}

/// The whole number `field` of a stored value, if it has one.
fn stored_number(stored: Option<&[u8]>, field: &str) -> Option<u64> {
    let doc = RawDoc::from_bytes(stored?).ok()?;
    match Document::from(doc).get(field)? {
        Value::Int64(n) => u64::try_from(*n).ok(),
        Value::Int32(n) => u64::try_from(*n).ok(),
        _ => None,
    }
}

/// The history every range keeps, in milliseconds, from the stored value of
/// `teselaHistoryRetention` (`None`: not set).
pub fn history_retention_ms(stored: Option<&[u8]>) -> u64 {
    let secs = stored_number(stored, SECONDS).unwrap_or(DEFAULT_HISTORY_SECONDS as u64);
    secs.saturating_mul(1_000)
}

/// The load split threshold from the stored value of `teselaLoadSplitOpsPerSecond`.
pub fn load_split_ops(stored: Option<&[u8]>) -> u64 {
    stored_number(stored, OPS_PER_SECOND).unwrap_or(DEFAULT_LOAD_SPLIT_OPS as u64)
}

fn default_max_time_default() -> Document {
    Document::new().with(READ_OPERATIONS, 0i64)
}

/// Reads `defaultMaxTimeMS`: `readOperations`, a whole number of milliseconds (a double is
/// truncated), 0 if missing.
fn default_max_time_parse(v: &Document) -> Result<Document, (Code, String)> {
    let mut ms = 0i64;
    for (k, x) in v.iter() {
        if k != READ_OPERATIONS {
            let why = format!("BSON field 'ClusterServerParameter.{k}' is an unknown field.");
            return Err((IDL_UNKNOWN_FIELD, why));
        }
        let Some(n) = number(x) else {
            let e = wrong_types(
                &format!("ClusterServerParameter.{READ_OPERATIONS}"),
                x,
                &["long", "int", "double", "decimal"],
            );
            return Err((e.code, e.errmsg));
        };
        if n < 0.0 {
            let why = format!(
                "BSON field '{READ_OPERATIONS}' value must be >= 0, actual value '{}'",
                n.trunc()
            );
            return Err((BAD_VALUE, why));
        }
        ms = n.trunc() as i64;
    }
    Ok(Document::new().with(READ_OPERATIONS, ms))
}

/// `a` followed by the fields of `b`.
fn joined(a: Document, b: Document) -> Document {
    b.iter().fold(a, |d, (k, v)| d.with(k, v.clone()))
}

fn known(name: &str) -> Result<&'static Known, CommandError> {
    KNOWN
        .iter()
        .find(|k| k.name == name)
        .ok_or_else(|| CommandError::new(NO_SUCH_KEY, format!("Unknown server parameter: {name}")))
}

/// The parameter as stored: its time and its fields, or the time 0 and its default.
fn stored_value(k: &Known, stored: &BTreeMap<String, Vec<u8>>) -> Document {
    stored.get(k.name).and_then(|b| RawDoc::from_bytes(b).ok().map(Document::from)).unwrap_or_else(
        || {
            let unset =
                Document::new().with(PARAMETER_TIME, Value::Timestamp { time: 0, increment: 0 });
            joined(unset, (k.default)())
        },
    )
}

/// The parameter's value as `getClusterParameter` shows it: `_id`, its time and its fields, or
/// for `auditConfig` its fields and then its time inside a field with its name.
fn shown(k: &Known, stored: &BTreeMap<String, Vec<u8>>) -> Document {
    let value = stored_value(k, stored);
    let id = Document::new().with("_id", k.name);
    if !k.nested {
        return joined(id, value);
    }
    let time = value.get(PARAMETER_TIME).cloned().unwrap_or(Value::Null);
    let mut fields = value;
    fields.remove(PARAMETER_TIME);
    id.with(k.name, fields.with(PARAMETER_TIME, time))
}

/// The stored value of `name` as `getClusterParameter` would show it (013: `auditConfig` on the
/// nodes that apply it later).
pub fn shown_value(name: &str, stored: &BTreeMap<String, Vec<u8>>) -> Option<Document> {
    KNOWN.iter().find(|k| k.name == name).map(|k| shown(k, stored))
}

/// `setClusterParameter`: one parameter, stored with the cluster time of the change.
pub fn set_cluster_parameter<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin_only(SET_CLUSTER_PARAMETER, ctx.db)?;
    let cmd = ctx.cmd;
    let arg = match cmd.iter().next().map_or(&Value::Null, |(_, v)| v) {
        Value::Document(d) => d,
        other => {
            let path = format!("{SET_CLUSTER_PARAMETER}.{SET_CLUSTER_PARAMETER}");
            return Err(wrong_type(&path, other, "object"));
        }
    };
    let mut fields = arg.iter();
    let Some((name, value)) = fields.next() else {
        return Err(CommandError::new(NO_SUCH_KEY, "No cluster parameter provided"));
    };
    if fields.next().is_some() {
        let msg = "setClusterParameter only supports setting exactly one parameter";
        return Err(CommandError::new(INVALID_OPTIONS, msg));
    }
    let k = known(name)?;
    if k.name == audit_log::AUDIT_CONFIG {
        audit_log::runtime_enabled(ctx.shared)?;
    }
    let Value::Document(v) = value else {
        return Err(CommandError::new(BAD_VALUE, "Cluster parameter value must be an object"));
    };
    if v.get("_id").is_some() {
        return Err(CommandError::new(BAD_VALUE, "Can't have multiple _id fields in one document"));
    }
    let failed = |code, why: String| match k.prefixed {
        true => CommandError::new(
            code,
            format!("Failed parsing ClusterServerParameter '{name}' :: caused by :: {why}"),
        ),
        false => CommandError::new(code, why),
    };
    if v.get(PARAMETER_TIME).is_some() {
        let why = format!("BSON field '{}.{PARAMETER_TIME}' is a duplicate field", k.idl);
        return Err(failed(IDL_DUPLICATE_FIELD, why));
    }
    let mut parsed = (k.parse)(v).map_err(|(code, why)| failed(code, why))?;
    if k.name == super::presentation::TOPOLOGY_PRESENTATION {
        let before = super::presentation::stored(ctx.backend)?;
        let hosts = super::presentation::hosts(ctx.backend)?;
        parsed = super::presentation::next(&before, &parsed, &hosts)?.to_document();
    }
    let secs = u32::try_from(ctx.backend.now_ms() / 1000).unwrap_or(u32::MAX);
    let time_value = Value::Timestamp { time: secs, increment: 1 };
    let stored = joined(Document::new().with(PARAMETER_TIME, time_value.clone()), parsed);
    let all = ctx.backend.cluster_parameters()?;
    let before = shown(k, &all);
    let previous = stored_value(k, &all);
    ctx.backend.set_cluster_parameter(k.name, stored.to_bytes()?)?;
    // The value given, as it came, with its name and its new time (measured).
    let id = Document::new().with("_id", k.name).with(PARAMETER_TIME, time_value);
    let p = Document::new()
        .with("originalClusterServerParameter", before)
        .with("updatedClusterServerParameter", joined(id, v.clone()));
    ctx.backend.audit_event(AuditType::SetClusterParameter, p.clone());
    ctx.backend.audit_event(AuditType::UpdateCachedClusterServerParameter, p);
    if k.name == audit_log::AUDIT_CONFIG {
        let configure = audit_log::apply_audit_config(ctx.shared, &previous, &stored)?;
        ctx.backend.audit_event(AuditType::AuditConfigure, configure);
    }
    Ok(Document::new().with("ok", 1.0))
}

/// `getClusterParameter`: a name, a list of names, or `"*"` for all.
pub fn get_cluster_parameter<B: Backend + ?Sized>(
    ctx: &mut Ctx<'_, B>,
) -> Result<Document, CommandError> {
    admin_only(GET_CLUSTER_PARAMETER, ctx.db)?;
    // Recorded as asked, before the names are checked (measured).
    let asked = Document::new().with("requestedClusterServerParameters", ctx.first().clone());
    ctx.backend.audit_event(AuditType::GetClusterParameter, asked);
    let path = format!("{GET_CLUSTER_PARAMETER}.{GET_CLUSTER_PARAMETER}");
    let names: Vec<&str> = match ctx.first() {
        Value::String(s) if s == ALL_PARAMETERS => KNOWN.iter().map(|k| k.name).collect(),
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => {
            if a.is_empty() {
                let msg =
                    "Must supply at least one cluster server parameter name to getClusterParameter";
                return Err(CommandError::new(BAD_VALUE, msg));
            }
            let mut out = Vec::new();
            for (i, v) in a.iter().enumerate() {
                match v {
                    Value::String(s) => out.push(s.as_str()),
                    other => return Err(wrong_type(&format!("{path}.{i}"), other, "string")),
                }
            }
            out
        }
        other => return Err(wrong_types(&path, other, &["string"])),
    };
    let ks = names.iter().map(|n| known(n)).collect::<Result<Vec<_>, _>>()?;
    let stored = ctx.backend.cluster_parameters()?;
    let list: Vec<Value> = ks.iter().map(|k| Value::Document(shown(k, &stored))).collect();
    Ok(Document::new().with("clusterParameters", Value::Array(list)).with("ok", 1.0))
}
