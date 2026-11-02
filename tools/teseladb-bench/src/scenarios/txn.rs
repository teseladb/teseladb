//! Transaction scenarios (008), over the wire protocol against `--target` or a local cluster of
//! three nodes:
//!
//! - `txn-readers` (SC-005): 100 readers on 1 000 documents, first with no transaction open, then
//!   while ten transactions hold an intent on every document; the readers' p99 may rise by at most
//!   one consensus round, which is the p50 of the writes of one more client to another collection
//!   while the readers read. Every read has to see the version before the intents: a reader never
//!   waits for their commit.
//! - `txn` (SC-004): a transaction of one document (one range, its commit in one phase, under
//!   300 µs at p50) and one of two documents in two ranges (under 1 ms at p50), each timed from
//!   its first statement to its commit; the local cluster splits its data range for it.
//! - `contention` (FR-017): `--concurrency` clients that each add one to a random document out of
//!   ten in a transaction; the latency of the transactions that commit, the share that aborts on
//!   a conflict and the share of reads that restart for their uncertainty window.

use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_types::{Histogram, Rng};

use super::wire::each;
use crate::cli::Args;
use crate::client::{Client, Target, check, debug, doc};
use crate::load::{Load, drive};
use crate::report::{Report, Target as Goal};

/// Database and collection of the documents.
const DB: &str = "bench";
const COLLECTION: &str = "txn";
/// The collection of the writer that times a consensus round in `txn-readers`.
const ROUND_COLLECTION: &str = "txn_round";
/// SC-005: the documents the readers read and the transactions hold.
const KEYS: i64 = 1000;
/// SC-005: readers at once.
const READERS: usize = 100;
/// SC-005: transactions that hold the intents, each on `KEYS / HOLDERS` documents.
const HOLDERS: i64 = 10;
/// Documents per insert while loading.
const LOAD_BATCH: i64 = 100;
/// SC-004: a transaction of two ranges under 1 ms at p50.
const TWO_RANGES_P50_NS: f64 = 1_000_000.0;
/// SC-004: a transaction of one range, with the latency of a write: under 300 µs at p50.
const ONE_RANGE_P50_NS: f64 = 300_000.0;
/// FR-017: the hot documents of `contention`.
const HOT_KEYS: i64 = 10;
/// The label of a conflict the client repeats.
const TRANSIENT: &str = "TransientTransactionError";
/// Seconds a transaction may live beyond the scenario's own length: the holders of
/// `txn-readers` stay open while the readers run.
const LIFETIME_MARGIN_SECS: u64 = 60;
/// The admin port's counter of the reads restarted for their uncertainty window (008 FR-019).
const UNCERTAINTY_RESTARTS: &str = "teseladb_txn_uncertainty_restarts_total";

/// The session `n`: an `lsid` the scenario makes up, as a driver does.
fn lsid(n: u64) -> Value {
    let mut id = [0u8; 16];
    id[..8].copy_from_slice(&n.to_be_bytes());
    id[8] = 0x40;
    Value::Document(doc(vec![("id", Value::Binary { subtype: 4, bytes: id.to_vec() })]))
}

/// `command` as statement of transaction `txn` of `session`, the first one if `start`.
fn in_txn(command: Vec<(&str, Value)>, session: &Value, txn: i64, start: bool) -> Document {
    let mut d = doc(command);
    d.insert("lsid", session.clone());
    d.insert("txnNumber", Value::Int64(txn));
    if start {
        d.insert("startTransaction", Value::Boolean(true));
    }
    d.insert("autocommit", Value::Boolean(false));
    d
}

fn coll() -> Value {
    Value::String(COLLECTION.into())
}

fn by_id(k: i64) -> Value {
    Value::Document(doc(vec![("_id", Value::Int64(k))]))
}

/// The update of `filter` that sets `v` to `to` (every document when `multi`).
fn set_v(filter: Value, to: i64, multi: bool) -> Vec<(&'static str, Value)> {
    set_v_in(COLLECTION, filter, to, multi)
}

