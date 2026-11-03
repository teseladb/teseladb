//! The node's own mutation records read back when they are malformed: the rules of a write and
//! the result of one are written by the node, so an unreadable one is an `InternalError`.

use teseladb_bson::{Document, RawDoc, Value};

use super::{MutationResult, f, read_capped, read_error, read_mark, read_timeseries};
use crate::error::INTERNAL_ERROR;

fn bytes(d: &Document) -> Vec<u8> {
    d.to_bytes().expect("encodes")
}

#[test]
fn malformed_rules_are_internal_errors() {
    let rules = [
        Document::new().with(f::TIMESERIES, 1),
        Document::new().with(f::TIMESERIES, Document::new().with(f::TS_TIME, "t")),
        Document::new().with(f::CAPPED, 1),
    ];
    for d in &rules {
        let b = bytes(d);
        let raw = RawDoc::from_bytes(&b).expect("reads");
        let e = read_timeseries(raw).err().or_else(|| read_capped(raw).err());
        assert_eq!(e.map(|e| e.code), Some(INTERNAL_ERROR), "{d:?}");
    }
}

#[test]
fn a_malformed_result_is_an_internal_error() {
    let lists = Document::new().with(f::ERRORS, vec![Value::Int32(1)]);
    let e = MutationResult::decode(&bytes(&lists)).err().map(|e| e.code);
    assert_eq!(e, Some(INTERNAL_ERROR));
    let no_index = Document::new().with(f::CODE, 11000);
    assert!(read_error(RawDoc::from_bytes(&bytes(&no_index)).expect("reads")).is_none());
    // A result with no lists reads as one with no errors and no warnings.
    let r = MutationResult::decode(&bytes(&Document::new().with(f::N, 2i64))).expect("decodes");
    assert_eq!((r.n, r.errors.len(), r.warnings.len()), (2, 0, 0));
}

#[test]
fn a_malformed_mark_reads_as_none() {
    let half = Document::new().with("p", 1i64);
    let b = bytes(&Document::new().with("m", half));
    let raw = RawDoc::from_bytes(&b).expect("reads");
    assert_eq!(read_mark(raw.get("m")), None);
    assert_eq!(read_mark(None), None);
}
