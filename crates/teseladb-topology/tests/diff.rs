//! The `o` of an update by operators in `local.oplog.rs` (018 FR-009, T011): `{$v: 2, diff}`
//! from the document before the write and the change stream's description of it. The expected
//! diffs are those RefDb 9.0.4 (`ref:9.0`) wrote on 02-11-2026 for the same updates.

use teseladb_bson::{Document, Value};
use teseladb_changestream::update_desc::{PathPart, UpdateDescription};
use teseladb_topology::diff::update_entry;

fn ints(v: &[i32]) -> Value {
    Value::Array(v.iter().copied().map(Value::Int32).collect())
}

fn before() -> Document {
    Document::new()
        .with("_id", 1)
        .with("a", 1)
        .with("b", Document::new().with("c", 1).with("d", 2))
        .with("arr", ints(&[1, 2, 3, 4]))
        .with("s", "x")
}

fn desc(updated: Vec<(&str, Value)>, removed: &[&str]) -> UpdateDescription {
    UpdateDescription {
        updated: updated.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        removed: removed.iter().map(|s| (*s).to_owned()).collect(),
        ..UpdateDescription::default()
    }
}

fn entry(diff: Document) -> Document {
    Document::new().with("$v", 2).with("diff", diff)
}

/// `$set: {a: 2, n: 5, "b.c": 9, "b.e": 1, "arr.1": 7}, $unset: {s: 1}`.
#[test]
fn sections_and_subdiffs_in_refdb_order() {
    let d = desc(
        vec![
            ("a", Value::Int32(2)),
            ("b.c", Value::Int32(9)),
            ("b.e", Value::Int32(1)),
            ("arr.1", Value::Int32(7)),
            ("n", Value::Int32(5)),
        ],
        &["s"],
    );
    let expected = entry(
        Document::new()
            .with("d", Document::new().with("s", false))
            .with("u", Document::new().with("a", 2))
            .with("i", Document::new().with("n", 5))
            .with("sarr", Document::new().with("a", true).with("u1", 7))
            .with(
                "sb",
                Document::new()
                    .with("u", Document::new().with("c", 9))
                    .with("i", Document::new().with("e", 1)),
            ),
    );
    assert_eq!(update_entry(&before(), &d), expected);
}

/// `$push: {arr: 9}` sets a new position; `$pop` gives the whole array; a pipeline `$slice`
/// truncates it.
#[test]
fn arrays() {
    let push = desc(vec![("arr.4", Value::Int32(9))], &[]);
    let expected =
        entry(Document::new().with("sarr", Document::new().with("a", true).with("u4", 9)));
    assert_eq!(update_entry(&before(), &push), expected);

    let pop = desc(vec![("arr", ints(&[1, 7, 3, 4]))], &[]);
    let expected =
        entry(Document::new().with("u", Document::new().with("arr", ints(&[1, 7, 3, 4]))));
    assert_eq!(update_entry(&before(), &pop), expected);

    let cut = UpdateDescription {
        truncated: vec![("arr".to_owned(), 2)],
        updated: vec![("arr.0".to_owned(), Value::Int32(5))],
        ..UpdateDescription::default()
    };
    let expected = entry(
        Document::new().with("sarr", Document::new().with("a", true).with("l", 2).with("u0", 5)),
    );
    assert_eq!(update_entry(&before(), &cut), expected);
}

/// `$set: {"q.w": 1}` on a document without `q` inserts `q` whole; a field of a document inside
/// an array goes in the array's subdiff.
#[test]
fn missing_parents_and_documents_in_arrays() {
    let d = desc(vec![("q.w", Value::Int32(1))], &[]);
    let expected =
        entry(Document::new().with("i", Document::new().with("q", Document::new().with("w", 1))));
    assert_eq!(update_entry(&before(), &d), expected);

    let doc = Document::new()
        .with("_id", 1)
        .with("l", Value::Array(vec![Value::Document(Document::new().with("x", 1))]));
    let d = desc(vec![("l.0.x", Value::Int32(2)), ("l.0.y", Value::Int32(3))], &["l.0.z"]);
    let inner = Document::new()
        .with("d", Document::new().with("z", false))
        .with("u", Document::new().with("x", 2))
        .with("i", Document::new().with("y", 3));
    let expected =
        entry(Document::new().with("sl", Document::new().with("a", true).with("s0", inner)));
    assert_eq!(update_entry(&doc, &d), expected);
}

/// A field whose name is digits inside a document is a field, not a position, and the
/// disambiguated parts of a path decide over its dotted form.
#[test]
fn numeric_field_names() {
    let doc = Document::new().with("m", Document::new().with("0", 1));
    let d = desc(vec![("m.0", Value::Int32(2))], &[]);
    let expected =
        entry(Document::new().with("sm", Document::new().with("u", Document::new().with("0", 2))));
    assert_eq!(update_entry(&doc, &d), expected);

    let doc = Document::new().with("a.b", 1);
    let d = UpdateDescription {
        updated: vec![("a.b".to_owned(), Value::Int32(2))],
        disambiguated: vec![("a.b".to_owned(), vec![PathPart::Field("a.b".to_owned())])],
        ..UpdateDescription::default()
    };
    let expected = entry(Document::new().with("u", Document::new().with("a.b", 2)));
    assert_eq!(update_entry(&doc, &d), expected);
}
