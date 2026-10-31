//! RefDb's limits and the errors' offsets (003 FR-002, FR-003), as a `refrouter` 9.0.4 answers
//! them (003 T009): 16 MiB exactly is accepted and one byte more is not, a stored document takes
//! 180 levels and not 181, reading and writing alike, and every error says where.

use teseladb_bson::{
    BsonError, Document, Limits, MAX_DEPTH, MAX_SIZE, RawDoc, RawValue, Value, first_element,
    types, validate_message,
};

/// A document of exactly `size` bytes: one binary field filling it.
fn of_size(size: usize) -> Vec<u8> {
    // Length, type, "a\0", binary length, subtype, data and terminator.
    let data = size - 13;
    let d = Document::new()
        .with("a", Value::Binary { subtype: types::BINARY_GENERIC, bytes: vec![7; data] });
    let mut out = Vec::with_capacity(size);
    // `to_bytes` refuses more than 16 MiB, so the oversized one is written by hand.
    out.extend_from_slice(&(size as i32).to_le_bytes());
    out.extend_from_slice(&[types::BINARY, b'a', 0]);
    out.extend_from_slice(&(data as i32).to_le_bytes());
    out.push(types::BINARY_GENERIC);
    out.extend(std::iter::repeat_n(7, data));
    out.push(0);
    if size <= MAX_SIZE {
        assert_eq!(d.to_bytes().unwrap(), out);
    }
    out
}

/// `levels` nested documents, `{a: {a: … {}}}`, the top one included.
fn nested(levels: usize) -> Document {
    (1..levels).fold(Document::new(), |inner, _| Document::new().with("a", inner))
}

#[test]
fn sixteen_mebibytes_exactly_and_not_one_byte_more() {
    let exact = of_size(MAX_SIZE);
    assert_eq!(RawDoc::from_bytes(&exact).unwrap().as_bytes().len(), MAX_SIZE);
    let over = of_size(MAX_SIZE + 1);
    let e = RawDoc::from_bytes(&over).unwrap_err();
    assert_eq!(e, BsonError::TooLarge { size: MAX_SIZE + 1, max: MAX_SIZE });
    assert_eq!(e.code(), (10334, "BSONObjectTooLarge"));
    assert_eq!(e.to_string(), "object of 16777217 bytes, over the maximum of 16777216");
    // Writing refuses it too.
    let d = Document::new().with("a", Value::Binary { subtype: 0, bytes: vec![0; MAX_SIZE - 12] });
    assert_eq!(
        d.to_bytes().unwrap_err(),
        BsonError::TooLarge { size: MAX_SIZE + 1, max: MAX_SIZE }
    );
    // A limit of the caller's.
    let small = Limits { max_size: 100, ..Limits::default() };
    assert!(RawDoc::from_bytes_with(&of_size(101), small).is_err());
    assert!(RawDoc::from_bytes_with(&of_size(100), small).is_ok());
}

