//! The corners of compaction, range domains, query rewrites and the catalog option (017 FR-002 to
//! FR-005) that the other tests do not reach: each case says what it checks.

use std::collections::BTreeMap;

use teseladb_bson::types as t;
use teseladb_bson::{Decimal128, Document, RawDoc, Value};
use teseladb_fle::compact::{self, EcocEntry, EscOp, FieldTokens};
use teseladb_fle::esc::{EscKeys, EscReader, Overlay};
use teseladb_fle::fields::{self, EncryptedFieldConfig};
use teseladb_fle::query::Rewriter;
use teseladb_fle::subtype::BlobSubtype;
use teseladb_fle::tokens::{self, Token};
use teseladb_fle::{FleError, cipher, range, write};

fn bin(subtype: u8, bytes: &[u8]) -> Value {
    Value::Binary { subtype, bytes: bytes.to_vec() }
}

fn create(ef: &Document) -> Result<EncryptedFieldConfig, FleError> {
    let b = ef.to_bytes().expect("bytes");
    fields::for_create("c", RawDoc::from_bytes(&b).expect("raw"))
}

fn field(key: u8, path: &str, ty: &str, q: Option<Document>) -> Value {
    let mut f = Document::new()
        .with("keyId", bin(t::BINARY_UUID, &[key; 16]))
        .with("path", path)
        .with("bsonType", ty);
    if let Some(q) = q {
        f.push("queries", q);
    }
    Value::Document(f)
}

fn config() -> EncryptedFieldConfig {
    let range = Document::new()
        .with("queryType", "range")
        .with("contention", 0)
        .with("min", 0)
        .with("max", 255)
        .with("sparsity", 1);
    let eq = Document::new().with("queryType", "equality").with("contention", 0);
    let ef = Document::new().with(
        "fields",
        vec![field(1, "ssn", "string", Some(eq)), field(2, "age", "int", Some(range))],
    );
    create(&ef).expect("valid")
}

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

