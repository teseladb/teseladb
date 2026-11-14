//! `reshard` scenario (018 FR-013, SC-002): a `reshardCollection` of a loaded collection while
//! writers keep updating it, against `--target` or a local cluster of three nodes.
//!
//! The collection `bench.reshard` is sharded by `{a: 1}` and loaded with `--records` documents of
//! [`PAYLOAD`] bytes each (100 GB are about 100 million). `--concurrency` writers then set field
//! `v` of their own documents (writer `w` takes the `_id`s equal to `w` modulo the writers) to
//! a counter, noting the last value each one acknowledged; meanwhile the collection is resharded
//! by `{b: 1}`. Once it ends and the writers stop, every document must hold the last value its
//! writer was told was written (`lost` 0), and the longest write while the resharding ran is the
//! cutover, which SC-002 wants under 2 s.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_bson::{Document, RawDoc, RawValue, Value};

use crate::cli::Args;
use crate::client::{Client, Target as Server, check, doc};
use crate::report::{Report, Target};

const DB: &str = "bench";
const COLLECTION: &str = "reshard";
/// Bytes of each document's filler.
const PAYLOAD: usize = 1024;
/// Documents per insert while loading.
const LOAD_BATCH: u64 = 500;
/// SC-002: the cutover under 2 s.
const CUTOVER_MS: f64 = 2_000.0;
/// The chunks a resharding asks for with enough documents (a reference router asks 90).
const CHUNKS: u64 = 90;
/// Documents per chunk the cardinality check of a resharding needs.
const DOCS_PER_CHUNK: u64 = 10;
/// Documents read per `find` while checking.
const CHECK_BATCH: i32 = 10_000;

/// The last value each writer had acknowledged, by `_id`.
type Acked = Arc<Mutex<BTreeMap<i64, i64>>>;

/// The `b` of document `id` of `n`: a permutation, so the new key has the cardinality of `_id`.
fn b_of(id: u64, n: u64) -> i64 {
    i64::try_from(id.wrapping_mul(7_919) % n.max(1)).unwrap_or(i64::MAX)
}

fn int(id: u64) -> Value {
    Value::Int64(i64::try_from(id).unwrap_or(i64::MAX))
}

/// Creates and loads the collection.
fn load(c: &mut Client, n: u64) -> Result<(), String> {
    // The collection may not exist yet.
    let _ = c.command(DB, doc(vec![("drop", Value::String(COLLECTION.into()))]));
    let ns = format!("{DB}.{COLLECTION}");
    let key = doc(vec![("a", Value::Int32(1))]);
    c.until_ready(
        "admin",
        doc(vec![("shardCollection", Value::String(ns)), ("key", Value::Document(key))]),
    )?;
    let filler = "x".repeat(PAYLOAD);
    let mut next = 0u64;
    while next < n {
        let end = (next + LOAD_BATCH).min(n);
        let docs = (next..end)
            .map(|i| {
                Value::Document(doc(vec![
                    ("_id", int(i)),
                    ("a", int(i)),
                    ("b", Value::Int64(b_of(i, n))),
                    ("v", Value::Int64(0)),
                    ("p", Value::String(filler.clone())),
                ]))
            })
            .collect();
        let insert = doc(vec![
            ("insert", Value::String(COLLECTION.into())),
            ("documents", Value::Array(docs)),
        ]);
        check(&c.command(DB, insert)?)?;
        next = end;
    }
    Ok(())
}

