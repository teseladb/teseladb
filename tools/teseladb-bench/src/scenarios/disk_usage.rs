//! `disk-usage` (012 FR-009): bytes on disk per document against BSON, an encoding that carries
//! the name and type of every field in every document (what RefDb keeps per document before its
//! block compression). A node of one core on the real disk (`FileDisk`, in `--data` or a temporary
//! folder) takes `--records` YCSB records (ten fields of 100 bytes) over the wire protocol, with
//! small memtables so that the documents reach the LSM's files. It reports:
//! - `disk_usage.bson_per_doc`: BSON bytes per document;
//! - `disk_usage.sst_per_doc`: bytes of the LSM's files per document;
//! - `disk_usage.total_per_doc`: bytes of the whole data directory (log included) per document;
//! - `disk_usage.sst_over_bson`: the ratio of the two first.

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use teseladb_bson::Value;
use teseladb_io::FileDisk;
use teseladb_server::client_listener::ClientConfig;
use teseladb_server::node::{Node, NodeConfig};

use super::ycsb::{insert, record};
use super::ycsb_gen::Rng;
use super::{DataDir, at};
use crate::cli::Args;
use crate::client::{Client, doc};
use crate::report::Report;

/// Memtable of the measurement: small, so that the documents are flushed to files.
const MEMTABLE_BYTES: u64 = 256 << 10;
/// Records per insert.
const BATCH: u64 = 500;
/// How long the node has to start and to flush.
const READY: Duration = Duration::from_secs(60);

/// Bytes of the files under `dir` whose name ends in `suffix` (all with an empty suffix).
pub fn bytes_under(dir: &Path, suffix: &str) -> std::io::Result<u64> {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let meta = e.metadata()?;
            if meta.is_dir() {
                stack.push(e.path());
            } else if e.file_name().to_string_lossy().ends_with(suffix) {
                total += meta.len();
            }
        }
    }
    Ok(total)
}

/// Runs the measurement.
pub fn run(args: &Args) -> Result<Report, String> {
    let dir = DataDir::new(args.data.as_deref(), "disk-usage")?;
    let path = dir.path.clone();
    let loopback = SocketAddr::from(([127, 0, 0, 1], 0));
    let mut cfg = NodeConfig {
        cores: Some(1),
        client: Some(ClientConfig { addrs: vec![loopback], ..ClientConfig::new(Vec::new()) }),
        ..NodeConfig::default()
    };
    cfg.lsm.memtable_bytes = MEMTABLE_BYTES;
    let node = Node::start(cfg, || FileDisk::open(&path), None).map_err(|e| format!("{e:?}"))?;
    let measured = measure(args, &node, &path);
    node.stop();
    let (bson, sst, total) = measured?;
    let n = args.records.max(1) as f64;
    let mut report = Report::new("disk-usage");
    report.push("disk_usage.bson_per_doc", "B", bson as f64 / n, None);
    report.push("disk_usage.sst_per_doc", "B", sst as f64 / n, None);
    report.push("disk_usage.total_per_doc", "B", total as f64 / n, None);
    let ratio = if bson == 0 { 0.0 } else { sst as f64 / bson as f64 };
    report.push("disk_usage.sst_over_bson", "ratio", ratio, None);
    report.notes.push(format!(
        "{} YCSB records of ten 100-byte fields, one node of one core on FileDisk, memtables of \
         {} KiB",
        args.records,
        MEMTABLE_BYTES >> 10
    ));
    Ok(report)
}

/// Loads the records; returns their BSON bytes, the LSM's bytes and the directory's bytes.
fn measure(args: &Args, node: &Node<FileDisk>, dir: &Path) -> Result<(u64, u64, u64), String> {
    if !node.wait_ready(READY) {
        return Err("the node did not start".into());
    }
    let addr = *node.client_addrs().first().ok_or("the node has no clients' port")?;
    let mut c = Client::connect(addr)?;
    let mut rng = Rng::new(1);
    let mut bson = 0u64;
    let mut i = 0;
    while i < args.records {
        let n = BATCH.min(args.records - i);
        let docs: Vec<Value> = (i..i + n).map(|j| record(j, &mut rng)).collect();
        for d in &docs {
            if let Value::Document(d) = d {
                bson += d.to_bytes().map_err(|e| format!("{e:?}"))?.len() as u64;
            }
        }
        insert_retrying(&mut c, docs)?;
        i += n;
    }
    // The last memtable may still be in memory: the LSM's files grow until it is flushed or the
    // flushes stop, whichever comes first.
    let deadline = Instant::now() + READY;
    let mut sst = bytes_under(dir, ".sst").map_err(at(dir))?;
    loop {
        std::thread::sleep(Duration::from_millis(500));
        let now = bytes_under(dir, ".sst").map_err(at(dir))?;
        if now == sst || Instant::now() > deadline {
            break;
        }
        sst = now;
    }
    let count = doc(vec![("count", Value::String("usertable".into()))]);
    c.command("ycsb", count)?;
    Ok((bson, sst, bytes_under(dir, "").map_err(at(dir))?))
}

/// The first insert may come before the data range has its leader.
fn insert_retrying(c: &mut Client, docs: Vec<Value>) -> Result<(), String> {
    let deadline = Instant::now() + READY;
    loop {
        match insert(c, docs.clone()) {
            Ok(()) => return Ok(()),
            Err(e) if Instant::now() > deadline => return Err(e),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn bytes_are_counted_by_suffix_in_every_folder() {
        let d = DataDir::new(None, "bytes").unwrap();
        std::fs::create_dir_all(d.path.join("a/b")).unwrap();
        std::fs::write(d.path.join("a/b/1.sst"), [0u8; 10]).unwrap();
        std::fs::write(d.path.join("a/2.wal"), [0u8; 5]).unwrap();
        assert_eq!(bytes_under(&d.path, ".sst").unwrap(), 10);
        assert_eq!(bytes_under(&d.path, "").unwrap(), 15);
        assert!(bytes_under(&d.path.join("missing"), "").is_err());
    }

    #[test]
    fn a_small_load_reports_its_bytes_per_document() {
        let args = Args { records: 2000, ..Args::default() };
        let r = run(&args).unwrap();
        let get = |n: &str| r.metrics.iter().find(|m| m.name == n).unwrap().value;
        let bson = get("disk_usage.bson_per_doc");
        assert!(bson > 1000.0 && bson < 1300.0, "ten fields of 100 bytes: {bson}");
        assert!(get("disk_usage.sst_per_doc") > 0.0, "the documents reached the files");
        assert!(get("disk_usage.total_per_doc") >= get("disk_usage.sst_per_doc"));
    }
}
