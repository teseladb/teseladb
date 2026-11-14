//! A write, compaction, cleanup and the implicit schema end to end (017 FR-003, FR-005, FR-006),
//! with payloads, keys and the ESC built here: what each step leaves in the ESC and the ECOC, what
//! it refuses and the failure details of the implicit schema.

use std::collections::BTreeMap;

use teseladb_bson::types as t;
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_fle::cipher;
use teseladb_fle::compact::{self, EcocEntry, EscOp, FieldTokens};
use teseladb_fle::esc::{self, EscKeys, EscReader, Overlay};
use teseladb_fle::fields::{self, EncryptedFieldConfig, QueryType};
use teseladb_fle::iev::{IndexedValue, Kind};
use teseladb_fle::schema;
use teseladb_fle::subtype::BlobSubtype;
use teseladb_fle::tokens::{self, Token};
use teseladb_fle::write::{self, WriteError};

const SSN_KEY: [u8; 16] = [1; 16];
const AGE_KEY: [u8; 16] = [2; 16];
const NOTE_KEY: [u8; 16] = [3; 16];

fn bin(subtype: u8, bytes: &[u8]) -> Value {
    Value::Binary { subtype, bytes: bytes.to_vec() }
}

fn config() -> EncryptedFieldConfig {
    let field = |key: [u8; 16], path: &str, ty: &str, q: Option<Document>| {
        let mut f = Document::new()
            .with("keyId", bin(t::BINARY_UUID, &key))
            .with("path", path)
            .with("bsonType", ty);
        if let Some(q) = q {
            f.push("queries", q);
        }
        Value::Document(f)
    };
    let eq = Document::new().with("queryType", "equality").with("contention", Value::Int64(2));
    let range = Document::new()
        .with("queryType", "range")
        .with("contention", Value::Int64(0))
        .with("min", Value::Int32(0))
        .with("max", Value::Int32(255))
        .with("sparsity", Value::Int64(1));
    let ef = Document::new().with(
        "fields",
        vec![
            field(SSN_KEY, "ssn", "string", Some(eq)),
            field(AGE_KEY, "age", "int", Some(range)),
            field(NOTE_KEY, "a.note", "string", None),
        ],
    );
    let b = ef.to_bytes().expect("bytes");
    fields::for_create("c", RawDoc::from_bytes(&b).expect("raw")).expect("valid config")
}

/// An ESC in memory that applies compaction's changes.
#[derive(Default)]
struct Esc(BTreeMap<Token, Vec<u8>>);

impl EscReader for Esc {
    type Error = ();
    fn get(&mut self, id: &Token) -> Result<Option<Vec<u8>>, ()> {
        Ok(self.0.get(id).cloned())
    }
}

impl Esc {
    fn apply(&mut self, ops: Vec<EscOp>) {
        for op in ops {
            match op {
                EscOp::Insert(id, v) => {
                    self.0.insert(id, v.unwrap_or_default());
                }
                EscOp::Update(id, v) => {
                    self.0.insert(id, v);
                }
                EscOp::Delete(id) => {
                    self.0.remove(&id);
                }
            }
        }
    }
}

fn ivs() -> impl FnMut() -> [u8; 16] {
    let mut n = 0u128;
    move || {
        n += 1;
        n.to_be_bytes()
    }
}

/// The client side of one value: its EDC and ESC tokens and the field's ECOC token.
struct Client {
    edc: Token,
    esc: Token,
    ecoc: Token,
}

fn client(seed: u8) -> Client {
    Client { edc: Token([seed; 32]), esc: Token([seed + 1; 32]), ecoc: Token([0xec; 32]) }
}

fn entry_for(c: &Client, leaf: bool) -> Vec<u8> {
    let mut plain = c.esc.0.to_vec();
    plain.push(u8::from(leaf));
    cipher::encrypt(&c.ecoc, [3; 16], &plain)
}

fn token_set(c: &Client, leaf: bool) -> Document {
    Document::new()
        .with("d", bin(t::BINARY_GENERIC, &c.edc.0))
        .with("s", bin(t::BINARY_GENERIC, &c.esc.0))
        .with("l", bin(t::BINARY_GENERIC, &[0x5e; 32]))
        .with("p", bin(t::BINARY_GENERIC, &entry_for(c, leaf)))
}