/// One writer: sets `v` of its documents to a counter until `stop`; the longest write after
/// `started` was set, in milliseconds.
fn write(
    mut c: Client,
    (w, writers, n): (u64, u64, u64),
    acked: &Acked,
    (started, stop): (&AtomicBool, &AtomicBool),
) -> Result<(u64, f64), String> {
    let (mut count, mut longest, mut value) = (0u64, 0f64, 0i64);
    let mut id = w;
    while !stop.load(Ordering::Relaxed) {
        value += 1;
        let update = doc(vec![
            ("q", Value::Document(doc(vec![("_id", int(id))]))),
            (
                "u",
                Value::Document(doc(vec![(
                    "$set",
                    Value::Document(doc(vec![("v", Value::Int64(value))])),
                )])),
            ),
        ]);
        let cmd = doc(vec![
            ("update", Value::String(COLLECTION.into())),
            ("updates", Value::Array(vec![Value::Document(update)])),
        ]);
        let at = Instant::now();
        check(&c.command(DB, cmd)?)?;
        let took = at.elapsed().as_secs_f64() * 1_000.0;
        if started.load(Ordering::Relaxed) {
            longest = longest.max(took);
        }
        acked
            .lock()
            .map_err(|_| "a writer panicked")?
            .insert(i64::try_from(id).unwrap_or(0), value);
        count += 1;
        id = if id + writers < n { id + writers } else { w };
    }
    Ok((count, longest))
}

/// The `v` of every document, by `_id`.
fn values(c: &mut Client) -> Result<BTreeMap<i64, i64>, String> {
    let mut out = BTreeMap::new();
    let find = doc(vec![
        ("find", Value::String(COLLECTION.into())),
        ("projection", Value::Document(doc(vec![("v", Value::Int32(1))]))),
        ("batchSize", Value::Int32(CHECK_BATCH)),
    ]);
    let mut reply = c.command(DB, find)?;
    loop {
        check(&reply)?;
        let Some(id) = batch_values(&reply, &mut out)? else { return Ok(out) };
        let more = doc(vec![
            ("getMore", Value::Int64(id)),
            ("collection", Value::String(COLLECTION.into())),
            ("batchSize", Value::Int32(CHECK_BATCH)),
        ]);
        reply = c.command(DB, more)?;
    }
}

