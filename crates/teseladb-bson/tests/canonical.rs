//! A document that keeps the canonical bytes it was read from (`Document::from_canonical`): it
//! reads and writes as the same document parsed with `Document::from`, writes those bytes until it
//! changes, and keeps the limits of the document it is written in.

use teseladb_bson::{BsonError, Document, Limits, RawDoc, Value};

/// A document with nested documents, an array and a regular expression, in canonical BSON.
fn sample() -> Vec<u8> {
    Document::new()
        .with("_id", 7)
        .with("a", Document::new().with("b", "x").with("c", vec![Value::from(1), Value::Null]))
        .with("r", Value::Regex { pattern: "^z".into(), options: "im".into() })
        .to_bytes()
        .expect("encodes")
}

/// `n` documents nested one in another, the top one included.
fn nested(n: usize) -> Vec<u8> {
    let mut d = Document::new().with("leaf", 1);
    for _ in 1..n {
        d = Document::new().with("d", d);
    }
    d.to_bytes_with(Limits { max_size: usize::MAX, max_depth: usize::MAX }).expect("encodes")
}

#[test]
fn a_kept_document_reads_and_writes_as_the_parsed_one() {
    let bytes = sample();
    let raw = RawDoc::from_bytes(&bytes).expect("valid");
    let (kept, parsed) = (Document::from_canonical(raw), Document::from(raw));
    assert_eq!(kept.to_bytes().expect("encodes"), bytes);
    assert_eq!(kept.canonical_bytes(), Some(bytes.as_slice()));
    assert_eq!(parsed.canonical_bytes(), None);
    assert_eq!(kept, parsed);
    assert_eq!(kept.clone(), parsed);
    assert_eq!(kept.len(), 3);
    assert!(!kept.is_empty());
    assert_eq!(kept.get("_id"), Some(&Value::Int32(7)));
    let names: Vec<&str> = kept.iter().map(|(k, _)| k).collect();
    assert_eq!(names, ["_id", "a", "r"]);
    assert_eq!(format!("{kept:?}"), format!("{parsed:?}"));
}

#[test]
fn a_kept_document_inside_another_writes_its_bytes() {
    let bytes = sample();
    let raw = RawDoc::from_bytes(&bytes).expect("valid");
    let outer = |d: Document| Document::new().with("batch", vec![Value::Document(d)]);
    let kept = outer(Document::from_canonical(raw)).to_bytes().expect("encodes");
    assert_eq!(kept, outer(Document::from(raw)).to_bytes().expect("encodes"));
}

#[test]
fn a_changed_document_is_written_from_its_fields() {
    let bytes = sample();
    let raw = RawDoc::from_bytes(&bytes).expect("valid");
    let changes: [fn(&mut Document); 4] = [
        |d| {
            d.insert("_id", 8);
        },
        |d| d.push("z", true),
        |d| {
            d.remove("a");
        },
        |d| {
            if let Some(v) = d.get_mut("_id") {
                *v = Value::Int64(9);
            }
        },
    ];
    for change in changes {
        let (mut kept, mut parsed) = (Document::from_canonical(raw), Document::from(raw));
        change(&mut kept);
        change(&mut parsed);
        assert_eq!(kept.canonical_bytes(), None);
        assert_eq!(kept, parsed);
        assert_ne!(kept.to_bytes().expect("encodes"), bytes);
        assert_eq!(kept.to_bytes(), parsed.to_bytes());
    }
}

#[test]
fn a_kept_document_keeps_the_limits_of_the_document_it_is_written_in() {
    // 180 levels fit a stored document; one level around them does not.
    let bytes = nested(180);
    let raw = RawDoc::from_bytes(&bytes).expect("valid");
    let around = |d: Document| Document::new().with("x", d);
    for d in [Document::from_canonical(raw), Document::from(raw)] {
        assert_eq!(d.to_bytes().expect("fits"), bytes);
        assert!(matches!(around(d).to_bytes(), Err(BsonError::TooDeep { .. })));
    }
    // A limit on the size holds as well.
    let small = Limits { max_size: bytes.len() - 1, max_depth: usize::MAX };
    for d in [Document::from_canonical(raw), Document::from(raw)] {
        assert!(matches!(d.to_bytes_with(small), Err(BsonError::TooLarge { .. })));
    }
}

#[test]
fn the_levels_of_a_kept_document_are_counted_only_when_they_could_pass_the_limit() {
    // Ten levels written under a limit of eleven: one level around them fits, two do not.
    let bytes = nested(10);
    let raw = RawDoc::from_bytes(&bytes).expect("valid");
    let limits = Limits { max_size: teseladb_bson::MAX_SIZE, max_depth: 11 };
    let once = Document::new().with("x", Document::from_canonical(raw));
    assert!(once.to_bytes_with(limits).is_ok());
    let twice = Document::new().with("y", once);
    assert!(matches!(twice.to_bytes_with(limits), Err(BsonError::TooDeep { .. })));
}

