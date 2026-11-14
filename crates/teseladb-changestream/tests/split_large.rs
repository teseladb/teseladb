//! 014 T018: `$changeStreamSplitLargeEvent` (FR-011), as a reference router 9.0.4 splits an update
//! of a 9 MiB document with its pre- and post-image (measured on 01-11): two fragments, the first
//! with `_id`, `splitEvent`, `wallTime`, `clusterTime`, `operationType`, `documentKey`, `ns`,
//! `updateDescription` and `fullDocument`, the second with `fullDocumentBeforeChange`.

use teseladb_bson::{Document, Value};
use teseladb_changestream::order::EventKey;
use teseladb_changestream::split_large::{MAX_EVENT_BYTES, SplitError, metadata, size, split};
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

/// The bytes of the `_id` element of `d`.
fn id_element(d: &Document) -> usize {
    size(&Document::new().with("_id", d.get("_id").unwrap().clone())).unwrap() - 5
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
        assert!(size(d).unwrap() + metadata(id_element(d)) <= MAX_EVENT_BYTES);
    }
}

#[test]
fn a_field_larger_than_a_fragment_cannot_be_split() {
    let huge = Document::new()
        .with("_id", Document::new().with("_data", token().encode()))
        .with("fullDocument", Document::new().with("a", "x".repeat(MAX_EVENT_BYTES)));
    assert!(matches!(split(&huge, &token()), Err(SplitError::FieldTooLarge(_))));
}

/// The limit as measured on a reference router 9.0.4: the event with its metadata up to 16 MiB and
/// 8 KiB comes whole, a byte more goes in fragments.
#[test]
fn an_event_up_to_the_limit_with_its_metadata_is_whole() {
    let base = event(8 * 1024 * 1024);
    let room = MAX_EVENT_BYTES - size(&base).unwrap() - metadata(id_element(&base));
    let padded = |n: usize| {
        let mut e = base.clone();
        e.insert("pad", Value::String("p".repeat(n)));
        e
    };
    // The element `pad` takes 1 + 4 + 4 + 1 bytes besides its text.
    let at_limit = padded(room - 10);
    assert_eq!(size(&at_limit).unwrap() + metadata(id_element(&at_limit)), MAX_EVENT_BYTES);
    assert_eq!(split(&at_limit, &token()).unwrap().len(), 1);
    assert_eq!(split(&padded(room - 9), &token()).unwrap().len(), 2);
}
