//! The payloads drivers send (017 FR-001, FR-003, FR-004), built field by field: each kind reads
//! back its tokens and options, and the IDL errors (unknown field, missing field, wrong type, wrong
//! binary subtype, wrong token length, negative contention) come with their code and message.

use teseladb_bson::types as t;
use teseladb_bson::{Document, Value};
use teseladb_fle::error;
use teseladb_fle::payloads::{
    self, FindEqualityPayload, FindRangePayload, FindTextPayload, InsertUpdatePayload,
};
use teseladb_fle::subtype::BlobSubtype;
use teseladb_fle::tokens::Token;

fn bin(subtype: u8, bytes: &[u8]) -> Value {
    Value::Binary { subtype, bytes: bytes.to_vec() }
}

fn tok(b: u8) -> Value {
    bin(t::BINARY_GENERIC, &[b; 32])
}

fn payload(subtype: BlobSubtype, doc: &Document) -> Vec<u8> {
    let mut out = vec![subtype as u8];
    out.extend(doc.to_bytes().expect("small document"));
    out
}

fn token_set(b: u8) -> Document {
    Document::new()
        .with("d", tok(b))
        .with("s", tok(b + 1))
        .with("l", tok(b + 2))
        .with("p", bin(t::BINARY_GENERIC, &[b; 48]))
}

fn insert() -> Document {
    Document::new()
        .with("d", tok(1))
        .with("s", tok(2))
        .with("p", bin(t::BINARY_GENERIC, &[3; 48]))
        .with("u", bin(t::BINARY_UUID, &[4; 16]))
        .with("t", Value::Int32(0x10))
        .with("v", bin(t::BINARY_GENERIC, b"user key and value"))
        .with("e", tok(5))
        .with("l", tok(6))
        .with("k", Value::Int64(2))
}

fn insert_bytes(doc: &Document) -> Vec<u8> {
    payload(BlobSubtype::Fle2InsertUpdate, doc)
}

#[test]
fn an_equality_insert_payload_reads_its_tokens_and_value() {
    let p = InsertUpdatePayload::parse(&insert_bytes(&insert())).expect("valid payload");
    assert_eq!(p.tokens.edc, Token([1; 32]));
    assert_eq!(p.tokens.esc, Token([2; 32]));
    assert_eq!(p.tokens.server, Token([6; 32]));
    assert_eq!(p.tokens.encrypted_tokens, vec![3; 48]);
    assert_eq!(p.index_key_id, [4; 16]);
    assert_eq!(p.bson_type, 0x10);
    assert_eq!(p.value, b"user key and value");
    assert_eq!(p.server_encryption, Token([5; 32]));
    assert_eq!(p.contention, 2);
    assert!(p.edges.is_none() && p.text.is_none());
    assert_eq!(p.all_sets(), vec![&p.tokens]);
}

#[test]
fn a_range_insert_payload_reads_its_edges_and_options() {
    let doc = insert()
        .with(
            "g",
            Value::Array(vec![Value::Document(token_set(10)), Value::Document(token_set(20))]),
        )
        .with("sp", Value::Int64(2))
        .with("pn", Value::Int32(3))
        .with("tf", Value::Int32(1))
        .with("mn", Value::Int32(0))
        .with("mx", Value::Int32(100));
    let p = InsertUpdatePayload::parse(&insert_bytes(&doc)).expect("valid payload");
    let edges = p.edges.as_ref().expect("edges");
    assert_eq!(edges.len(), 2);
    assert_eq!(edges[1].esc, Token([21; 32]));
    assert_eq!((p.sparsity, p.precision, p.trim_factor), (Some(2), Some(3), Some(1)));
    assert_eq!((p.min.clone(), p.max.clone()), (Some(Value::Int32(0)), Some(Value::Int32(100))));
    assert_eq!(p.all_sets().len(), 2);
}

#[test]
fn a_text_insert_payload_orders_its_sets_exact_substrings_suffixes_prefixes() {
    let sets = Document::new()
        .with("e", Value::Document(token_set(10)))
        .with("s", Value::Array(vec![Value::Document(token_set(20))]))
        .with("u", Value::Array(vec![Value::Document(token_set(30))]))
        .with(
            "p",
            Value::Array(vec![Value::Document(token_set(40)), Value::Document(token_set(50))]),
        );
    let p = InsertUpdatePayload::parse(&insert_bytes(&insert().with("b", Value::Document(sets))))
        .expect("valid payload");
    let firsts: Vec<Token> = p.all_sets().iter().map(|s| s.edc).collect();
    let want: Vec<Token> = [10, 20, 30, 40, 50].iter().map(|b| Token([*b; 32])).collect();
    assert_eq!(firsts, want);
}