fn run(
    cfg: &EncryptedFieldConfig,
    tokens: &[FieldTokens],
    entries: &[EcocEntry<'_>],
    esc: &mut Esc,
    cleanup: bool,
) -> (Vec<EscOp>, compact::EscStats) {
    let mut n = 0u8;
    let mut ivs = || {
        n = n.wrapping_add(1);
        [n; 16]
    };
    compact::run(cfg, tokens, entries, esc, (cleanup, 1.0), &mut ivs).expect("in memory")
}

fn ecoc_entry(ecoc: &Token, esc: &Token, leaf: bool) -> Vec<u8> {
    let mut plain = esc.0.to_vec();
    plain.push(u8::from(leaf));
    cipher::encrypt(ecoc, [1; 16], &plain)
}

#[test]
fn a_cleanup_of_a_value_with_nothing_written_still_leaves_its_null_anchor() {
    let cfg = config();
    let (ecoc, esc_token) = (Token([0xe0; 32]), Token([0x10; 32]));
    let value = ecoc_entry(&ecoc, &esc_token, false);
    let entries = [EcocEntry { field: "ssn", value: &value }];
    let tokens = [FieldTokens { path: "ssn".into(), ecoc, anchor_padding: None }];
    let mut esc = Esc::default();
    let (ops, stats) = run(&cfg, &tokens, &entries, &mut esc, true);
    assert_eq!((stats.deleted, stats.inserted), (0, 1));
    esc.apply(ops);
    let keys = EscKeys::of(&esc_token);
    assert_eq!(keys.read_anchor(&esc.0[&keys.anchor_id(0)]), Some((0, 0)));
}

#[test]
fn a_leaf_named_twice_counts_once_and_padding_on_an_equality_field_pads_nothing() {
    let cfg = config();
    let (ecoc, edge) = (Token([0xe1; 32]), Token([0x20; 32]));
    let leaf = ecoc_entry(&ecoc, &edge, true);
    let entries =
        [EcocEntry { field: "age", value: &leaf }, EcocEntry { field: "age", value: &leaf }];
    let root = Token([0x99; 32]);
    let tokens = [FieldTokens { path: "age".into(), ecoc, anchor_padding: Some(root) }];
    let mut esc = Esc::default();
    let (_, stats) = run(&cfg, &tokens, &entries, &mut esc, false);
    // One value with three edges and nothing written for its edge: two padding anchors.
    assert_eq!(stats.inserted, 2);

    let eq_value = Token([0x30; 32]);
    let eq_entry = ecoc_entry(&ecoc, &eq_value, false);
    let eq_entries = [EcocEntry { field: "ssn", value: &eq_entry }];
    let eq_tokens = [FieldTokens { path: "ssn".into(), ecoc, anchor_padding: Some(root) }];
    let mut fresh = Esc::default();
    fresh.0.insert(EscKeys::of(&eq_value).position_id(1), Vec::new());
    let (_, stats) = run(&cfg, &eq_tokens, &eq_entries, &mut fresh, false);
    assert_eq!(stats.inserted, 1, "the value's anchor and no padding");
}

#[test]
fn padding_anchors_added_after_a_cleanup_fold_into_the_existing_null_anchor() {
    let cfg = config();
    let (ecoc, edge) = (Token([0xe2; 32]), Token([0x40; 32]));
    let leaf = ecoc_entry(&ecoc, &edge, true);
    let entries = [EcocEntry { field: "age", value: &leaf }];
    let root = Token([0x88; 32]);
    let pads = EscKeys {
        tag: tokens::anchor_padding_key(&root),
        value: tokens::anchor_padding_value(&root),
    };
    let tokens = [FieldTokens { path: "age".into(), ecoc, anchor_padding: Some(root) }];
    let mut esc = Esc::default();
    for cleanup in [false, true, false] {
        let (ops, _) = run(&cfg, &tokens, &entries, &mut esc, cleanup);
        esc.apply(ops);
    }
    // The value itself has no writes, so its null anchor stays as the first cleanup left it; only
    // the padding null anchor takes the two padding anchors added after it.
    let (ops, stats) = run(&cfg, &tokens, &entries, &mut esc, true);
    assert_eq!((stats.updated, stats.deleted), (1, 2));
    esc.apply(ops);
    assert_eq!(pads.read_anchor(&esc.0[&pads.anchor_id(0)]), Some((4, 0)));
}

#[test]
fn domains_fall_back_to_full_width_when_precision_cannot_be_used() {
    let d = Value::Double;
    // More decimals than the precision, or bounds past what a double keeps exact.
    assert_eq!(range::domain_bits(t::DOUBLE, Some(&d(0.0)), Some(&d(1.25)), Some(1)), 64);
    assert_eq!(range::domain_bits(t::DOUBLE, Some(&d(-1e17)), Some(&d(0.0)), Some(0)), 64);
    let (lo, hi) = (Value::DateTime(0), Value::DateTime(7));
    assert_eq!(range::domain_bits(t::DATE_TIME, Some(&lo), Some(&hi), None), 3);
}

fn dec(c: u128, e: i32) -> Value {
    Value::Decimal128(Decimal128::from_parts(false, c, e).expect("ok"))
}

fn code(ty: u8, min: &Value, max: &Value, precision: i32) -> i32 {
    range::check_precision(ty, (min, max), precision, "x").expect_err("refused").code
}

#[test]
fn precision_checks_count_decimals() {
    // 0.12 has two decimals: refused with precision 1, taken with precision 2.
    assert_eq!(code(t::DECIMAL128, &dec(12, -2), &dec(1, 0), 1), 6_966_807);
    assert!(range::check_precision(t::DECIMAL128, (&dec(12, -2), &dec(1, 0)), 2, "x").is_ok());
    assert_eq!(code(t::DECIMAL128, &dec(0, 0), &dec(125, -3), 2), 6_966_808);
    assert_eq!(code(t::DOUBLE, &Value::Double(0.15), &Value::Double(1.0), 1), 6_966_805);
    let inf = Value::Decimal128(Decimal128::infinity(false));
    assert!(range::check_precision(t::DECIMAL128, (&dec(0, 0), &inf), 0, "x").is_err());
    let ints = (&Value::Int32(0), &Value::Int32(1));
    assert!(range::check_precision(t::DOUBLE, ints, 0, "x").is_ok());
}

#[test]
fn precision_checks_bound_the_scaled_values() {
    let e = range::check_precision(t::DOUBLE, (&Value::Double(0.0), &Value::Double(1e19)), 0, "x")
        .expect_err("upper");
    assert_eq!(
        (e.code, e.message.as_str()),
        (
            9_178_803,
            "Invalid upper bounds for double precision, must be less than 9223372036854775807"
        )
    );
    assert_eq!(code(t::DOUBLE, &Value::Double(-1e19), &Value::Double(0.0), 0), 9_178_804);
    assert_eq!(code(t::DECIMAL128, &dec(0, 0), &dec(1, 40), 0), 9_178_810);
}

#[test]
fn floating_ranges_without_bounds_and_long_domains_with_small_trims_are_taken() {
    let range = Document::new().with("queryType", "range");
    let double = Document::new().with("fields", vec![field(1, "a", "double", Some(range.clone()))]);
    assert!(create(&double).is_ok());
    let long = range.with("trimFactor", 1).with("sparsity", 2);
    let cfg = create(&Document::new().with("fields", vec![field(1, "a", "long", Some(long))]))
        .expect("ok");
    let doc = cfg.to_document();
    assert_eq!(doc.get("eccCollection"), None);
    assert_eq!(doc.get("strEncodeVersion"), None);
}

#[test]
fn an_error_shows_its_name_code_and_message() {
    let e = create(&Document::new()).expect_err("no fields");
    assert_eq!(
        e.to_string(),
        "IDLFailedToParse (40414): BSON field 'create.encryptedFields.fields' is missing but a \
         required field"
    );
}

fn encrypted(subtype: BlobSubtype, doc: &Document) -> Value {
    let mut bytes = vec![subtype as u8];
    bytes.extend(doc.to_bytes().expect("bytes"));
    bin(t::BINARY_ENCRYPTED, &bytes)
}

fn equality(seed: u8) -> Value {
    let tokens = Document::new()
        .with("d", bin(t::BINARY_GENERIC, &[seed; 32]))
        .with("s", bin(t::BINARY_GENERIC, &[seed + 1; 32]))
        .with("l", bin(t::BINARY_GENERIC, &[seed + 2; 32]))
        .with("cm", Value::Int64(0));
    encrypted(BlobSubtype::Fle2FindEquality, &tokens)
}

fn range_stub() -> Value {
    encrypted(
        BlobSubtype::Fle2FindRange,
        &Document::new().with("payloadId", Value::Int32(1)).with("firstOperator", Value::Int32(1)),
    )
}

fn field_ref(p: &str) -> Value {
    Value::String(format!("${p}"))
}

#[test]
fn equality_and_range_payloads_inside_text_expressions_are_kept() {
    let cfg = config();
    let mut esc = Esc::default();
    let mut r = Rewriter::new(&cfg, &mut esc);
    for payload in [equality(10), range_stub()] {
        let e = Value::Document(Document::new().with(
            "$encStrStartsWith",
            Document::new().with("input", field_ref("ssn")).with("prefix", payload),
        ));
        assert_eq!(r.expr(&e).expect("kept"), e);
    }
}

#[test]
fn payload_clauses_join_with_and_and_odd_shapes_stay_as_they_are() {
    let cfg = config();
    let mut esc = Esc::default();
    let mut r = Rewriter::new(&cfg, &mut esc);
    // A payload clause and a range stub that asks for nothing: the one clause alone.
    let one = Document::new().with("ssn", equality(10)).with("age", range_stub());
    let out = r.filter(&one).expect("rewrite");
    assert!(out.get(fields::SAFE_CONTENT).is_some(), "{out:?}");
    // Two clauses and nothing else: an `$and` of the clauses alone.
    let both = Document::new()
        .with("ssn", Document::new().with("$eq", equality(10)).with("$ne", equality(10)));
    let out = r.filter(&both).expect("rewrite");
    assert!(matches!(out.get("$and"), Some(Value::Array(a)) if a.len() == 2), "{out:?}");
    // `$not` with more than `$eq` is no payload predicate.
    let inner = Document::new().with("$eq", equality(10)).with("$gt", 1);
    let not = Document::new().with("ssn", Document::new().with("$not", inner));
    assert_eq!(r.filter(&not).expect("kept"), not);
    // An argument with two keys is no `$const`, an `$in` over no field and a non-document `$expr`
    // are ordinary expressions.
    let two_keys = Value::Document(Document::new().with("$const", equality(10)).with("x", 1));
    let e = Value::Document(Document::new().with("$eq", vec![field_ref("ssn"), two_keys]));
    assert_eq!(r.expr(&e).expect("kept"), e);
    let list = Value::Array(vec![equality(10)]);
    let no_field = Value::Document(Document::new().with("$in", vec![Value::Int32(1), list]));
    assert_eq!(r.expr(&no_field).expect("kept"), no_field);
    let plain_expr = Document::new().with("$expr", true);
    assert_eq!(r.filter(&plain_expr).expect("kept"), plain_expr);
}

#[test]
fn a_write_leaves_plain_values_of_encrypted_fields_for_the_implicit_schema() {
    let cfg = config();
    let mut doc = Document::new().with("ssn", "plain text");
    let mut esc = Esc::default();
    let mut overlay = Overlay::new(&mut esc);
    let mut ivs = || [0u8; 16];
    let added = write::transform(&cfg, &mut doc, &mut overlay, &mut ivs).expect("left as it is");
    assert_eq!(added, write::Added::default());
    assert_eq!(doc.get("ssn"), Some(&Value::String("plain text".into())));
}
