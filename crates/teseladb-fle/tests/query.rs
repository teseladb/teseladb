//! Queries with find payloads (017 FR-004): each payload becomes the tags its value has in
//! `__safeContent__`, from an ESC built here with known counts per contention factor, in the shapes
//! of a filter (`$elemMatch`) and of an expression (`$or` of `$in`); a predicate without a payload
//! is left as it is.

use std::collections::BTreeMap;

use teseladb_bson::types as t;
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_fle::esc::{EscKeys, EscReader};
use teseladb_fle::fields::{self, EncryptedFieldConfig};
use teseladb_fle::query::Rewriter;
use teseladb_fle::subtype::BlobSubtype;
use teseladb_fle::tokens::{self, Token};
use teseladb_fle::write::WriteError;

fn bin(subtype: u8, bytes: &[u8]) -> Value {
    Value::Binary { subtype, bytes: bytes.to_vec() }
}

fn config() -> EncryptedFieldConfig {
    let field = |key: u8, path: &str, ty: &str, q: Document| {
        Value::Document(
            Document::new()
                .with("keyId", bin(t::BINARY_UUID, &[key; 16]))
                .with("path", path)
                .with("bsonType", ty)
                .with("queries", q),
        )
    };
    let eq = Document::new().with("queryType", "equality").with("contention", Value::Int64(2));
    let range = Document::new()
        .with("queryType", "range")
        .with("contention", Value::Int64(0))
        .with("min", Value::Int32(0))
        .with("max", Value::Int32(255))
        .with("sparsity", Value::Int64(1));
    let prefix = Document::new()
        .with("queryType", "prefix")
        .with("contention", Value::Int64(0))
        .with("strMinQueryLength", 2)
        .with("strMaxQueryLength", 6)
        .with("caseSensitive", true)
        .with("diacriticSensitive", true);
    let ef = Document::new().with(
        "fields",
        vec![
            field(1, "ssn", "string", eq),
            field(2, "age", "int", range),
            field(3, "name", "string", prefix),
        ],
    );
    let b = ef.to_bytes().expect("bytes");
    fields::for_create("c", RawDoc::from_bytes(&b).expect("raw")).expect("valid config")
}

/// An ESC with known counts, or one that fails every read.
#[derive(Default)]
struct Esc {
    docs: BTreeMap<Token, Vec<u8>>,
    down: bool,
}

impl EscReader for Esc {
    type Error = &'static str;
    fn get(&mut self, id: &Token) -> Result<Option<Vec<u8>>, &'static str> {
        if self.down {
            return Err("down");
        }
        Ok(self.docs.get(id).cloned())
    }
}

/// The tokens a driver derives for one value: `EDCDerivedFromDataToken` and
/// `ESCDerivedFromDataToken`.
struct Searched {
    edc: Token,
    esc: Token,
}

fn value(seed: u8) -> Searched {
    Searched { edc: Token([seed; 32]), esc: Token([seed + 1; 32]) }
}

impl Esc {
    /// `count` writes of `v` with contention factor `u`.
    fn written(&mut self, v: &Searched, u: u64, count: u64) {
        let keys = EscKeys::of(&v.esc.derive_u64(u));
        for p in 1..=count {
            self.docs.insert(keys.position_id(p), Vec::new());
        }
    }
}

/// The tags of positions `1..=count` of `v` with contention `u`, as a filter compares them.
fn tags(v: &Searched, u: u64, count: u64) -> Vec<Value> {
    let twice = tokens::edc_twice_derived(&v.edc.derive_u64(u));
    (1..=count).map(|c| bin(t::BINARY_GENERIC, &tokens::tag(&twice, c).0)).collect()
}

fn find_tokens(v: &Searched) -> Document {
    Document::new()
        .with("d", bin(t::BINARY_GENERIC, &v.edc.0))
        .with("s", bin(t::BINARY_GENERIC, &v.esc.0))
        .with("l", bin(t::BINARY_GENERIC, &[0x5e; 32]))
}

fn encrypted(subtype: BlobSubtype, doc: &Document) -> Value {
    let mut bytes = vec![subtype as u8];
    bytes.extend(doc.to_bytes().expect("bytes"));
    bin(t::BINARY_ENCRYPTED, &bytes)
}

fn equality(v: &Searched, cm: i64) -> Value {
    encrypted(BlobSubtype::Fle2FindEquality, &find_tokens(v).with("cm", Value::Int64(cm)))
}

