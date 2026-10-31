//! `redactClientLogData` (013 FR-027): the documents of the clients written to the log keep their
//! field names and the length of their arrays, and every value becomes `"###"`, as RefDb
//! Enterprise 9.0.4 writes them (measured 31-10-2026 on the validation lines 20294 and 7488700).

use teseladb_bson::{Document, Value};

/// What a redacted value is written as.
pub const REDACTED: &str = "###";

/// `doc` with each value redacted.
pub fn document(doc: &Document) -> Document {
    let mut out = Document::new();
    for (k, v) in doc.iter() {
        out.push(k, value(v));
    }
    out
}

/// `v` redacted: a document and an array keep their shape, anything else is [`REDACTED`].
pub fn value(v: &Value) -> Value {
    match v {
        Value::Document(d) => Value::Document(document(d)),
        Value::Array(a) => Value::Array(a.iter().map(value).collect()),
        _ => Value::from(REDACTED),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn values_go_and_names_and_shapes_stay() {
        let doc = Document::new()
            .with("_id", 2)
            .with("a", "secretvalue2")
            .with("n", Document::new().with("x", 6))
            .with("arr", Value::Array(vec![Value::Int32(1), Value::from("s")]));
        let want = Document::new()
            .with("_id", REDACTED)
            .with("a", REDACTED)
            .with("n", Document::new().with("x", REDACTED))
            .with("arr", Value::Array(vec![Value::from(REDACTED), Value::from(REDACTED)]));
        assert_eq!(document(&doc), want);
        assert_eq!(value(&Value::Null), Value::from(REDACTED));
    }
}
