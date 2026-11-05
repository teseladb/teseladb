//! The `queries` workload (010 FR-020, T020): each client turn draws a collection, cuts it into
//! ranges at random places and draws a pipeline from the stages the distributed run splits
//! (streamed stages, a sort with its limit, a group whose accumulators combine, and what follows
//! at the coordinator); the distributed run over the ranges must give what the reference
//! evaluator gives over all the documents at the same mark (`queries-oracle`). The variant
//! `partial_double_count` sums one range's partial group twice.

use teseladb_exec::distribute::run_distributed;
use teseladb_exec::reference::Reference;
use teseladb_bson::{Document, Value};
use teseladb_raftlog::Completions;
use teseladb_types::Nanos;

use crate::checkers::Violation;
use crate::workloads::Workload;
use crate::world::{Event, World};

/// The checker's name.
pub const CHECKER: &str = "queries-oracle";

/// Clients drawing pipelines.
const CLIENTS: u32 = 4;
/// Documents of a drawn collection at most.
const MAX_DOCS: u64 = 40;
/// Ranges a collection is cut into at most.
const MAX_RANGES: u64 = 6;

/// The `queries` workload.
#[derive(Debug, Default)]
pub struct Queries {
    /// Pipelines checked.
    checked: u64,
    /// The first difference found.
    failed: Option<Violation>,
}

impl Queries {
    /// A new workload (it runs no cluster: the ranges are cut from the drawn documents).
    pub fn boxed(_nodes: u32, _cores: u16) -> Box<dyn Workload> {
        Box::new(Queries::default())
    }

    /// One pipeline checked: the distributed run has to give what the reference gives.
    fn judge<T: PartialEq + std::fmt::Debug>(&mut self, stages: &[Document], want: T, got: T) {
        self.checked += 1;
        if want != got {
            self.failed = Some(Violation::safety(
                CHECKER,
                format!("pipeline {stages:?}: distributed {got:?}, reference {want:?}"),
            ));
        }
    }

    fn next_turn(w: &mut World, client: u32) {
        let at = w.now() + Nanos::from_millis(w.rng.range(20, 80));
        w.schedule(at, Event::Client { client });
    }
}

/// One of `items`, drawn.
fn pick<'a>(w: &mut World, items: &[&'a str]) -> &'a str {
    items[w.rng.range(0, items.len() as u64) as usize]
}

/// A collection of drawn documents.
fn documents(w: &mut World) -> Vec<Document> {
    let n = w.rng.range(0, MAX_DOCS + 1);
    (0..n)
        .map(|i| {
            let mut d = Document::new()
                .with("_id", i as i64)
                .with("g", pick(w, &["a", "b", "c"]))
                .with("n", w.rng.range(0, 20) as i32);
            if w.rng.chance(0.8) {
                d.push("x", w.rng.range(0, 1000) as f64 / 8.0);
            }
            d
        })
        .collect()
}

/// A pipeline drawn from the stages the distributed run splits.
fn pipeline(w: &mut World) -> Vec<Document> {
    let stage = |name: &str, v: Value| Document::new().with(name, v);
    let mut out = Vec::new();
    if w.rng.chance(0.5) {
        out.push(stage(
            "$match",
            Value::Document(
                Document::new().with("n", Document::new().with("$gte", w.rng.range(0, 10) as i32)),
            ),
        ));
    }
    if w.rng.chance(0.3) {
        out.push(stage(
            "$set",
            Value::Document(
                Document::new().with(
                    "y",
                    Document::new().with("$mod", vec![Value::from("$n"), Value::Int32(3)]),
                ),
            ),
        ));
    }
    match w.rng.range(0, 3) {
        0 => {
            out.push(stage("$sort", Value::Document(Document::new().with("n", -1).with("_id", 1))));
            if w.rng.chance(0.7) {
                out.push(stage("$limit", Value::Int64(w.rng.range(1, 15) as i64)));
            }
        }
        1 => {
            let acc =
                pick(w, &["$sum", "$min", "$max", "$avg", "$first", "$last", "$push", "$addToSet"]);
            let group = Document::new()
                .with("_id", "$g")
                .with("a", Document::new().with(acc, "$n"))
                .with("c", Document::new().with("$count", Document::new()));
            out.push(stage("$group", Value::Document(group)));
            out.push(stage("$sort", Value::Document(Document::new().with("_id", 1))));
        }
        _ => {
            out.push(stage("$limit", Value::Int64(w.rng.range(0, 50) as i64)));
            out.push(stage("$sort", Value::Document(Document::new().with("_id", 1))));
        }
    }
    out
}

