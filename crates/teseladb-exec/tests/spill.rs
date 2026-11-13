//! The blocking stages taking their input a batch at a time (010 FR-028): `$sort`, `$sort` with
//! `$limit`, `$sortByCount`, `$bucket`, `$bucketAuto` and the pipelines of `$facet` give, after
//! stages that change or filter each document, what they give over the whole of those stages'
//! output; past their limit they write to the store as the batches come and give the same
//! documents, and without `allowDiskUse` they fail with 292 as soon as they hold more than their
//! limit, before the stages before them read the rest of the input, as in RefDb.

use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::spill::{Counting, MemorySpill, SpillStore};
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions};
use teseladb_bson::{Document, RawValue, Value, extjson};

/// The documents of the input: more than three batches.
const DOCS: i32 = 3_500;

/// The limit the tests give a stage: a few hundred documents.
const SMALL: usize = 40_000;

struct SpillEnv<'a> {
    limit: usize,
    disk: bool,
    store: Option<&'a dyn SpillStore>,
}

impl Env for SpillEnv<'_> {
    fn ext(&self) -> &dyn Extensions {
        &NoExtensions
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, _: &str) -> Result<Vec<Document>, MqlError> {
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

    fn spill(&self) -> Option<&dyn SpillStore> {
        self.store
    }
}

/// Documents of about 100 bytes, out of order, with a divisor that is 0 only in the last.
fn docs() -> Vec<Document> {
    (0..DOCS)
        .map(|i| {
            let v = (i * 7919) % DOCS;
            Document::new()
                .with("_id", i)
                .with("g", v % 37)
                .with("v", v)
                .with("t", if v % 3 == 0 { 1.5 } else { 2.0 })
                .with("z", if i == DOCS - 1 { 0 } else { 1 })
                .with("s", "x".repeat(40))
        })
        .collect()
}

fn pipeline(text: &str) -> Pipeline {
    parsed(&extjson::parse(&format!(r#"{{"p":{text}}}"#)).expect("pipeline"))
}

/// The pipeline in field `p` of `holder`.
fn parsed(holder: &Document) -> Pipeline {
    let bytes = holder.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    let Some(RawValue::Array(stages)) = raw.get("p") else { panic!("not a pipeline") };
    let stages: Vec<RawValue<'_>> = stages.iter().collect();
    Pipeline::parse(&stages, &Ctx::default()).expect("parses")
}

/// The stages of `text` run one at a time, each over the whole output of the one before.
fn one_by_one(text: &str, env: &dyn Env) -> Vec<Document> {
    let holder = extjson::parse(&format!(r#"{{"p":{text}}}"#)).expect("pipeline");
    let Some(Value::Array(stages)) = holder.get("p") else { panic!("not a pipeline") };
    stages.iter().fold(docs(), |docs, st| {
        let one = Document::new().with("p", Value::Array(vec![st.clone()]));
        parsed(&one).run(docs, env).expect("runs")
    })
}

/// The output with no limit, and through a store with a small limit with what it was given.
fn both(text: &str) -> (Vec<Document>, Vec<Document>, teseladb_exec::pipeline::spill::SpillStats) {
    let p = pipeline(text);
    let memory = p.run(docs(), &SpillEnv { limit: usize::MAX, disk: true, store: None });
    let inner = MemorySpill::default();
    let store = Counting::new(&inner);
    let spilled = p.run(docs(), &SpillEnv { limit: SMALL, disk: true, store: Some(&store) });
    assert!(inner.is_empty(), "the runs are removed");
    (memory.expect("in memory"), spilled.expect("spilled"), store.stats())
}

/// The prefix of stages that change or filter each document before the blocking stage.
const HEAD: &str = r#"{"$match":{"v":{"$ne":5}}},{"$addFields":{"w":{"$divide":["$v","$z"]}}}"#;

#[test]
fn blocking_stages_after_batches_give_what_they_give_over_the_whole_input() {
    let env = SpillEnv { limit: usize::MAX, disk: true, store: None };
    let tails = [
        r#"{"$sort":{"t":1,"g":-1}}"#,
        r#"{"$sort":{"g":1}},{"$limit":25}"#,
        r#"{"$sortByCount":"$g"}"#,
        r#"{"$bucket":{"groupBy":"$v","boundaries":[0,100,1000,3000],"default":"rest",
            "output":{"n":{"$sum":1},"f":{"$first":"$_id"}}}}"#,
        r#"{"$bucketAuto":{"groupBy":"$v","buckets":7}}"#,
        r#"{"$facet":{"a":[{"$match":{"g":3}},{"$sort":{"v":-1}},{"$limit":4}],
            "b":[{"$group":{"_id":"$t","n":{"$sum":1}}},{"$sort":{"_id":1}}],"c":[{"$count":"n"}]}}"#,
    ];
    for tail in tails {
        let text = format!("[{HEAD},{tail},{{\"$project\":{{\"s\":0}}}}]");
        // Without the divisor 0 of the last document, which stops every run.
        let text = text.replace(r#""$z""#, "1");
        let batched = pipeline(&text).run(docs(), &env).expect("runs");
        assert!(!batched.is_empty(), "{tail}");
        assert_eq!(batched, one_by_one(&text, &env), "{tail}");
    }
}

#[test]
fn a_sort_past_its_limit_writes_runs_as_the_batches_come_and_gives_the_same_order() {
    let text = format!(r#"[{},{{"$sort":{{"t":-1,"v":1}}}}]"#, HEAD.replace(r#""$z""#, "1"));
    let (memory, spilled, stats) = both(&text);
    assert_eq!(memory.len(), usize::try_from(DOCS - 1).expect("small"));
    assert_eq!(memory, spilled);
    assert!(stats.spills > 3, "{stats:?}");
    assert_eq!(stats.records, u64::try_from(DOCS - 1).expect("small"));
}

#[test]
fn buckets_and_counts_past_their_limit_go_through_the_store() {
    let bucket = r#"[{"$bucket":{"groupBy":"$v","boundaries":[0,1000,2000,4000],
        "output":{"p":{"$push":"$s"},"n":{"$sum":1}}}}]"#;
    let (memory, spilled, stats) = both(bucket);
    assert_eq!(memory.len(), 3);
    assert_eq!(memory, spilled);
    assert!(stats.spills > 1, "{stats:?}");
    let auto = r#"[{"$bucketAuto":{"groupBy":"$v","buckets":5,"output":{"n":{"$sum":1}}}}]"#;
    let (memory, spilled, stats) = both(auto);
    assert_eq!(memory, spilled);
    assert!(stats.spills > 1, "{stats:?}");
    let counts = r#"[{"$sortByCount":{"$concat":["$s",{"$toString":"$v"}]}}]"#;
    let (memory, spilled, stats) = both(counts);
    let ids = |docs: &[Document]| {
        let mut ids: Vec<String> = docs.iter().map(|d| format!("{:?}", d.get("_id"))).collect();
        ids.sort();
        ids
    };
    assert_eq!(memory.len(), usize::try_from(DOCS).expect("small"));
    assert_eq!(ids(&memory), ids(&spilled));
    assert!(stats.spills > 1, "{stats:?}");
}

