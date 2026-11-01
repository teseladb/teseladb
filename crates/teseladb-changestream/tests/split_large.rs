//! 014 T018: `$changeStreamSplitLargeEvent` (FR-011), as a `refrouter` 9.0.4 splits an update of a
//! 9 MiB document with its pre- and post-image (measured on 01-11): two fragments, the first with
//! `_id`, `splitEvent`, `wallTime`, `clusterTime`, `operationType`, `documentKey`, `ns`,
//! `updateDescription` and `fullDocument`, the second with `fullDocumentBeforeChange`.

use teseladb_bson::{Document, Value};
use teseladb_changestream::order::EventKey;
use teseladb_changestream::split_large::{MAX_EVENT_BYTES, SplitError, split};
use teseladb_changestream::token::{EventKind, ResumeToken};
use teseladb_hlc::Hlc;

fn token() -> ResumeToken {
    let event = EventKey { ts: Hlc { physical: 7, logical: 1 }, txn: 0, op: 0, key: b"k".to_vec() };
    ResumeToken::new(event, EventKind::Update)
}

fn event(big: usize) -> Document {
    let s = "x".repeat(big);
    Document::new()
        .with("_id", Document::new().with("_data", token().encode()))
        .with("operationType", "update")
        .with("clusterTime", Value::Timestamp { time: 1, increment: 1 })
        .with("wallTime", Value::DateTime(1))
        .with("ns", Document::new().with("db", "big").with("coll", "c"))
        .with("documentKey", Document::new().with("_id", 1))
        .with(
            "updateDescription",
            Document::new().with("updatedFields", Document::new().with("b", 1)),
        )
        .with("fullDocument", Document::new().with("_id", 1).with("a", s.clone()).with("b", 1))
        .with("fullDocumentBeforeChange", Document::new().with("_id", 1).with("a", s))
}

fn keys(d: &Document) -> Vec<&str> {
    d.iter().map(|(k, _)| k).collect()
}

#[test]
fn an_event_that_fits_is_returned_whole() {
    let e = event(10);
    assert_eq!(split(&e, &token()), Ok(vec![(token(), e)]));
}

#[test]
fn a_large_event_goes_in_fragments_by_field_size() {
    let e = event(9 * 1024 * 1024);
    let parts = split(&e, &token()).unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!(
        keys(&parts[0].1),
        [
            "_id",
            "splitEvent",
            "wallTime",
            "clusterTime",
            "operationType",
            "documentKey",
            "ns",
            "updateDescription",
            "fullDocument"
        ]
    );
    assert_eq!(keys(&parts[1].1), ["_id", "splitEvent", "fullDocumentBeforeChange"]);
    for (i, (t, d)) in parts.iter().enumerate() {
        let n = u32::try_from(i + 1).unwrap();
        assert_eq!(t.fragment, Some(n));
        assert_eq!((t.event.clone(), t.kind), (token().event, EventKind::Update));
        let split = Document::new().with("fragment", i32::try_from(n).unwrap()).with("of", 2);
        assert_eq!(d.get("splitEvent"), Some(&Value::Document(split)));
        assert_eq!(d.get("_id"), Some(&Value::Document(Document::new().with("_data", t.encode()))));
        assert!(teseladb_changestream::split_large::size(d).unwrap() <= MAX_EVENT_BYTES);
    }
}

#[test]
fn a_field_larger_than_a_fragment_cannot_be_split() {
    let huge = Document::new()
        .with("_id", Document::new().with("_data", token().encode()))
        .with("fullDocument", Document::new().with("a", "x".repeat(MAX_EVENT_BYTES)));
    assert!(matches!(split(&huge, &token()), Err(SplitError::FieldTooLarge(_))));
}
