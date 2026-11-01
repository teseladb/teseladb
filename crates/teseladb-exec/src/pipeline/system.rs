//! The specifications of the stages a node answers itself (measured on a `refrouter` 9.0.4); the
//! pipeline checks them and its [`super::Env`] gives their documents.

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_mql::MqlError;

use super::errors::{
    BAD_VALUE, COLL_STATS_OBJECT, FAILED_TO_PARSE, INVALID_OPTIONS, TYPE_MISMATCH,
    UNSUPPORTED_FORMAT, error, missing_field, unknown_field, wrong_type,
};
use crate::expr::arith::raw_type_name;
use crate::expr::values::{display, display_bson};

/// RefDb's code for options in a section that takes none.
const NOT_EMPTY: teseladb_mql::ErrorCode =
    teseladb_mql::ErrorCode { code: 31170, name: "Location31170" };

/// The options of `storageStats` that are booleans.
const STORAGE_FLAGS: &[&str] = &["verbose", "waitForLock", "numericOnly"];

/// The sections `$collStats` may ask for, each an object.
const COLL_STATS_SECTIONS: [&str; 4] = ["latencyStats", "storageStats", "count", "queryExecStats"];

/// The stage that lists a collection's search indexes (011 FR-008).
pub const LIST_SEARCH_INDEXES: &str = "$listSearchIndexes";

/// Checks the specification of `$listSearchIndexes`: an object with at most one of `id` and
/// `name`, both strings (measured on a 9.0.4 replica set with `refsearch` 1.70.6). Whether `id` is
/// an `ObjectId` is checked when the stage runs, as RefDb does.
pub fn list_search_indexes(v: RawValue<'_>) -> Result<Document, MqlError> {
    let RawValue::Document(d) = v else {
        return Err(error(
            FAILED_TO_PARSE,
            format!(
                "The {LIST_SEARCH_INDEXES} stage specification must be an object. Found: {}",
                raw_type_name(v)
            ),
        ));
    };
    for (k, x) in d.iter() {
        if k != "id" && k != "name" {
            return Err(unknown_field(LIST_SEARCH_INDEXES, k));
        }
        if !matches!(x, RawValue::String(_)) {
            return Err(wrong_type(&format!("{LIST_SEARCH_INDEXES}.{k}"), x, "type 'string'"));
        }
    }
    let spec = Document::from(d);
    if spec.get("id").is_some() && spec.get("name").is_some() {
        return Err(error(
            INVALID_OPTIONS,
            format!("Cannot set both 'name' and 'id' for {LIST_SEARCH_INDEXES}."),
        ));
    }
    Ok(spec)
}

/// Checks the specification of `$collStats`: an object whose fields are its sections, each an
/// object (`storageStats.scale` at least 1).
pub fn coll_stats(v: RawValue<'_>) -> Result<Document, MqlError> {
    let RawValue::Document(d) = v else {
        return Err(error(
            COLL_STATS_OBJECT,
            format!(
                "$collStats must take a nested object but found: $collStats: {}",
                display(&Value::from(v))
            ),
        ));
    };
    for (k, x) in d.iter() {
        if !COLL_STATS_SECTIONS.contains(&k) {
            return Err(unknown_field("$collStats", k));
        }
        let RawValue::Document(section) = x else {
            return Err(wrong_type(&format!("$collStats.{k}"), x, "type 'object'"));
        };
        match k {
            "latencyStats" => booleans(section, &["histograms"])?,
            "storageStats" => storage_stats(section)?,
            _ if section.iter().next().is_some() => {
                return Err(error(
                    NOT_EMPTY,
                    format!(
                        "expected an empty object, but got {}",
                        display_bson(&Value::Document(Document::from(section)))
                    ),
                ));
            }
            _ => {}
        }
    }
    Ok(Document::from(d))
}

/// Checks that the options `names` of a section, when present, are booleans.
fn booleans(section: RawDoc<'_>, names: &[&str]) -> Result<(), MqlError> {
    section.iter().try_for_each(|(k, x)| boolean(k, x, names))
}

/// Checks that option `k`, when one of `names`, holds a boolean.
fn boolean(k: &str, x: RawValue<'_>, names: &[&str]) -> Result<(), MqlError> {
    if names.contains(&k) && !matches!(x, RawValue::Boolean(_)) {
        return Err(error(
            TYPE_MISMATCH,
            format!("Field '{k}' should be a boolean value, but found: {}", raw_type_name(x)),
        ));
    }
    Ok(())
}

/// The `storageStats.scale` a number asks for: a double truncated, a decimal rounded half to
/// even (measured on a `refrouter` 9.0.4); `None` for anything but a number.
pub fn scale_of(v: &Value) -> Option<i64> {
    match v {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) => Some(*n),
        Value::Double(f) => Some(*f as i64),
        Value::Decimal128(d) => Some(teseladb_mql::decimal_to_f64(*d).round_ties_even() as i64),
        _ => None,
    }
}

/// Checks `storageStats`: `scale` a number of at least 1 (a double truncated, a decimal
/// rounded), its other options booleans, in the order they come.
fn storage_stats(section: RawDoc<'_>) -> Result<(), MqlError> {
    for (k, x) in section.iter() {
        if k != "scale" {
            boolean(k, x, STORAGE_FLAGS)?;
            continue;
        }
        let Some(n) = scale_of(&Value::from(x)) else {
            return Err(wrong_type(
                "$collStats.storageStats.scale",
                x,
                "types '[int, decimal, double, long]'",
            ));
        };
        if n < 1 {
            return Err(error(
                BAD_VALUE,
                format!("BSON field 'scale' value must be >= 1, actual value '{n}'"),
            ));
        }
    }
    Ok(())
}

