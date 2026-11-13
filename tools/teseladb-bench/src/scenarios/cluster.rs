//! The cluster's scenarios of 007 (FR-024), on a local cluster of three nodes in this process:
//!
//! - `split`: one writer inserts documents of 1 KiB one at a time while the data range splits in
//!   two by the administration port; `split.write` is the latency of the inserts from the split's
//!   request until [`AFTER`] past its end, under 50 ms at p99 (SC-002), and `split.pause` the
//!   longest of them.
//! - `add-node`: with `--records` documents of 1 KiB loaded and split in six ranges, a fourth
//!   node joins and the balancer gives it replicas; `add-node.copy` is the bytes of the data over
//!   the time until the new node holds replicas that all have their data, at least 200 MB/s
//!   (SC-003), and `add-node.write` the latency of the inserts meanwhile, at p99 at most twice
//!   `add-node.write-before`'s.
//! - `rebalance`: as `add-node`, then the cluster's first node is drained (`node drain`) and its
//!   replicas go to the others; `rebalance.copy` is the bytes over the time until no range lists
//!   it among its voters, at least 200 MB/s.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use teseladb_server::client_listener::ClientConfig;
use teseladb_server::data::{DataReply, DataReq};
use teseladb_server::node::{Node, NodeConfig};
use teseladb_bson::Value;
use teseladb_raftlog::MemDisk;
use teseladb_types::Histogram;

use crate::cli::Args;
use crate::client::{Client, Target, check, debug, doc};
use crate::report::{Report, Target as Goal};

/// Database and collection of the documents.
const DB: &str = "bench";
const COLLECTION: &str = "cluster";
/// The padding of a document: with its `_id` and field names it takes 1 KiB.
const PAD: usize = 1000;
/// Bytes of a document.
const DOC_BYTES: f64 = 1024.0;
/// Documents per insert of the load.
const LOAD_BATCH: u64 = 500;
/// How long past a split's end its inserts still count.
const AFTER: Duration = Duration::from_millis(200);
/// SC-002: the p99 of the inserts around a split, in nanoseconds.
const SPLIT_P99_NS: f64 = 50_000_000.0;
/// SC-003: bytes per second of a copy of replicas.
const COPY_BYTES_PER_S: f64 = 200.0 * 1024.0 * 1024.0;
/// SC-003: how many times the p99 of the load may grow while replicas move.
const LOAD_GROWTH: f64 = 2.0;
/// Longest wait for a cluster's change.
pub(crate) const CHANGE_WAIT: Duration = Duration::from_secs(600);
/// How often the scenario looks at the nodes.
const POLL: Duration = Duration::from_millis(5);
/// How long a node's core may take to answer.
const ASK: Duration = Duration::from_secs(2);
/// Ranges the load is split into before a node joins or leaves.
const RANGES: u64 = 6;
/// How long a new node may take to join.
const READY: Duration = Duration::from_secs(60);

fn coll() -> Value {
    Value::String(COLLECTION.into())
}

fn insert(c: &mut Client, ids: std::ops::Range<u64>) -> Result<(), String> {
    let pad = "x".repeat(PAD);
    let docs: Vec<Value> = ids
        .map(|i| {
            let id = Value::Int64(i64::try_from(i).unwrap_or(i64::MAX));
            Value::Document(doc(vec![("_id", id), ("pad", Value::String(pad.clone()))]))
        })
        .collect();
    check(&c.command(DB, doc(vec![("insert", coll()), ("documents", Value::Array(docs))]))?)
}

/// Loads `n` documents of 1 KiB from `_id` 0.
fn load(c: &mut Client, n: u64) -> Result<(), String> {
    let _ = c.command(DB, doc(vec![("drop", coll())]));
    let mut i = 0;
    while i < n {
        let end = (i + LOAD_BATCH).min(n);
        insert(c, i..end)?;
        i = end;
    }
    Ok(())
}