fn fails(bytes: &[u8], code: (i32, &str), message: &str) {
    let e = InsertUpdatePayload::parse(bytes).expect_err("must fail");
    assert_eq!((e.code, e.name), code, "{e}");
    assert_eq!(e.message, message);
}

#[test]
fn the_insert_payload_errors_carry_the_idl_codes_and_messages() {
    fails(
        &[],
        error::OVERFLOW,
        "buffer size too small to read (1) bytes out of buffer[0] at offset: 0",
    );
    fails(
        &[11, 1, 2],
        error::INVALID_BSON,
        "BSON data has to be at least 5 bytes in element with field name '?' in object with \
         unknown _id",
    );
    fails(
        &insert_bytes(&insert().with("zz", Value::Int32(1))),
        error::IDL_UNKNOWN_FIELD,
        "BSON field 'root.zz' is an unknown field.",
    );
    let mut no_k = insert();
    no_k.remove("k");
    fails(
        &insert_bytes(&no_k),
        error::IDL_FAILED_TO_PARSE,
        "BSON field 'root.k' is missing but a required field",
    );
    fails(
        &insert_bytes(&insert().with("k", Value::Int64(-1))),
        error::BAD_VALUE,
        "BSON field 'k' value must be >= 0, actual value '-1'",
    );
    fails(
        &insert_bytes(&insert().with("d", bin(t::BINARY_GENERIC, &[1; 31]))),
        error::TOKEN_LENGTH,
        "Invalid prf length for EDCDerivedFromDataTokenAndContentionFactor",
    );
    fails(
        &insert_bytes(&insert().with("u", bin(t::BINARY_GENERIC, &[4; 16]))),
        error::TYPE_MISMATCH,
        "BSON field 'root.u' is the wrong binData type 'general', expected type 'UUID'",
    );
    fails(
        &insert_bytes(&insert().with("u", bin(t::BINARY_UUID, &[4; 15]))),
        error::BAD_VALUE,
        "uuid must be a 16-byte binary field with UUID (4) subtype",
    );
    fails(
        &insert_bytes(&insert().with("t", Value::Int64(16))),
        error::TYPE_MISMATCH,
        "BSON field 'root.t' is the wrong type 'long', expected type 'int'",
    );
    fails(
        &insert_bytes(&insert().with("v", Value::String("x".into()))),
        error::TYPE_MISMATCH,
        "BSON field 'root.v' is the wrong type 'string', expected type 'binData'",
    );
    fails(
        &insert_bytes(&insert().with("g", Value::Array(vec![Value::Int32(1)]))),
        error::TYPE_MISMATCH,
        "BSON field 'EdgeTokenSetV2' is the wrong type 'int', expected type 'object'",
    );
    fails(
        &insert_bytes(&insert().with("g", Value::Int32(1))),
        error::TYPE_MISMATCH,
        "BSON field 'root.g' is the wrong type 'int', expected type 'array'",
    );
    fails(
        &insert_bytes(&insert().with("b", Value::Int32(1))),
        error::TYPE_MISMATCH,
        "BSON field 'root.b' is the wrong type 'int', expected type 'object'",
    );
    let mut missing_p = token_set(10);
    missing_p.remove("p");
    fails(
        &insert_bytes(&insert().with("g", Value::Array(vec![Value::Document(missing_p)]))),
        error::IDL_FAILED_TO_PARSE,
        "BSON field 'EdgeTokenSetV2.p' is missing but a required field",
    );
}

#[test]
fn an_equality_find_payload_reads_its_tokens_and_contention() {
    let doc = Document::new()
        .with("d", tok(1))
        .with("s", tok(2))
        .with("l", tok(3))
        .with("cm", Value::Int64(4));
    let p =
        FindEqualityPayload::parse(&payload(BlobSubtype::Fle2FindEquality, &doc)).expect("valid");
    assert_eq!(p.tokens.edc, Token([1; 32]));
    assert_eq!(p.tokens.esc, Token([2; 32]));
    assert_eq!(p.tokens.server, Token([3; 32]));
    assert_eq!(p.max_contention, 4);
    let no_cm = Document::new().with("d", tok(1)).with("s", tok(2)).with("l", tok(3));
    let p = FindEqualityPayload::parse(&payload(BlobSubtype::Fle2FindEquality, &no_cm))
        .expect("cm is optional");
    assert_eq!(p.max_contention, 0);
    let negative = no_cm.clone().with("cm", Value::Int64(-2));
    let e = FindEqualityPayload::parse(&payload(BlobSubtype::Fle2FindEquality, &negative))
        .expect_err("negative");
    assert_eq!(e.message, "BSON field 'cm' value must be >= 0, actual value '-2'");
    let mut no_l = no_cm;
    no_l.remove("l");
    let e = FindEqualityPayload::parse(&payload(BlobSubtype::Fle2FindEquality, &no_l))
        .expect_err("missing");
    assert_eq!(e.message, "BSON field 'root.l' is missing but a required field");
}

