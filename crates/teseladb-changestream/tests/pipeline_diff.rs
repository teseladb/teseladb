//! The description of pipeline updates against what a `refnode` 9.0.4 gives for them (014 FR-009,
//! 018 FR-009; measured on 04-11-2026 on a one-member replica set): each case of
//! `fixtures/pipeline_updates.jsonl` is the document before and after a pipeline `update`, the
//! type of its change event and, for an `update`, its `updateDescription` and the `diff` of its
//! oplog entry.

use teseladb_changestream::event::operation_type;
use teseladb_changestream::pipeline_diff::{describe_diff, pipeline_diff};
use teseladb_changestream::token::EventKind;
use teseladb_bson::{Document, Value, extjson};

const FIXTURE: &str = include_str!("fixtures/pipeline_updates.jsonl");

/// A measured case.
struct Case {
    name: String,
    before: Document,
    after: Document,
    event: Option<String>,
    description: Option<Document>,
    diff: Option<Document>,
}

fn sub(d: &Document, key: &str) -> Option<Document> {
    match d.get(key) {
        Some(Value::Document(v)) => Some(v.clone()),
        _ => None,
    }
}

fn cases() -> Vec<Case> {
    FIXTURE
        .lines()
        .map(|line| {
            let d = extjson::parse(line).unwrap();
            let text = |k: &str| match d.get(k) {
                Some(Value::String(s)) => Some(s.clone()),
                _ => None,
            };
            Case {
                name: text("case").unwrap(),
                before: sub(&d, "before").unwrap(),
                after: sub(&d, "after").unwrap(),
                event: text("event"),
                description: sub(&d, "updateDescription"),
                diff: sub(&d, "diff"),
            }
        })
        .collect()
}

#[test]
fn the_fixture_covers_updates_replacements_and_no_ops() {
    let all = cases();
    let count = |e: Option<&str>| all.iter().filter(|c| c.event.as_deref() == e).count();
    let (update, replace) = (operation_type(EventKind::Update), operation_type(EventKind::Replace));
    assert_eq!((count(Some(update)), count(Some(replace)), count(None)), (106, 42, 5));
}

/// The pipeline updates a `refnode` gives as `update` have the same diff, in the same order, and
/// the same `updateDescription`; those it gives as `replace` have no diff.
#[test]
fn pipeline_updates_are_described_as_a_refnode_describes_them() {
    for c in cases() {
        let got = pipeline_diff(&c.before, &c.after);
        let update = operation_type(EventKind::Update);
        let replace = operation_type(EventKind::Replace);
        match c.event.as_deref() {
            Some(e) if e == update => {
                let diff = got.unwrap_or_else(|| panic!("{}: a replacement", c.name));
                assert_eq!(Some(&diff), c.diff.as_ref(), "{}: diff", c.name);
                let desc = describe_diff(&diff).to_document(false);
                assert_eq!(Some(&desc), c.description.as_ref(), "{}: description", c.name);
            }
            Some(e) if e == replace => assert_eq!(got, None, "{}: not a replacement", c.name),
            Some(e) => panic!("{}: event {e}", c.name),
            // A pipeline that changes no byte writes nothing (the document is the same).
            None => {
                assert_eq!(c.before.to_bytes().unwrap(), c.after.to_bytes().unwrap(), "{}", c.name)
            }
        }
    }
}

/// A field named like a number is an ambiguous path (`disambiguatedPaths`), an array position is
/// not.
#[test]
fn numeric_field_names_are_disambiguated() {
    let pad = "x".repeat(200);
    let before = extjson::parse(&format!(
        r#"{{"_id": 1, "a": {{"0": 1, "b": "{pad}"}}, "c": [1, 2, "{pad}"]}}"#
    ))
    .unwrap();
    let after = extjson::parse(&format!(
        r#"{{"_id": 1, "a": {{"0": 2, "b": "{pad}"}}, "c": [1, 3, "{pad}"]}}"#
    ))
    .unwrap();
    let diff = pipeline_diff(&before, &after).unwrap();
    let d = describe_diff(&diff).to_document(true);
    let want = extjson::parse(
        r#"{"updatedFields": {"a.0": 2, "c.1": 3}, "removedFields": [], "truncatedArrays": [],
            "disambiguatedPaths": {"a.0": ["a", "0"]}}"#,
    )
    .unwrap();
    assert_eq!(d, want);
}
