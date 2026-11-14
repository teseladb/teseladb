//! The register machine against the tree evaluator (010 FR-031): a table of filters on the edges
//! of RefDb's semantics (mixed numbers, NaN, `null` and missing fields, arrays on the path,
//! collation), then a seeded fuzz of filters and documents. Every filter gives, on every
//! document, the tree evaluator's answer one document at a time and in batches of 64.

use teseladb_bson::{Decimal128, Document, RawDoc, Value};

use super::{DefaultSelectivity, Program, Selectivity};
use crate::ext::NoExtensions;
use crate::matcher::{Eval, Filter, MatchContext};
use crate::parse::{Expr, FilterUse, Pred};
use teseladb_keys::stored::Stored;

/// The tree evaluator's, the machine's and the batch machine's answers for each document.
fn answers(
    filter: &Document,
    docs: &[Vec<u8>],
    ctx: &MatchContext<'_>,
    sel: &dyn Selectivity,
) -> Option<(Vec<bool>, Vec<bool>, Vec<bool>)> {
    let bytes = filter.to_bytes().expect("small");
    let f = Filter::parse(RawDoc::from_bytes(&bytes).expect("valid"), FilterUse::Query).ok()?;
    let root = f.run_tree();
    let program = Program::compile(root, sel);
    let raws: Vec<RawDoc<'_>> =
        docs.iter().map(|d| RawDoc::from_bytes(d).expect("valid")).collect();
    let tree: Vec<bool> =
        raws.iter().map(|d| Eval::new(ctx, false).expr(root, *d).expect("no error")).collect();
    let one: Vec<bool> =
        raws.iter().map(|d| program.run(root, *d, ctx).expect("no error")).collect();
    let plain: Vec<Stored<'_>> = raws.iter().map(|d| Stored::plain(*d)).collect();
    let batch = batches(&program, root, &plain, ctx);
    // The same documents stored with their tables of offsets (003 FR-021) give the same answers.
    let kept: Vec<Vec<u8>> =
        docs.iter().map(|d| teseladb_keys::stored::encode(d.clone())).collect();
    let tabled: Vec<Stored<'_>> =
        kept.iter().map(|k| teseladb_keys::stored::split(k).expect("stored")).collect();
    let one_tabled: Vec<bool> =
        tabled.iter().map(|d| program.run(root, *d, ctx).expect("no error")).collect();
    assert_eq!(one_tabled, one, "one by one, with and without tables");
    assert_eq!(batches(&program, root, &tabled, ctx), batch, "in batches, with and without tables");
    numbered_answers(&program, root, docs, ctx, (&one, &batch));
    Some((tree, one, batch))
}

/// Every name of `d` and of the documents in it, through documents and arrays.
fn all_names(d: RawDoc<'_>, out: &mut Vec<Vec<u8>>) {
    for (k, v) in d.iter() {
        out.push(k.as_bytes().to_vec());
        match v {
            teseladb_bson::RawValue::Document(inner) => all_names(inner, out),
            teseladb_bson::RawValue::Array(inner) => all_names(inner.as_doc(), out),
            _ => {}
        }
    }
}

/// The documents stored with the tables of version 2 (003 FR-022) give `one` and `batch` too:
/// written with a part of the collection's dictionary (the names numbered later have no number in
/// them) and read by a program bound to the whole dictionary, to none, or to another one.
fn numbered_answers(
    program: &Program,
    root: &Expr,
    docs: &[Vec<u8>],
    ctx: &MatchContext<'_>,
    (one, batch): (&[bool], &[bool]),
) {
    use teseladb_keys::fields::FieldNumbers;
    let mut names = Vec::new();
    for d in docs {
        all_names(RawDoc::from_bytes(d).expect("valid"), &mut names);
    }
    let mut whole = FieldNumbers::new(7);
    for n in &names {
        whole.add(n);
    }
    let early = whole.subset(names.iter().step_by(2).map(Vec::as_slice));
    let kept: Vec<Vec<u8>> = docs
        .iter()
        .map(|d| teseladb_keys::stored::encode_numbered(d.clone(), &early, None))
        .collect();
    let tabled: Vec<Stored<'_>> =
        kept.iter().map(|k| teseladb_keys::stored::split(k).expect("stored")).collect();
    let other = FieldNumbers::from_names(8, names.iter().rev().map(Vec::as_slice));
    for reader in [&whole, &early, &FieldNumbers::default(), &other] {
        let mut bound = program.clone();
        bound.bind_fields(reader);
        let got: Vec<bool> =
            tabled.iter().map(|d| bound.run(root, *d, ctx).expect("no error")).collect();
        assert_eq!(got, one, "one by one, numbered tables");
        assert_eq!(batches(&bound, root, &tabled, ctx), batch, "in batches, numbered tables");
    }
}

