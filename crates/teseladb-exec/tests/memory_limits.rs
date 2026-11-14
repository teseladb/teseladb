//! The memory a blocking stage may hold without `allowDiskUse` (010 FR-010, T024): 100 MiB, as a
//! reference router 9.0.4 measured with 110 documents of 1 MiB (292 and its messages); with
//! `allowDiskUse` the stages run.

use teseladb_bson::{Document, RawValue, extjson};
use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{Extensions, MqlError, NoExtensions};

struct BigEnv {
    disk: bool,
}

impl Env for BigEnv {
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
}

fn big() -> Vec<Document> {
    let s = "x".repeat(1 << 20);
    (0..110)
        .map(|i| {
            Document::new().with("_id", i).with("k", 110 - i).with("g", i % 3).with("s", s.clone())
        })
        .collect()
}

fn run(pipeline: &str, disk: bool) -> Result<usize, MqlError> {
    let holder = extjson::parse(&format!(r#"{{"p":{pipeline}}}"#)).expect("pipeline");
    let bytes = holder.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    let Some(RawValue::Array(stages)) = raw.get("p") else { panic!("not a pipeline") };
    let stages: Vec<RawValue<'_>> = stages.iter().collect();
    let p = Pipeline::parse(&stages, &Ctx::default()).expect("parses");
    p.run(big(), &BigEnv { disk }).map(|d| d.len())
}

fn message(pipeline: &str) -> (i32, String) {
    let e = run(pipeline, false).expect_err("over the limit");
    (e.code.code, e.message)
}

const SORT: &str =
    "Sort exceeded memory limit of 104857600 bytes, but did not opt in to external sorting.";

#[test]
fn blocking_stages_refuse_more_than_their_memory_without_disk() {
    assert_eq!(message(r#"[{"$sort":{"k":1}}]"#), (292, SORT.to_owned()));
    assert_eq!(
        message(r#"[{"$bucketAuto":{"groupBy":"$k","buckets":2,"output":{"a":{"$push":"$s"}}}}]"#),
        (292, SORT.to_owned())
    );
    assert_eq!(
        message(r#"[{"$setWindowFields":{"sortBy":{"k":1},"output":{"n":{"$count":{}}}}}]"#),
        (292, SORT.to_owned())
    );
    assert_eq!(
        message(r#"[{"$group":{"_id":"$g","a":{"$push":"$s"}}}]"#),
        (
            292,
            "Exceeded memory limit for group, but didn't allow external spilling; pass allowDiskUse:true to opt in"
                .to_owned()
        )
    );
    assert_eq!(
        message(r#"[{"$unwind":"$_id"},{"$group":{"_id":"$_id","a":{"$first":"$s"}}}]"#),
        (
            292,
            "Exceeded memory limit for $group, but didn't allow external spilling; pass allowDiskUse:true to opt in"
                .to_owned()
        )
    );
}

#[test]
fn what_stays_under_the_limit_or_may_use_disk_runs() {
    // The slot-based engine counts only the arrays of `$push` and `$addToSet`.
    assert_eq!(run(r#"[{"$group":{"_id":"$_id","a":{"$first":"$s"}}}]"#, false), Ok(110));
    assert_eq!(run(r#"[{"$group":{"_id":"$s","n":{"$sum":1}}}]"#, false), Ok(1));
    assert_eq!(run(r#"[{"$project":{"k":1}},{"$sort":{"k":1}}]"#, false), Ok(110));
    assert_eq!(run(r#"[{"$sort":{"k":1}}]"#, true), Ok(110));
    assert_eq!(run(r#"[{"$group":{"_id":"$g","a":{"$push":"$s"}}}]"#, true), Ok(3));
}