/// [`set_v`] on collection `name`.
fn set_v_in(name: &str, filter: Value, to: i64, multi: bool) -> Vec<(&'static str, Value)> {
    let set =
        Value::Document(doc(vec![("$set", Value::Document(doc(vec![("v", Value::Int64(to))])))]));
    let u = doc(vec![("q", filter), ("u", set), ("multi", Value::Boolean(multi))]);
    vec![
        ("update", Value::String(name.into())),
        ("updates", Value::Array(vec![Value::Document(u)])),
    ]
}

/// The update that adds one to `v` of document `k`.
fn inc_v(k: i64) -> Vec<(&'static str, Value)> {
    let inc =
        Value::Document(doc(vec![("$inc", Value::Document(doc(vec![("v", Value::Int64(1))])))]));
    let u = doc(vec![("q", by_id(k)), ("u", inc)]);
    vec![("update", coll()), ("updates", Value::Array(vec![Value::Document(u)]))]
}

/// Ends transaction `txn` of `session` with `commitTransaction` or `abortTransaction`.
fn end(c: &mut Client, session: &Value, txn: i64, commit: bool) -> Result<Vec<u8>, String> {
    let name = if commit { "commitTransaction" } else { "abortTransaction" };
    c.send("admin", in_txn(vec![(name, Value::Int32(1))], session, txn, false))
}

/// The collection afresh with documents `0..n`, `v: 0` in each.
fn load(c: &mut Client, n: i64) -> Result<(), String> {
    load_into(c, COLLECTION, n)
}

/// Collection `name` afresh with documents `0..n`, `v: 0` in each.
fn load_into(c: &mut Client, name: &str, n: i64) -> Result<(), String> {
    let coll = || Value::String(name.into());
    // The collection may not exist yet.
    let _ = c.command(DB, doc(vec![("drop", coll())]));
    let mut first = 0;
    while first < n {
        let docs = (first..n.min(first + LOAD_BATCH))
            .map(|k| Value::Document(doc(vec![("_id", Value::Int64(k)), ("v", Value::Int64(0))])))
            .collect();
        c.command(DB, doc(vec![("insert", coll()), ("documents", Value::Array(docs))]))?;
        first += LOAD_BATCH;
    }
    Ok(())
}

/// Lets transactions live for the scenario's warm-ups and durations and a margin.
fn lifetime(c: &mut Client, args: &Args) -> Result<(), String> {
    let secs = (args.warmup + args.duration).as_secs().saturating_mul(2) + LIFETIME_MARGIN_SECS;
    let set = doc(vec![
        ("setParameter", Value::Int32(1)),
        ("transactionLifetimeLimitSeconds", Value::Int64(i64::try_from(secs).unwrap_or(i64::MAX))),
    ]);
    c.command("admin", set).map(drop)
}

/// The `v` of the first document of a `find` reply.
fn first_v(reply: &[u8]) -> Result<Option<i64>, String> {
    let d = RawDoc::from_bytes(reply).map_err(debug)?;
    let Some(RawValue::Document(cursor)) = d.get("cursor") else {
        return Err("a find reply without a cursor".into());
    };
    let Some(RawValue::Array(batch)) = cursor.get("firstBatch") else {
        return Err("a cursor without a first batch".into());
    };
    let Some(RawValue::Document(first)) = batch.iter().next() else {
        return Ok(None);
    };
    match first.get("v") {
        Some(RawValue::Int64(v)) => Ok(Some(v)),
        Some(RawValue::Int32(v)) => Ok(Some(i64::from(v))),
        other => Err(format!("a document whose v is {other:?}")),
    }
}

/// Whether a reply failed with the label of a conflict the client repeats.
fn transient(reply: &[u8]) -> bool {
    let Ok(d) = RawDoc::from_bytes(reply) else { return false };
    matches!(d.get("errorLabels"), Some(RawValue::Array(l))
        if l.iter().any(|v| matches!(v, RawValue::String(TRANSIENT))))
}