/// A writer that inserts one document at a time from `_id` `from`, each insert with when it
/// ended and how long it took, until it is stopped.
struct Writer {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<Result<Vec<(Instant, u64)>, String>>,
}

impl Writer {
    fn start(addr: SocketAddr, from: u64) -> Result<Writer, String> {
        let mut c = Client::connect(addr)?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = thread::spawn(move || {
            let mut out = Vec::new();
            let mut i = from;
            while !flag.load(Ordering::Relaxed) {
                let t0 = Instant::now();
                insert(&mut c, i..i + 1)?;
                out.push((Instant::now(), t0.elapsed().as_nanos() as u64));
                i += 1;
            }
            Ok(out)
        });
        Ok(Writer { stop, thread })
    }

    fn finish(self) -> Result<Vec<(Instant, u64)>, String> {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.join().map_err(crate::client::panicked("the writer"))?
    }
}

/// The latencies of the inserts that ended in `[from, to]`.
fn between(samples: &[(Instant, u64)], from: Instant, to: Instant) -> Histogram {
    let mut h = Histogram::new();
    for (_, ns) in samples.iter().filter(|(at, _)| *at >= from && *at <= to) {
        h.record(*ns);
    }
    h
}

/// Runs `teseladb admin` with `words` against the node's administration port.
pub(crate) fn admin(addr: SocketAddr, words: &[&str]) -> Result<(), String> {
    let mut argv = vec!["--node".to_owned(), addr.to_string()];
    argv.extend(words.iter().map(|w| (*w).to_owned()));
    let opts = teseladb_server::admin::parse(&argv)?;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    match teseladb_server::admin::run(&opts, &mut out, &mut err) {
        0 => Ok(()),
        _ => Err(String::from_utf8_lossy(&err).trim().to_owned()),
    }
}

/// Waits for `done`, at most [`CHANGE_WAIT`].
fn wait_for(what: &str, done: impl FnMut() -> bool) -> Result<(), String> {
    crate::client::poll_until(what, CHANGE_WAIT, POLL, done)
}

/// What Group 0 answers to a change of a range while another one is under way: the range of a
/// cluster that just started still gets its replicas.
const BUSY: &str = "RangeBusy";

/// Asks for a change of a range with `ask` again while Group 0 answers that the range is busy,
/// for at most `wait`: nothing changed then. The instant of the request it took; any other
/// answer is the error.
pub(crate) fn when_free(
    mut ask: impl FnMut() -> Result<(), String>,
    wait: Duration,
) -> Result<Instant, String> {
    let until = Instant::now() + wait;
    loop {
        let at = Instant::now();
        match ask() {
            Ok(()) => return Ok(at),
            Err(e) if e.contains(BUSY) && Instant::now() < until => thread::sleep(POLL),
            Err(e) => return Err(e),
        }
    }
}

/// The local cluster's nodes (a remote server has none to change).
fn nodes(target: &Target) -> Result<&Vec<Node<MemDisk>>, String> {
    match target {
        Target::Local(nodes) => Ok(nodes),
        Target::Remote(_) => {
            Err("this scenario changes the cluster: it runs on a local one".into())
        }
    }
}

/// `split` (SC-002).
pub fn split(args: &Args) -> Result<Report, String> {
    let target = Target::open(None)?;
    let admin_at = target.admin_addr().ok_or("the local cluster has no administration port")?;
    let mut c = Client::connect(target.addr()?)?;
    load(&mut c, args.records)?;
    let writer = Writer::start(target.addr()?, args.records)?;
    thread::sleep(args.warmup);
    let range = target.ranges().and_then(|r| r.first().copied()).ok_or("no data range")?;
    let at = format!("{{\"ns\": \"{DB}.{COLLECTION}\", \"_id\": {}}}", args.records / 2);
    let split = || admin(admin_at, &["range", "split", &range.to_string(), "--at", &at]);
    let asked = when_free(split, CHANGE_WAIT);
    let t0 = asked.as_ref().map_or_else(|_| Instant::now(), |t| *t);
    let routed = asked.and_then(|_| target.wait_ranges(2));
    let t1 = Instant::now();
    thread::sleep(AFTER);
    let samples = writer.finish()?;
    routed?;
    let h = between(&samples, t0, t1 + AFTER);
    let mut report = Report::new("split");
    report.latency("split.write", &h, None, Some(SPLIT_P99_NS));
    report.push("split.pause", "ns", h.max() as f64, None);
    report.push("split.took", "ns", (t1 - t0).as_nanos() as f64, None);
    report.notes.push(format!(
        "{} documents of 1 KiB, one writer, local cluster of three nodes",
        args.records
    ));
    Ok(report)
}

