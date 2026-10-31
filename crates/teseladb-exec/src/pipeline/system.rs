//! The specifications of the stages a node answers itself (measured on a `refrouter` 9.0.4); the
//! pipeline checks them and its [`super::Env`] gives their documents.

use teseladb_bson::{Document, RawValue, Value};
use teseladb_mql::MqlError;

use super::errors::{BAD_VALUE, COLL_STATS_OBJECT, TYPE_MISMATCH, UNKNOWN_FIELD, error};
use crate::expr::arith::raw_type_name;
use crate::expr::values::{display, int64_of};

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
            return Err(error(
                UNKNOWN_FIELD,
                format!("BSON field '$collStats.{k}' is an unknown field."),
            ));
        }
        let RawValue::Document(section) = x else {
            return Err(error(
                TYPE_MISMATCH,
                format!(
                    "BSON field '$collStats.{k}' is the wrong type '{}', expected type 'object'",
                    raw_type_name(x)
                ),
            ));
        };
        if k == "storageStats"
            && let Some(scale) = section.get("scale")
        {
            let n = int64_of(&Value::from(scale)).unwrap_or(0);
            if n < 1 {
                return Err(error(
                    BAD_VALUE,
                    format!("BSON field 'scale' value must be >= 1, actual value '{n}'"),
                ));
            }
        }
    }
    Ok(Document::from(d))
}
