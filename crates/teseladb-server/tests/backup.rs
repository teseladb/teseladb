//! 007 T034: a backup at a mark while the writes go on, and its restore into an empty cluster
//! (FR-021, SC-007, plan «Copias de seguridad», contract `admin-cli.md`). `teseladb admin backup
//! --at <mark|now> --out <dir>` writes the files of every range up to the mark and
//! `manifest.bson`; `teseladb admin restore --from <dir>` brings them into a cluster without
//! collections, and refuses a cluster that has any.

mod common;

use std::net::SocketAddr;
use std::path::PathBuf;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_raftlog::MemDisk;
use teseladb_server::node::{Node, NodeConfig};

fn with_admin() -> NodeConfig {
    joining(Vec::new())
}

fn joining(join: Vec<String>) -> NodeConfig {
    NodeConfig { admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))), ..config(join) }
}

/// How long a node's core may take to answer a test.
const ASK: std::time::Duration = std::time::Duration::from_secs(2);

fn admin(at: &str, args: &[&str]) -> (i32, String, String) {
    let argv: Vec<String> =
        ["admin", "--node", at].iter().chain(args).map(|s| (*s).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::run(&argv, &mut out, &mut err, &|| true);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

/// A directory of its own under the system's temporary one, removed at the end.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("teseladb-backup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The SST files under `dir`, at any depth.
fn sst_files(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            found.extend(sst_files(&p));
        } else if p.extension().is_some_and(|e| e == "sst") {
            found.push(p);
        }
    }
    found
}

/// The count of `app.c`, or `None` while the cluster cannot answer it.
fn try_count(c: &mut Client) -> Option<i32> {
    match c.run("app", Document::new().with("count", "c")).get("n") {
        Some(Value::Int32(n)) => Some(*n),
        _ => None,
    }
}

fn count(c: &mut Client) -> i32 {
    let r = c.run("app", Document::new().with("count", "c"));
    match r.get("n") {
        Some(Value::Int32(n)) => *n,
        other => panic!("{other:?} in {r:?}"),
    }
}

#[test]
fn a_backup_at_a_mark_restores_what_was_written_up_to_it_into_an_empty_cluster() {
    let out = Dir::new("out");
    let n = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(READY));
    let at = n.admin_addr().unwrap().to_string();
    let mut c = Client::new(&n);
    let r = c.run("app", Document::new().with("insert", "c").with("documents", docs(0..20)));
    assert!(ok(&r), "{r:?}");
    let (code, text, err) =
        admin(&at, &["backup", "--at", "now", "--out", out.0.to_str().unwrap()]);
    assert_eq!(code, 0, "{text}{err}");
    // A write after the backup's mark is not in it.
    let r = c.run("app", Document::new().with("insert", "c").with("documents", docs(20..25)));
    assert!(ok(&r), "{r:?}");
    let manifest = std::fs::read(out.0.join("manifest.bson")).expect("manifest.bson");
    let m = Document::from(RawDoc::from_bytes(&manifest).unwrap());
    assert_eq!(m.get("format"), Some(&Value::Int32(1)), "{m:?}");
    assert!(matches!(m.get("at"), Some(Value::Timestamp { .. })), "{m:?}");
    let Some(Value::Array(ranges)) = m.get("ranges") else { panic!("{m:?}") };
    assert!(!ranges.is_empty());
    // A cluster with collections refuses the restore and stays as it was.
    let (code, _, err) = admin(&at, &["restore", "--from", out.0.to_str().unwrap()]);
    assert_ne!(code, 0);
    assert!(err.contains("empty"), "{err}");
    assert_eq!(count(&mut c), 25);
    n.stop();
    // An empty cluster takes it: the catalog and the documents up to the mark.
    let fresh = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(fresh.wait_ready(READY));
    let at = fresh.admin_addr().unwrap().to_string();
    let (code, text, err) = admin(&at, &["restore", "--from", out.0.to_str().unwrap()]);
    assert_eq!(code, 0, "{text}{err}");
    let mut c = Client::new(&fresh);
    until("the restored documents", || (count(&mut c) == 20).then_some(()));
    let r = c.run(
        "app",
        Document::new().with("find", "c").with("filter", Document::new().with("_id", 3)),
    );
    assert_eq!(batch(&r, "firstBatch").len(), 1, "{r:?}");
    // A damaged file is refused before anything changes.
    let file = sst_files(&out.0.join("ranges")).into_iter().next().expect("an SST file");
    let mut bytes = std::fs::read(&file).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&file, bytes).unwrap();
    let other = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(other.wait_ready(READY));
    let at = other.admin_addr().unwrap().to_string();
    let (code, _, err) = admin(&at, &["restore", "--from", out.0.to_str().unwrap()]);
    assert_ne!(code, 0);
    assert!(err.contains("SHA-256"), "{err}");
    other.stop();
    fresh.stop();
}