fn edge(b: u8) -> Value {
    Value::Document(Document::new().with("d", tok(b)).with("s", tok(b + 1)).with("l", tok(b + 2)))
}

fn range(doc: &Document) -> Result<FindRangePayload, teseladb_fle::FleError> {
    FindRangePayload::parse(&payload(BlobSubtype::Fle2FindRange, doc))
}

#[test]
fn a_range_find_payload_reads_its_edges_or_its_stub() {
    let inner = Document::new()
        .with("g", Value::Array(vec![edge(10), edge(20)]))
        .with("cm", Value::Int64(1));
    let doc = Document::new()
        .with("payload", Value::Document(inner))
        .with("payloadId", Value::Int32(7))
        .with("firstOperator", Value::Int32(2))
        .with("secondOperator", Value::Int32(4));
    let p = range(&doc).expect("valid");
    let (edges, cm) = p.edges.expect("edges");
    assert_eq!((edges.len(), cm), (2, 1));
    assert_eq!(edges[1].server, Token([22; 32]));
    assert_eq!((p.payload_id, p.first_operator, p.second_operator), (7, 2, Some(4)));

    let stub =
        Document::new().with("payloadId", Value::Int32(7)).with("firstOperator", Value::Int32(3));
    let p = range(&stub).expect("stub");
    assert_eq!((p.edges, p.second_operator), (None, None));

    let no_id = Document::new().with("firstOperator", Value::Int32(3));
    let e = range(&no_id).expect_err("missing");
    assert_eq!(e.message, "BSON field 'root.payloadId' is missing but a required field");

    let bad_inner =
        Document::new().with("g", Value::Array(vec![Value::Int32(1)])).with("cm", Value::Int64(0));
    let bad_edge = Document::new()
        .with("payload", Value::Document(bad_inner))
        .with("payloadId", Value::Int32(7))
        .with("firstOperator", Value::Int32(2));
    let e = range(&bad_edge).expect_err("int edge");
    assert_eq!(e.message, "BSON field 'g' is the wrong type 'int', expected type 'object'");

    let no_cm = Document::new()
        .with("payload", Value::Document(Document::new().with("g", Value::Array(vec![]))))
        .with("payloadId", Value::Int32(7))
        .with("firstOperator", Value::Int32(2));
    let e = range(&no_cm).expect_err("no cm");
    assert_eq!(e.message, "BSON field 'payload.cm' is missing but a required field");
}

fn text(doc: &Document) -> Result<FindTextPayload, teseladb_fle::FleError> {
    FindTextPayload::parse(&payload(BlobSubtype::Fle2FindText, doc))
}

#[test]
fn a_text_find_payload_reads_whichever_kind_it_carries() {
    let base = |kind: &str| {
        Document::new()
            .with("ts", Value::Document(Document::new().with(kind, edge(10))))
            .with("cm", Value::Int64(2))
            .with("cf", Value::Boolean(true))
            .with("df", Value::Boolean(false))
    };
    for kind in ["e", "s", "u", "p"] {
        let p = text(&base(kind)).expect("valid");
        assert_eq!(p.tokens().expect("one kind").edc, Token([10; 32]), "{kind}");
        assert_eq!((p.max_contention, p.case_fold, p.diacritic_fold), (2, true, false));
    }
    let mut no_df = base("e");
    no_df.remove("df");
    let e = text(&no_df).expect_err("df");
    assert_eq!(e.message, "BSON field 'root.df' is missing but a required field");
    let e = text(&base("e").with("cf", Value::Int32(1))).expect_err("cf type");
    assert_eq!(e.message, "BSON field 'root.cf' is the wrong type 'int', expected type 'bool'");
    let p = text(&base("e").with("ts", Value::Document(Document::new()))).expect("no kind");
    assert!(p.tokens().is_none());
}

#[test]
fn subtype_of_reads_only_encrypted_binaries() {
    let enc = bin(t::BINARY_ENCRYPTED, &[14, 0, 0]);
    let read = payloads::subtype_of(&enc).map(|(s, b)| (s, b.len()));
    assert_eq!(read, Some((BlobSubtype::Fle2EqualityIndexed, 3)));
    assert_eq!(payloads::subtype_of(&bin(t::BINARY_GENERIC, &[14])), None);
    assert_eq!(payloads::subtype_of(&bin(t::BINARY_ENCRYPTED, &[99])), None);
    assert_eq!(payloads::subtype_of(&Value::Int32(14)), None);
}