fn insert_payload(key: [u8; 16], ty: u8, c: &Client, contention: i64, extra: &Document) -> Value {
    let mut doc = token_set(c, false)
        .with("u", bin(t::BINARY_UUID, &key))
        .with("t", Value::Int32(i32::from(ty)))
        .with("v", bin(t::BINARY_GENERIC, b"key id and client ciphertext"))
        .with("e", bin(t::BINARY_GENERIC, &[0x5d; 32]))
        .with("k", Value::Int64(contention));
    for (k, v) in extra.iter() {
        doc.insert(k, v.clone());
    }
    let mut bytes = vec![BlobSubtype::Fle2InsertUpdate as u8];
    bytes.extend(doc.to_bytes().expect("bytes"));
    bin(t::BINARY_ENCRYPTED, &bytes)
}

fn ssn_payload(c: &Client, key: [u8; 16], contention: i64) -> Document {
    Document::new().with("ssn", insert_payload(key, t::STRING, c, contention, &Document::new()))
}

#[test]
fn a_write_keeps_an_indexed_value_one_esc_position_and_one_ecoc_entry() {
    let cfg = config();
    let mut esc = Esc::default();
    let ssn = client(10);
    let mut next = ivs();
    for round in 1..=3u64 {
        let mut doc = ssn_payload(&ssn, SSN_KEY, 1);
        let mut overlay = Overlay::new(&mut esc);
        let added = write::transform(&cfg, &mut doc, &mut overlay, &mut next).expect("write");
        let written = overlay.into_written();
        for id in &written {
            esc.0.insert(*id, Vec::new());
        }
        assert_eq!(written, vec![EscKeys::of(&ssn.esc).position_id(round)]);
        let edc_twice = tokens::edc_twice_derived(&ssn.edc);
        assert_eq!(added.tags, vec![tokens::tag(&edc_twice, round)]);
        assert_eq!(added.ecoc.len(), 1);
        assert_eq!(added.ecoc[0].field, "ssn");
        let Some(Value::Binary { subtype: t::BINARY_ENCRYPTED, bytes }) = doc.get("ssn") else {
            panic!("an encrypted value: {doc:?}")
        };
        let iev = IndexedValue::parse(bytes).expect("indexed value");
        assert_eq!((iev.kind, iev.key_id, iev.bson_type), (Kind::Equality, SSN_KEY, t::STRING));
        assert_eq!(iev.blocks[0].count_and_contention(&Token([0x5e; 32])), (round, 1));
        write::add_tags(&mut doc, &added.tags);
        write::add_tags(&mut doc, &added.tags);
        let Some(Value::Array(tags)) = doc.get(fields::SAFE_CONTENT) else { panic!("tags") };
        assert_eq!(tags.len(), 2);
        assert_eq!(schema::implicit_failure(&cfg, &doc), None);
    }
}