/// The options of `$currentOp`, each a boolean.
const CURRENT_OP_FLAGS: [&str; 8] = [
    "allUsers",
    "idleConnections",
    "idleCursors",
    "idleSessions",
    "localOps",
    "backtrace",
    "targetAllNodes",
    "truncateOps",
];

/// Checks the specification of `$currentOp`: an object of boolean options (measured on a
/// `refrouter` 9.0.4).
pub fn current_op(v: RawValue<'_>) -> Result<Document, MqlError> {
    let RawValue::Document(d) = v else {
        return Err(error(
            TYPE_MISMATCH,
            format!(
                "$currentOp options must be specified in an object, but found: {}",
                raw_type_name(v)
            ),
        ));
    };
    for (k, x) in d.iter() {
        if !CURRENT_OP_FLAGS.contains(&k) {
            return Err(unknown_field("$currentOp", k));
        }
        if !matches!(x, RawValue::Boolean(_)) {
            return Err(error(
                TYPE_MISMATCH,
                format!(
                    "The '{k}' parameter of the $currentOp stage must be a boolean value, but found: {}",
                    raw_type_name(x)
                ),
            ));
        }
    }
    Ok(Document::from(d))
}

/// The options of `$listClusterCatalog`, each a flag.
const CLUSTER_CATALOG_FLAGS: [&str; 3] = ["shards", "balancingConfiguration", "tracked"];

/// RefDb's code for a `$listClusterCatalog` that is no object.
const CLUSTER_CATALOG_OBJECT: teseladb_mql::ErrorCode =
    teseladb_mql::ErrorCode { code: 9_621_302, name: "Location9621302" };

/// Checks the specification of `$listClusterCatalog`: an object of its flags (measured on a
/// `refrouter` 9.0.4).
pub fn cluster_catalog(v: RawValue<'_>) -> Result<Document, MqlError> {
    let RawValue::Document(d) = v else {
        return Err(error(
            CLUSTER_CATALOG_OBJECT,
            format!(
                "$listClusterCatalog must take an object but found: $listClusterCatalog: {}",
                display(&Value::from(v))
            ),
        ));
    };
    if let Some((k, _)) = d.iter().find(|(k, _)| !CLUSTER_CATALOG_FLAGS.contains(k)) {
        return Err(unknown_field("$listClusterCatalog", k));
    }
    Ok(Document::from(d))
}

/// Checks the specification of `$planCacheStats`: an object whose only field may be
/// `allHosts` (measured on a `refrouter` 9.0.4).
pub fn plan_cache_stats(v: RawValue<'_>) -> Result<Document, MqlError> {
    let RawValue::Document(d) = v else {
        return Err(error(
            FAILED_TO_PARSE,
            format!("$planCacheStats value must be an object. Found: {}", raw_type_name(v)),
        ));
    };
    if let Some((k, _)) = d.iter().find(|(k, _)| *k != "allHosts") {
        return Err(error(
            FAILED_TO_PARSE,
            format!(
                "$planCacheStats parameters object may contain only 'allHosts' field. Found: {k}"
            ),
        ));
    }
    Ok(Document::from(d))
}

/// Checks the specification of `$listLocalSessions` or `$listSessions` (`name`): an object with
/// `allUsers` (a boolean) or `users` (an array of `{user, db}`), not both (measured on a `refrouter`
/// 9.0.4).
pub fn sessions(name: &str, v: RawValue<'_>) -> Result<Document, MqlError> {
    let RawValue::Document(d) = v else {
        return Err(error(
            TYPE_MISMATCH,
            format!(
                "{name} options must be specified in an object, but found: {}",
                raw_type_name(v)
            ),
        ));
    };
    let mut all = false;
    let mut users = 0;
    for (k, x) in d.iter() {
        match (k, x) {
            ("allUsers", RawValue::Boolean(b)) => all = b,
            ("allUsers", other) => {
                return Err(wrong_type(&format!("{name}.allUsers"), other, "type 'bool'"));
            }
            ("users", RawValue::Array(list)) => {
                for (i, u) in list.iter().enumerate() {
                    session_user(name, i, u)?;
                    users += 1;
                }
            }
            ("users", other) => {
                return Err(wrong_type(&format!("{name}.users"), other, "type 'array'"));
            }
            (other, _) => return Err(unknown_field(name, other)),
        }
    }
    if all && users > 0 {
        return Err(error(
            UNSUPPORTED_FORMAT,
            format!(
                "{name} may not specify {{allUsers:true}} and {{users:[...]}} at the same time"
            ),
        ));
    }
    Ok(Document::from(d))
}

/// Checks user `i` of the `users` of a sessions stage: `{user, db}`, both strings.
fn session_user(name: &str, i: usize, u: RawValue<'_>) -> Result<(), MqlError> {
    let RawValue::Document(user) = u else {
        return Err(wrong_type(&format!("{name}.users.{i}"), u, "type 'object'"));
    };
    let path = format!("{name}.users");
    for (k, x) in user.iter() {
        match (k, x) {
            ("user" | "db", RawValue::String(_)) => {}
            ("user" | "db", other) => {
                return Err(wrong_type(&format!("{path}.{k}"), other, "type 'string'"));
            }
            (other, _) => return Err(unknown_field(&path, other)),
        }
    }
    for k in ["user", "db"] {
        if user.get(k).is_none() {
            return Err(missing_field(&path, k));
        }
    }
    Ok(())
}
