//! The specifications of the stages a node answers itself (measured on a `refrouter` 9.0.4); the
//! pipeline checks them and its [`super::Env`] gives their documents.

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_mql::MqlError;

use super::errors::{
    BAD_VALUE, COLL_STATS_OBJECT, TYPE_MISMATCH, error, unknown_field, wrong_type,
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