fn refused(cfg: &EncryptedFieldConfig, doc: Document) -> (i32, String) {
    let mut doc = doc;
    let mut esc = Esc::default();
    let mut overlay = Overlay::new(&mut esc);
    match write::transform(cfg, &mut doc, &mut overlay, &mut ivs()) {
        Err(WriteError::Fle(e)) => (e.code, e.message),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_write_refuses_another_key_more_contention_and_unexpected_payloads() {
    let cfg = config();
    let ssn = client(10);
    let (code, message) = refused(&cfg, ssn_payload(&ssn, [9u8; 16], 0));
    assert_eq!(code, 6_726_301);
    assert_eq!(
        message,
        "Mismatched keyId for field 'ssn' expected 01010101-0101-0101-0101-010101010101, found \
         09090909-0909-0909-0909-090909090909"
    );
    let (code, message) = refused(&cfg, ssn_payload(&ssn, SSN_KEY, 3));
    assert_eq!(code, 9_188_700);
    assert_eq!(
        message,
        "contentionFactor 3 in payload for field 'ssn' exceeds collection's configured contention \
         2"
    );
    let random = bin(t::BINARY_ENCRYPTED, &[BlobSubtype::Fle1Random as u8, 0]);
    let (code, message) = refused(&cfg, Document::new().with("ssn", random));
    assert_eq!((code, message.as_str()), (6_373_503, "Unexpected encrypted payload type: 2"));
}

#[test]
fn unindexed_values_find_payloads_and_plain_fields_pass_through() {
    let cfg = config();
    let unindexed = bin(t::BINARY_ENCRYPTED, &[BlobSubtype::Fle2Unindexed as u8, 1, 2, 3]);
    let find = bin(t::BINARY_ENCRYPTED, &[BlobSubtype::Fle2FindEquality as u8, 1]);
    let mut doc = Document::new()
        .with("a", Document::new().with("note", unindexed.clone()))
        .with("ssn", find)
        .with("plain", 1);
    let before = doc.clone();
    let mut esc = Esc::default();
    let mut overlay = Overlay::new(&mut esc);
    let added = write::transform(&cfg, &mut doc, &mut overlay, &mut ivs()).expect("passes");
    assert_eq!(added, write::Added::default());
    assert_eq!(doc, before);
    assert_eq!(write::get_path(&doc, "a.note"), Some(&unindexed));
    assert_eq!(write::get_path(&doc, "a.missing"), None);
    assert_eq!(write::get_path(&doc, "plain.x"), None);
}

#[test]
fn a_range_write_keeps_one_block_per_edge() {
    let cfg = config();
    let age = client(30);
    let edges: Vec<Value> =
        (0u8..3).map(|i| Value::Document(token_set(&client(40 + 2 * i), i == 2))).collect();
    let extra = Document::new()
        .with("g", Value::Array(edges))
        .with("sp", Value::Int64(1))
        .with("mn", Value::Int32(0))
        .with("mx", Value::Int32(255));
    let mut doc = Document::new().with("age", insert_payload(AGE_KEY, t::INT32, &age, 0, &extra));
    let mut esc = Esc::default();
    let mut overlay = Overlay::new(&mut esc);
    let added = write::transform(&cfg, &mut doc, &mut overlay, &mut ivs()).expect("range write");
    assert_eq!(overlay.written().len(), 3);
    assert_eq!(added.tags.len(), 3);
    let Some(Value::Binary { bytes, .. }) = doc.get("age") else { panic!("value") };
    let iev = IndexedValue::parse(bytes).expect("range value");
    assert_eq!((iev.kind, iev.blocks.len()), (Kind::Range, 3));
    let age_field = cfg.field("age").expect("age");
    assert!(write::indexed_by(age_field, QueryType::Range));
    assert!(!write::indexed_by(age_field, QueryType::Equality));
}

#[test]
fn the_metadata_documents_and_uuid_text() {
    let e = write::EcocEntry { field: "ssn".into(), value: vec![1, 2, 3] };
    let d = write::ecoc_doc([5; 12], &e);
    assert_eq!(d.get("_id"), Some(&Value::ObjectId([5; 12])));
    assert_eq!(d.get("fieldName"), Some(&Value::String("ssn".into())));
    let esc_id = write::esc_doc(&Token([4; 32]));
    assert_eq!(esc_id.get("_id"), Some(&bin(t::BINARY_GENERIC, &[4; 32])));
    let mut id = [0u8; 16];
    id[0] = 0xab;
    id[15] = 0x01;
    assert_eq!(write::uuid_text(&id), "ab000000-0000-0000-0000-000000000001");
}

fn compact_once(
    cfg: &EncryptedFieldConfig,
    tokens: &[FieldTokens],
    entries: &[EcocEntry<'_>],
    esc: &mut Esc,
    cleanup: bool,
) -> (Vec<EscOp>, compact::EscStats) {
    compact::run(cfg, tokens, entries, esc, (cleanup, 1.0), &mut ivs()).expect("in memory")
}

#[test]
fn compaction_anchors_new_positions_and_cleanup_leaves_the_null_anchor() {
    let cfg = config();
    let ssn = client(10);
    let keys = EscKeys::of(&ssn.esc);
    let mut esc = Esc::default();
    for p in 1..=5 {
        esc.0.insert(keys.position_id(p), Vec::new());
    }
    let value = entry_for(&ssn, false);
    let entries =
        [EcocEntry { field: "ssn", value: &value }, EcocEntry { field: "ssn", value: &value }];
    let tokens = [FieldTokens { path: "ssn".into(), ecoc: ssn.ecoc, anchor_padding: None }];

    let (ops, stats) = compact_once(&cfg, &tokens, &entries, &mut esc, false);
    assert_eq!((stats.deleted, stats.inserted, stats.updated), (5, 1, 0));
    assert!(stats.read > 0);
    esc.apply(ops);
    let state = esc::state(&mut esc, &keys).expect("state");
    assert_eq!((state.apos, state.anchored, state.count), (1, 5, 5));

    // Nothing new: compaction does nothing.
    assert!(compact_once(&cfg, &tokens, &entries, &mut esc, false).0.is_empty());

    // Two more writes, then cleanup: everything gives way to the null anchor.
    esc.0.insert(keys.position_id(6), Vec::new());
    esc.0.insert(keys.position_id(7), Vec::new());
    let (ops, stats) = compact_once(&cfg, &tokens, &entries, &mut esc, true);
    assert_eq!((stats.deleted, stats.inserted, stats.updated), (3, 1, 0));
    esc.apply(ops);
    let state = esc::state(&mut esc, &keys).expect("state");
    assert_eq!(state.null, Some((1, 7)));
    assert_eq!(state.count, 7);

    // A second cleanup with nothing new leaves the null anchor as it is.
    let (ops, stats) = compact_once(&cfg, &tokens, &entries, &mut esc, true);
    assert!(ops.is_empty());
    assert_eq!(stats.inserted + stats.updated, 0);

    // A write after cleanup, then cleanup again: the null anchor is updated.
    esc.0.insert(keys.position_id(8), Vec::new());
    let (ops, stats) = compact_once(&cfg, &tokens, &entries, &mut esc, true);
    assert_eq!((stats.deleted, stats.updated), (1, 1));
    esc.apply(ops);
    assert_eq!(esc::state(&mut esc, &keys).expect("state").null, Some((1, 8)));
}

#[test]
fn entries_of_other_fields_or_another_key_are_skipped() {
    let cfg = config();
    let ssn = client(10);
    let mut esc = Esc::default();
    esc.0.insert(EscKeys::of(&ssn.esc).position_id(1), Vec::new());
    let wrong_key = cipher::encrypt(&Token([0x11; 32]), [3; 16], &ssn.esc.0);
    let short = cipher::encrypt(&ssn.ecoc, [3; 16], &[1, 2, 3]);
    let entries = [
        EcocEntry { field: "other", value: &wrong_key },
        EcocEntry { field: "ssn", value: &short },
        EcocEntry { field: "ssn", value: &[1, 2] },
    ];
    let tokens = [FieldTokens { path: "ssn".into(), ecoc: ssn.ecoc, anchor_padding: None }];
    assert!(compact_once(&cfg, &tokens, &entries, &mut esc, false).0.is_empty());
}

#[test]
fn range_compaction_pads_with_anchors_and_cleanup_folds_them() {
    let cfg = config();
    let edge = client(50);
    let root = Token([0x77; 32]);
    let pads = EscKeys {
        tag: tokens::anchor_padding_key(&root),
        value: tokens::anchor_padding_value(&root),
    };
    let mut esc = Esc::default();
    esc.0.insert(EscKeys::of(&edge.esc).position_id(1), Vec::new());
    let leaf = entry_for(&edge, true);
    let entries = [EcocEntry { field: "age", value: &leaf }];
    let tokens = [FieldTokens { path: "age".into(), ecoc: edge.ecoc, anchor_padding: Some(root) }];
    let (ops, stats) = compact_once(&cfg, &tokens, &entries, &mut esc, false);
    // One anchor for the edge compacted, and padding anchors for the edges the value has besides
    // it: a domain of 0 to 255 has 8 bits, and with sparsity 1 and the default trim factor of 6
    // each value has the leaf and the levels 6 and 7, 3 edges, so one leaf asks for 3 - 1 = 2.
    let padding = stats.inserted - 1;
    assert_eq!(padding, 2);
    esc.apply(ops);
    for a in 1..=padding {
        assert!(esc.0.contains_key(&pads.anchor_id(a)), "padding anchor {a}");
    }
    let (ops, stats) = compact_once(&cfg, &tokens, &entries, &mut esc, true);
    assert_eq!(stats.deleted, padding + 1);
    esc.apply(ops);
    assert_eq!(pads.read_anchor(&esc.0[&pads.anchor_id(0)]), Some((padding, 0)));
    // A second cleanup finds no padding anchor past the null one and leaves it.
    let (ops, _) = compact_once(&cfg, &tokens, &[], &mut esc, true);
    assert!(ops.is_empty());
}

fn read(d: &Document) -> Result<Vec<FieldTokens>, teseladb_fle::FleError> {
    let b = d.to_bytes().expect("bytes");
    compact::read_tokens(RawDoc::from_bytes(&b).expect("raw"), "c")
}

fn read_err(d: &Document) -> (i32, String) {
    let e = read(d).expect_err("must fail");
    (e.code, e.message)
}

#[test]
fn compaction_tokens_are_read_with_their_idl_errors() {
    assert_eq!(read_err(&Document::new().with("a", bin(0, &[1; 31]))).0, 9_616_300);
    assert_eq!(
        read_err(&Document::new().with("a", Value::Int32(1))).1,
        "BSON field 'c.a' is the wrong type 'int', expected types '[binData, object]'"
    );
    let unknown = Document::new().with("a", Document::new().with("x", bin(0, &[1; 32])));
    assert_eq!(read_err(&unknown).0, 40_415);
    let int_token = Document::new().with("a", Document::new().with("ecoc", Value::Int32(1)));
    assert_eq!(read_err(&int_token).0, 14);
    assert_eq!(read_err(&Document::new().with("a", Document::new())).0, 40_414);
    let both = Document::new().with(
        "a",
        Document::new().with("ecoc", bin(0, &[1; 32])).with("anchorPaddingToken", bin(0, &[2; 32])),
    );
    assert_eq!(read(&both).expect("tokens")[0].anchor_padding, Some(Token([2; 32])));
    let plain = read(&Document::new().with("b", bin(0, &[3; 32]))).expect("binary token");
    assert_eq!((plain[0].path.as_str(), plain[0].anchor_padding), ("b", None));
}

fn stored(subtype: BlobSubtype, ty: u8) -> Value {
    let mut bytes = vec![subtype as u8];
    bytes.extend([0u8; 16]);
    bytes.push(ty);
    bytes.extend([0u8; 8]);
    bin(t::BINARY_ENCRYPTED, &bytes)
}

fn failure_text(cfg: &EncryptedFieldConfig, doc: &Document) -> String {
    format!("{:?}", schema::implicit_failure(cfg, doc).expect("fails"))
}

#[test]
fn the_implicit_schema_passes_stored_values_of_the_right_type() {
    let cfg = config();
    let ok = Document::new()
        .with("ssn", stored(BlobSubtype::Fle2EqualityIndexed, t::STRING))
        .with("age", stored(BlobSubtype::Fle2RangeIndexed, t::INT32))
        .with("a", Document::new().with("note", stored(BlobSubtype::Fle2Unindexed, t::STRING)));
    assert_eq!(schema::implicit_failure(&cfg, &ok), None);
    // Any stored kind passes on a field when its type is right.
    let other_kind = Document::new().with("ssn", stored(BlobSubtype::Fle2TextIndexed, t::STRING));
    assert_eq!(schema::implicit_failure(&cfg, &other_kind), None);
    assert_eq!(schema::implicit_failure(&cfg, &Document::new().with("a", 5)), None);
    let fine_nested = Document::new().with("a", Document::new().with("other", 1));
    assert_eq!(schema::implicit_failure(&cfg, &fine_nested), None);
}

#[test]
fn the_implicit_schema_explains_each_failure_along_its_path() {
    let cfg = config();
    let d = failure_text(&cfg, &Document::new().with("ssn", "123"));
    assert!(d.contains(schema::NOT_ENCRYPTED) && d.contains("implicitFLESchema"));
    let wrong_type =
        Document::new().with("ssn", stored(BlobSubtype::Fle2EqualityIndexed, t::INT32));
    assert!(failure_text(&cfg, &wrong_type).contains(schema::WRONG_TYPE));
    let payload_bytes = [BlobSubtype::Fle2InsertUpdate as u8];
    let payload = Document::new().with("ssn", bin(t::BINARY_ENCRYPTED, &payload_bytes));
    assert!(failure_text(&cfg, &payload).contains(schema::WRONG_TYPE));
    let short_bytes = [BlobSubtype::Fle2EqualityIndexed as u8, 0];
    let no_type = Document::new().with("ssn", bin(t::BINARY_ENCRYPTED, &short_bytes));
    assert!(failure_text(&cfg, &no_type).contains(schema::WRONG_TYPE));
    let nested = Document::new().with("a", Document::new().with("note", 5));
    let d = failure_text(&cfg, &nested);
    assert!(d.contains("note") && d.contains("properties") && d.contains(schema::NOT_ENCRYPTED));
    let d = failure_text(&cfg, &Document::new().with("a", vec![Value::Int32(1)]));
    assert!(d.contains("$not") && d.contains("type did match"));
}

#[test]
fn each_field_expects_the_stored_kind_of_its_query() {
    let cfg = config();
    let kind = |p: &str| schema::expected_subtype(cfg.field(p).expect("field"));
    assert_eq!(kind("ssn"), BlobSubtype::Fle2EqualityIndexed);
    assert_eq!(kind("age"), BlobSubtype::Fle2RangeIndexed);
    assert_eq!(kind("a.note"), BlobSubtype::Fle2Unindexed);
}