fn elem_match_in(tags: Vec<Value>) -> Document {
    Document::new().with(
        fields::SAFE_CONTENT,
        Document::new().with("$elemMatch", Document::new().with("$in", tags)),
    )
}

fn ssn_esc() -> (Esc, Searched) {
    let v = value(10);
    let mut esc = Esc::default();
    esc.written(&v, 0, 2);
    esc.written(&v, 1, 1);
    (esc, v)
}

#[test]
fn an_equality_payload_becomes_the_tags_of_every_contention_factor() {
    let cfg = config();
    let (mut esc, v) = ssn_esc();
    let out = Rewriter::new(&cfg, &mut esc)
        .filter(&Document::new().with("ssn", equality(&v, 2)))
        .expect("rewrite");
    let mut want = tags(&v, 0, 2);
    want.extend(tags(&v, 1, 1));
    assert_eq!(out, elem_match_in(want));
}

#[test]
fn a_single_tag_is_an_eq_and_plain_predicates_stay_beside_it() {
    let cfg = config();
    let (mut esc, v) = ssn_esc();
    let filter =
        Document::new().with("ssn", equality(&v, 0)).with("plain", 1).with("$comment", "x");
    let out = Rewriter::new(&cfg, &mut esc).filter(&filter).expect("rewrite");
    let Some(Value::Array(all)) = out.get("$and") else { panic!("{out:?}") };
    assert_eq!(all.len(), 2);
    assert_eq!(all[0], Value::Document(elem_match_in(tags(&v, 0, 2))));
    assert_eq!(all[1], Value::Document(Document::new().with("plain", 1).with("$comment", "x")));
    let mut one = Esc::default();
    one.written(&v, 0, 1);
    let out = Rewriter::new(&cfg, &mut one)
        .filter(&Document::new().with("ssn", equality(&v, 0)))
        .expect("rewrite");
    let first = tags(&v, 0, 1).remove(0);
    let eq = Document::new().with(
        fields::SAFE_CONTENT,
        Document::new().with("$elemMatch", Document::new().with("$eq", first)),
    );
    assert_eq!(out, eq);
}

#[test]
fn operators_over_payloads_become_tag_clauses_and_the_rest_is_kept() {
    let cfg = config();
    let (mut esc, v) = ssn_esc();
    let p = equality(&v, 0);
    let ops = Document::new()
        .with("$ne", p.clone())
        .with("$in", vec![p.clone(), p.clone()])
        .with("$not", Document::new().with("$eq", p.clone()))
        .with("$exists", true);
    let out =
        Rewriter::new(&cfg, &mut esc).filter(&Document::new().with("ssn", ops)).expect("rewrite");
    let Some(Value::Array(all)) = out.get("$and") else { panic!("{out:?}") };
    // `$ne`, `$in` and `$not` give three clauses; `$exists` stays on the field.
    assert_eq!(all.len(), 4);
    let text = format!("{out:?}");
    assert!(text.contains("$not") && text.contains("$elemMatch") && text.contains("$exists"));

    // Operators without payloads, an `$in` with a plain value, and fields of plain values are
    // left as they are.
    let plain = Document::new()
        .with("ssn", Document::new().with("$in", vec![p, Value::Int32(1)]).with("$gt", 1))
        .with("other", Document::new().with("x", 1))
        .with("name", Document::new().with("$not", 5).with("$in", 1))
        .with("age", Document::new().with("$not", Document::new().with("$gt", 1).with("$lt", 2)));
    let out = Rewriter::new(&cfg, &mut esc).filter(&plain).expect("rewrite");
    assert_eq!(out, plain);
}

#[test]
fn logical_operators_are_rewritten_inside() {
    let cfg = config();
    let (mut esc, v) = ssn_esc();
    let branch = Value::Document(Document::new().with("ssn", equality(&v, 0)));
    let filter =
        Document::new().with("$or", vec![branch, Value::Int32(3)]).with("$nor", "not an array");
    let out = Rewriter::new(&cfg, &mut esc).filter(&filter).expect("rewrite");
    let Some(Value::Array(or)) = out.get("$or") else { panic!("{out:?}") };
    assert_eq!(or[0], Value::Document(elem_match_in(tags(&v, 0, 2))));
    assert_eq!(or[1], Value::Int32(3));
    assert_eq!(out.get("$nor"), Some(&Value::String("not an array".into())));
}