#[test]
fn a_stored_document_takes_180_levels_and_not_one_more() {
    // A `refrouter` 9.0.4 stores 180 levels and refuses 181 ("object exceeds 180 levels of nesting").
    assert_eq!(MAX_DEPTH, 180);
    let ok = nested(MAX_DEPTH).to_bytes().unwrap();
    RawDoc::from_bytes(&ok).unwrap();
    // Written by hand: one more level around the 180.
    let mut deeper = Vec::new();
    deeper.extend_from_slice(&((ok.len() + 8) as i32).to_le_bytes());
    deeper.extend_from_slice(&[types::DOCUMENT, b'a', 0]);
    deeper.extend_from_slice(&ok);
    deeper.push(0);
    // Each level starts 7 bytes after the one around it: the 181st at 1260, the value of the
    // 180th `a`.
    let path = vec!["a"; MAX_DEPTH].join(".");
    let e = RawDoc::from_bytes(&deeper).unwrap_err();
    assert_eq!(e, BsonError::TooDeep { offset: 1260, path: path.clone() });
    assert_eq!(e.code(), (15, "Overflow"));
    assert_eq!(
        e.to_string(),
        format!("BSONObj exceeds maximum nested object depth in element with field name '{path}'")
    );
    assert_eq!(
        nested(MAX_DEPTH + 1).to_bytes().unwrap_err(),
        BsonError::TooDeep { offset: 1260, path }
    );
    // Arrays and the scope of code count as levels too, and name their place.
    let in_array = (1..MAX_DEPTH).fold(Value::Null, |inner, _| Value::Array(vec![inner]));
    let e = Document::new().with("a", Value::Array(vec![in_array])).to_bytes().unwrap_err();
    let zeros = vec!["0"; MAX_DEPTH - 1].join(".");
    assert!(matches!(e, BsonError::TooDeep { path, .. } if path == format!("a.{zeros}")));
    let scope = Value::JavaScriptWithScope { code: "x".into(), scope: nested(MAX_DEPTH) };
    let e = Document::new().with("c", scope).to_bytes().unwrap_err();
    assert!(matches!(e, BsonError::TooDeep { path, .. } if path.starts_with("c.a.a")));
    // A caller can read deeper documents, such as a command's.
    let deep = Limits { max_depth: 201, ..Limits::default() };
    assert!(RawDoc::from_bytes_with(&deeper, deep).is_ok());
}

/// `{name: 1}` followed by `rest` elements, written by hand.
fn by_hand(elements: &[&[u8]]) -> Vec<u8> {
    let body: Vec<u8> = elements.concat();
    let mut out = ((body.len() + 5) as i32).to_le_bytes().to_vec();
    out.extend(body);
    out.push(0);
    out
}

#[test]
fn a_message_is_checked_as_refdb_checks_it() {
    // A `refrouter` 9.0.4 takes a string or a name that is not UTF-8 and an old binary whose two
    // lengths differ; reading refuses them (the BSON corpus has them as decode errors).
    let bad_string: &[u8] = &[types::STRING, b's', 0, 3, 0, 0, 0, 0xFF, b'b', 0];
    let bad_name: &[u8] = &[types::INT32, 0xFF, b'k', 0, 1, 0, 0, 0];
    let old_binary: &[u8] =
        &[types::BINARY, b'o', 0, 6, 0, 0, 0, types::BINARY_OLD, 9, 0, 0, 0, b'a', b'b'];
    for element in [bad_string, bad_name, old_binary] {
        let doc = by_hand(&[element]);
        assert_eq!(validate_message(&doc, Limits::default()), Ok(()));
        assert!(RawDoc::from_bytes(&doc).is_err());
    }
    // What RefDb refuses, the message check refuses too, at the first fault in document order.
    let boolean: &[u8] = &[types::BOOLEAN, b'b', 0, 2];
    let e = validate_message(&by_hand(&[bad_string, boolean]), Limits::default()).unwrap_err();
    assert_eq!(e, BsonError::InvalidBson { offset: 17, reason: "boolean other than 0 or 1" });
    let unknown: &[u8] = &[0x20, b'u', 0];
    assert!(matches!(
        validate_message(&by_hand(&[unknown]), Limits::default()),
        Err(BsonError::InvalidBson { offset: 4, .. })
    ));
    // Too deep before a fault further on: the depth is what RefDb answers.
    let deep = nested(4).to_bytes().unwrap();
    let mut element = vec![types::DOCUMENT, b'f', 0];
    element.extend(&deep);
    let doc = by_hand(&[&element, boolean]);
    let shallow = Limits { max_depth: 3, ..Limits::default() };
    assert!(matches!(
        validate_message(&doc, shallow),
        Err(BsonError::TooDeep { path, .. }) if path == "f.a.a"
    ));
    assert!(matches!(validate_message(&doc[..10], shallow), Err(BsonError::InvalidBson { .. })));
    assert!(matches!(
        validate_message(&by_hand(&[]), Limits { max_size: 4, ..Limits::default() }),
        Err(BsonError::TooLarge { size: 5, max: 4 })
    ));
}

