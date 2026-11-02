//! `$group`, `$bucketAuto` and `$setWindowFields` with `allowDiskUse` past their memory limit
//! (010 FR-010, T024): they sort through the node's store and give what they give in memory
//! (`$group` in the order of its keys, as a `refnode` 9.0.4 gives a group that spilled), and
//! leave nothing in the store.

use teseladb_bson::{Document, RawValue, extjson};
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::spill::{Counting, MemorySpill, SpillStore};
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions};

/// The limit the tests give a stage: a few documents.
const SMALL: usize = 2_000;

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

/// 200 documents of about 130 bytes in 50 groups, out of order.
fn docs() -> Vec<Document> {
    (0..200)
        .map(|i: i32| {
            let v = (i * 7919) % 200;
            Document::new()
                .with("_id", i)
                .with("g", v % 50)
                .with("v", v)
                .with("t", if v % 2 == 0 { 1.0 } else { 2.0 })
                .with("s", "x".repeat(100))
        })
        .collect()
}

fn pipeline(text: &str) -> Pipeline {
    let holder = extjson::parse(&format!(r#"{{"p":{text}}}"#)).expect("pipeline");
    let bytes = holder.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    let Some(RawValue::Array(stages)) = raw.get("p") else { panic!("not a pipeline") };
    let stages: Vec<RawValue<'_>> = stages.iter().collect();
    Pipeline::parse(&stages, &Ctx::default()).expect("parses")
}

/// The output in memory and through a store with a small limit, and what the store was given.
fn both(text: &str) -> (Vec<Document>, Vec<Document>, teseladb_exec::pipeline::spill::SpillStats) {
    let p = pipeline(text);
    let memory = p.run(docs(), &SpillEnv { limit: usize::MAX, disk: true, store: None });
    let inner = MemorySpill::default();
    let store = Counting::new(&inner);
    let spilled = p.run(docs(), &SpillEnv { limit: SMALL, disk: true, store: Some(&store) });
    assert!(inner.is_empty(), "the runs are removed");
    (memory.expect("in memory"), spilled.expect("spilled"), store.stats())
}

fn by_id(mut docs: Vec<Document>) -> Vec<Document> {
    docs.sort_by_key(|d| format!("{:?}", d.get("_id")));
    docs
}

#[test]
fn a_group_past_its_limit_sorts_through_the_store_and_gives_the_same_groups() {
    let text = r#"[{"$group":{"_id":"$g","n":{"$sum":1},"p":{"$push":"$s"},"f":{"$first":"$v"},
        "l":{"$last":"$v"},"a":{"$avg":"$t"},"x":{"$addToSet":"$t"}}}]"#;
    let (memory, spilled, stats) = both(text);
    assert_eq!(memory.len(), 50);
    assert_eq!(by_id(memory), by_id(spilled.clone()));
    let ids: Vec<i32> = spilled
        .iter()
        .map(|d| match d.get("_id") {
            Some(teseladb_bson::Value::Int32(n)) => *n,
            other => panic!("an _id {other:?}"),
        })
        .collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "a spilled group comes out in the order of its keys");
    assert!(stats.spills > 1);
    assert_eq!(stats.records, 200);
}

#[test]
fn a_small_group_stays_in_memory_in_first_seen_order() {
    let p = pipeline(r#"[{"$group":{"_id":"$t","n":{"$sum":1}}}]"#);
    let store = MemorySpill::default();
    let env = SpillEnv { limit: SMALL, disk: true, store: Some(&store) };
    let out = p.run(docs(), &env).expect("runs");
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].get("_id"), Some(&teseladb_bson::Value::Double(1.0)));
    assert_eq!(store.written(), 0);
}

#[test]
fn a_group_without_disk_use_fails_past_its_limit() {
    let p = pipeline(r#"[{"$group":{"_id":"$_id","p":{"$push":"$s"}}}]"#);
    let store = MemorySpill::default();
    let e = p
        .run(docs(), &SpillEnv { limit: SMALL, disk: false, store: Some(&store) })
        .expect_err("over the limit");
    assert_eq!(e.code.code, 292);
    assert_eq!(store.written(), 0);
}

#[test]
fn bucket_auto_past_its_limit_sorts_through_the_store() {
    let (memory, spilled, stats) = both(
        r#"[{"$bucketAuto":{"groupBy":"$v","buckets":7,"output":{"n":{"$sum":1},"f":{"$first":"$_id"}}}}]"#,
    );
    assert_eq!(memory.len(), 7);
    assert_eq!(memory, spilled);
    assert!(stats.spills > 1);
}

#[test]
fn set_window_fields_past_its_limit_sorts_through_the_store() {
    let (memory, spilled, stats) = both(
        r#"[{"$setWindowFields":{"partitionBy":"$g","sortBy":{"t":1},"output":{
            "r":{"$sum":"$v","window":{"documents":["unbounded","current"]}},"k":{"$rank":{}}}}}]"#,
    );
    assert_eq!(memory.len(), 200);
    assert_eq!(memory, spilled);
    assert!(stats.spills > 1);
    let (memory, spilled, _) = both(
        r#"[{"$setWindowFields":{"output":{"c":{"$count":{},"window":{"documents":["unbounded","current"]}}}}}]"#,
    );
    assert_eq!(memory, spilled);
}