fn range_payload(edges: &[&Searched], cm: i64, op: i32) -> Value {
    let g: Vec<Value> = edges.iter().map(|e| Value::Document(find_tokens(e))).collect();
    let payload = Document::new().with("g", g).with("cm", Value::Int64(cm));
    encrypted(
        BlobSubtype::Fle2FindRange,
        &Document::new()
            .with("payload", payload)
            .with("payloadId", Value::Int32(1))
            .with("firstOperator", Value::Int32(op)),
    )
}

fn range_stub(op: i32) -> Value {
    encrypted(
        BlobSubtype::Fle2FindRange,
        &Document::new().with("payloadId", Value::Int32(1)).with("firstOperator", Value::Int32(op)),
    )
}

#[test]
fn a_range_query_asks_for_the_tags_of_its_edges_and_its_stub_for_nothing() {
    let cfg = config();
    let (e1, e2) = (value(40), value(50));
    let mut esc = Esc::default();
    esc.written(&e1, 0, 1);
    esc.written(&e2, 0, 2);
    let ops =
        Document::new().with("$gt", range_payload(&[&e1, &e2], 0, 1)).with("$lt", range_stub(3));
    let out =
        Rewriter::new(&cfg, &mut esc).filter(&Document::new().with("age", ops)).expect("rewrite");
    let mut want = tags(&e1, 0, 1);
    want.extend(tags(&e2, 0, 2));
    assert_eq!(out, elem_match_in(want));
}

#[test]
fn a_payload_for_a_query_type_the_field_lacks_is_refused() {
    let cfg = config();
    let (mut esc, v) = ssn_esc();
    let e = Rewriter::new(&cfg, &mut esc)
        .filter(&Document::new().with("age", equality(&v, 0)))
        .expect_err("not indexed");
    let WriteError::Fle(e) = e else { panic!("{e:?}") };
    assert_eq!(e.code, 9_188_707);
    assert_eq!(
        e.message,
        "Encrypted payload for field 'age' was generated for query type 'equality' but the field \
         is not indexed for that query type"
    );
    let e = Rewriter::new(&cfg, &mut esc)
        .filter(&Document::new().with("nothere", range_stub(1)))
        .expect_err("no field");
    assert!(matches!(e, WriteError::Fle(e) if e.code == 9_188_707));
}

#[test]
fn a_failing_esc_fails_the_rewrite() {
    let cfg = config();
    let v = value(10);
    let mut esc = Esc { down: true, ..Esc::default() };
    let e = Rewriter::new(&cfg, &mut esc)
        .filter(&Document::new().with("ssn", equality(&v, 0)))
        .expect_err("down");
    assert!(matches!(e, WriteError::Read("down")));
}

fn expr_in(tags: &[Value]) -> Value {
    let list: Vec<Value> = tags
        .iter()
        .map(|t| {
            Value::Document(Document::new().with(
                "$in",
                vec![
                    Value::Document(Document::new().with("$const", t.clone())),
                    Value::String(format!("${}", fields::SAFE_CONTENT)),
                ],
            ))
        })
        .collect();
    Value::Document(Document::new().with("$or", list))
}

fn konst(v: Value) -> Value {
    Value::Document(Document::new().with("$const", v))
}

fn field_ref(path: &str) -> Value {
    Value::String(format!("${path}"))
}

#[test]
fn a_whole_expr_over_a_payload_becomes_a_filter_clause() {
    let cfg = config();
    let (mut esc, v) = ssn_esc();
    for args in [
        vec![field_ref("ssn"), konst(equality(&v, 0))],
        vec![konst(equality(&v, 0)), field_ref("ssn")],
    ] {
        let filter = Document::new().with("$expr", Document::new().with("$eq", args));
        let out = Rewriter::new(&cfg, &mut esc).filter(&filter).expect("rewrite");
        assert_eq!(out, elem_match_in(tags(&v, 0, 2)));
    }
    let lt = Document::new().with("$lt", vec![field_ref("age"), konst(range_stub(3))]);
    let stub = Document::new().with("$expr", lt);
    assert_eq!(Rewriter::new(&cfg, &mut esc).filter(&stub).expect("rewrite"), Document::new());
}