/// A node that joins the local cluster of `target`.
fn join(target: &Target) -> Result<Node<MemDisk>, String> {
    let founder = nodes(target)?.first().ok_or("an empty cluster")?;
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    let cfg = NodeConfig {
        cores: Some(1),
        join: vec![founder.peer_addr().to_string()],
        client: Some(ClientConfig { addrs: vec![loopback], ..ClientConfig::new(Vec::new()) }),
        admin_listen: Some(loopback),
        ..NodeConfig::default()
    };
    let n = Node::start(cfg, || Ok(MemDisk::new()), None).map_err(debug)?;
    super::ready(n, READY, "the new node did not join")
}

/// The ranges of which node `n` holds a replica, and whether every one of them has its data
/// (none waits for a snapshot's data from another node).
fn replicas(n: &Node<MemDisk>) -> (usize, bool) {
    let statuses: Vec<_> = n
        .ranges()
        .iter()
        .filter_map(|e| match n.data(DataReq::Status(e.group), ASK) {
            Ok(DataReply::Status(s)) => Some(s),
            _ => None,
        })
        .filter(|s| s.role.is_some())
        .collect();
    let ready = statuses.iter().all(|s| !s.waiting_data);
    (statuses.len(), ready)
}

/// Splits the local cluster's data range in [`RANGES`] at documents of the load, so that a node
/// that joins takes replicas for the count rule.
fn split_load(target: &Target, records: u64) -> Result<(), String> {
    let admin_at = target.admin_addr().ok_or("the local cluster has no administration port")?;
    for k in 1..RANGES {
        let id = records * k / RANGES;
        let at = format!("{{\"ns\": \"{DB}.{COLLECTION}\", \"_id\": {id}}}");
        // A range still settling its last split refuses another one for a while.
        let mut last = String::new();
        let split = wait_for("a split", || {
            for g in target.ranges().unwrap_or_default() {
                match admin(admin_at, &["range", "split", &g.to_string(), "--at", &at]) {
                    Ok(()) => return true,
                    Err(e) => last = e,
                }
            }
            thread::sleep(POLL * 20);
            false
        });
        split.map_err(|e| format!("{e}: {last}"))?;
        target.wait_ranges(usize::try_from(k).unwrap_or(usize::MAX) + 1)?;
    }
    Ok(())
}

/// The load, a fourth node and the time until it holds a replica with its data: the new node,
/// the copy's time, and the inserts before and during the copy.
struct Added {
    node: Node<MemDisk>,
    took: Duration,
    before: Histogram,
    during: Histogram,
}

fn add(target: &Target, args: &Args) -> Result<Added, String> {
    let mut c = Client::connect(target.addr()?)?;
    load(&mut c, args.records)?;
    split_load(target, args.records)?;
    let writer = Writer::start(target.addr()?, args.records)?;
    thread::sleep(args.warmup);
    let t0 = Instant::now();
    let node = join(target);
    let copied = node.as_ref().map_err(Clone::clone).and_then(|n| {
        wait_for("the copy", || {
            let (held, ready) = replicas(n);
            held > 0 && ready
        })
    });
    let t1 = Instant::now();
    let samples = writer.finish()?;
    let node = node?;
    copied?;
    let start = samples.first().map_or(t0, |(at, _)| *at);
    Ok(Added {
        node,
        took: t1 - t0,
        before: between(&samples, start, t0),
        during: between(&samples, t0, t1),
    })
}

