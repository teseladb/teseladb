//! An aggregation split over the ranges of several cores (010 FR-034, T064): each part of the
//! input, in key order, runs the program of the split on its own, and merging the partial groups
//! in the order of the parts gives exactly what the pipeline gives over the whole input in one
//! core, the order of the groups, `$first` and `$last` included; a merge that would not give the
//! same values is refused instead.

use teseladb_exec::collation::Collation;
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::{Env, PartialGroups, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions};
use teseladb_bson::{Decimal128, Document, RawDoc, RawValue, Value};

/// A command's environment: no variables, no collation, `limit` bytes per stage and whether it
/// allows writing to disk (it has no store, so it fails past the limit when it does not).
struct TestEnv {
    limit: usize,
    disk: bool,
}

impl Env for TestEnv {
    fn collation(&self) -> Collation {
        Collation::default()
    }

    fn ext(&self) -> &dyn Extensions {
        &NoExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, _name: &str) -> Result<Vec<Document>, MqlError> {
        Ok(Vec::new())
    }

    fn random(&self) -> f64 {
        0.5
    }

    fn allow_disk_use(&self) -> bool {
        self.disk
    }

    fn stage_memory_limit(&self) -> usize {
        self.limit
    }
}

const PLENTY: TestEnv = TestEnv { limit: 100 << 20, disk: true };

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

/// A value whose sums merge exactly: integers of every width and whole doubles, or nothing.
fn whole(rng: &mut Rng) -> Option<Value> {
    Some(match rng.below(6) {
        0 => Value::Int32(rng.below(1000) as i32 - 500),
        1 => Value::Int64(rng.below(1 << 40) as i64 - (1 << 39)),
        2 => Value::Double((rng.below(200) as f64) - 100.0),
        3 => Value::Int32(i32::MAX - rng.below(3) as i32),
        4 => Value::Null,
        _ => return None,
    })
}

/// Any value: fractions, decimals, NaN, huge longs, strings, booleans.
fn any(rng: &mut Rng) -> Option<Value> {
    Some(match rng.below(9) {
        0 => Value::Double(rng.below(9) as f64 / 4.0),
        1 => Value::Decimal128(Decimal128::from_parts(false, rng.below(7) as u128, 1).expect("d")),
        2 => Value::Double(f64::NAN),
        3 => Value::Int64(i64::MAX - rng.below(3) as i64),
        4 => Value::String(["a", "b", "B"][rng.below(3)].into()),
        5 => Value::Boolean(rng.below(2) == 0),
        _ => return whole(rng),
    })
}

fn key(rng: &mut Rng) -> Value {
    match rng.below(7) {
        0 => Value::Int32(1),
        1 => Value::Double(1.0),
        2 => Value::Int64(2),
        3 => Value::String("x".into()),
        4 => Value::Null,
        5 => Value::Document(Document::new().with("a", rng.below(2) as i32)),
        _ => Value::Int32(rng.below(20) as i32),
    }
}

