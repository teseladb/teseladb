//! A range moves to another core of its node (007 T020, FR-011, plan «Cambio de núcleo»):
//! `teseladb admin range relocate` records the intent in Group 0, the range's log moves it, its
//! documents stay readable and writable throughout, and Group 0's descriptor ends with the new
//! core. Over real files the moved range survives a restart on its new core.

mod common;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_io::FileDisk;
use teseladb_server::node::{Node, NodeConfig, lock_dir};
use teseladb_bson::Document;
use teseladb_raftlog::MemDisk;

fn with_admin() -> NodeConfig {
    NodeConfig { admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))), ..config(Vec::new()) }
}

/// Runs `teseladb admin --node <at> <args>`: exit code, standard output and error.
fn admin(at: &str, args: &[&str]) -> (i32, String, String) {
    let argv: Vec<String> =
        ["admin", "--node", at].iter().chain(args).map(|s| (*s).to_owned()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::run(&argv, &mut out, &mut err, &|| true);
    (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
}

/// The core of range 1 as `teseladb admin ranges` gives it.
fn core_of_range_1(at: &str) -> Option<u16> {
    let (code, out, _) = admin(at, &["--format", "json", "ranges"]);
    if code != 0 {
        return None;
    }
    let compact: String = out.chars().filter(|c| !c.is_whitespace()).collect();
    let at = compact.find("\"_id\":1,")?;
    let rest = &compact[at..];
    let core = rest.find("\"core\":")? + "\"core\":".len();
    rest[core..].split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()
}

/// Every document of `db.c`, once a find answers.
fn all(c: &mut Client) -> usize {
    until("a find", || {
        let r = c.run("db", Document::new().with("find", "c").with("batchSize", 10_000));
        ok(&r).then(|| batch(&r, "firstBatch").len())
    })
}

#[test]
fn a_range_moves_to_another_core_with_its_documents() {
    let n = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(READY));
    let at = n.admin_addr().unwrap().to_string();
    let mut c = Client::new(&n);
    assert!(ok(&c.run("db", Document::new().with("insert", "c").with("documents", docs(0..50)))));
    let from = until("range 1", || core_of_range_1(&at));
    let to = 1 - from;
    // Refused: the range's own core, a core the node lacks, an unknown range.
    let (code, _, err) = admin(&at, &["range", "relocate", "1", "--core", &from.to_string()]);
    assert_eq!(code, 1, "{err}");
    let (code, _, err) = admin(&at, &["range", "relocate", "1", "--core", "7"]);
    assert_eq!(code, 1, "{err}");
    let (code, _, err) = admin(&at, &["range", "relocate", "77", "--core", "0"]);
    assert_eq!((code, err.as_str()), (1, "teseladb admin: no range 77\n"));
    let (code, out, err) = admin(&at, &["range", "relocate", "1", "--core", &to.to_string()]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("\"ok\""), "{out}");
    until("the range on its new core", || (core_of_range_1(&at) == Some(to)).then_some(()));
    assert_eq!(all(&mut c), 50);
    let more = Document::new().with("insert", "c").with("documents", docs(50..60));
    until("writes on the new core", || ok(&c.run("db", more.clone())).then_some(()));
    assert_eq!(all(&mut c), 60);
    // And back, which needs the copy the first move left on the old core gone.
    let (code, _, err) = admin(&at, &["range", "relocate", "1", "--core", &from.to_string()]);
    assert_eq!(code, 0, "{err}");
    until("the range back", || (core_of_range_1(&at) == Some(from)).then_some(()));
    assert_eq!(all(&mut c), 60);
    n.stop();
}

#[test]
fn relocate_without_a_core_is_a_usage_error() {
    for args in [&["range", "relocate", "1"][..], &["range", "relocate", "1", "--core", "x"]] {
        let (code, _, err) = admin("127.0.0.1:1", args);
        assert_eq!(code, 2, "{args:?}: {err}");
    }
}

/// A data directory removed when the test ends.
struct Dir(PathBuf);

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// What tells a hard link from a copy: inode, size and modification time.
type FileId = (u64, u64, std::time::SystemTime);

/// The SST files under `dir`, each with its [`FileId`].
fn sst_files(dir: &std::path::Path) -> Vec<FileId> {
    use std::os::unix::fs::MetadataExt;
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            let Ok(m) = fs::metadata(&path) else { continue };
            if m.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "sst") {
                out.push((m.ino(), m.size(), m.modified().unwrap()));
            }
        }
    }
    out
}

/// Documents of about 1 KiB, enough to fill several small files.
fn big_docs(range: std::ops::Range<i32>) -> teseladb_bson::Value {
    let pad = "x".repeat(1000);
    teseladb_bson::Value::Array(
        range.map(|i| Document::new().with("_id", i).with("pad", pad.as_str()).into()).collect(),
    )
}

