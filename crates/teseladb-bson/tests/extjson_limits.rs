//! Extended JSON read within other limits than a stored document's: a REST
//! request body may hold up to `maxMessageSizeBytes`.

use teseladb_bson::{Limits, MAX_DEPTH, MAX_SIZE, extjson};

#[test]
fn parse_with_takes_bodies_larger_than_a_document() {
    let big = "x".repeat(MAX_SIZE);
    let text = format!(r#"{{"documents":[{{"a":"{big}"}}]}}"#);
    assert!(extjson::parse(&text).is_err(), "a stored document is at most 16 MiB");
    let limits = Limits { max_size: 48_000_000, max_depth: MAX_DEPTH };
    let doc = extjson::parse_with(&text, limits).expect("within the larger limit");
    assert_eq!(doc.len(), 1);
    let small = Limits { max_size: 8, max_depth: MAX_DEPTH };
    assert!(extjson::parse_with(r#"{"a":1}"#, small).is_err());
}