#[test]
fn expressions_over_payloads_become_an_or_of_in() {
    let cfg = config();
    let (mut esc, v) = ssn_esc();
    let mut r = Rewriter::new(&cfg, &mut esc);
    let want = tags(&v, 0, 2);
    let ne = Value::Document(
        Document::new().with("$ne", vec![field_ref("ssn"), konst(equality(&v, 0))]),
    );
    let negated = Value::Document(Document::new().with("$not", vec![expr_in(&want)]));
    assert_eq!(r.expr(&ne).expect("rewrite"), negated);
    let inn = Value::Document(
        Document::new()
            .with("$in", vec![field_ref("ssn"), Value::Array(vec![konst(equality(&v, 0))])]),
    );
    assert_eq!(r.expr(&inn).expect("rewrite"), expr_in(&want));
    let stub =
        Value::Document(Document::new().with("$gte", vec![field_ref("age"), konst(range_stub(2))]));
    assert_eq!(r.expr(&stub).expect("rewrite"), Value::Boolean(true));
    // A `$ne` as the whole `$expr` stays an expression, rewritten inside.
    let out = r.filter(&Document::new().with("$expr", ne.clone())).expect("rewrite");
    assert_eq!(out.get("$expr"), Some(&negated));
    // Nested and plain expressions are walked and kept.
    let plain = [
        Value::Int32(1),
        Value::Document(Document::new().with("$eq", vec![field_ref("x"), Value::Int32(1)])),
        Value::Document(Document::new().with("$eq", vec![Value::Int32(1), Value::Int32(1)])),
        Value::Document(Document::new().with("$in", vec![Value::Int32(1), Value::Int32(2)])),
        Value::Document(Document::new().with("$in", vec![field_ref("x"), Value::Int32(2)])),
        Value::Document(Document::new().with("$eq", Value::Int32(1))),
        Value::Document(Document::new().with("$add", vec![Value::Int32(1)])),
    ];
    for p in &plain {
        assert_eq!(&r.expr(p).expect("kept"), p);
    }
    let nested = Value::Array(vec![Value::Document(Document::new().with("$and", vec![ne]))]);
    let Value::Array(items) = r.expr(&nested).expect("rewrite") else { panic!("array") };
    assert_eq!(items[0], Value::Document(Document::new().with("$and", vec![negated])));
}

fn text_payload(v: &Searched, kind: &str) -> Value {
    encrypted(
        BlobSubtype::Fle2FindText,
        &Document::new()
            .with("ts", Document::new().with(kind, find_tokens(v)))
            .with("cm", Value::Int64(0))
            .with("cf", Value::Boolean(false))
            .with("df", Value::Boolean(false)),
    )
}

fn text_expr(op: &str, path: &str, key: &str, payload: Value) -> Value {
    Value::Document(
        Document::new().with(op, Document::new().with("input", field_ref(path)).with(key, payload)),
    )
}

#[test]
fn text_expressions_ask_for_the_tags_of_their_payload() {
    let cfg = config();
    let v = value(60);
    let mut esc = Esc::default();
    esc.written(&v, 0, 2);
    let mut r = Rewriter::new(&cfg, &mut esc);
    let want = expr_in(&tags(&v, 0, 2));
    let starts = text_expr("$encStrStartsWith", "name", "prefix", konst(text_payload(&v, "p")));
    assert_eq!(r.expr(&starts).expect("rewrite"), want);
    // The whole string is indexed by every text field.
    let whole = text_expr("$encStrNormalizedEq", "name", "string", text_payload(&v, "e"));
    assert_eq!(r.expr(&whole).expect("rewrite"), want);
    // A suffix query on a field indexed only for prefixes is refused.
    let ends = text_expr("$encStrEndsWith", "name", "suffix", text_payload(&v, "u"));
    assert!(matches!(r.expr(&ends), Err(WriteError::Fle(e)) if e.code == 9_188_707));
    // `$encStrNormalizedEq` on a field with no text query is refused too.
    let on_ssn = text_expr("$encStrNormalizedEq", "ssn", "string", text_payload(&v, "e"));
    assert!(r.expr(&on_ssn).is_err());
}

#[test]
fn odd_text_expressions_and_text_payloads_outside_them_are_kept() {
    let cfg = config();
    let v = value(60);
    let mut esc = Esc::default();
    let mut r = Rewriter::new(&cfg, &mut esc);
    let no_input = Document::new().with("substring", 1);
    let variable = Document::new().with("input", "$$x").with("substring", 1);
    for odd in [
        Document::new().with("$encStrContains", no_input),
        Document::new().with("$encStrContains", variable),
        Document::new().with("$encStrContains", Value::Int32(1)),
    ] {
        let e = Value::Document(odd);
        assert_eq!(r.expr(&e).expect("kept"), e);
    }
    let filter = Document::new().with("name", text_payload(&v, "p"));
    assert_eq!(r.filter(&filter).expect("kept"), filter);
}
