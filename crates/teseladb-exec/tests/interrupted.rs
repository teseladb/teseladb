//! A pipeline that works in memory stops when its environment says so (006 FR-012): before each
//! stage, for each document a `$lookup` joins, also inside the pipeline of a `$lookup` within
//! another, and for each document `$unwind` spreads. Without that, a join of every document with
//! every other ran past its `maxTimeMS` on the node, since it reads its collection once.

use std::cell::Cell;

use teseladb_exec::expr::Ctx;
use teseladb_exec::pipeline::{Env, Pipeline};
use teseladb_mql::{ErrorCode, Extensions, MqlError, NoExtensions};
use teseladb_bson::{Document, RawValue, Value, extjson};

/// RefDb's code of a command past its `maxTimeMS`.
const MAX_TIME_MS_EXPIRED: i32 = 50;

/// A collection of `size` documents that stops the pipeline on its `budget`-th question.
struct Budget {
    size: i64,
    budget: usize,
    asked: Cell<usize>,
}

impl Budget {
    fn new(size: i64, budget: usize) -> Budget {
        Budget { size, budget, asked: Cell::new(0) }
    }
}

impl Env for Budget {
    fn ext(&self) -> &dyn Extensions {
        &NoExtensions
    }

    fn interrupted(&self) -> Result<(), MqlError> {
        self.asked.set(self.asked.get() + 1);
        if self.asked.get() >= self.budget {
            let code = ErrorCode { code: MAX_TIME_MS_EXPIRED, name: "MaxTimeMSExpired" };
            return Err(MqlError::new(code, "operation exceeded time limit"));
        }
        Ok(())
    }

    fn vars(&self) -> Option<&Document> {
        None
    }

    fn collection(&self, _: &str) -> Result<Vec<Document>, MqlError> {
        Ok((0..self.size).map(|n| Document::new().with("_id", n).with("n", n)).collect())
    }

    fn random(&self) -> f64 {
        0.5
    }
}

fn pipeline(text: &str) -> Pipeline {
    let holder = extjson::parse(&format!(r#"{{"p":{text}}}"#)).expect("JSON");
    let bytes = holder.to_bytes().expect("small");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document");
    let Some(RawValue::Array(stages)) = raw.get("p") else { panic!("not a pipeline") };
    let stages: Vec<RawValue<'_>> = stages.iter().collect();
    Pipeline::parse_command(&stages, &Ctx::default(), true).expect("valid")
}

fn input(n: i64) -> Vec<Document> {
    (0..n).map(|i| Document::new().with("_id", i).with("list", vec![Value::Int64(1); 3])).collect()
}

fn code(r: Result<Vec<Document>, MqlError>) -> Option<i32> {
    r.err().map(|e| e.code.code)
}

#[test]
fn a_join_of_every_document_with_every_other_stops_inside_the_lookup() {
    let join = pipeline(r#"[{"$lookup":{"from":"c","pipeline":[],"as":"all"}},{"$count":"n"}]"#);
    let env = Budget::new(50, 10);
    assert_eq!(code(join.run(input(50), &env)), Some(MAX_TIME_MS_EXPIRED));
    assert_eq!(env.asked.get(), 10, "it stops at the question that says so");
    let all = Budget::new(50, usize::MAX);
    assert_eq!(join.run(input(50), &all).expect("runs").len(), 1);
}

#[test]
fn a_lookup_within_a_lookup_asks_the_same_environment() {
    let nested = pipeline(
        r#"[{"$lookup":{"from":"c","as":"x","pipeline":[
            {"$lookup":{"from":"c","pipeline":[{"$count":"n"}],"as":"y"}}]}}]"#,
    );
    // One outer document asks once per stage and once per joined document, inner ones too.
    let env = Budget::new(20, 30);
    assert_eq!(code(nested.run(input(1), &env)), Some(MAX_TIME_MS_EXPIRED));
}

#[test]
fn unwind_and_the_stages_ask_before_they_run() {
    let unwind = pipeline(r#"[{"$unwind":"$list"}]"#);
    let env = Budget::new(0, 3);
    assert_eq!(code(unwind.run(input(5), &env)), Some(MAX_TIME_MS_EXPIRED));
    let free = Budget::new(0, usize::MAX);
    assert_eq!(unwind.run(input(5), &free).expect("runs").len(), 15);
    let stages = pipeline(r#"[{"$match":{}},{"$project":{"_id":1}}]"#);
    assert_eq!(code(stages.run(input(1), &Budget::new(0, 2))), Some(MAX_TIME_MS_EXPIRED));
}

#[test]
fn a_lookup_then_unwind_then_count_stops() {
    let p = pipeline(
        r#"[{"$lookup":{"from":"c","pipeline":[],"as":"all"}},{"$unwind":"$all"},{"$count":"pairs"}]"#,
    );
    let env = Budget::new(100, 5);
    assert_eq!(code(p.run(input(100), &env)), Some(MAX_TIME_MS_EXPIRED));
}