/// The cores of a node over real files share them: the moved range's whole files go to its new
/// core as hard links (004 FR-033). Small memtables and files leave most of the range in files
/// that hold nothing else, and at least one of them reaches the new core as the same inode,
/// unread and unwritten.
#[test]
fn a_moved_range_survives_a_restart_on_its_new_core() {
    const DOCS: i32 = 1500;
    let dir = Dir(std::env::temp_dir().join(format!("teseladb-relocate-{}", std::process::id())));
    let _ = fs::remove_dir_all(&dir.0);
    let lsm = teseladb_lsm::LsmConfig {
        memtable_bytes: 64 << 10,
        target_file_bytes: 32 << 10,
        block_bytes: 4 << 10,
        l1_bytes: 128 << 10,
        ..teseladb_lsm::LsmConfig::default()
    };
    let start = || {
        let path = dir.0.clone();
        let cfg = NodeConfig { shared_files: true, lsm: lsm.clone(), ..with_admin() };
        Node::start(cfg, move || FileDisk::open(&path), Some(lock_dir(&dir.0).unwrap())).unwrap()
    };
    let n = start();
    assert!(n.wait_ready(READY));
    let at = n.admin_addr().unwrap().to_string();
    let mut c = Client::new(&n);
    for k in (0..DOCS).step_by(100) {
        let more = Document::new().with("insert", "c").with("documents", big_docs(k..k + 100));
        assert!(ok(&c.run("db", more)));
    }
    let from = until("range 1", || core_of_range_1(&at));
    let to = 1 - from;
    let from_dir = dir.0.join(teseladb_raftlog::paths::core_dir(from));
    let to_dir = dir.0.join(teseladb_raftlog::paths::core_dir(to));
    // The flushes and compactions of the inserts settle: the same files for a second.
    let mut last = Vec::new();
    let before = until("the files of the range settled on its core", || {
        std::thread::sleep(std::time::Duration::from_secs(1));
        let mut files = sst_files(&from_dir);
        files.sort();
        let settled = files.len() >= 4 && files == last;
        last = files.clone();
        settled.then_some(files)
    });
    let (code, _, err) = admin(&at, &["range", "relocate", "1", "--core", &to.to_string()]);
    assert_eq!(code, 0, "{err}");
    until("the range on its new core", || (core_of_range_1(&at) == Some(to)).then_some(()));
    let linked = sst_files(&to_dir).iter().filter(|id| before.contains(id)).count();
    assert!(
        linked > 0,
        "no file of the range reached core {to} as a link: {before:?} {:?}",
        sst_files(&to_dir)
    );
    assert_eq!(all(&mut c), DOCS as usize);
    drop(c);
    n.stop();
    let n = start();
    assert!(n.wait_ready(READY));
    let at = n.admin_addr().unwrap().to_string();
    assert_eq!(until("range 1", || core_of_range_1(&at)), to);
    let mut c = Client::new(&n);
    assert_eq!(all(&mut c), DOCS as usize);
    // And back, with what the first move linked now in the files of the new core.
    let (code, _, err) = admin(&at, &["range", "relocate", "1", "--core", &from.to_string()]);
    assert_eq!(code, 0, "{err}");
    until("the range back", || (core_of_range_1(&at) == Some(from)).then_some(()));
    assert_eq!(all(&mut c), DOCS as usize);
    n.stop();
}

/// Writes that are not retryable keep answering while their range changes cores (007 FR-011):
/// the core that is to hand the range over refuses new writes of it before proposing them, so
/// none lands after the `Relocate` entry with its outcome unknown, and the node sends them again to
/// the range's new core (H-025 of the native tests: 25 of them failed with «interrupted at
/// shutdown» under YCSB-A while the ranges moved).
#[test]
fn writes_keep_answering_while_their_range_changes_cores() {
    let n = Node::start(with_admin(), || Ok(MemDisk::new()), None).unwrap();
    assert!(n.wait_ready(READY));
    let at = n.admin_addr().unwrap().to_string();
    let mut c = Client::new(&n);
    assert!(ok(&c.run("db", Document::new().with("insert", "c").with("documents", docs(0..10)))));
    let from = until("range 1", || core_of_range_1(&at));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Eight writers, as YCSB-A's clients: each inserts its own documents and updates them.
    let writers: Vec<_> = (0..8)
        .map(|k| {
            let stop = stop.clone();
            let mut w = Client::new(&n);
            std::thread::spawn(move || {
                let (mut sent, mut failed) = (0, Vec::new());
                let mut id = 1_000 + k * 100_000;
                while !stop.load(std::sync::atomic::Ordering::Acquire) {
                    let one =
                        Document::new().with("insert", "c").with("documents", docs(id..id + 1));
                    let set = Document::new().with("$set", Document::new().with("v", id));
                    let update =
                        Document::new().with("q", Document::new().with("_id", id)).with("u", set);
                    let up = Document::new()
                        .with("update", "c")
                        .with("updates", teseladb_bson::Value::Array(vec![update.into()]));
                    for cmd in [one, up] {
                        let r = w.run("db", cmd);
                        if !ok(&r) {
                            failed.push(format!("{r:?}"));
                        }
                        sent += 1;
                    }
                    id += 1;
                }
                (sent, failed)
            })
        })
        .collect();
    for core in [1 - from, from, 1 - from, from, 1 - from, from, 1 - from, from] {
        // A split by load, which the writers bring about, keeps the range busy for a moment.
        until("the core change accepted", || {
            let (code, _, err) =
                admin(&at, &["range", "relocate", "1", "--core", &core.to_string()]);
            assert!(code == 0 || err.contains("RangeBusy"), "{err}");
            (code == 0).then_some(())
        });
        until("the range on its core", || (core_of_range_1(&at) == Some(core)).then_some(()));
    }
    stop.store(true, std::sync::atomic::Ordering::Release);
    let (mut sent, mut failed) = (0, Vec::new());
    for w in writers {
        let (s, f) = w.join().unwrap();
        sent += s;
        failed.extend(f);
    }
    assert!(sent > 0);
    assert!(failed.is_empty(), "{} of {sent} writes failed: {:?}", failed.len(), failed.first());
    n.stop();
}
