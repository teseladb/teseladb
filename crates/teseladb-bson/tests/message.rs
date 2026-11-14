//! [`read_message`]: one pass over a document of a message gives what [`validate_message`]
//! followed by [`RawDoc::from_bytes_with`] gave, and keeps the bytes of a canonical document
//! instead of reading its fields.

use teseladb_bson::{BsonError, Document, Limits, RawDoc, Value, read_message, validate_message};

const LIMITS: Limits = Limits {
    max_size: teseladb_bson::MAX_SIZE + 16 * 1024,
    max_depth: teseladb_bson::MAX_COMMAND_DEPTH,
};

/// What the two passes gave: the message fault, or the fault of the specification, or the
/// document read whole.
fn two_passes(bytes: &[u8]) -> Result<Result<Document, BsonError>, BsonError> {
    validate_message(bytes, LIMITS)?;
    Ok(RawDoc::from_bytes_with(bytes, LIMITS).map(Document::from))
}

fn same(bytes: &[u8]) {
    let one = read_message(bytes, LIMITS);
    let two = two_passes(bytes);
    match (&one, &two) {
        (Err(a), Err(b)) | (Ok(Err(a)), Ok(Err(b))) => assert_eq!(a, b, "{bytes:?}"),
        (Ok(Ok(a)), Ok(Ok(b))) => {
            // Compared as bytes: a double that is NaN is not equal to itself.
            let written = a.to_bytes_with(LIMITS).expect("writes");
            assert_eq!(written, b.to_bytes_with(LIMITS).expect("writes"));
            if let Some(kept) = a.canonical_bytes() {
                assert_eq!(kept, bytes, "a kept document keeps the bytes it was read from");
            }
        }
        _ => panic!("{one:?} against {two:?} for {bytes:?}"),
    }
}

fn bytes(d: &Document) -> Vec<u8> {
    d.to_bytes_with(LIMITS).expect("encodes")
}

#[test]
fn a_canonical_document_is_kept_as_its_bytes() {
    let d = Document::new()
        .with("insert", "c")
        .with("a", vec![Value::Int32(1), Value::from("x")])
        .with("r", Value::Regex { pattern: "p".into(), options: "imx".into() })
        .with("n", Document::new().with("k", 2i64));
    let b = bytes(&d);
    let read = read_message(&b, LIMITS).expect("valid").expect("valid");
    assert_eq!(read.canonical_bytes(), Some(b.as_slice()));
    assert_eq!(read, d);
    same(&b);
}

#[test]
fn an_array_named_out_of_order_is_read_into_its_fields() {
    // `{ a: { "1": 1, "0": 2 } }` written as an array: the writer names its elements again.
    let mut b = bytes(&Document::new().with("a", Document::new().with("1", 1).with("0", 2)));
    b[4] = 0x04;
    let read = read_message(&b, LIMITS).expect("valid").expect("valid");
    assert_eq!(read.canonical_bytes(), None);
    same(&b);
}

#[test]
fn an_array_with_a_name_that_is_not_a_position_is_read_into_its_fields() {
    for name in ["00", "01", "x", "", "+0", "10"] {
        let mut b = bytes(&Document::new().with("a", Document::new().with(name, 1)));
        b[4] = 0x04;
        assert_eq!(
            read_message(&b, LIMITS).expect("valid").expect("valid").canonical_bytes(),
            None,
            "{name:?}"
        );
        same(&b);
    }
}

#[test]
fn regular_expression_options_out_of_order_are_read_into_its_fields() {
    let mut b = bytes(
        &Document::new().with("r", Value::Regex { pattern: "p".into(), options: "im".into() }),
    );
    // The options sit before the last two bytes (their terminator and the document's).
    let at = b.len() - 4;
    b.swap(at, at + 1);
    assert_eq!(read_message(&b, LIMITS).expect("valid").expect("valid").canonical_bytes(), None);
    same(&b);
}

#[test]
fn a_string_that_is_not_utf8_gives_the_fault_of_the_specification() {
    let mut b = bytes(&Document::new().with("s", "ab").with("t", "cd"));
    let at = b.iter().position(|&c| c == b'a').expect("the string");
    b[at] = 0xff;
    assert!(matches!(read_message(&b, LIMITS), Ok(Err(BsonError::InvalidUtf8 { .. }))));
    same(&b);
}

#[test]
fn an_old_binary_whose_two_lengths_differ_gives_the_fault_of_the_specification() {
    let d = Document::new().with("b", Value::Binary { subtype: 2, bytes: vec![1, 2, 3] });
    let mut b = bytes(&d);
    // The inner length follows the outer length and the subtype: `b\0`, 4 bytes, 1 byte.
    let inner = 4 + 1 + 2 + 4 + 1;
    b[inner] = 9;
    assert!(matches!(read_message(&b, LIMITS), Ok(Err(BsonError::InvalidBson { .. }))));
    same(&b);
}

#[test]
fn a_fault_of_the_message_wins_over_an_earlier_fault_of_the_specification() {
    // A string that is not UTF-8, then an unknown element type: RefDb closes the connection.
    let mut b = bytes(&Document::new().with("s", "ab").with("t", 1));
    let at = b.iter().position(|&c| c == b'a').expect("the string");
    b[at] = 0xff;
    let t = b.iter().rposition(|&c| c == b't').expect("the name") - 1;
    b[t] = 0x77;
    assert!(read_message(&b, LIMITS).is_err());
    same(&b);
}

#[test]
fn a_long_array_named_by_its_positions_is_kept() {
    let items: Vec<Value> = (0..12).map(Value::Int32).collect();
    let b = bytes(&Document::new().with("a", items));
    let read = read_message(&b, LIMITS).expect("valid").expect("valid");
    assert_eq!(read.canonical_bytes(), Some(b.as_slice()));
    same(&b);
}

#[test]
fn a_document_read_in_a_shared_buffer_keeps_a_view_of_it() {
    let d = Document::new().with("_id", 1).with("a", "x");
    let mut buf = b"xx".to_vec();
    buf.extend_from_slice(&bytes(&d));
    let shared = teseladb_types::Bytes::from_vec(buf);
    let part = shared.slice(2..);
    let read = teseladb_bson::read_message_in(&part, LIMITS).expect("valid").expect("valid");
    let kept = read.canonical_bytes().expect("kept");
    assert_eq!(kept.as_ptr(), part.as_ptr(), "a view of the buffer, not a copy");
    assert_eq!(read, d);
    assert_eq!(read.clone().to_bytes().expect("writes"), bytes(&d));
    // What is not canonical, or breaks a rule, reads as `read_message` reads it.
    let mut odd = bytes(&Document::new().with("a", Document::new().with("1", 1)));
    odd[4] = 0x04;
    let odd = teseladb_types::Bytes::from_vec(odd);
    let read = teseladb_bson::read_message_in(&odd, LIMITS).expect("valid").expect("valid");
    assert_eq!(read.canonical_bytes(), None);
}
