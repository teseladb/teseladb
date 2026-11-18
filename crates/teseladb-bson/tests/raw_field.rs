//! `RawDoc::get` finds the same value as walking the document's elements one by one, for every BSON
//! type it steps over: it compares the names' bytes and steps over each value by its type and its
//! length, and reads only the field it finds.

use teseladb_bson::{RawDoc, RawValue};

/// An element `type, name, 0, value` of a document being built by hand.
fn element(out: &mut Vec<u8>, t: u8, name: &str, value: &[u8]) {
    out.push(t);
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    out.extend_from_slice(value);
}

/// `n` as the little-endian `i32` of a BSON length.
fn len(n: usize) -> [u8; 4] {
    i32::try_from(n).expect("a length of a test document").to_le_bytes()
}

fn string(s: &str) -> Vec<u8> {
    let mut v = len(s.len() + 1).to_vec();
    v.extend_from_slice(s.as_bytes());
    v.push(0);
    v
}

fn document(elements: &[u8]) -> Vec<u8> {
    let mut v = len(elements.len() + 5).to_vec();
    v.extend_from_slice(elements);
    v.push(0);
    v
}

/// A document with one element of every type, each followed by the next, and the names that test
/// the comparison: a name that is a prefix of another, an empty one and a repeated one.
fn every_type() -> Vec<u8> {
    let mut inner = Vec::new();
    element(&mut inner, 0x10, "x", &7i32.to_le_bytes());
    element(&mut inner, 0x02, "y", &string("deep"));
    let mut array = Vec::new();
    element(&mut array, 0x10, "0", &1i32.to_le_bytes());
    element(&mut array, 0x10, "1", &2i32.to_le_bytes());
    let mut binary = 3i32.to_le_bytes().to_vec();
    binary.push(0x00);
    binary.extend_from_slice(b"abc");
    let mut regex = b"^a.*".to_vec();
    regex.push(0);
    regex.extend_from_slice(b"i");
    regex.push(0);
    let mut pointer = string("db.coll");
    pointer.extend_from_slice(&[9u8; 12]);
    let mut scope_doc = Vec::new();
    element(&mut scope_doc, 0x10, "s", &1i32.to_le_bytes());
    let scope_doc = document(&scope_doc);
    let code = string("return s;");
    let mut with_scope = len(4 + code.len() + scope_doc.len()).to_vec();
    with_scope.extend_from_slice(&code);
    with_scope.extend_from_slice(&scope_doc);

    let mut e = Vec::new();
    element(&mut e, 0x01, "double", &1.5f64.to_le_bytes());
    element(&mut e, 0x02, "string", &string("héllo"));
    element(&mut e, 0x03, "doc", &document(&inner));
    element(&mut e, 0x04, "array", &document(&array));
    element(&mut e, 0x05, "binary", &binary);
    element(&mut e, 0x06, "undefined", &[]);
    element(&mut e, 0x07, "oid", &[1u8; 12]);
    element(&mut e, 0x08, "bool", &[1]);
    element(&mut e, 0x09, "date", &123i64.to_le_bytes());
    element(&mut e, 0x0A, "null", &[]);
    element(&mut e, 0x0B, "regex", &regex);
    element(&mut e, 0x0C, "pointer", &pointer);
    element(&mut e, 0x0D, "code", &string("f()"));
    element(&mut e, 0x0E, "symbol", &string("sym"));
    element(&mut e, 0x0F, "scoped", &with_scope);
    element(&mut e, 0x10, "int32", &(-4i32).to_le_bytes());
    element(&mut e, 0x11, "ts", &[2u8; 8]);
    element(&mut e, 0x12, "int64", &(1i64 << 40).to_le_bytes());
    element(&mut e, 0x13, "dec", &[3u8; 16]);
    element(&mut e, 0xFF, "min", &[]);
    element(&mut e, 0x7F, "max", &[]);
    element(&mut e, 0x10, "a", &1i32.to_le_bytes());
    element(&mut e, 0x10, "ab", &2i32.to_le_bytes());
    element(&mut e, 0x10, "", &3i32.to_le_bytes());
    element(&mut e, 0x10, "a", &4i32.to_le_bytes());
    element(&mut e, 0x02, "last", &string("end"));
    document(&e)
}

/// The value of `name` found by walking the document's elements one by one.
fn walked<'a>(doc: &RawDoc<'a>, name: &str) -> Option<RawValue<'a>> {
    doc.iter().find(|(k, _)| *k == name).map(|(_, v)| v)
}

#[test]
fn every_field_is_found_as_the_walk_finds_it() {
    let bytes = every_type();
    let doc = RawDoc::from_bytes(&bytes).expect("a valid document");
    let names: Vec<&str> = doc.iter().map(|(k, _)| k).collect();
    assert_eq!(names.len(), 26);
    for name in names.iter().copied().chain(["absent", "ab ", "abc", "dou"]) {
        assert_eq!(doc.get(name), walked(&doc, name), "field {name:?}");
    }
    // The first of two fields with one name, as the walk finds it.
    assert_eq!(doc.get("a"), Some(RawValue::Int32(1)));
    assert_eq!(doc.get("ab"), Some(RawValue::Int32(2)));
    assert_eq!(doc.get(""), Some(RawValue::Int32(3)));
    assert_eq!(doc.get("last"), Some(RawValue::String("end")));
}

#[test]
fn a_path_goes_into_documents_and_arrays() {
    let bytes = every_type();
    let doc = RawDoc::from_stored(&bytes).expect("a stored document");
    assert_eq!(doc.get("doc.x"), Some(RawValue::Int32(7)));
    assert_eq!(doc.get("doc.y"), Some(RawValue::String("deep")));
    assert_eq!(doc.get("array.1"), Some(RawValue::Int32(2)));
    assert_eq!(doc.get("doc.z"), None);
    assert_eq!(doc.get("int32.x"), None, "a path below a scalar");
}

#[test]
fn an_empty_document_has_no_fields() {
    let bytes = document(&[]);
    let doc = RawDoc::from_bytes(&bytes).expect("a valid document");
    assert_eq!(doc.get("a"), None);
    assert_eq!(doc.get(""), None);
}
