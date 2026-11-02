//! The implicit schema of a collection with encrypted fields (FR-006): every encrypted field a
//! document has must hold a stored value of Queryable Encryption of its kind (equality 14, range
//! 15, text 17, unindexed 16) and of its configured BSON type. A `refnode` 9.0.4 reports a failure
//! as the validator `implicitFLESchema` with one `fle2Encrypt` rule per field, nested along the
//! paths, and refuses an array where a path needs a document (measured).

use teseladb_bson::types as t;
use teseladb_bson::{Document, Value};

use crate::fields::{EncryptedField, EncryptedFieldConfig, QueryType};
use crate::subtype::BlobSubtype;

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

/// One name of the implicit schema: an encrypted field, or a parent of some.
struct Node<'c> {
    name: &'c str,
    field: Option<&'c EncryptedField>,
    children: Vec<Node<'c>>,
}

/// The schema's tree: at each level, names in the order of the first field that has them.
fn tree(config: &EncryptedFieldConfig) -> Vec<Node<'_>> {
    let mut roots: Vec<Node<'_>> = Vec::new();
    for f in &config.fields {
        let mut level = &mut roots;
        let parts: Vec<&str> = f.path.split('.').collect();
        for (i, name) in parts.iter().copied().enumerate() {
            let at = match level.iter().position(|n| n.name == name) {
                Some(at) => at,
                None => {
                    level.push(Node { name, field: None, children: Vec::new() });
                    level.len() - 1
                }
            };
            if i + 1 == parts.len() {
                level[at].field = Some(f);
            }
            level = &mut level[at].children;
        }
    }
    roots
}

fn property(name: &str, detail: Document) -> Document {
    Document::new().with("propertyName", name).with("details", vec![Value::Document(detail)])
}

fn properties(failures: Vec<Document>) -> Document {
    Document::new().with("operatorName", "properties").with(
        "propertiesNotSatisfied",
        failures.into_iter().map(Value::Document).collect::<Vec<_>>(),
    )
}

/// The failure of an array where the schema expects a document: `not: {type: "array"}`.
fn not_array(v: &Value) -> Document {
    let rule = Document::new()
        .with("operatorName", "type")
        .with("specifiedAs", Document::new().with("type", vec![Value::Int32(4)]))
        .with("reason", "type did match")
        .with("consideredValue", v.clone())
        .with("consideredType", "array");
    Document::new().with("operatorName", "$not").with("details", rule)
}

/// The failures of `doc` against the names of one level.
fn failures(nodes: &[Node<'_>], doc: &Document) -> Vec<Document> {
    let mut out = Vec::new();
    for node in nodes {
        let Some(v) = doc.get(node.name) else { continue };
        if let Some(f) = node.field {
            if let Some(r) = reason(f, v) {
                let rule = Document::new().with("operatorName", "fle2Encrypt").with("reason", r);
                out.push(property(node.name, rule));
            }
            continue;
        }
        match v {
            Value::Array(_) => out.push(property(node.name, not_array(v))),
            Value::Document(d) => {
                let inner = failures(&node.children, d);
                if !inner.is_empty() {
                    out.push(property(node.name, properties(inner)));
                }
            }
            _ => {}
        }
    }
    out
}

/// The `details` of the validation failure of `doc`, if it fails.
pub fn implicit_failure(config: &EncryptedFieldConfig, doc: &Document) -> Option<Document> {
    let failures = failures(&tree(config), doc);
    if failures.is_empty() {
        return None;
    }
    Some(
        Document::new()
            .with("operatorName", "implicitFLESchema")
            .with("schemaRulesNotSatisfied", vec![Value::Document(properties(failures))]),
    )
}