#[test]
fn a_restore_into_three_nodes_leaves_every_replica_with_the_data() {
    let out = Dir::new("three");
    let n = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(READY));
    let mut c = Client::new(&n);
    let r = c.run("app", Document::new().with("insert", "c").with("documents", docs(0..30)));
    assert!(ok(&r), "{r:?}");
    let at = n.admin_addr().unwrap().to_string();
    let (code, text, err) = admin(&at, &["backup", "--out", out.0.to_str().unwrap()]);
    assert_eq!(code, 0, "{text}{err}");
    n.stop();
    // A cluster of three nodes, its data range on all three.
    let n1 = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(n1.wait_ready(READY));
    let seed = vec![n1.peer_addr().to_string()];
    let n2 = Node::start(joining(seed.clone()), || Ok(MemDisk::new()), None).unwrap();
    let n3 = Node::start(joining(seed), || Ok(MemDisk::new()), None).unwrap();
    assert!(n2.wait_ready(READY) && n3.wait_ready(READY));
    let voters = |n: &Node<MemDisk>| n.data_status(ASK).map_or(0, |s| s.voters.len());
    until("three replicas", || (voters(&n1) == 3).then_some(()));
    let at = n1.admin_addr().unwrap().to_string();
    let (code, text, err) = admin(&at, &["restore", "--from", out.0.to_str().unwrap()]);
    assert_eq!(code, 0, "{text}{err}");
    // Every replica has the data: with the range's leader stopped, the others still answer with
    // every document.
    let mut nodes = vec![n1, n2, n3];
    until("three replicas again", || nodes.iter().all(|n| voters(n) == 3).then_some(()));
    let leads = |n: &Node<MemDisk>| {
        n.data_status(ASK).is_some_and(|s| s.role == Some(teseladb_raft::Role::Leader))
    };
    let leader = until("a leader", || nodes.iter().position(leads));
    let mut c = Client::new(&nodes[(leader + 1) % 3]);
    until("the restored documents", || (try_count(&mut c) == Some(30)).then_some(()));
    nodes.remove(leader).stop();
    let mut c = Client::new(&nodes[1]);
    until("the documents without the old leader", || (try_count(&mut c) == Some(30)).then_some(()));
    for n in nodes {
        n.stop();
    }
}

#[test]
fn a_backup_under_writes_holds_everything_acknowledged_before_it_and_nothing_torn() {
    let out = Dir::new("load");
    let n = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(READY));
    let at = n.admin_addr().unwrap().to_string();
    let mut c = Client::new(&n);
    let r = c.run("app", Document::new().with("insert", "c").with("documents", docs(0..10)));
    assert!(ok(&r), "{r:?}");
    // A writer inserts one document at a time while the backup runs.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let acked = std::sync::Arc::new(std::sync::atomic::AtomicI32::new(10));
    let writer = {
        let (stop, acked) = (stop.clone(), acked.clone());
        let mut w = Client::new(&n);
        std::thread::spawn(move || {
            let mut i = 10;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) && i < 2_000 {
                let d = Document::new().with("insert", "c").with("documents", docs(i..i + 1));
                assert!(ok(&w.run("app", d)));
                i += 1;
                acked.store(i, std::sync::atomic::Ordering::Relaxed);
            }
        })
    };
    let before = acked.load(std::sync::atomic::Ordering::Relaxed);
    let (code, text, err) = admin(&at, &["backup", "--out", out.0.to_str().unwrap()]);
    let after = acked.load(std::sync::atomic::Ordering::Relaxed);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    writer.join().unwrap();
    assert_eq!(code, 0, "{text}{err}");
    n.stop();
    let fresh = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(fresh.wait_ready(READY));
    let at = fresh.admin_addr().unwrap().to_string();
    let (code, text, err) = admin(&at, &["restore", "--from", out.0.to_str().unwrap()]);
    assert_eq!(code, 0, "{text}{err}");
    let mut c = Client::new(&fresh);
    let n = until("the restored documents", || try_count(&mut c));
    // The documents are a prefix of the writer's: every one acknowledged before the backup
    // started, none torn, and each `_id` below the count present.
    assert!(before <= n && n <= after, "{before} <= {n} <= {after}");
    let r = c.run(
        "app",
        Document::new().with("find", "c").with("filter", Document::new().with("_id", n - 1)),
    );
    assert_eq!(batch(&r, "firstBatch").len(), 1, "{r:?}");
    fresh.stop();
}