fn copy_rate(records: u64, took: Duration) -> f64 {
    records as f64 * DOC_BYTES / took.as_secs_f64().max(f64::MIN_POSITIVE)
}

/// `add-node` (SC-003).
pub fn add_node(args: &Args) -> Result<Report, String> {
    let target = Target::open(None)?;
    let added = add(&target, args)?;
    let mut report = Report::new("add-node");
    let rate = copy_rate(args.records, added.took);
    report.push("add-node.copy", "B/s", rate, Some(Goal::AtLeast(COPY_BYTES_PER_S)));
    report.latency("add-node.write-before", &added.before, None, None);
    let limit = added.before.percentile(99.0) as f64 * LOAD_GROWTH;
    report.latency("add-node.write", &added.during, None, Some(limit));
    report.notes.push(format!("{} documents of 1 KiB, one writer", args.records));
    added.node.stop();
    Ok(report)
}

/// `rebalance` (FR-024): the replica of a drained node moves to the others.
pub fn rebalance(args: &Args) -> Result<Report, String> {
    let target = Target::open(None)?;
    let added = add(&target, args)?;
    let admin_at = target.admin_addr().ok_or("the local cluster has no administration port")?;
    let founder = nodes(&target)?.first().ok_or("an empty cluster")?;
    let t0 = Instant::now();
    admin(admin_at, &["node", "drain", &founder.node_id().0.to_string()])?;
    // Drained once no range lists it among its voters.
    let id = founder.node_id();
    wait_for("the drain", || founder.ranges().iter().all(|e| !e.voters.contains(&id)))?;
    let took = t0.elapsed();
    let mut report = Report::new("rebalance");
    let rate = copy_rate(args.records, took);
    report.push("rebalance.copy", "B/s", rate, Some(Goal::AtLeast(COPY_BYTES_PER_S)));
    report
        .notes
        .push(format!("{} documents of 1 KiB, a fourth node, the first drained", args.records));
    added.node.stop();
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn args(records: u64) -> Args {
        Args { records, warmup: Duration::from_millis(20), ..Args::default() }
    }

    #[test]
    fn a_short_split_reports_its_latencies() {
        let r = split(&args(200)).unwrap();
        let names: Vec<&str> = r.metrics.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"split.write.p99") && names.contains(&"split.pause"), "{names:?}");
    }

    #[test]
    fn a_short_add_node_and_rebalance_report_their_copies() {
        let r = add_node(&args(200)).unwrap();
        assert!(r.metrics.iter().any(|m| m.name == "add-node.copy" && m.value > 0.0));
        let r = rebalance(&args(200)).unwrap();
        assert!(r.metrics.iter().any(|m| m.name == "rebalance.copy" && m.value > 0.0));
    }

    #[test]
    fn a_change_waits_while_its_range_is_busy() {
        let mut busy = 3;
        let ask = || match busy {
            0 => Ok(()),
            _ => {
                busy -= 1;
                Err(format!("teseladb admin: Group 0 refused it: {BUSY}"))
            }
        };
        assert!(when_free(ask, Duration::from_secs(5)).is_ok());
        assert_eq!(busy, 0);
        let refused = when_free(|| Err("BadSplit".to_owned()), Duration::from_secs(5));
        assert_eq!(refused.unwrap_err(), "BadSplit");
        let busy = when_free(|| Err(BUSY.to_owned()), Duration::ZERO);
        assert_eq!(busy.unwrap_err(), BUSY);
    }

    #[test]
    fn samples_count_in_their_window_and_a_remote_server_is_refused() {
        let t = Instant::now();
        let later = t + Duration::from_millis(5);
        let h = between(&[(t, 3), (later, 9)], t, t);
        assert_eq!(h.count(), 1);
        assert!(copy_rate(1, Duration::ZERO) > 0.0);
        let remote = Target::Remote(SocketAddr::from(([127, 0, 0, 1], 1)));
        assert!(nodes(&remote).is_err());
    }
}
