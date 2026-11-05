//! Media types of the REST API (019 FR-006, contract `rest-api.md`, T010): bodies in relaxed
//! and canonical Extended JSON or BSON by `Content-Type`, replies by `Accept`.

use teseladb_rest::media::{
    BSON, CANONICAL_JSON, Format, JSON, MediaError, decode, encode, request_format, response_format,
};
use teseladb_bson::{Document, Value};

#[test]
fn request_format_by_content_type() {
    assert_eq!(request_format(None), Ok(Format::Relaxed));
    assert_eq!(request_format(Some("application/json")), Ok(Format::Relaxed));
    assert_eq!(request_format(Some("Application/JSON; charset=utf-8")), Ok(Format::Relaxed));
    assert_eq!(request_format(Some(CANONICAL_JSON)), Ok(Format::Canonical));
    assert_eq!(request_format(Some(" application/bson ")), Ok(Format::Bson));
    assert_eq!(request_format(Some("text/plain")), Err(MediaError::UnsupportedMediaType));
    assert_eq!(MediaError::UnsupportedMediaType.status(), 415);
}

#[test]
fn response_format_by_accept() {
    assert_eq!(response_format(None), Ok(Format::Relaxed));
    assert_eq!(response_format(Some("*/*")), Ok(Format::Relaxed));
    assert_eq!(response_format(Some("application/*")), Ok(Format::Relaxed));
    assert_eq!(response_format(Some("application/bson")), Ok(Format::Bson));
    assert_eq!(
        response_format(Some("application/json;q=0.5, application/bson;q=0.9")),
        Ok(Format::Bson)
    );
    assert_eq!(
        response_format(Some(&format!("{CANONICAL_JSON}, {JSON}"))),
        Ok(Format::Canonical),
        "on a tie, the first listed"
    );
    assert_eq!(
        response_format(Some("text/html, application/json;q=0")),
        Err(MediaError::NotAcceptable)
    );
    assert_eq!(
        response_format(Some("application/bson;q=abc")),
        Ok(Format::Bson),
        "a bad q counts as 1"
    );
    assert_eq!(response_format(Some("application/bson;q=2")), Ok(Format::Bson));
    assert_eq!(response_format(Some(", ,")), Err(MediaError::NotAcceptable));
    assert_eq!(MediaError::NotAcceptable.status(), 406);
    for f in [Format::Relaxed, Format::Canonical, Format::Bson] {
        assert_eq!(request_format(Some(f.media_type())), Ok(f));
    }
    assert_eq!(BSON, Format::Bson.media_type());
}

fn sample() -> Document {
    Document::new().with("_id", 1).with("n", 5_000_000_000i64).with("d", Value::DateTime(0))
}

#[test]
fn bodies_round_trip_in_every_format() {
    let relaxed = encode(&sample(), Format::Relaxed).expect("relaxed");
    assert_eq!(
        String::from_utf8(relaxed.clone()).expect("utf-8"),
        r#"{"_id":1,"n":5000000000,"d":{"$date":"1970-01-01T00:00:00Z"}}"#
    );
    let canonical = encode(&sample(), Format::Canonical).expect("canonical");
    assert_eq!(decode(&canonical, Format::Canonical), Ok(sample()));
    let bson = encode(&sample(), Format::Bson).expect("bson");
    assert_eq!(decode(&bson, Format::Bson), Ok(sample()));
    let back = decode(&relaxed, Format::Relaxed).expect("relaxed back");
    assert_eq!(back.get("n"), Some(&Value::Int64(5_000_000_000)));
}

#[test]
fn bad_bodies_are_bad_values() {
    let bad = |e: Result<Document, MediaError>| match e {
        Err(MediaError::BadBody(msg)) => msg,
        other => panic!("{other:?}"),
    };
    assert!(bad(decode(b"{", Format::Relaxed)).contains("Extended JSON"));
    assert!(bad(decode(&[0xff, 0xfe], Format::Canonical)).contains("UTF-8"));
    assert!(bad(decode(&[5, 0, 0, 0], Format::Bson)).contains("BSON"));
    assert_eq!(MediaError::BadBody(String::new()).status(), 400);
}

/// A body and a reply may hold up to `maxMessageSizeBytes` (48 000 000 bytes) although a stored
/// document stops at 16 MiB.
#[test]
fn bodies_up_to_the_message_size() {
    let big = Document::new().with("documents", vec![Value::from("x".repeat(17 * 1024 * 1024))]);
    let json = encode(&big, Format::Relaxed).expect("reply larger than a document");
    assert_eq!(decode(&json, Format::Relaxed), Ok(big.clone()));
    let bson = encode(&big, Format::Bson).expect("bson");
    assert_eq!(decode(&bson, Format::Bson), Ok(big));
    let huge = Document::new().with("a", "x".repeat(49_000_000));
    assert!(matches!(encode(&huge, Format::Relaxed), Err(MediaError::ReplyTooLarge)));
    assert_eq!(MediaError::ReplyTooLarge.status(), 500);
}