#[test]
fn set_window_fields_without_disk_use_fails_past_its_limit() {
    let p = pipeline(r#"[{"$setWindowFields":{"sortBy":{"v":1},"output":{"k":{"$rank":{}}}}}]"#);
    let e = p
        .run(docs(), &SpillEnv { limit: SMALL, disk: false, store: None })
        .expect_err("over the limit");
    assert_eq!(e.code.code, 292);
}

#[test]
fn a_failing_store_fails_the_stage() {
    #[derive(Debug)]
    struct Broken;
    impl SpillStore for Broken {
        fn write(&self, _: &[u8]) -> Result<u64, MqlError> {
            Err(MqlError::new(teseladb_mql::codes::INTERNAL_ERROR, "disk full"))
        }
        fn read(&self, _: u64, _: u64, _: usize) -> Result<Vec<u8>, MqlError> {
            Err(MqlError::new(teseladb_mql::codes::INTERNAL_ERROR, "no run"))
        }
        fn remove(&self, _: u64) {}
    }
    for text in [
        r#"[{"$group":{"_id":"$_id","p":{"$push":"$s"}}}]"#,
        r#"[{"$bucketAuto":{"groupBy":"$v","buckets":3}}]"#,
        r#"[{"$setWindowFields":{"partitionBy":"$g","output":{"k":{"$count":{}}}}}]"#,
    ] {
        let e = pipeline(text)
            .run(docs(), &SpillEnv { limit: SMALL, disk: true, store: Some(&Broken) })
            .expect_err("the store fails");
        assert_eq!(e.message, "disk full", "{text}");
    }
}

#[test]
fn sort_by_count_keeps_the_memory_limit_of_its_group() {
    // After `$redact` the classic engine runs it, which counts the groups' keys (the slot-based
    // one counts only collected values, and a count collects none).
    let text = r#"[{"$redact":"$$KEEP"},{"$sortByCount":"$_id"}]"#;
    let e = pipeline(text)
        .run(docs(), &SpillEnv { limit: 100, disk: false, store: None })
        .expect_err("over the limit");
    assert_eq!(e.code.code, 292);
    assert!(e.message.contains("$group"), "{}", e.message);
    let (memory, spilled, stats) = both(r#"[{"$sortByCount":"$s"}]"#);
    assert_eq!(memory, spilled);
    assert_eq!(stats.spills, 0, "one group stays in memory");
    let (memory, spilled, stats) =
        both(r#"[{"$sortByCount":{"$concat":["$s",{"$toString":"$_id"}]}}]"#);
    assert_eq!(by_id(memory), by_id(spilled));
    assert!(stats.spills > 1);
}

#[test]
fn bucket_keeps_the_memory_limit_of_its_group() {
    let text = r#"[{"$bucket":{"groupBy":"$v","boundaries":[0,50,100,150],"default":"rest","output":{"p":{"$push":"$s"},"f":{"$first":"$_id"}}}}]"#;
    let e = pipeline(text)
        .run(docs(), &SpillEnv { limit: SMALL, disk: false, store: None })
        .expect_err("over the limit");
    assert_eq!(e.code.code, 292);
    let (memory, spilled, stats) = both(text);
    assert_eq!(memory.len(), 4);
    assert_eq!(memory, spilled);
    assert!(stats.spills > 1);
}

#[test]
fn without_disk_use_a_store_is_never_used() {
    let store = MemorySpill::default();
    let env = SpillEnv { limit: usize::MAX, disk: false, store: Some(&store) };
    for text in [
        r#"[{"$bucket":{"groupBy":"$v","boundaries":[0,100,200]}}]"#,
        r#"[{"$bucketAuto":{"groupBy":"$v","buckets":3}}]"#,
        r#"[{"$setWindowFields":{"sortBy":{"v":1},"output":{"k":{"$rank":{}}}}}]"#,
    ] {
        assert!(pipeline(text).run(docs(), &env).is_ok(), "{text}");
    }
    assert_eq!(store.written(), 0);
}

#[test]
fn a_value_outside_every_bucket_fails_with_the_code_of_its_engine() {
    let env = SpillEnv { limit: usize::MAX, disk: true, store: None };
    let text = r#"[{"$bucket":{"groupBy":"$v","boundaries":[0,10]}}]"#;
    let sbe = pipeline(text).run(docs(), &env).expect_err("no bucket");
    let classic = pipeline(&format!(r#"[{{"$redact":"$$KEEP"}},{}"#, &text[1..]))
        .run(docs(), &env)
        .expect_err("no bucket");
    assert_ne!(sbe.code.code, classic.code.code);
}