#[test]
fn a_document_takes_the_room_of_its_fields_vector() {
    assert_eq!(std::mem::size_of::<Document>(), std::mem::size_of::<Vec<(String, Value)>>());
}

#[test]
fn a_stored_document_is_taken_with_a_check_of_its_frame_only() {
    let bytes = sample();
    let raw = RawDoc::from_stored(&bytes).expect("a whole frame");
    assert_eq!(raw.as_bytes(), bytes.as_slice());
    assert_eq!(raw.get("_id"), RawDoc::from_bytes(&bytes).expect("valid").get("_id"));
    let kept = Document::from_stored(bytes.clone()).expect("a whole frame");
    assert_eq!(kept.canonical_bytes(), Some(bytes.as_slice()));
    assert_eq!(kept, Document::from(RawDoc::from_bytes(&bytes).expect("valid")));
    // A frame that does not hold: short, a length other than the bytes', no final zero.
    let mut longer = bytes.clone();
    longer.push(0);
    let mut unterminated = bytes.clone();
    *unterminated.last_mut().expect("bytes") = 1;
    for bad in [&[5u8, 0, 0][..], &longer, &unterminated, &[4, 0, 0, 0, 0][..]] {
        assert!(matches!(RawDoc::from_stored(bad), Err(BsonError::InvalidBson { .. })));
        assert!(Document::from_stored(bad.to_vec()).is_err());
    }
}

#[test]
fn the_encoded_length_is_that_of_the_written_document() {
    let text = r#"{"d":1.5,"s":"héllo","e":"","o":{"a":{"b":[1,{"c":null}]}},"a":[],
        "big":[0,1,2,3,4,5,6,7,8,9,10,11,12],
        "bin":{"$binary":{"base64":"AQID","subType":"00"}},
        "old":{"$binary":{"base64":"AQID","subType":"02"}},
        "u":{"$undefined":true},"oid":{"$oid":"650000000000000000000001"},"t":true,
        "dt":{"$date":{"$numberLong":"1700000000000"}},"n":null,
        "re":{"$regularExpression":{"pattern":"^a.c$","options":"ix"}},
        "ptr":{"$dbPointer":{"$ref":"db.c","$id":{"$oid":"650000000000000000000002"}}},
        "js":{"$code":"function(){}"},"sym":{"$symbol":"x"},
        "jss":{"$code":"f()","$scope":{"x":1}},"i":{"$numberInt":"7"},
        "ts":{"$timestamp":{"t":1,"i":2}},"l":{"$numberLong":"9"},
        "dec":{"$numberDecimal":"1.5"},"min":{"$minKey":1},"max":{"$maxKey":1}}"#;
    let doc = teseladb_bson::extjson::parse(text).expect("a document");
    assert_eq!(doc.encoded_len(), doc.to_bytes().expect("writes").len());
    assert_eq!(Document::new().encoded_len(), 5);
    // A kept document inside a reply counts its bytes.
    let bytes = sample();
    let kept = Document::from_canonical(RawDoc::from_bytes(&bytes).expect("valid"));
    let reply = Document::new().with("batch", vec![Value::Document(kept)]);
    assert_eq!(reply.encoded_len(), reply.to_bytes().expect("writes").len());
}

#[test]
fn a_document_appended_after_other_bytes_is_written_as_alone() {
    let bytes = sample();
    let kept = Document::from_canonical(RawDoc::from_bytes(&bytes).expect("valid"));
    for d in [Document::from(RawDoc::from_bytes(&bytes).expect("valid")), kept] {
        let reply = Document::new().with("batch", vec![Value::Document(d)]);
        let mut out = vec![9, 9, 9];
        reply.append_to(&mut out, Limits::default()).expect("fits");
        assert_eq!(&out[..3], &[9, 9, 9]);
        assert_eq!(&out[3..], reply.to_bytes().expect("writes").as_slice());
        // The limit counts the document, not what was in the buffer before.
        let tight = Limits { max_size: out.len() - 4, max_depth: usize::MAX };
        assert!(matches!(
            reply.append_to(&mut vec![0; 100], tight),
            Err(BsonError::TooLarge { .. })
        ));
        let exact = Limits { max_size: out.len() - 3, max_depth: usize::MAX };
        assert!(reply.append_to(&mut vec![0; 100], exact).is_ok());
    }
}