/// The reads restarted for their uncertainty window on the local cluster's first node, which
/// runs the scenario's transactions, from its administration port's metrics; `None` for a remote
/// server, which has no such port.
fn uncertainty_restarts(target: &Target) -> Result<Option<i64>, String> {
    let Some(admin) = target.admin_addr() else { return Ok(None) };
    let get = teseladb_server::admin_http::Method::Get;
    let metrics = teseladb_server::admin::METRICS;
    let (status, body) =
        teseladb_server::admin_http::request(&admin.to_string(), get, metrics, b"")
            .map_err(|e| format!("{admin}: {e}"))?;
    if status != 200 {
        return Err(format!("{admin}{metrics} answered {status}"));
    }
    counter(&String::from_utf8_lossy(&body), UNCERTAINTY_RESTARTS).map(Some)
}

/// The value of counter `name` in a page of metrics.
fn counter(page: &str, name: &str) -> Result<i64, String> {
    page.lines()
        .find_map(|l| l.strip_prefix(name)?.strip_prefix(' '))
        .ok_or_else(|| format!("the metrics have no {name}"))?
        .trim()
        .parse()
        .map_err(|e| format!("{name}: {e}"))
}

/// 100 readers of random documents for `--duration`, and one writer of the documents of
/// [`ROUND_COLLECTION`], whose writes are the consensus round under the same load: a read that does
/// not see `v: 0` is an error, since no transaction commits while they read. The reads' and the
/// writes' latencies.
fn readers(
    args: &Args,
    addr: std::net::SocketAddr,
    seed: u64,
) -> Result<(Histogram, Histogram), String> {
    let load =
        Load { warmup: args.warmup, duration: args.duration, concurrency: READERS + 1, rate: None };
    let out = drive(
        &load,
        |w| Ok((Client::connect(addr)?, Rng::new(seed + w as u64), w == READERS)),
        |(c, rng, writer)| {
            let k = rng.range(0, KEYS as u64) as i64;
            if *writer {
                c.command(DB, doc(set_v_in(ROUND_COLLECTION, by_id(k), 0, false)))?;
                return Ok("write");
            }
            let find =
                doc(vec![("find", coll()), ("filter", by_id(k)), ("limit", Value::Int32(1))]);
            match first_v(&c.command(DB, find)?)? {
                Some(0) => Ok("read"),
                other => Err(format!(
                    "document {k} read as v = {other:?}, not the version before the intents"
                )),
            }
        },
    )?;
    if let Some(e) = out.first_error {
        return Err(format!("{} operations failed; the first: {e}", out.errors));
    }
    let of = |kind: &str| out.latencies.get(kind).cloned().unwrap_or_default();
    Ok((of("read"), of("write")))
}