fn docs(rng: &mut Rng, n: usize, value: fn(&mut Rng) -> Option<Value>) -> Vec<Document> {
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

fn parse(stages: &[Document]) -> Pipeline {
    let bytes: Vec<Vec<u8>> = stages.iter().map(|s| s.to_bytes().expect("small")).collect();
    let values: Vec<RawValue<'_>> = bytes
        .iter()
        .map(|b| RawValue::Document(RawDoc::from_bytes(b).expect("a document")))
        .collect();
    Pipeline::parse(&values, &Ctx::checked(std::iter::empty())).expect("a pipeline")
}

fn op(name: &str, arg: impl Into<Value>) -> Value {
    Value::Document(Document::new().with(name, arg.into()))
}

/// The `$group` of every accumulator that merges, by `id`.
fn group_all(id: Value) -> Document {
    let spec = Document::new()
        .with("_id", id)
        .with("s", op("$sum", "$v"))
        .with("one", op("$sum", 1))
        .with("a", op("$avg", "$v"))
        .with("mn", op("$min", "$v"))
        .with("mx", op("$max", "$v"))
        .with("f", op("$first", "$v"))
        .with("l", op("$last", "$v"))
        .with("c", op("$count", Document::new()));
    Document::new().with("$group", spec)
}

fn pipelines() -> Vec<Vec<Document>> {
    let matches = Document::new().with("$match", Document::new().with("w", op("$lt", 3)));
    let by_mod = op("$mod", Value::Array(vec!["$_id".into(), 7.into()]));
    vec![
        vec![group_all("$k".into())],
        vec![matches.clone(), group_all("$k".into())],
        vec![matches.clone(), matches, group_all(Value::Null)],
        vec![
            group_all(by_mod),
            Document::new().with("$sort", Document::new().with("s", -1).with("_id", 1)),
            Document::new().with("$limit", 4),
        ],
    ]
}

/// The stored rows of `docs` cut at random points into consecutive parts, as the ranges of
/// several cores hold them.
fn parts(rng: &mut Rng, docs: &[Document]) -> Vec<Vec<(Vec<u8>, Vec<u8>)>> {
    let n = 1 + rng.below(16);
    let mut cuts: Vec<usize> = (0..n - 1).map(|_| rng.below(docs.len() + 1)).collect();
    cuts.push(0);
    cuts.push(docs.len());
    cuts.sort_unstable();
    cuts.windows(2)
        .map(|w| {
            docs[w[0]..w[1]].iter().map(|d| (Vec::new(), d.to_bytes().expect("small"))).collect()
        })
        .collect()
}

/// The result of the split run over `docs`, or `None` when a part or the merge refused.
fn scattered(
    stages: &[Document],
    rows: &[Vec<(Vec<u8>, Vec<u8>)>],
    env: &TestEnv,
    sbe: bool,
) -> Option<Result<Vec<Document>, MqlError>> {
    let pipeline = parse(stages);
    let split = pipeline.scatter(stages, sbe, env.limit).expect("the pipeline splits");
    let mut partials: Vec<PartialGroups> = Vec::new();
    for part in rows {
        partials.push(split.program().run(part)?);
    }
    split.finish(&pipeline, partials, env).transpose()
}

fn canonical(docs: &[Document]) -> Vec<Vec<u8>> {
    docs.iter().map(|d| d.to_bytes().expect("small")).collect()
}

#[test]
fn whole_numbers_merge_into_the_run_in_one_core() {
    for seed in 1..=60u64 {
        let mut rng = Rng(seed * 0x9E37_79B9);
        let n = 1 + rng.below(400);
        let input = docs(&mut rng, n, whole);
        let rows = parts(&mut rng, &input);
        for stages in pipelines() {
            for sbe in [false, true] {
                let one = parse(&stages).run_in(input.clone(), &PLENTY, sbe).expect("runs");
                let split = scattered(&stages, &rows, &PLENTY, sbe)
                    .expect("whole numbers always merge")
                    .expect("runs");
                assert_eq!(canonical(&split), canonical(&one), "seed {seed} {stages:?} {sbe}");
            }
        }
    }
}

#[test]
fn any_values_merge_into_the_run_in_one_core_or_are_refused() {
    let mut merged = 0;
    for seed in 1..=80u64 {
        let mut rng = Rng(seed * 0xD1B5_4A32);
        let n = 1 + rng.below(300);
        let input = docs(&mut rng, n, any);
        let rows = parts(&mut rng, &input);
        for stages in pipelines() {
            for sbe in [false, true] {
                let one = parse(&stages).run_in(input.clone(), &PLENTY, sbe).expect("runs");
                if let Some(split) = scattered(&stages, &rows, &PLENTY, sbe) {
                    merged += 1;
                    let split = split.expect("runs");
                    assert_eq!(canonical(&split), canonical(&one), "seed {seed} {stages:?}");
                }
            }
        }
    }
    // Inputs without fractions in a group, and parts of one, still merge.
    assert!(merged > 0);
}

#[test]
fn groups_past_the_limit_fail_as_in_one_core_or_are_refused() {
    let mut rng = Rng(7);
    let input = docs(&mut rng, 500, whole);
    // Parts small enough that none passes the limit on its own.
    let rows: Vec<Vec<(Vec<u8>, Vec<u8>)>> = input
        .chunks(20)
        .map(|c| c.iter().map(|d| (Vec::new(), d.to_bytes().expect("small"))).collect())
        .collect();
    let stages = vec![group_all("$_id".into())];
    for sbe in [false, true] {
        // Without disk, the merged groups fail as the run in one core does.
        let strict = TestEnv { limit: 4_000, disk: false };
        let one = parse(&stages).run_in(input.clone(), &strict, sbe);
        let split = scattered(&stages, &rows, &strict, sbe).expect("each part fits");
        match (one, split) {
            (Err(a), Err(b)) => assert_eq!(format!("{a:?}"), format!("{b:?}")),
            (Ok(a), Ok(b)) => assert_eq!(canonical(&a), canonical(&b)),
            (a, b) => panic!("one core {a:?}, split {b:?}"),
        }
    }
    // With disk allowed and no store, the run in one core finishes in memory, and so does the
    // merge.
    let loose = TestEnv { limit: 4_000, disk: true };
    let one = parse(&stages).run_in(input.clone(), &loose, true).expect("runs");
    let split = scattered(&stages, &rows, &loose, true).expect("each part fits");
    assert_eq!(canonical(&split.expect("runs")), canonical(&one));
}

#[test]
fn pipelines_that_read_more_than_the_document_do_not_split() {
    let push = Document::new()
        .with("$group", Document::new().with("_id", "$k").with("p", op("$push", "$v")));
    let root = Document::new()
        .with("$group", Document::new().with("_id", "$k").with("f", op("$first", "$$ROOT")));
    let expr = Document::new().with(
        "$match",
        Document::new().with("$expr", op("$gt", Value::Array(vec!["$w".into(), 1.into()]))),
    );
    let rand = Document::new().with(
        "$group",
        Document::new()
            .with("_id", op("$rand", Document::new()))
            .with("c", op("$count", Document::new())),
    );
    let sort_first = Document::new().with("$sort", Document::new().with("v", 1));
    let cases = vec![
        vec![push],
        vec![root],
        vec![expr, group_all("$k".into())],
        vec![rand],
        vec![sort_first, group_all("$k".into())],
        vec![Document::new().with("$limit", 3)],
    ];
    for stages in cases {
        assert!(parse(&stages).scatter(&stages, true, PLENTY.limit).is_none(), "{stages:?}");
    }
}
