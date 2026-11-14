//! The program of a split aggregation over column blocks (010 FR-033): a part of the input that
//! comes as stored documents and column blocks, in key order, gives exactly the partial groups,
//! every state bit for bit, that the same documents give as rows; with and without `$match`es,
//! for each accumulator, with doubles, decimals, NaN, mixed types and missing fields.

use teseladb_bson::{Decimal128, Document, RawDoc, RawValue, Value};
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::{Input, Pipeline, Program};
use teseladb_keys::columns::{ColumnsBuilder, ColumnsView};

/// xorshift64*.
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
}

fn value(rng: &mut Rng) -> Option<Value> {
    Some(match rng.below(12) {
        0 => Value::Double(rng.below(9) as f64 / 4.0),
        1 => Value::Decimal128(Decimal128::from_parts(false, rng.below(7) as u128, 1).expect("d")),
        2 => Value::Double(f64::NAN),
        3 => Value::Int64(i64::MAX - rng.below(3) as i64),
        4 => Value::String(["a", "b", "B"][rng.below(3)].into()),
        5 => Value::Boolean(rng.below(2) == 0),
        6 => Value::Null,
        7 => Value::Double(0.1 * rng.below(30) as f64),
        8 => Value::Int32(i32::MAX - rng.below(3) as i32),
        9 => return None,
        _ => Value::Int32(rng.below(1000) as i32 - 500),
    })
}

fn key(rng: &mut Rng) -> Value {
    match rng.below(9) {
        0 => Value::Int32(1),
        1 => Value::Double(1.0),
        2 => Value::Int64(2),
        3 => Value::String("x".into()),
        4 => Value::Null,
        // A document: the block that holds one has no column of `k`.
        5 if rng.below(4) == 0 => Value::Document(Document::new().with("a", 1)),
        _ => Value::Int32(rng.below(20) as i32),
    }
}

fn docs(rng: &mut Rng, n: usize) -> Vec<Document> {
    (0..n)
        .map(|i| {
            let mut d = Document::new().with("_id", i as i32);
            if rng.below(8) != 0 {
                d.insert("k", key(rng));
            }
            if let Some(v) = value(rng) {
                d.insert("v", v);
            }
            d.insert("w", rng.below(4) as i32);
            d
        })
        .collect()
}

fn op(name: &str, arg: impl Into<Value>) -> Value {
    Value::Document(Document::new().with(name, arg.into()))
}

fn group(id: Value, every: bool) -> Document {
    let mut spec = Document::new()
        .with("_id", id)
        .with("s", op("$sum", "$v"))
        .with("one", op("$sum", 1))
        .with("a", op("$avg", "$v"))
        .with("mn", op("$min", "$v"))
        .with("mx", op("$max", "$v"))
        .with("c", op("$count", Document::new()));
    if every {
        spec = spec.with("f", op("$first", "$v")).with("l", op("$last", "$w"));
    }
    Document::new().with("$group", spec)
}

fn pipelines() -> Vec<Vec<Document>> {
    let matches = Document::new().with("$match", Document::new().with("w", op("$lt", 3)));
    let by_mod = op("$mod", Value::Array(vec!["$_id".into(), 7.into()]));
    vec![
        // On the columns of the plan.
        vec![group("$k".into(), false)],
        vec![group(Value::Null, false)],
        vec![group("$k.a".into(), false)],
        // As documents of the fields read.
        vec![group("$k".into(), true)],
        vec![group(by_mod, false)],
        vec![matches.clone(), group("$k".into(), false)],
        vec![matches.clone(), matches, group(Value::Null, true)],
    ]
}

fn program(stages: &[Document]) -> Program {
    let bytes: Vec<Vec<u8>> = stages.iter().map(|s| s.to_bytes().expect("small")).collect();
    let values: Vec<RawValue<'_>> = bytes
        .iter()
        .map(|b| RawValue::Document(RawDoc::from_bytes(b).expect("a document")))
        .collect();
    let pipeline = Pipeline::parse(&values, &Ctx::checked(std::iter::empty())).expect("a pipeline");
    let split = pipeline.scatter(stages, false, 100 << 20).expect("the pipeline splits");
    (**split.program()).clone()
}

/// The column block of `docs`.
fn block(docs: &[Vec<u8>]) -> Vec<u8> {
    let mut b = ColumnsBuilder::default();
    for d in docs {
        b.add(RawDoc::from_bytes(d).unwrap());
    }
    b.finish().expect("documents of one version")
}

#[test]
fn column_blocks_give_the_partial_groups_of_their_rows_bit_for_bit() {
    let mut rng = Rng(0x5eed);
    let mut as_columns = 0;
    for round in 0..150 {
        let n = 1 + rng.below(400);
        let docs = docs(&mut rng, n);
        let stored: Vec<Vec<u8>> = docs.iter().map(|d| d.to_bytes().unwrap()).collect();
        let rows: Vec<(Vec<u8>, Vec<u8>)> =
            stored.iter().map(|b| (Vec::new(), b.clone())).collect();
        // Cut into rows and blocks, as a scan gives them.
        let mut cuts = Vec::new();
        let mut at = 0;
        while at < stored.len() {
            let n = (1 + rng.below(150)).min(stored.len() - at);
            cuts.push((at, at + n, rng.below(4) != 0));
            at += n;
        }
        let blocks: Vec<Option<Vec<u8>>> =
            cuts.iter().map(|&(a, b, col)| col.then(|| block(&stored[a..b]))).collect();
        for stages in pipelines() {
            let p = program(&stages);
            let want = p.columns().expect("the program reads top-level fields");
            let mut inputs = Vec::new();
            for (&(a, b, _), payload) in cuts.iter().zip(&blocks) {
                let view = payload.as_ref().map(|x| ColumnsView::parse(x).unwrap());
                match view.filter(|v| v.has_all(&want)) {
                    Some(v) => {
                        as_columns += 1;
                        inputs.push(Input::Columns(v));
                    }
                    None => inputs.extend(stored[a..b].iter().map(|s| Input::Row(s))),
                }
            }
            let by_rows = p.run(&rows).map(|g| g.encode().expect("groups encode"));
            let by_columns = p.run_inputs(&inputs).map(|g| g.encode().expect("groups encode"));
            assert_eq!(by_columns, by_rows, "round {round}: {stages:?}");
        }
    }
    assert!(as_columns > 500, "{as_columns} blocks went as columns");
}

#[test]
fn a_program_that_reads_more_than_top_level_fields_takes_no_columns() {
    let by_name = Document::new().with(
        "$group",
        Document::new().with("_id", Value::Null).with("s", op("$sum", op("$getField", "v"))),
    );
    let set = Document::new()
        .with("field", "z")
        .with("input", Document::new().with("a", "$k"))
        .with("value", 1);
    let by_set = Document::new().with("$group", Document::new().with("_id", op("$setField", set)));
    for stages in [vec![by_name], vec![by_set]] {
        assert_eq!(program(&stages).columns(), None, "{stages:?}");
    }
    let fields = program(&[group("$k.a".into(), true)]).columns().unwrap();
    assert_eq!(fields, vec!["k".to_owned(), "v".to_owned(), "w".to_owned()]);
}