/// The answers of `program` on `docs` run in batches.
fn batches(
    program: &Program,
    root: &Expr,
    docs: &[Stored<'_>],
    ctx: &MatchContext<'_>,
) -> Vec<bool> {
    let mut batch = Vec::new();
    for chunk in docs.chunks(super::BATCH) {
        let mask = program.run_batch(root, chunk, ctx).expect("no error");
        batch.extend((0..chunk.len()).map(|l| mask >> l & 1 == 1));
    }
    batch
}

fn check(filter: &Document, docs: &[Vec<u8>], ctx: &MatchContext<'_>) {
    for sel in [&DefaultSelectivity as &dyn Selectivity, &Reversed] {
        let Some((tree, one, batch)) = answers(filter, docs, ctx, sel) else { return };
        let bytes = filter.to_bytes().expect("small");
        let parsed = Filter::parse(RawDoc::from_bytes(&bytes).expect("valid"), FilterUse::Query)
            .expect("parsed above");
        assert_eq!(
            Program::compile(parsed.run_tree(), sel).tree_calls(),
            0,
            "only $where and geometry ask the tree: {filter:?}"
        );
        for (i, d) in docs.iter().enumerate() {
            let doc = Document::from(RawDoc::from_bytes(d).expect("valid"));
            assert_eq!(one[i], tree[i], "run: {filter:?} on {doc:?}");
            assert_eq!(batch[i], tree[i], "batch: {filter:?} on {doc:?}");
        }
    }
}

/// An estimate that orders conditions the other way round, to show order does not change the
/// answer.
struct Reversed;

impl Selectivity for Reversed {
    fn fraction(&self, path: &str, pred: &Pred) -> f64 {
        1.0 - DefaultSelectivity.fraction(path, pred)
    }
}

fn d() -> Document {
    Document::new()
}

fn arr(items: Vec<Value>) -> Value {
    Value::Array(items)
}

/// Documents on the edges: each numeric type, NaN, -0.0, null, missing, arrays, nested
/// documents, strings that a collation orders differently.
fn edge_docs() -> Vec<Vec<u8>> {
    let values = vec![
        Value::Int32(5),
        Value::Int64(5),
        Value::Double(5.0),
        Value::Double(5.5),
        Value::Double(f64::NAN),
        Value::Double(-0.0),
        Value::Int32(0),
        Value::Decimal128(Decimal128::from_parts(false, 5, 0).expect("five")),
        Value::Null,
        Value::String("a".into()),
        Value::String("B".into()),
        Value::String("b".into()),
        Value::Boolean(true),
        arr(vec![Value::Int32(1), Value::Int32(5)]),
        arr(vec![]),
        arr(vec![Value::Document(d().with("b", 5)), Value::Document(d().with("b", "x"))]),
        arr(vec![arr(vec![Value::Int32(5)])]),
        Value::Document(d().with("b", 5)),
        Value::Document(d().with("b", Value::Null)),
        Value::Document(d().with("b", arr(vec![Value::Int32(5), Value::Int32(7)]))),
        Value::MinKey,
        Value::MaxKey,
    ];
    let mut out = vec![d().to_bytes().expect("small")];
    for v in values {
        out.push(d().with("a", v.clone()).to_bytes().expect("small"));
        out.push(d().with("x", 1).with("a", v).to_bytes().expect("small"));
    }
    out
}

fn op(name: &str, v: Value) -> Value {
    Value::Document(d().with(name, v))
}

#[test]
fn the_edges_of_the_semantics_match_the_tree() {
    let docs = edge_docs();
    let literals = vec![
        Value::Int32(5),
        Value::Int64(5),
        Value::Double(5.0),
        Value::Double(0.0),
        Value::Double(f64::NAN),
        Value::Null,
        Value::String("b".into()),
        Value::MinKey,
        Value::MaxKey,
        Value::Document(d().with("b", 5)),
        arr(vec![Value::Int32(1), Value::Int32(5)]),
    ];
    let mut filters = Vec::new();
    for path in ["a", "a.b", "a.0", "a.0.b", "x"] {
        for l in &literals {
            filters.push(d().with(path, l.clone()));
            for name in ["$eq", "$ne", "$gt", "$gte", "$lt", "$lte"] {
                filters.push(d().with(path, op(name, l.clone())));
            }
            filters.push(d().with(path, op("$in", arr(vec![l.clone(), Value::Int32(7)]))));
            filters.push(d().with(path, op("$nin", arr(vec![l.clone()]))));
            filters.push(d().with(path, op("$not", op("$gt", l.clone()))));
        }
        filters.push(d().with(path, op("$exists", Value::Boolean(true))));
        filters.push(d().with(path, op("$exists", Value::Boolean(false))));
        filters.push(d().with(path, op("$type", Value::String("number".into()))));
        filters.push(d().with(path, op("$size", Value::Int32(2))));
        filters.push(d().with(path, op("$regex", Value::String("^b".into()))));
        filters.push(d().with(path, op("$not", op("$regex", Value::String("^b".into())))));
        filters.push(d().with(path, op("$mod", arr(vec![Value::Int32(2), Value::Int32(1)]))));
        filters.push(d().with(path, op("$bitsAllSet", Value::Int32(5))));
        filters.push(d().with(path, op("$elemMatch", Value::Document(d().with("$gt", 4)))));
        filters.push(d().with(path, op("$elemMatch", Value::Document(d().with("b", 5)))));
        filters.push(d().with(
            path,
            op("$elemMatch", Value::Document(d().with("$elemMatch", op("$gte", Value::Int32(5))))),
        ));
        filters.push(d().with(path, op("$all", arr(vec![Value::Int32(5), Value::Int32(1)]))));
        filters.push(d().with(
            path,
            op(
                "$in",
                arr(vec![
                    Value::Regex { pattern: "^B".into(), options: "i".into() },
                    Value::Int32(5),
                ]),
            ),
        ));
        filters.push(
            d().with(path, Value::Document(d().with("$gt", 1).with("$lt", 6).with("$ne", 5.5))),
        );
    }
    filters.push(d().with(
        "$or",
        arr(vec![Value::Document(d().with("a", 5)), Value::Document(d().with("x", 1))]),
    ));
    filters.push(d().with(
        "$nor",
        arr(vec![Value::Document(d().with("a", "b")), Value::Document(d().with("a.b", 5))]),
    ));
    filters.push(d().with(
        "$and",
        arr(vec![
            Value::Document(d().with("a", op("$exists", Value::Boolean(true)))),
            Value::Document(d().with("a", op("$gte", Value::Int32(0)))),
        ]),
    ));
    filters.push(d());
    let ext = NoExtensions;
    let plain = MatchContext { collation: None, ext: &ext, vars: None };
    let coll = teseladb_collation::Collation::for_locale("en").expect("en");
    let collated = MatchContext { collation: Some(&coll), ext: &ext, vars: None };
    for f in &filters {
        check(f, &docs, &plain);
        check(f, &docs, &collated);
    }
}

/// A small deterministic generator (xorshift64*).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

fn scalar(rng: &mut Rng) -> Value {
    match rng.below(9) {
        0 => Value::Int32(rng.below(6) as i32 - 1),
        1 => Value::Int64(rng.below(6) as i64 - 1),
        2 => Value::Double(rng.below(12) as f64 / 2.0 - 1.0),
        3 => Value::Null,
        4 => Value::String(rng.pick(&["a", "b", "B", ""]).to_string()),
        5 => Value::Boolean(rng.below(2) == 0),
        6 => Value::Double(f64::NAN),
        7 => Value::MinKey,
        _ => Value::Int32(2),
    }
}

fn value(rng: &mut Rng, depth: u32) -> Value {
    match rng.below(if depth > 2 { 1 } else { 4 }) {
        0 | 1 => scalar(rng),
        2 => Value::Document(doc(rng, depth + 1)),
        _ => arr((0..rng.below(4)).map(|_| value(rng, depth + 1)).collect()),
    }
}

const FIELDS: [&str; 3] = ["a", "b", "0"];

fn doc(rng: &mut Rng, depth: u32) -> Document {
    let mut out = d();
    for f in FIELDS {
        if rng.below(4) != 0 {
            out.insert(f, value(rng, depth));
        }
    }
    out
}

fn path(rng: &mut Rng) -> String {
    let n = 1 + rng.below(3);
    (0..n).map(|_| *rng.pick(&FIELDS)).collect::<Vec<_>>().join(".")
}

fn predicate(rng: &mut Rng) -> Value {
    match rng.below(10) {
        0 => scalar(rng),
        1..=4 => op(rng.pick(&["$eq", "$ne", "$gt", "$gte", "$lt", "$lte"]), scalar(rng)),
        5 => op(rng.pick(&["$in", "$nin"]), arr((0..rng.below(4)).map(|_| scalar(rng)).collect())),
        6 => op("$exists", Value::Boolean(rng.below(2) == 0)),
        7 => op("$not", op(rng.pick(&["$gt", "$lte", "$eq"]), scalar(rng))),
        8 => op("$type", Value::String(rng.pick(&["number", "string", "null"]).to_string())),
        9 if rng.below(2) == 0 => op(
            "$elemMatch",
            Value::Document(d().with("$gte", scalar(rng)).with("$ne", scalar(rng))),
        ),
        9 if rng.below(2) == 0 => op("$size", Value::Int32(rng.below(3) as i32)),
        9 => native(rng),
        _ => Value::Document(d().with("$gte", scalar(rng)).with("$lt", scalar(rng))),
    }
}

/// The predicates that run as one `Pred` instruction.
fn native(rng: &mut Rng) -> Value {
    match rng.below(6) {
        0 => op("$regex", Value::String(rng.pick(&["^a", "b", "^$", "B"]).to_string())),
        1 => op("$mod", arr(vec![Value::Int32(2), Value::Int32(rng.below(2) as i32)])),
        2 => op(rng.pick(&["$bitsAllSet", "$bitsAnyClear"]), Value::Int32(rng.below(4) as i32)),
        3 => op("$elemMatch", Value::Document(d().with(*rng.pick(&FIELDS), scalar(rng)))),
        4 => op(
            "$in",
            arr(vec![Value::Regex { pattern: "^a".into(), options: String::new() }, scalar(rng)]),
        ),
        _ => op("$elemMatch", Value::Document(d().with("$regex", Value::String("a".into())))),
    }
}

fn filter(rng: &mut Rng, depth: u32) -> Document {
    if depth < 2 && rng.below(3) == 0 {
        let name = *rng.pick(&["$and", "$or", "$nor"]);
        let items =
            (0..1 + rng.below(3)).map(|_| Value::Document(filter(rng, depth + 1))).collect();
        return d().with(name, arr(items));
    }
    let mut out = d();
    for _ in 0..1 + rng.below(2) {
        out.insert(path(rng), predicate(rng));
    }
    out
}

#[test]
fn seeded_fuzz_matches_the_tree() {
    let mut rng = Rng(0x5EED_0FF1_17E5);
    let docs: Vec<Vec<u8>> =
        (0..130).map(|_| doc(&mut rng, 0).to_bytes().expect("small")).collect();
    let ext = NoExtensions;
    let plain = MatchContext { collation: None, ext: &ext, vars: None };
    let coll = teseladb_collation::Collation::for_locale("en").expect("en");
    let collated = MatchContext { collation: Some(&coll), ext: &ext, vars: None };
    for i in 0..1500 {
        let f = filter(&mut rng, 0);
        check(&f, &docs, if i % 4 == 0 { &collated } else { &plain });
    }
}

fn compiled(f: &Document) -> (Filter, Program) {
    let bytes = f.to_bytes().expect("small");
    let filter = Filter::parse(RawDoc::from_bytes(&bytes).expect("valid"), FilterUse::Query)
        .expect("a filter");
    let p = Program::compile(filter.run_tree(), &DefaultSelectivity);
    (filter, p)
}

/// The owner's example: `estado` first (an equality), then `total` and `cliente_vip` in a `$or`
/// that yields on the first that holds; a cancelled order runs four instructions.
#[test]
fn the_example_compiles_to_jumps_and_stops_early() {
    let f = d().with(
        "$and",
        arr(vec![
            Value::Document(d().with("estado", "pagado")),
            Value::Document(d().with(
                "$or",
                arr(vec![
                    Value::Document(d().with("total", op("$gt", Value::Int32(500)))),
                    Value::Document(d().with("cliente_vip", true)),
                ]),
            )),
        ]),
    );
    let (filter, p) = compiled(&f);
    let ops: Vec<String> =
        p.listing().iter().map(|l| l.split(' ').next().unwrap_or("").to_owned()).collect();
    assert_eq!(
        ops,
        [
            "LoadField",
            "CmpEqStr",
            "JumpIfFalse",
            "LoadField",
            "CmpI64Imm",
            "JumpIfTrue",
            "LoadField",
            "CmpEqBoolImm",
            "JumpIfFalse",
            "YieldDoc",
            "RejectDoc"
        ],
        "{:#?}",
        p.listing()
    );
    assert!(p.code().len() * 8 + 3 * 32 < 400, "a few hundred bytes");
    let ext = NoExtensions;
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    let run = |doc: Document| {
        let b = doc.to_bytes().expect("small");
        p.run_counted(filter.run_tree(), RawDoc::from_bytes(&b).expect("valid"), &ctx)
            .expect("no error")
    };
    let cancelled = d().with("estado", "cancelado").with("total", 900).with("cliente_vip", true);
    assert_eq!(run(cancelled), (false, 4));
    assert_eq!(run(d().with("estado", "pagado").with("total", 900)), (true, 7));
    assert_eq!(
        run(d().with("estado", "pagado").with("total", 10).with("cliente_vip", true)),
        (true, 10)
    );
    assert_eq!(run(d().with("estado", "pagado").with("total", 10)), (false, 10));
}

#[test]
fn an_array_at_the_end_of_the_path_matches_by_any_element() {
    let (filter, p) = compiled(&d().with("tags", "rust"));
    assert_eq!(p.tree_calls(), 0);
    let ext = NoExtensions;
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    for (doc, want) in [
        (d().with("tags", "rust"), true),
        (
            d().with("tags", arr(vec![Value::String("go".into()), Value::String("rust".into())])),
            true,
        ),
        (d().with("tags", arr(vec![Value::String("go".into())])), false),
        (d().with("tags", arr(vec![arr(vec![Value::String("rust".into())])])), false),
    ] {
        let b = doc.to_bytes().expect("small");
        let raw = RawDoc::from_bytes(&b).expect("valid");
        assert_eq!(p.run(filter.run_tree(), raw, &ctx).expect("no error"), want, "{doc:?}");
        assert_eq!(filter.matches_by_tree(raw, &ctx).expect("no error"), want, "{doc:?}");
    }
}

#[test]
fn elem_match_on_values_loops_over_the_array() {
    let (_, p) = compiled(
        &d().with("a", op("$elemMatch", Value::Document(d().with("$gt", 1).with("$lt", 5)))),
    );
    assert_eq!(p.tree_calls(), 0, "{:#?}", p.listing());
    let text = p.listing().join("\n");
    assert!(text.contains("IterInit") && text.contains("IterNext"), "{text}");
}

#[test]
fn registers_are_16_bytes() {
    assert_eq!(std::mem::size_of::<super::value::ValueRef<'_>>(), 16);
}

#[test]
fn every_instruction_is_64_bits() {
    assert_eq!(std::mem::size_of::<super::Insn>(), 8);
}

#[test]
fn an_error_in_a_batch_is_the_first_document_s() {
    // `$regex` hands its subtree to the tree evaluator; an invalid pattern is refused when the
    // filter is read, so errors here come from the evaluator only through extensions: `$expr`
    // without an engine fails on every document.
    let (filter, p) = compiled(&d().with("$expr", Value::Boolean(true)));
    let doc = d().to_bytes().expect("small");
    let raw = RawDoc::from_bytes(&doc).expect("valid");
    let ext = NoExtensions;
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    let tree = Eval::new(&ctx, false).expr(filter.run_tree(), raw).expect_err("no engine");
    let batch =
        p.run_batch(filter.run_tree(), &[raw.into(), raw.into()], &ctx).expect_err("no engine");
    assert_eq!(batch.code, tree.code);
}

#[test]
fn a_path_through_an_array_walks_on_the_bytes() {
    let (filter, p) = compiled(&d().with("a.b", op("$gt", Value::Int32(4))));
    assert_eq!(p.tree_calls(), 0);
    let ext = NoExtensions;
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    let item = |b: i32| Value::Document(d().with("b", b));
    for (doc, want) in [
        (d().with("a", arr(vec![item(1), item(5)])), true),
        (d().with("a", arr(vec![item(1), arr(vec![item(9)])])), false),
        (
            d().with("a", arr(vec![Value::Document(d().with("b", arr(vec![Value::Int32(7)])))])),
            true,
        ),
        (d().with("a", arr(vec![Value::Int32(9)])), false),
    ] {
        let b = doc.to_bytes().expect("small");
        let raw = RawDoc::from_bytes(&b).expect("valid");
        assert_eq!(p.run(filter.run_tree(), raw, &ctx).expect("no error"), want, "{doc:?}");
        assert_eq!(filter.matches_by_tree(raw, &ctx).expect("no error"), want, "{doc:?}");
    }
}

#[test]
fn regex_mod_bits_and_elem_match_on_documents_run_without_the_tree() {
    for f in [
        d().with("a", op("$regex", Value::String("^x".into()))),
        d().with("a", op("$mod", arr(vec![Value::Int32(3), Value::Int32(0)]))),
        d().with("a", op("$bitsAnySet", Value::Int32(1))),
        d().with(
            "a",
            op(
                "$elemMatch",
                Value::Document(d().with("b", 1).with("c", op("$gt", Value::Int32(2)))),
            ),
        ),
    ] {
        let (_, p) = compiled(&f);
        assert_eq!(p.tree_calls(), 0, "{f:?}: {:#?}", p.listing());
        assert!(p.listing().iter().any(|l| l.starts_with("Pred")), "{f:?}");
    }
}

#[test]
fn where_and_geometry_still_ask_the_tree() {
    let (_, p) = compiled(&d().with("$where", Value::String("true".into())));
    assert_eq!(p.tree_calls(), 1);
    let near = op(
        "$geoWithin",
        Value::Document(d().with(
            "$center",
            arr(vec![arr(vec![Value::Int32(0), Value::Int32(0)]), Value::Int32(1)]),
        )),
    );
    let (_, p) = compiled(&d().with("loc", near));
    assert_eq!(p.tree_calls(), 1);
}

#[test]
fn expr_comparisons_compile_to_register_comparisons() {
    let f = d().with(
        "$expr",
        Value::Document(d().with(
            "$and",
            arr(vec![
                Value::Document(d().with(
                    "$gt",
                    arr(vec![Value::String("$a".into()), Value::String("$b".into())]),
                )),
                Value::Document(
                    d().with("$ne", arr(vec![Value::Int32(3), Value::String("$a".into())])),
                ),
            ]),
        )),
    );
    let (filter, p) = compiled(&f);
    let text = p.listing().join("\n");
    assert!(text.contains("ExprCmp ") && text.contains("ExprCmpLit"), "{text}");
    assert!(!text.contains("Ext ") && p.tree_calls() == 0, "{text}");
    let ext = NoExtensions;
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    for (doc, want) in [
        (d().with("a", 5).with("b", 2), true),
        (d().with("a", 3).with("b", 2), false),
        (d().with("a", 5).with("b", 9), false),
        (d().with("a", 5), true),
        (d().with("b", 5), false),
        (d().with("a", "x").with("b", 5), true),
        (d().with("a", Value::Null), true),
    ] {
        let b = doc.to_bytes().expect("small");
        let raw = RawDoc::from_bytes(&b).expect("valid");
        assert_eq!(p.run(filter.run_tree(), raw, &ctx).expect("no error"), want, "{doc:?}");
        let mask =
            p.run_batch(filter.run_tree(), &[raw.into(), raw.into()], &ctx).expect("no error");
        assert_eq!(mask, if want { 3 } else { 0 }, "{doc:?}");
    }
}

#[test]
fn other_expressions_call_the_extensions_directly() {
    let f = d().with("$expr", Value::Document(d().with("$add", arr(vec![Value::Int32(1)]))));
    let (_, p) = compiled(&f);
    assert_eq!(p.tree_calls(), 0);
    assert!(p.listing().iter().any(|l| l.starts_with("Ext")), "{:#?}", p.listing());
}

#[test]
fn a_cached_program_takes_the_literals_of_each_query() {
    let ext = NoExtensions;
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    let shaped = |n: i64, s: &str| {
        d().with("n", op("$gt", Value::Int64(n)))
            .with("s", Value::String(s.into()))
            .with("t", op("$in", arr(vec![Value::Int32(n as i32), Value::Boolean(true)])))
            .with("e", op("$elemMatch", Value::Document(d().with("$lt", Value::Int64(n)))))
    };
    let docs: Vec<Vec<u8>> = (0..6)
        .map(|k| {
            d().with("n", k)
                .with("s", if k % 2 == 0 { "x" } else { "y" })
                .with("t", k)
                .with("e", arr(vec![Value::Int32(k), Value::Int32(10)]))
                .to_bytes()
                .expect("small")
        })
        .collect();
    let mut first = None;
    for (n, s) in [(1, "x"), (3, "y"), (0, "x"), (4, "x")] {
        let f = shaped(n, s);
        let bytes = f.to_bytes().expect("small");
        let filter = Filter::parse(RawDoc::from_bytes(&bytes).expect("valid"), FilterUse::Query)
            .expect("a filter");
        let key = super::shape_key(filter.run_tree());
        assert_eq!(*first.get_or_insert_with(|| key.clone()), key, "one shape");
        let cached = Program::compile_cached(filter.run_tree(), &DefaultSelectivity, 77);
        let fresh = Program::compile(filter.run_tree(), &DefaultSelectivity);
        assert_eq!(cached.listing(), fresh.listing(), "{f:?}");
        for d in &docs {
            let raw = RawDoc::from_bytes(d).expect("valid");
            assert_eq!(
                cached.run(filter.run_tree(), raw, &ctx).expect("no error"),
                filter.matches_by_tree(raw, &ctx).expect("no error"),
                "{f:?}"
            );
        }
    }
    assert!(Program::cached_programs() >= 1);
}

#[test]
fn shapes_tell_literal_kinds_apart() {
    let key = |f: Document| {
        let bytes = f.to_bytes().expect("small");
        let filter = Filter::parse(RawDoc::from_bytes(&bytes).expect("valid"), FilterUse::Query)
            .expect("a filter");
        super::shape_key(filter.run_tree())
    };
    assert_eq!(key(d().with("a", 1)), key(d().with("a", 2)));
    assert_ne!(key(d().with("a", 1)), key(d().with("a", "1")));
    assert_ne!(key(d().with("a", 1)), key(d().with("a", Value::Int64(1 << 40))));
    assert_ne!(key(d().with("a", 1)), key(d().with("a", Value::Null)));
    assert_ne!(key(d().with("a", 1)), key(d().with("b", 1)));
    assert_ne!(
        key(d().with("a", op("$regex", Value::String("x".into())))),
        key(d().with("a", op("$regex", Value::String("y".into()))))
    );
}

/// The comparisons the batch machine answers with vector kernels (010 FR-030): integers against
/// literals that fit the immediate and against those of the pool, doubles with `-0.0`, NaN and
/// infinities, lanes of other kinds mixed in, give the tree evaluator's answers.
#[test]
fn vector_comparisons_match_the_tree() {
    let numbers = [
        Value::Int32(0),
        Value::Int32(-1),
        Value::Int32(i32::MAX),
        Value::Int32(i32::MIN),
        Value::Int64(i64::MAX),
        Value::Int64(i64::MIN),
        Value::Int64(1 << 40),
        Value::Int64(-(1 << 40)),
        Value::Double(0.0),
        Value::Double(-0.0),
        Value::Double(1.5),
        Value::Double(f64::NAN),
        Value::Double(f64::INFINITY),
        Value::Double(f64::NEG_INFINITY),
        Value::Double((1u64 << 40) as f64),
        Value::Decimal128(Decimal128::from_parts(false, 1, 0).expect("1")),
        Value::Null,
        Value::String("1".into()),
        arr(vec![Value::Int32(3), Value::Double(-0.0)]),
    ];
    let mut rng = Rng(0x0FEC_70B5);
    let docs: Vec<Vec<u8>> = (0..200)
        .map(|_| {
            let mut out = d();
            if rng.below(8) != 0 {
                out.insert("n", rng.pick(&numbers).clone());
            }
            out.to_bytes().expect("small")
        })
        .collect();
    let ext = NoExtensions;
    let ctx = MatchContext { collation: None, ext: &ext, vars: None };
    let literals = [
        Value::Int32(0),
        Value::Int32(-1),
        Value::Int64(i64::MAX),
        Value::Int64(1 << 40),
        Value::Int64(i64::MIN),
        Value::Double(0.0),
        Value::Double(-0.0),
        Value::Double(1.5),
        Value::Double(f64::INFINITY),
        Value::Double((1u64 << 40) as f64),
    ];
    for k in &literals {
        for name in ["$eq", "$gt", "$gte", "$lt", "$lte"] {
            check(&d().with("n", op(name, k.clone())), &docs, &ctx);
        }
    }
}

/// `src` with enough fields to be stored with a table of offsets (003 FR-021), as are the documents
/// embedded in it through documents, and a repeated name at the end: the lookup through the
/// table finds the first one, as the walk does.
fn widen(src: &Document) -> Document {
    let mut out = d();
    for (k, v) in src.iter() {
        let v = match v {
            Value::Document(inner) => Value::Document(widen(inner)),
            other => other.clone(),
        };
        out.push(k, v);
    }
    for i in 0..teseladb_keys::stored::MIN_TABLE_FIELDS {
        out.push(format!("pad{i}"), Value::Int32(i as i32));
    }
    out.push("a", "a repeated name");
    out
}

#[test]
fn seeded_fuzz_on_stored_tables_matches_the_tree() {
    let mut rng = Rng(0x7AB1_E0FF_5E75);
    let docs: Vec<Vec<u8>> =
        (0..130).map(|_| widen(&doc(&mut rng, 0)).to_bytes().expect("small")).collect();
    assert!(docs.iter().all(|b| teseladb_keys::stored::encode(b.clone()).len() > b.len()));
    let ext = NoExtensions;
    let plain = MatchContext { collation: None, ext: &ext, vars: None };
    for _ in 0..1500 {
        check(&filter(&mut rng, 0), &docs, &plain);
    }
}