#[test]
fn the_first_element_of_a_document() {
    let doc = Document::new().with("_id", 7).with("a", "x").to_bytes().unwrap();
    assert_eq!(first_element(&doc, Limits::default()), Some(("_id", RawValue::Int32(7))));
    // Whatever follows it does not matter.
    let mut broken = doc.clone();
    let last = broken.len() - 2;
    broken[last] = 0xFF;
    assert_eq!(first_element(&broken, Limits::default()), Some(("_id", RawValue::Int32(7))));
    // An empty document, a cut one, or a first element that does not read have none.
    assert_eq!(first_element(&Document::new().to_bytes().unwrap(), Limits::default()), None);
    assert_eq!(first_element(&doc[..3], Limits::default()), None);
    assert_eq!(first_element(&doc[..9], Limits::default()), None);
    let mut bad = doc.clone();
    bad[4] = 0x20;
    assert_eq!(first_element(&bad, Limits::default()), None);
}

#[test]
fn every_error_says_where() {
    let good = Document::new().with("s", "héllo").with("n", 1).to_bytes().unwrap();
    // The string's first byte is at 4 (length) + 1 (type) + 2 ("s\0") + 4 (string length) = 11.
    let mut bad_utf8 = good.clone();
    bad_utf8[12] = 0xC3;
    bad_utf8[13] = 0x28;
    assert_eq!(RawDoc::from_bytes(&bad_utf8).unwrap_err(), BsonError::InvalidUtf8 { offset: 11 });
    let mut bad_name = good.clone();
    bad_name[5] = 0xFF;
    assert_eq!(RawDoc::from_bytes(&bad_name).unwrap_err(), BsonError::InvalidUtf8 { offset: 5 });
    // A byte left over after the document.
    let mut extra = good.clone();
    extra.push(0);
    let e = RawDoc::from_bytes(&extra).unwrap_err();
    assert_eq!(
        e,
        BsonError::InvalidBson {
            offset: good.len(),
            reason: "bytes after the end of the document"
        }
    );
    // A length that leaves the terminator inside the document.
    let mut trailing = good.clone();
    let n = trailing.len() as i32 + 1;
    trailing[..4].copy_from_slice(&n.to_le_bytes());
    trailing.push(0);
    let e = RawDoc::from_bytes(&trailing).unwrap_err();
    assert!(matches!(e, BsonError::InvalidBson { offset, .. } if offset == good.len() - 1), "{e}");
    let mut unknown = good.clone();
    unknown[4] = 0x20;
    assert!(matches!(
        RawDoc::from_bytes(&unknown).unwrap_err(),
        BsonError::InvalidBson { offset: 4, .. }
    ));
    let mut boolean = Document::new().with("b", true).to_bytes().unwrap();
    boolean[7] = 2;
    let e = RawDoc::from_bytes(&boolean).unwrap_err();
    assert_eq!(e, BsonError::InvalidBson { offset: 7, reason: "boolean other than 0 or 1" });
    assert_eq!(e.code(), (22, "InvalidBSON"));
    assert_eq!(e.to_string(), "invalid BSON at 7: boolean other than 0 or 1");
    assert_eq!(BsonError::InvalidUtf8 { offset: 3 }.to_string(), "invalid UTF-8 at 3");
    assert_eq!(BsonError::InvalidUtf8 { offset: 3 }.code(), (22, "InvalidBSON"));
    assert!(BsonError::TooLarge { size: 9, max: 8 }.to_string().starts_with("object of 9 bytes"));
}

