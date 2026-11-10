//! The length of a document's BSON without writing it (H-009 of the native tests: the replies of
//! a scan wrote each document once to measure it and once more to send it).

use teseladb_bson::{Document, Value};

/// The old binary subtype, written with its own length inside.
const BINARY_OLD: u8 = 2;

/// The bytes `doc` takes written as BSON: its length, each element and the closing zero; a
/// document read back with its canonical bytes is as long as they are (H-009c).
pub(crate) fn encoded_len(doc: &Document) -> usize {
    if let Some(b) = doc.canonical_bytes() {
        return b.len();
    }
    5 + doc.iter().map(|(k, v)| element_len(k.len(), v)).sum::<usize>()
}

/// An element: its type, its name with its zero and its value.
fn element_len(name: usize, v: &Value) -> usize {
    2 + name + value_len(v)
}

/// A string written with its length and its zero.
fn string_len(s: &str) -> usize {
    5 + s.len()
}

/// The decimal digits of `i`, the name of an array's element `i`.
fn digits(i: usize) -> usize {
    i.checked_ilog10().map_or(1, |d| usize::try_from(d).unwrap_or(0) + 1)
}

fn value_len(v: &Value) -> usize {
    match v {
        Value::Double(_) | Value::DateTime(_) | Value::Timestamp { .. } | Value::Int64(_) => 8,
        Value::String(s) | Value::JavaScript(s) | Value::Symbol(s) => string_len(s),
        Value::Document(d) => encoded_len(d),
        Value::Array(items) => {
            5 + items.iter().enumerate().map(|(i, x)| element_len(digits(i), x)).sum::<usize>()
        }
        Value::Binary { subtype, bytes } => {
            5 + bytes.len() + if *subtype == BINARY_OLD { 4 } else { 0 }
        }
        Value::Undefined | Value::Null | Value::MinKey | Value::MaxKey => 0,
        Value::ObjectId(_) => 12,
        Value::Boolean(_) => 1,
        Value::Regex { pattern, options } => pattern.len() + options.len() + 2,
        Value::DbPointer { namespace, .. } => string_len(namespace) + 12,
        Value::JavaScriptWithScope { code, scope } => 4 + string_len(code) + encoded_len(scope),
        Value::Int32(_) => 4,
        Value::Decimal128(_) => 16,
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::encoded_len;
    use teseladb_bson::{Document, Value, extjson};

    #[test]
    fn the_length_is_that_of_the_written_document_for_every_kind_of_value() {
        let text = r#"{"d":1.5,"s":"héllo","e":"","o":{"a":{"b":[1,{"c":null}]}},"a":[],
            "big":[0,1,2,3,4,5,6,7,8,9,10,11,12],
            "bin":{"$binary":{"base64":"AQID","subType":"00"}},
            "old":{"$binary":{"base64":"AQID","subType":"02"}},
            "u":{"$undefined":true},"oid":{"$oid":"650000000000000000000001"},"t":true,
            "dt":{"$date":{"$numberLong":"1700000000000"}},"n":null,
            "re":{"$regularExpression":{"pattern":"^a.c$","options":"ix"}},
            "ptr":{"$dbPointer":{"$ref":"db.c","$id":{"$oid":"650000000000000000000002"}}},
            "js":{"$code":"function(){}"},"sym":{"$symbol":"x"},
            "jss":{"$code":"f()","$scope":{"x":1}},"i":{"$numberInt":"7"},
            "ts":{"$timestamp":{"t":1,"i":2}},"l":{"$numberLong":"9"},
            "dec":{"$numberDecimal":"1.5"},"min":{"$minKey":1},"max":{"$maxKey":1}}"#;
        let doc = extjson::parse(text).expect("a document");
        assert_eq!(encoded_len(&doc), doc.to_bytes().expect("writes").len());
        let empty = Document::new();
        assert_eq!(encoded_len(&empty), 5);
        let deep = (0..50).fold(Document::new(), |d, i| {
            Document::new().with("x", Value::Document(d)).with("i", i)
        });
        assert_eq!(encoded_len(&deep), deep.to_bytes().expect("writes").len());
        // A document read back with its bytes, and the same one once it changes.
        let bytes = doc.to_bytes().expect("writes");
        let mut kept =
            Document::from_canonical(teseladb_bson::RawDoc::from_bytes(&bytes).expect("valid"));
        assert_eq!(encoded_len(&kept), bytes.len());
        kept.insert("more", 1);
        assert_eq!(encoded_len(&kept), kept.to_bytes().expect("writes").len());
    }
}
