//! `changestream-lag` scenario (014 FR-016, SC-002): the time from a write's commit to its event,
//! against `--target` or a local cluster of three nodes. One connection inserts a document every
//! [`WRITE_EVERY`] and notes when the insert answered; another one follows a change stream on the
//! collection with `getMore` and, for each event, takes the time since its insert answered. SC-002
//! asks for a p99 under 500 ms.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_types::Histogram;

use crate::cli::Args;
use crate::client::{Client, Target, debug, doc};
use crate::report::Report;

/// Database and collection of the documents.
const DB: &str = "bench";
const COLLECTION: &str = "changestream";
/// SC-002: from the commit to the event under 500 ms at p99.
const LAG_P99_NS: f64 = 500_000_000.0;
/// Pause between two inserts.
const WRITE_EVERY: Duration = Duration::from_millis(10);
/// How long a `getMore` waits for events.
const AWAIT_MS: i32 = 1000;
/// At least this many events are timed, however short the duration.
const MIN_EVENTS: u64 = 200;

/// When each insert answered, by `_id`.
type Acked = Arc<Mutex<BTreeMap<i64, Instant>>>;

/// The cursor's batch: the `_id` of each event's document, the cursor id.
fn events(reply: &[u8]) -> Result<(Vec<i64>, i64), String> {
    let d = RawDoc::from_bytes(reply).map_err(debug)?;
    let Some(RawValue::Document(cursor)) = d.get("cursor") else {
        return Err(format!("a reply without a cursor: {:?}", Document::from(d)));
    };
    let id = match cursor.get("id") {
        Some(RawValue::Int64(n)) => n,
        _ => return Err("a cursor without an id".into()),
    };
    let batch = match (cursor.get("firstBatch"), cursor.get("nextBatch")) {
        (Some(RawValue::Array(a)), _) | (_, Some(RawValue::Array(a))) => a,
        _ => return Err("a cursor without a batch".into()),
    };
    let ids = batch
        .iter()
        .filter_map(|e| match e {
            RawValue::Document(e) => match e.get("documentKey") {
                Some(RawValue::Document(k)) => match k.get("_id") {
                    Some(RawValue::Int64(n)) => Some(n),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect();
    Ok((ids, id))
}

/// Follows the stream until `stop`, timing each event against its insert.
fn follow(mut c: Client, acked: &Acked, stop: &AtomicBool) -> Result<Histogram, String> {
    let open = doc(vec![
        ("aggregate", Value::String(COLLECTION.into())),
        (
            "pipeline",
            Value::Array(vec![Value::Document(doc(vec![(
                "$changeStream",
                Value::Document(Document::new()),
            )]))]),
        ),
        ("cursor", Value::Document(Document::new())),
    ]);
    let (_, id) = events(&c.command(DB, open)?)?;
    let mut h = Histogram::new();
    let mut timed = 0u64;
    while !stop.load(Ordering::Relaxed) || timed < MIN_EVENTS {
        let more = doc(vec![
            ("getMore", Value::Int64(id)),
            ("collection", Value::String(COLLECTION.into())),
            ("maxTimeMS", Value::Int32(AWAIT_MS)),
        ]);
        let (ids, _) = events(&c.command(DB, more)?)?;
        let now = Instant::now();
        let acked = acked.lock().map_err(|_| "a writer panicked")?;
        for at in ids.iter().filter_map(|i| acked.get(i)) {
            h.record(now.saturating_duration_since(*at).as_nanos() as u64);
            timed += 1;
        }
    }
    Ok(h)
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let addr = target.addr()?;
    let mut w = Client::connect(addr)?;
    // The collection may not exist yet.
    let _ = w.command(DB, doc(vec![("drop", Value::String(COLLECTION.into()))]));
    w.command(DB, doc(vec![("create", Value::String(COLLECTION.into()))]))?;
    let acked: Acked = Arc::default();
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (c, acked, stop) = (Client::connect(addr)?, acked.clone(), stop.clone());
        thread::spawn(move || follow(c, &acked, &stop))
    };
    let start = Instant::now();
    let (mut next, mut recorded) = (0i64, 0u64);
    while start.elapsed() < args.warmup + args.duration || recorded < MIN_EVENTS {
        next += 1;
        let d = doc(vec![("_id", Value::Int64(next))]);
        let insert = doc(vec![
            ("insert", Value::String(COLLECTION.into())),
            ("documents", Value::Array(vec![Value::Document(d)])),
        ]);
        w.command(DB, insert)?;
        // The events of the warm-up are not timed.
        if start.elapsed() >= args.warmup {
            acked.lock().map_err(|_| "the reader panicked")?.insert(next, Instant::now());
            recorded += 1;
        }
        thread::sleep(WRITE_EVERY);
    }
    stop.store(true, Ordering::Relaxed);
    let lag = reader.join().map_err(|_| "the reader panicked")??;
    let mut report = Report::new("changestream-lag");
    report.latency("lag", &lag, None, Some(LAG_P99_NS));
    report.notes.push(format!(
        "one insert every {} ms, one stream on the collection, target {}",
        WRITE_EVERY.as_millis(),
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn reply(cursor: Vec<(&str, Value)>) -> Vec<u8> {
        doc(vec![("cursor", Value::Document(doc(cursor)))]).to_bytes().expect("bson")
    }

    fn event(id: Value) -> Value {
        Value::Document(doc(vec![("documentKey", Value::Document(doc(vec![("_id", id)])))]))
    }

    #[test]
    fn a_batch_gives_the_ids_of_its_events_and_the_cursor() {
        let batch = vec![
            event(Value::Int64(7)),
            event(Value::Int32(8)),
            Value::Document(doc(vec![("documentKey", Value::Int32(1))])),
            Value::Document(Document::new()),
            Value::Int32(9),
            event(Value::Int64(10)),
        ];
        let first = reply(vec![("id", Value::Int64(3)), ("firstBatch", Value::Array(batch))]);
        assert_eq!(events(&first), Ok((vec![7, 10], 3)));
        let next = reply(vec![("id", Value::Int64(0)), ("nextBatch", Value::Array(vec![]))]);
        assert_eq!(events(&next), Ok((vec![], 0)));
    }

    #[test]
    fn a_reply_without_its_cursor_parts_is_an_error() {
        let no_cursor = doc(vec![("ok", Value::Int32(0))]).to_bytes().expect("bson");
        assert!(events(&no_cursor).unwrap_err().contains("without a cursor"));
        let no_id = reply(vec![("firstBatch", Value::Array(vec![]))]);
        assert!(events(&no_id).unwrap_err().contains("without an id"));
        let no_batch = reply(vec![("id", Value::Int64(1))]);
        assert!(events(&no_batch).unwrap_err().contains("without a batch"));
        assert!(events(&[1, 2]).is_err());
    }
}
