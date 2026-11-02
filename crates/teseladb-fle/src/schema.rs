//! The implicit schema of a collection with encrypted fields (FR-006): every encrypted field a
//! document has must hold a stored value of Queryable Encryption of its kind (equality 14, range
//! 15, text 17, unindexed 16) and of its configured BSON type. A `refnode` 9.0.4 reports a failure
//! as the validator `implicitFLESchema` with one `fle2Encrypt` rule per field (measured).

use teseladb_bson::types as t;
use teseladb_bson::{Document, Value};

use crate::fields::{EncryptedField, EncryptedFieldConfig, QueryType};
use crate::subtype::BlobSubtype;
use crate::write::get_path;

/// The reason of a value that is no encrypted value.
pub const NOT_ENCRYPTED: &str = "value was not encrypted";
/// The reason of an encrypted value of another kind or type.
pub const WRONG_TYPE: &str = "Queryable Encryption encrypted value has wrong type";
/// Where the original BSON type sits in a stored value: after the subtype and the key id.
const TYPE_AT: usize = 17;

/// The stored kind a field's values have.
pub fn expected_subtype(f: &EncryptedField) -> BlobSubtype {
    match f.query().map(|q| q.kind) {
        None => BlobSubtype::Fle2Unindexed,
        Some(QueryType::Equality) => BlobSubtype::Fle2EqualityIndexed,
        Some(QueryType::Range) => BlobSubtype::Fle2RangeIndexed,
        Some(_) => BlobSubtype::Fle2TextIndexed,
    }
}

/// Why `v` is no value of field `f`, if it is not.
fn reason(f: &EncryptedField, v: &Value) -> Option<&'static str> {
    let Value::Binary { subtype: t::BINARY_ENCRYPTED, bytes } = v else {
        return Some(NOT_ENCRYPTED);
    };
    if BlobSubtype::of(bytes) != Some(expected_subtype(f)) {
        return Some(WRONG_TYPE);
    }
    match (&f.bson_type, bytes.get(TYPE_AT)) {
        (Some((ty, _)), Some(got)) if got != ty => Some(WRONG_TYPE),
        (_, None) => Some(WRONG_TYPE),
        _ => None,
    }
}

/// The `properties` failure of one field, nested along its path.
fn property(path: &[&str], reason: &str) -> Document {
    let details = match path {
        [_] => Document::new().with("operatorName", "fle2Encrypt").with("reason", reason),
        [_, rest @ ..] => properties(vec![property(rest, reason)]),
        [] => Document::new(),
    };
    Document::new().with("propertyName", path[0]).with("details", vec![Value::Document(details)])
}

fn properties(failures: Vec<Document>) -> Document {
    Document::new().with("operatorName", "properties").with(
        "propertiesNotSatisfied",
        failures.into_iter().map(Value::Document).collect::<Vec<_>>(),
    )
}

/// The `details` of the validation failure of `doc`, if it fails.
pub fn implicit_failure(config: &EncryptedFieldConfig, doc: &Document) -> Option<Document> {
    let failures: Vec<Document> = config
        .fields
        .iter()
        .filter_map(|f| {
            let v = get_path(doc, &f.path)?;
            let r = reason(f, v)?;
            let path: Vec<&str> = f.path.split('.').collect();
            Some(property(&path, r))
        })
        .collect();
    if failures.is_empty() {
        return None;
    }
    Some(
        Document::new()
            .with("operatorName", "implicitFLESchema")
            .with("schemaRulesNotSatisfied", vec![Value::Document(properties(failures))]),
    )
}