#[test]
fn uuids_and_md5_take_any_length_and_the_old_binary_keeps_its_own() {
    // A `refrouter` 9.0.4 stores a UUID of 15 bytes and an MD5 of 3 and reads them back as they are.
    for subtype in [types::BINARY_UUID_OLD, types::BINARY_UUID, types::BINARY_MD5] {
        for len in [0, 3, 15, 16, 17] {
            let d = Document::new().with("u", Value::Binary { subtype, bytes: vec![1; len] });
            let bytes = d.to_bytes().unwrap();
            assert_eq!(bytes[7], len as u8);
            assert_eq!(Document::from(RawDoc::from_bytes(&bytes).unwrap()), d);
        }
    }
    // Every other subtype, and the user ones, take any length, the vector included.
    for subtype in [0, 1, 6, 7, 8, types::BINARY_VECTOR, types::BINARY_USER, 0xFF] {
        let d = Document::new().with("b", Value::Binary { subtype, bytes: vec![3; 5] });
        let bytes = d.to_bytes().unwrap();
        assert_eq!(Document::from(RawDoc::from_bytes(&bytes).unwrap()), d);
    }
    // The old binary keeps its inner length, which must agree with the outer one.
    let old =
        Document::new().with("o", Value::Binary { subtype: types::BINARY_OLD, bytes: vec![9; 3] });
    let bytes = old.to_bytes().unwrap();
    assert_eq!(&bytes[7..16], &[7, 0, 0, 0, 2, 3, 0, 0, 0]);
    assert_eq!(Document::from(RawDoc::from_bytes(&bytes).unwrap()), old);
    let mut wrong = bytes.clone();
    wrong[12] = 4;
    assert!(RawDoc::from_bytes(&wrong).is_err());
}

#[test]
fn writing_refuses_what_cannot_be_read_back() {
    let e = Document::new().with("a\0b", 1).to_bytes().unwrap_err();
    assert_eq!(
        e,
        BsonError::InvalidBson { offset: 5, reason: "name or regular expression with a 00 byte" }
    );
    let regex = Value::Regex { pattern: "x\0".into(), options: String::new() };
    assert!(Document::new().with("r", regex).to_bytes().is_err());
    let regex = Value::Regex { pattern: "x".into(), options: "i\0".into() };
    assert!(Document::new().with("r", regex).to_bytes().is_err());
    // A binary past 16 MiB is refused before it is written.
    let huge =
        Document::new().with("b", Value::Binary { subtype: 0, bytes: vec![0; MAX_SIZE + 1] });
    assert_eq!(
        huge.to_bytes().unwrap_err(),
        BsonError::TooLarge { size: MAX_SIZE + 1, max: MAX_SIZE }
    );
    // A string too: no length past the limit is written.
    let long = Document::new().with("s", "a".repeat(100));
    let small = Limits { max_size: 50, ..Limits::default() };
    assert_eq!(long.to_bytes_with(small).unwrap_err(), BsonError::TooLarge { size: 101, max: 50 });
    // A string may hold 00.
    let s = Document::new().with("s", "a\0b").to_bytes().unwrap();
    assert_eq!(
        Document::from(RawDoc::from_bytes(&s).unwrap()).get("s"),
        Some(&Value::from("a\0b"))
    );
}

#[test]
fn writing_within_other_limits() {
    // A command reply may be deeper and larger than a stored document (006).
    let mut deep = Document::new().with("x", 1);
    for _ in 0..MAX_DEPTH {
        deep = Document::new().with("d", deep);
    }
    assert!(matches!(deep.to_bytes(), Err(BsonError::TooDeep { .. })));
    let wide = Limits { max_size: MAX_SIZE, max_depth: MAX_DEPTH + 1 };
    assert!(deep.to_bytes_with(wide).is_ok());
    let big = Document::new().with("s", "a".repeat(MAX_SIZE));
    assert!(matches!(big.to_bytes(), Err(BsonError::TooLarge { .. })));
    let larger = Limits { max_size: MAX_SIZE + 1024, max_depth: MAX_DEPTH };
    assert!(big.to_bytes_with(larger).is_ok());
    // A binary holding a whole stored document and more fits a larger limit: a mutation that
    // carries a document of 16 MiB (003 T009, a 16 MiB insert failed on this).
    let carrier =
        Document::new().with("m", Value::Binary { subtype: 0, bytes: vec![0; MAX_SIZE + 100] });
    let mutation = Limits { max_size: 64 << 20, max_depth: MAX_DEPTH };
    let bytes = carrier.to_bytes_with(mutation).unwrap();
    assert_eq!(Document::from(RawDoc::from_bytes_with(&bytes, mutation).unwrap()), carrier);
}