/// Adds the `v` of each document of a cursor's batch in `reply` to `out`, by `_id`; the
/// cursor's id when there is more to read.
fn batch_values(reply: &[u8], out: &mut BTreeMap<i64, i64>) -> Result<Option<i64>, String> {
    let d = RawDoc::from_bytes(reply).map_err(crate::client::debug)?;
    let Some(RawValue::Document(cursor)) = d.get("cursor") else {
        return Err(format!("a reply without a cursor: {:?}", Document::from(d)));
    };
    let batch = match (cursor.get("firstBatch"), cursor.get("nextBatch")) {
        (Some(RawValue::Array(a)), _) | (_, Some(RawValue::Array(a))) => a,
        _ => return Err("a cursor without a batch".into()),
    };
    for e in batch.iter() {
        if let RawValue::Document(e) = e
            && let (Some(RawValue::Int64(id)), Some(RawValue::Int64(v))) =
                (e.get("_id"), e.get("v"))
        {
            out.insert(id, v);
        }
    }
    match cursor.get("id") {
        Some(RawValue::Int64(0)) | None => Ok(None),
        Some(RawValue::Int64(id)) => Ok(Some(id)),
        Some(_) => Err("a cursor id that is not a long".into()),
    }
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let n = args.records.max(1);
    let writers = u64::try_from(args.concurrency.max(1)).unwrap_or(1).min(n);
    let server = Server::open(args.target.as_deref())?;
    let addr = server.addr()?;
    let mut admin = Client::connect(addr)?;
    load(&mut admin, n).map_err(crate::client::context("loading"))?;
    let acked: Acked = Arc::default();
    let started = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let handles: Vec<_> = (0..writers)
        .map(|w| {
            let c = Client::connect(addr)?;
            let (acked, started, stop) = (acked.clone(), started.clone(), stop.clone());
            Ok(thread::spawn(move || write(c, (w, writers, n), &acked, (&started, &stop))))
        })
        .collect::<Result<_, String>>()?;
    thread::sleep(args.warmup);
    started.store(true, Ordering::Relaxed);
    let chunks = CHUNKS.min((n / DOCS_PER_CHUNK).max(1));
    let reshard = doc(vec![
        ("reshardCollection", Value::String(format!("{DB}.{COLLECTION}"))),
        ("key", Value::Document(doc(vec![("b", Value::Int32(1))]))),
        ("numInitialChunks", int(chunks)),
    ]);
    let at = Instant::now();
    let resharded = admin
        .command("admin", reshard)
        .and_then(|r| check(&r))
        .map_err(crate::client::context("reshardCollection"));
    let took = at.elapsed();
    thread::sleep(args.duration.min(Duration::from_secs(1)));
    stop.store(true, Ordering::Relaxed);
    let (mut writes, mut cutover) = (0u64, 0f64);
    for h in handles {
        let (count, longest) = h
            .join()
            .map_err(crate::client::panicked("a writer"))?
            .map_err(crate::client::context("a writer"))?;
        writes += count;
        cutover = cutover.max(longest);
    }
    resharded?;
    let now = values(&mut admin).map_err(crate::client::context("reading back"))?;
    let acked = acked.lock().map_err(|_| "a writer panicked")?;
    let lost = acked.iter().filter(|(id, v)| now.get(id) != Some(v)).count();
    let missing = n.saturating_sub(now.len() as u64);
    let mut report = Report::new(COLLECTION);
    report.push("reshard", "s", took.as_secs_f64(), None);
    report.push("cutover", "ms", cutover, Some(Target::AtMost(CUTOVER_MS)));
    report.push("lost", "doc", (lost as u64 + missing) as f64, Some(Target::AtMost(0.0)));
    report.push("writes", "op", writes as f64, None);
    report.push("data", "B", (n * PAYLOAD as u64) as f64, None);
    report.notes.push(format!(
        "{n} documents of {PAYLOAD} B resharded from {{a: 1}} to {{b: 1}} in {chunks} chunks with {writers} writers, target {}",
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_new_key_is_a_permutation_of_the_ids() {
        let n = 1_000;
        let mut all: Vec<i64> = (0..n).map(|i| b_of(i, n)).collect();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len() as u64, n);
        assert_eq!(b_of(3, 0), 0);
    }

    fn reply(cursor: Document) -> Vec<u8> {
        Document::new().with("cursor", cursor).with("ok", 1.0).to_bytes().unwrap()
    }

    #[test]
    fn a_batch_gives_the_values_of_its_documents_and_the_cursor_to_go_on() {
        let mut out = BTreeMap::new();
        let entry =
            |id: i64, v: Value| Value::Document(doc(vec![("_id", Value::Int64(id)), ("v", v)]));
        let batch = vec![
            entry(1, Value::Int64(10)),
            // Neither a document nor one with a long `v`: not a value of the scenario.
            Value::Int32(7),
            entry(2, Value::Int32(20)),
        ];
        let first = Document::new().with("id", Value::Int64(9)).with("firstBatch", batch);
        assert_eq!(batch_values(&reply(first), &mut out), Ok(Some(9)));
        let next = Document::new()
            .with("id", Value::Int64(0))
            .with("nextBatch", vec![entry(3, Value::Int64(30))]);
        assert_eq!(batch_values(&reply(next), &mut out), Ok(None));
        let last = Document::new().with("nextBatch", Vec::<Value>::new());
        assert_eq!(batch_values(&reply(last), &mut out), Ok(None));
        assert_eq!(out, BTreeMap::from([(1, 10), (3, 30)]));
        let odd = Document::new().with("id", "x").with("firstBatch", Vec::<Value>::new());
        assert_eq!(
            batch_values(&reply(odd), &mut out),
            Err("a cursor id that is not a long".into())
        );
        let empty = Document::new().with("id", Value::Int64(0));
        assert_eq!(batch_values(&reply(empty), &mut out), Err("a cursor without a batch".into()));
        let none = Document::new().with("ok", 1.0).to_bytes().unwrap();
        assert!(batch_values(&none, &mut out).unwrap_err().starts_with("a reply without a cursor"));
        assert!(batch_values(b"not bson", &mut out).is_err());
    }
}