/// `txn-readers` (SC-005).
pub fn readers_run(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let addr = target.addr()?;
    let mut c = Client::connect(addr)?;
    load(&mut c, KEYS)?;
    load_into(&mut c, ROUND_COLLECTION, KEYS)?;
    lifetime(&mut c, args)?;
    let (clean, round) = readers(args, addr, 1)?;
    let per = KEYS / HOLDERS;
    let sessions: Vec<Value> = (0..HOLDERS).map(|h| lsid(h as u64 + 1)).collect();
    for (h, s) in (0..).zip(&sessions) {
        let range =
            doc(vec![("$gte", Value::Int64(h * per)), ("$lt", Value::Int64((h + 1) * per))]);
        let filter = Value::Document(doc(vec![("_id", Value::Document(range))]));
        c.command(DB, in_txn(set_v(filter, 1, true), s, 1, true))?;
    }
    let held = readers(args, addr, 1 + READERS as u64);
    // The holders end whatever the readers found; one that is gone ended early and the
    // measure does not hold.
    for s in &sessions {
        check(&end(&mut c, s, 1, false)?)
            .map_err(|e| format!("a transaction holding intents ended before the readers: {e}"))?;
    }
    let (held, _) = held?;
    let rise = held.percentile(99.0) as f64 - clean.percentile(99.0) as f64;
    let mut report = Report::new("txn-readers");
    report.latency("write", &round, None, None);
    report.latency("read_clean", &clean, None, None);
    report.latency("read_intents", &held, None, None);
    let goal = Goal::AtMost(round.percentile(50.0) as f64);
    report.push("read_intents.p99_rise", "ns", rise, Some(goal));
    report.notes.push(format!(
        "{READERS} readers on {KEYS} documents, {HOLDERS} transactions holding their intents, one \
         writer of another collection timing the consensus round under the same load, target {}",
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}

/// Splits the local cluster's data range at document `at`, through the administration port of
/// its first node, and waits until that node routes two ranges.
fn split(target: &Target, at: i64) -> Result<(), String> {
    let (Some(admin), Some(ranges)) = (target.admin_addr(), target.ranges()) else {
        return Ok(());
    };
    let at = format!("{{\"ns\": \"{DB}.{COLLECTION}\", \"_id\": {at}}}");
    let mut errors = Vec::new();
    for g in ranges {
        let words = ["--node", &admin.to_string(), "range", "split", &g.to_string(), "--at", &at];
        let opts = teseladb_server::admin::parse(&words.map(str::to_owned))?;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        if teseladb_server::admin::run(&opts, &mut out, &mut err) == 0 {
            return target.wait_ranges(2);
        }
        errors.push(String::from_utf8_lossy(&err).into_owned());
    }
    Err(format!("no range took the split: {errors:?}"))
}

/// `txn` (SC-004).
pub fn run(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let mut c = Client::connect(target.addr()?)?;
    load(&mut c, KEYS)?;
    split(&target, KEYS / 2)?;
    let session = lsid(1);
    let mut txn = 0;
    let mut k = 0;
    let mut one = |c: &mut Client, keys: &[i64]| -> Result<(), String> {
        txn += 1;
        for (i, key) in keys.iter().enumerate() {
            c.command(DB, in_txn(set_v(by_id(*key), txn, false), &session, txn, i == 0))?;
        }
        check(&end(c, &session, txn, true)?)
    };
    let one_range = each(args.warmup, args.duration, || {
        k = (k + 1) % (KEYS / 2);
        one(&mut c, &[k])
    })?;
    let two_ranges = each(args.warmup, args.duration, || {
        k = (k + 1) % (KEYS / 2);
        one(&mut c, &[k, k + KEYS / 2])
    })?;
    let mut report = Report::new("txn");
    report.latency("one_range", &one_range, Some(ONE_RANGE_P50_NS), None);
    report.latency("two_ranges", &two_ranges, Some(TWO_RANGES_P50_NS), None);
    report.notes.push(format!(
        "one connection, from the first statement to the commit, target {}",
        args.target
            .as_deref()
            .unwrap_or("local cluster of three nodes, its data range split in two")
    ));
    Ok(report)
}

/// `contention` (FR-017).
pub fn contention(args: &Args) -> Result<Report, String> {
    let target = Target::open(args.target.as_deref())?;
    let addr = target.addr()?;
    let mut c = Client::connect(addr)?;
    load(&mut c, HOT_KEYS)?;
    let restarts_before = uncertainty_restarts(&target)?;
    let aborted = AtomicU64::new(0);
    let next_txn = AtomicU64::new(0);
    let load = Load {
        warmup: args.warmup,
        duration: args.duration,
        concurrency: args.concurrency.max(1),
        rate: None,
    };
    let out = drive(
        &load,
        |w| Ok((Client::connect(addr)?, Rng::new(w as u64 + 1), lsid(w as u64 + 1))),
        |(c, rng, session)| {
            let txn = next_txn.fetch_add(1, Ordering::Relaxed) as i64 + 1;
            let k = rng.range(0, HOT_KEYS as u64) as i64;
            let wrote = c.send(DB, in_txn(inc_v(k), session, txn, true))?;
            let reply = if check(&wrote).is_ok() { end(c, session, txn, true)? } else { wrote };
            match check(&reply) {
                Ok(()) => Ok("commit"),
                Err(_) if transient(&reply) => {
                    aborted.fetch_add(1, Ordering::Relaxed);
                    // The statement's failure already aborted it; a commit's conflict did too.
                    let _ = end(c, session, txn, false);
                    Ok("abort")
                }
                Err(e) => Err(e),
            }
        },
    )?;
    if let Some(e) = &out.first_error {
        return Err(format!("{} transactions failed; the first: {e}", out.errors));
    }
    let restarts = match (restarts_before, uncertainty_restarts(&target)?) {
        (Some(a), Some(b)) => Some(b - a),
        _ => None,
    };
    let attempts = out.ops.max(1) as f64;
    let mut report = Report::new("contention");
    if let Some(h) = out.latencies.get("commit") {
        report.latency("commit", h, None, None);
    }
    report.push(
        "conflict_aborts",
        "ratio",
        aborted.load(Ordering::Relaxed) as f64 / attempts,
        None,
    );
    match restarts {
        Some(n) => report.push("uncertainty_restarts", "ratio", n as f64 / attempts, None),
        None => report.notes.push("the target does not count uncertainty restarts".into()),
    }
    report.push("throughput", "op/s", out.throughput(), None);
    report.notes.push(format!(
        "{} clients on {HOT_KEYS} documents, target {}",
        load.concurrency,
        args.target.as_deref().unwrap_or("local cluster of three nodes")
    ));
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn bytes(d: Document) -> Vec<u8> {
        d.to_bytes().expect("bson")
    }

    #[test]
    fn counters_are_read_from_a_page_of_metrics() {
        let page = "# HELP teseladb_txn_uncertainty_restarts_total Reads.\n\
                    teseladb_txn_uncertainty_restarts_total_x 9\n\
                    teseladb_txn_uncertainty_restarts_total 42\n";
        assert_eq!(counter(page, UNCERTAINTY_RESTARTS), Ok(42));
        assert!(counter("", UNCERTAINTY_RESTARTS).unwrap_err().contains("have no"));
        let bad = format!("{UNCERTAINTY_RESTARTS} x\n");
        assert!(counter(&bad, UNCERTAINTY_RESTARTS).is_err());
    }

    #[test]
    fn replies_are_read_for_their_value_and_their_label() {
        let batch = |docs: Vec<Value>| {
            bytes(doc(vec![(
                "cursor",
                Value::Document(doc(vec![("firstBatch", Value::Array(docs))])),
            )]))
        };
        let v = |x: Value| Value::Document(doc(vec![("v", x)]));
        assert_eq!(first_v(&batch(vec![v(Value::Int64(3))])), Ok(Some(3)));
        assert_eq!(first_v(&batch(vec![v(Value::Int32(4))])), Ok(Some(4)));
        assert_eq!(first_v(&batch(vec![])), Ok(None));
        assert!(first_v(&batch(vec![v(Value::String("x".into()))])).is_err());
        assert!(first_v(&bytes(doc(vec![("ok", Value::Int32(1))]))).is_err());
        let no_batch = bytes(doc(vec![("cursor", Value::Document(Document::new()))]));
        assert!(first_v(&no_batch).is_err());
        assert!(first_v(&[1, 2]).is_err());
        let labelled =
            bytes(doc(vec![("errorLabels", Value::Array(vec![Value::String(TRANSIENT.into())]))]));
        assert!(transient(&labelled));
        assert!(!transient(&bytes(doc(vec![("ok", Value::Int32(0))]))));
        let other = doc(vec![("errorLabels", Value::Array(vec![Value::String("Other".into())]))]);
        assert!(!transient(&bytes(other)));
        assert!(!transient(&[1, 2]));
    }

    #[test]
    fn a_remote_server_has_no_restarts_to_read() {
        let remote = Target::open(Some("refdb://127.0.0.1:1")).expect("target");
        assert_eq!(uncertainty_restarts(&remote), Ok(None));
        assert_eq!(split(&remote, 1), Ok(()), "a remote server is not split");
    }

    #[test]
    fn statements_carry_their_session_and_transaction() {
        let d = in_txn(vec![("find", coll())], &lsid(7), 3, true);
        assert_eq!(d.get("txnNumber"), Some(&Value::Int64(3)));
        assert_eq!(d.get("startTransaction"), Some(&Value::Boolean(true)));
        assert_eq!(d.get("autocommit"), Some(&Value::Boolean(false)));
        let next = in_txn(vec![("find", coll())], &lsid(7), 3, false);
        assert_eq!(next.get("startTransaction"), None);
        assert_ne!(lsid(1), lsid(2));
    }
}