/// `docs` cut into ranges at drawn places, in their order.
fn ranges(w: &mut World, docs: &[Document]) -> Vec<Vec<Document>> {
    let n = w.rng.range(1, MAX_RANGES + 1) as usize;
    let mut marks: Vec<usize> =
        (1..n).map(|_| w.rng.range(0, docs.len() as u64 + 1) as usize).collect();
    marks.sort_unstable();
    let mut out = Vec::new();
    let mut from = 0;
    for m in marks.into_iter().chain([docs.len()]) {
        out.push(docs[from..m.max(from)].to_vec());
        from = m.max(from);
    }
    out
}

/// The documents as one comparable text each, sorted when the pipeline's order is open.
fn shown(docs: &[Document]) -> Vec<String> {
    docs.iter()
        .map(|d| {
            let b = d.to_bytes().unwrap_or_default();
            teseladb_bson::RawDoc::from_bytes(&b)
                .map(teseladb_bson::extjson::to_canonical)
                .unwrap_or_default()
        })
        .collect()
}

impl Workload for Queries {
    fn name(&self) -> &'static str {
        "queries"
    }

    fn cluster(&self) -> bool {
        false
    }

    fn start(&mut self, w: &mut World) {
        for client in 0..CLIENTS {
            Self::next_turn(w, client);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        if self.failed.is_none() {
            let docs = documents(w);
            let stages = pipeline(w);
            let parts = ranges(w, &docs);
            let reference = Reference::new(w.rng.range(0, u64::MAX)).with_collection("c", docs);
            let want = reference.evaluate("c", &stages).map(|d| shown(&d));
            let got = run_distributed(&stages, parts, &reference).map(|d| shown(&d));
            self.judge(&stages, want, got);
        }
        Self::next_turn(w, client);
    }

    fn on_completions(&mut self, _w: &mut World, _node: u32, _core: u16, _c: Completions) {}

    fn on_crash(&mut self, _w: &mut World, _node: u32) {}

    fn on_restart(&mut self, _w: &mut World, _node: u32) -> Result<(), Violation> {
        self.failed.clone().map_or(Ok(()), Err)
    }

    fn on_heal(&mut self, _w: &mut World) {}

    fn finish(&mut self, _w: &mut World) -> Result<(), Violation> {
        if let Some(v) = self.failed.clone() {
            return Err(v);
        }
        if self.checked == 0 {
            return Err(Violation::liveness(CHECKER, "no pipeline was checked"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_distributed_answer_unlike_the_reference_fails_the_run() {
        let mut w = World::new(1, 1, 1, false);
        let mut q = Queries::default();
        assert!(q.finish(&mut w).expect_err("nothing checked").liveness);
        q.judge(&[], vec!["a"], vec!["a"]);
        q.finish(&mut w).expect("the same answer");
        q.judge(&[Document::new().with("$limit", 1)], vec!["a"], vec!["b"]);
        let v = q.on_restart(&mut w, 0).expect_err("kept");
        assert_eq!(v.checker, CHECKER);
        // After a difference no more pipelines are drawn.
        q.on_client(&mut w, 0);
        assert_eq!(q.checked, 2);
        assert_eq!(q.finish(&mut w).expect_err("failed").detail, v.detail);
    }
}
