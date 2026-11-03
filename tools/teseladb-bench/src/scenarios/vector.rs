//! `vector-search` scenario (011 T032, SC-005): `$vectorSearch` over `--records` vectors of 768
//! dimensions (a million for SC-005) against `--target` or a local cluster of three nodes. The
//! vectors come from a fixed seed; a `vectorSearch` index with cosine similarity is built before
//! the load, then each query asks for the 10 nearest with `numCandidates` = 100 and its recall is
//! measured against the exact 10 nearest computed here. SC-005 asks for a recall@10 of at least
//! 0.95 and a p50 under 20 ms.

use std::time::{Duration, Instant};

use teseladb_bson::{Document, Value};
use teseladb_types::Histogram;

use super::ycsb_gen::Rng;
use crate::cli::Args;
use crate::client::{Client, Target, doc};
use crate::report::{Report, Target as Goal};

const DB: &str = "bench";
const COLLECTION: &str = "vectors";
/// Dimensions of each vector (SC-005).
pub const DIMENSIONS: usize = 768;
/// Results of a query and candidates it asks for (`numCandidates` = 10 × `limit`).
const LIMIT: usize = 10;
const CANDIDATES: i32 = 100;
/// SC-005: recall@10 at least 0.95, p50 under 20 ms.
const RECALL: f64 = 0.95;
const P50_NS: f64 = 20_000_000.0;
/// Documents per `insert`.
const BATCH: usize = 500;
/// Queries measured at least, whatever the duration.
const MIN_QUERIES: u64 = 50;
/// Longest wait for the index to be `READY`.
const INDEX_READY: Duration = Duration::from_secs(3_600);
const SEED: u64 = 0x5645_4354;

/// Vector `i` of the corpus: components drawn from the seed.
pub fn vector(seed: u64, i: u64) -> Vec<f64> {
    let mut rng = Rng::new(seed ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    (0..DIMENSIONS).map(|_| rng.below(2_001) as f64 / 1_000.0 - 1.0).collect()
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    dot / (na * nb)
}

/// The `_id`s of the `LIMIT` vectors of the corpus nearest to `q` by cosine, best first.
pub fn exact(corpus: &[Vec<f64>], q: &[f64]) -> Vec<i64> {
    let mut all: Vec<(f64, i64)> =
        corpus.iter().enumerate().map(|(i, v)| (cosine(v, q), i as i64)).collect();
    all.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    all.into_iter().take(LIMIT).map(|(_, i)| i).collect()
}

/// The share of `want` that `got` holds.
pub fn recall(got: &[i64], want: &[i64]) -> f64 {
    if want.is_empty() {
        return 1.0;
    }
    want.iter().filter(|w| got.contains(w)).count() as f64 / want.len() as f64
}

fn array(v: &[f64]) -> Value {
    Value::Array(v.iter().map(|x| Value::Double(*x)).collect())
}

fn load(c: &mut Client, corpus: &[Vec<f64>]) -> Result<(), String> {
    for (n, chunk) in corpus.chunks(BATCH).enumerate() {
        let docs = chunk
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let id = (n * BATCH + i) as i64;
                Value::Document(doc(vec![("_id", Value::Int64(id)), ("v", array(v))]))
            })
            .collect();
        let insert = doc(vec![
            ("insert", Value::String(COLLECTION.into())),
            ("documents", Value::Array(docs)),
        ]);
        crate::client::check(&c.command(DB, insert)?)?;
    }
    Ok(())
}

fn index(c: &mut Client) -> Result<(), String> {
    let field = doc(vec![
        ("type", Value::String("vector".into())),
        ("path", Value::String("v".into())),
        ("numDimensions", Value::Int32(DIMENSIONS as i32)),
        ("similarity", Value::String("cosine".into())),
    ]);
    let definition = doc(vec![("fields", Value::Array(vec![Value::Document(field)]))]);
    let spec = doc(vec![
        ("name", Value::String("vectors".into())),
        ("type", Value::String("vectorSearch".into())),
        ("definition", Value::Document(definition)),
    ]);
    let create = doc(vec![
        ("createSearchIndexes", Value::String(COLLECTION.into())),
        ("indexes", Value::Array(vec![Value::Document(spec)])),
    ]);
    crate::client::check(&c.command(DB, create)?)?;
    super::search::wait_ready(c, DB, COLLECTION, INDEX_READY)
}

fn query(c: &mut Client, q: &[f64]) -> Result<Vec<i64>, String> {
    let stage = doc(vec![
        ("index", Value::String("vectors".into())),
        ("path", Value::String("v".into())),
        ("queryVector", array(q)),
        ("numCandidates", Value::Int32(CANDIDATES)),
        ("limit", Value::Int32(LIMIT as i32)),
    ]);
    let project = doc(vec![("$project", Value::Document(doc(vec![("_id", Value::Int32(1))])))]);
    let aggregate = doc(vec![
        ("aggregate", Value::String(COLLECTION.into())),
        (
            "pipeline",
            Value::Array(vec![
                Value::Document(doc(vec![("$vectorSearch", Value::Document(stage))])),
                Value::Document(project),
            ]),
        ),
        ("cursor", Value::Document(Document::new())),
    ]);
    super::search::ids(&c.command(DB, aggregate)?)
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    let _ = c.command(DB, doc(vec![("drop", Value::String(COLLECTION.into()))]));
    let corpus: Vec<Vec<f64>> = (0..args.records).map(|i| vector(SEED, i)).collect();
    load(&mut c, &corpus)?;
    index(&mut c)?;
    let mut latency = Histogram::new();
    let (mut found, mut queries) = (0.0, 0u64);
    let mut n = 0u64;
    super::warm_up(args.warmup, || {
        n += 1;
        query(&mut c, &vector(SEED.rotate_left(17), n)).map(drop)
    })?;
    let start = Instant::now();
    while start.elapsed() < args.duration || queries < MIN_QUERIES {
        n += 1;
        let q = vector(SEED.rotate_left(17), n);
        let asked = Instant::now();
        let got = query(&mut c, &q)?;
        let took = asked.elapsed();
        latency.record(took.as_nanos() as u64);
        found += recall(&got, &exact(&corpus, &q));
        queries += 1;
    }
    let mut report = Report::new("vector-search");
    report.latency("query", &latency, Some(P50_NS), None);
    report.push("recall@10", "ratio", found / queries as f64, Some(Goal::AtLeast(RECALL)));
    report.notes.push(format!(
        "{} vectors of {DIMENSIONS} dimensions, cosine, numCandidates {CANDIDATES}, limit {LIMIT}, \
         target {}",
        args.records,
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_exact_neighbours_and_the_recall_follow_the_corpus() {
        let corpus: Vec<Vec<f64>> = (0..30).map(|i| vector(SEED, i)).collect();
        assert_eq!(corpus[0].len(), DIMENSIONS);
        assert_eq!(vector(SEED, 3), corpus[3]);
        let nearest = exact(&corpus, &corpus[7]);
        assert_eq!((nearest.len(), nearest[0]), (LIMIT, 7));
        assert_eq!(recall(&nearest, &nearest), 1.0);
        assert_eq!(recall(&nearest[..5], &nearest), 0.5);
        assert_eq!(recall(&[], &[]), 1.0);
    }
}
