//! Reading a command's fields with the reference server's errors: a missing required field is
//! `IDLFailedToParse` (40414), a field of the wrong type `TypeMismatch` (14), and the common
//! options (`writeConcern`, `readConcern`, `collation`, `lsid`, `txnNumber`) have their own
//! readers.

use teseladb_bson::{Document, Value};
use teseladb_range::read::ReadConcern;

use crate::concerns::{self, ReadConcernArgs, WriteConcernArgs};
use crate::error::{CommandError, IDL_FAILED_TO_PARSE};
use crate::types::{NUMBER_TYPES, truthy, wrong_type, wrong_types};

/// `IDLFailedToParse` for a missing required field.
pub fn missing(cmd: &str, field: &str) -> CommandError {
    CommandError::new(
        IDL_FAILED_TO_PARSE,
        format!("BSON field '{cmd}.{field}' is missing but a required field"),
    )
}

/// A string field.
pub fn string<'a>(
    cmd: &'a Document,
    name: &str,
    field: &str,
) -> Result<Option<&'a str>, CommandError> {
    match cmd.get(field) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(v) => Err(wrong_type(&format!("{name}.{field}"), v, "string")),
    }
}

/// A document field.
pub fn doc<'a>(
    cmd: &'a Document,
    name: &str,
    field: &str,
) -> Result<Option<&'a Document>, CommandError> {
    match cmd.get(field) {
        None => Ok(None),
        Some(Value::Document(d)) => Ok(Some(d)),
        Some(v) => Err(wrong_type(&format!("{name}.{field}"), v, "object")),
    }
}

/// An array field.
pub fn array<'a>(
    cmd: &'a Document,
    name: &str,
    field: &str,
) -> Result<Option<&'a [Value]>, CommandError> {
    match cmd.get(field) {
        None => Ok(None),
        Some(Value::Array(a)) => Ok(Some(a)),
        Some(v) => Err(wrong_type(&format!("{name}.{field}"), v, "array")),
    }
}

/// A boolean of the query commands (`find`'s `singleBatch`): only `true` or `false`, `null`
/// included in what it refuses (measured in the reference router 9.0.4).
pub fn boolean(cmd: &Document, _name: &str, field: &str) -> Result<Option<bool>, CommandError> {
    match cmd.get(field) {
        None => Ok(None),
        Some(Value::Boolean(b)) => Ok(Some(*b)),
        Some(v) => {
            let msg = format!(
                "Field '{field}' should be a boolean value, but found: {}",
                crate::types::type_name(v)
            );
            Err(CommandError::new(crate::error::TYPE_MISMATCH, msg))
        }
    }
}

/// A field of IDL type `bool`: `true` or `false`, `null` as absent.
pub fn bool_field(cmd: &Document, name: &str, field: &str) -> Result<Option<bool>, CommandError> {
    match cmd.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Boolean(b)) => Ok(Some(*b)),
        Some(v) => Err(wrong_type(&format!("{name}.{field}"), v, "bool")),
    }
}

/// A field of IDL type `safeBool`: a boolean or a number (non-zero is true), `null` as absent;
/// `types` is the list the reference server writes for that command, whose order changes between
/// commands.
pub fn safe_bool(
    cmd: &Document,
    name: &str,
    field: &str,
    types: &[&str],
) -> Result<Option<bool>, CommandError> {
    match cmd.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => {
            truthy(v).map(Some).ok_or_else(|| wrong_types(&format!("{name}.{field}"), v, types))
        }
    }
}

/// A whole number field of any numeric type ("safe int64"): a double is cut toward zero, a
/// decimal rounded half to even, both saturate and NaN is 0, as the reference server reads them.
pub fn int(cmd: &Document, name: &str, field: &str) -> Result<Option<i64>, CommandError> {
    int_of(cmd, name, field, &NUMBER_TYPES)
}

/// [`int`] for a field whose message lists the numeric types in the order `types`.
pub fn int_of(
    cmd: &Document,
    name: &str,
    field: &str,
    types: &[&str],
) -> Result<Option<i64>, CommandError> {
    match cmd.get(field) {
        None => Ok(None),
        Some(Value::Int32(n)) => Ok(Some(i64::from(*n))),
        Some(Value::Int64(n)) => Ok(Some(*n)),
        // `as` cuts toward zero, saturates and reads NaN as 0, as the reference server does.
        Some(Value::Double(x)) => Ok(Some(*x as i64)),
        Some(Value::Decimal128(d)) => Ok(Some(teseladb_mql::decimal_saturating(*d))),
        Some(v) => Err(wrong_types(&format!("{name}.{field}"), v, types)),
    }
}

/// A field that must be a `long` (cursor identifiers).
pub fn long(v: &Value, path: &str) -> Result<i64, CommandError> {
    match v {
        Value::Int64(n) => Ok(*n),
        other => Err(wrong_type(path, other, "long")),
    }
}

/// The command's `writeConcern`, or the reference server's default (`w: "majority"`) without one.
pub fn write_concern(cmd: &Document, name: &str) -> Result<WriteConcernArgs, CommandError> {
    Ok(WriteConcernArgs::parse(cmd, name)?.unwrap_or_else(concerns::default_write_concern))
}

/// The range's read for the command's `readConcern` (without one, the cluster's default level,
/// `local` unless `setDefaultRWConcern` set another).
pub fn read_concern(cmd: &Document, name: &str) -> Result<ReadConcern, CommandError> {
    let implicit = || concerns::default_read_level().to_range();
    Ok(ReadConcernArgs::parse(cmd, name)?.map_or_else(implicit, |rc| rc.to_range()))
}

/// The session id of the command (`lsid.id`, a UUID), if any.
pub fn lsid(cmd: &Document) -> Result<Option<u128>, CommandError> {
    let name = cmd.iter().next().map_or("", |(k, _)| k);
    let Some(l) = doc(cmd, name, "lsid")? else { return Ok(None) };
    match l.get("id") {
        Some(Value::Binary { subtype: 4, bytes }) if bytes.len() == 16 => {
            let mut b = [0u8; 16];
            b.copy_from_slice(bytes);
            Ok(Some(u128::from_be_bytes(b)))
        }
        Some(v) => Err(wrong_type(&format!("{name}.lsid.id"), v, "binData")),
        None => Err(missing(&format!("{name}.lsid"), "id")),
    }
}

/// The command's `txnNumber`, if any.
pub fn txn_number(cmd: &Document) -> Result<Option<i64>, CommandError> {
    match cmd.get("txnNumber") {
        None => Ok(None),
        Some(v) => long(v, "OperationSessionInfo.txnNumber").map(Some),
    }
}

/// The UUID value of a session id.
pub fn uuid_value(lsid: u128) -> Value {
    Value::Binary { subtype: 4, bytes: lsid.to_be_bytes().to_vec() }
}