#[test]
fn the_pipelines_of_a_facet_each_take_every_batch_and_spill_on_their_own() {
    let text = r#"[{"$facet":{"up":[{"$sort":{"v":1}},{"$project":{"v":1}}],
        "down":[{"$sort":{"v":-1}},{"$skip":10},{"$project":{"v":1}}]}}]"#;
    let (memory, spilled, stats) = both(text);
    assert_eq!(memory, spilled);
    assert!(stats.spills > 2, "{stats:?}");
    assert_eq!(stats.records, 2 * u64::try_from(DOCS).expect("small"));
}

#[test]
fn without_disk_a_sort_fails_with_292_once_it_holds_its_limit_before_the_input_ends() {
    // The last document fails the `$addFields`; the stages after it fail first, past their
    // limit, as they take the batches before it.
    for tail in [
        r#"{"$sort":{"v":1}}"#,
        r#"{"$bucketAuto":{"groupBy":"$v","buckets":3}}"#,
        r#"{"$facet":{"a":[{"$sort":{"v":1}}]}}"#,
    ] {
        let p = pipeline(&format!("[{HEAD},{tail}]"));
        let env = SpillEnv { limit: SMALL, disk: false, store: None };
        let e = p.run(docs(), &env).expect_err("past the limit");
        assert_eq!(e.code.code, 292, "{tail}: {}", e.message);
        // With room, the run reaches the last document and its error.
        let roomy = SpillEnv { limit: usize::MAX, disk: false, store: None };
        let e = p.run(docs(), &roomy).expect_err("the divisor 0");
        assert_ne!(e.code.code, 292, "{tail}");
    }
}

#[test]
fn with_disk_a_sort_wrote_runs_before_the_stage_before_it_failed() {
    let p = pipeline(&format!(r#"[{HEAD},{{"$sort":{{"v":1}}}}]"#));
    let store = MemorySpill::default();
    let env = SpillEnv { limit: SMALL, disk: true, store: Some(&store) };
    let e = p.run(docs(), &env).expect_err("the divisor 0");
    assert_ne!(e.code.code, 292);
    assert!(store.written() > 1, "{}", store.written());
    assert!(store.is_empty(), "the runs are removed on the error");
}

#[test]
fn a_top_k_holds_only_the_documents_it_keeps() {
    let p = pipeline(r#"[{"$sort":{"v":-1}},{"$limit":10}]"#);
    let env = SpillEnv { limit: SMALL, disk: false, store: None };
    let out = p.run(docs(), &env).expect("ten documents are under the limit");
    let first: Vec<Option<&Value>> = out.iter().map(|d| d.get("v")).collect();
    assert_eq!(first.len(), 10);
    assert_eq!(first[0], Some(&Value::Int32(DOCS - 1)));
    let wide = pipeline(r#"[{"$sort":{"v":-1}},{"$limit":2000}]"#);
    let e = wide.run(docs(), &env).expect_err("two thousand are not");
    assert_eq!(e.code.code, 292);
}
