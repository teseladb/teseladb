//! The fields of RefDb's own protocol between a router and its shards that a client may send
//! a `refrouter` (006 FR-021): a `refrouter` 9.0.4 checks their types, refuses those meant for a
//! shard, and asks a privilege no user has for the others. TeselaDB has no such protocol and
//! uses none of them; it refuses them as that `refrouter` does (measured on 05-11-2026, scenario
//! `api-strict`).

use teseladb_bson::{Document, Value};

use crate::connection::Request;
use crate::error::{
    BAD_VALUE, CONTRADICTORY_MERGE, CommandError, FAILED_TO_PARSE, TIMESERIES_NS_ON_ROUTER,
    TYPE_MISMATCH,
};
use crate::types::{wrong_type, wrong_types};

/// The type a field must have.
#[derive(Clone, Copy)]
enum Want {
    /// An object, named `object` in the message.
    Object,
    /// A binary value.
    BinData,
    /// A string.
    Text,
    /// A number of these types, in the message's order.
    Numbers(&'static [&'static str]),
    /// An object, refused with `Expected field <name> to be of type object`.
    ObjectExpected,
}

impl Want {
    fn matches(self, v: &Value) -> bool {
        match self {
            Want::Object | Want::ObjectExpected => matches!(v, Value::Document(_)),
            Want::BinData => matches!(v, Value::Binary { .. }),
            Want::Text => matches!(v, Value::String(_)),
            Want::Numbers(_) => matches!(
                v,
                Value::Int32(_) | Value::Int64(_) | Value::Double(_) | Value::Decimal128(_)
            ),
        }
    }

    fn refusal(self, name: &str, field: &str, v: &Value) -> CommandError {
        let path = format!("{name}.{field}");
        match self {
            Want::Object => wrong_type(&path, v, "object"),
            Want::BinData => wrong_type(&path, v, "binData"),
            Want::Text => wrong_type(&path, v, "string"),
            Want::Numbers(types) => wrong_types(&path, v, types),
            Want::ObjectExpected => CommandError::new(
                TYPE_MISMATCH,
                format!("Expected field {field} to be of type object"),
            )
            .parsed(),
        }
    }
}

const PLAN_HASH_AGGREGATE: [&str; 4] = ["double", "long", "decimal", "int"];
const PLAN_HASH_FIND: [&str; 4] = ["long", "decimal", "double", "int"];
const ORIGINAL: [(&str, Want); 2] =
    [("$_originalQuery", Want::Object), ("$_originalCollation", Want::Object)];

/// The typed internal fields of each command.
fn typed(name: &str) -> Vec<(&'static str, Want)> {
    let mut out: Vec<(&'static str, Want)> = match name {
        "aggregate" => vec![
            ("exchange", Want::Object),
            ("runtimeConstants", Want::Object),
            ("querySettings", Want::Object),
            ("sampleId", Want::BinData),
            ("forcedPlanSolutionHash", Want::Numbers(&PLAN_HASH_AGGREGATE)),
        ],
        "find" => vec![
            ("querySettings", Want::Object),
            ("runtimeConstants", Want::Object),
            ("sampleId", Want::BinData),
            ("forcedPlanSolutionHash", Want::Numbers(&PLAN_HASH_FIND)),
            ("originalQueryShapeHash", Want::Text),
            ("$_resumeAfter", Want::ObjectExpected),
            ("$_startAt", Want::ObjectExpected),
        ],
        "count" => vec![("sampleId", Want::BinData), ("originalQueryShapeHash", Want::Text)],
        "update" | "delete" => vec![("runtimeConstants", Want::Object)],
        "findAndModify" => vec![("runtimeConstants", Want::Object), ("sampleId", Want::BinData)],
        _ => Vec::new(),
    };
    if matches!(name, "insert" | "update" | "delete" | "findAndModify") {
        out.extend(ORIGINAL);
    }
    out
}

/// Refuses the internal fields of command `req` (named `name`) as a `refrouter` 9.0.4 does.
pub fn check(req: &Request, name: &str) -> Result<(), CommandError> {
    let cmd = &req.command;
    let wanted = typed(name);
    for (k, v) in cmd.iter() {
        if let Some((_, want)) = wanted.iter().find(|(f, _)| *f == k)
            && !want.matches(v)
        {
            return Err(want.refusal(name, k, v));
        }
    }
    match name {
        "aggregate" => check_merge(cmd),
        "insert" | "update" | "delete" if cmd.get("isTimeseriesNamespace").is_some() => {
            let msg = "the 'isTimeseriesNamespace' parameter cannot be used on refrouter";
            Err(CommandError::new(TIMESERIES_NS_ON_ROUTER, msg))
        }
        _ => Ok(()),
    }
}

/// Refuses the internal fields of command `req` (named `name`) that ask a privilege no user has:
/// a `refrouter` 9.0.4 checks it after the API parameters, so `apiStrict` refuses an unstable one
/// first (RefDb r9.0.4 `coll_mod.idl`, measured on 09-11-2026).
pub fn check_privileged(req: &Request, name: &str) -> Result<(), CommandError> {
    match name {
        "collMod" if req.command.get(REMOVE_LEGACY_BUCKETING).is_some() => {
            Err(crate::commands::authorize::unauthorized(req))
        }
        _ => Ok(()),
    }
}

/// The internal field of `collMod` that only the server itself sends.
const REMOVE_LEGACY_BUCKETING: &str = "_removeLegacyTimeseriesBucketingParametersHaveChanged";

/// The fields of an `aggregate` a router sends its shards.
fn check_merge(cmd: &Document) -> Result<(), CommandError> {
    if cmd.get("fromRefRouter").is_some() || cmd.get("fromRouter").is_some() {
        return Err(CommandError::new(BAD_VALUE, "BSON field 'fromRouter' is an unknown field"));
    }
    let set = |k: &str| cmd.get(k).and_then(crate::types::truthy) == Some(true);
    if set("needsMerge") {
        let msg = "Cannot specify 'needsMerge' without 'fromRouter'";
        return Err(CommandError::new(FAILED_TO_PARSE, msg));
    }
    if set("needsSortedMerge") {
        let msg = "Encountered malformed AggregationCommandRequest: needsMerge = false and \
                   needsSortedMerge = true are contradictory";
        return Err(CommandError::new(CONTRADICTORY_MERGE, msg));
    }
    Ok(())
}
