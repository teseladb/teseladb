//! A range moves to another core of its node (007 T020, FR-011, plan «Cambio de núcleo»):
//! `teseladb admin range relocate` records the intent in Group 0, the range's log moves it, its
//! documents stay readable and writable throughout, and Group 0's descriptor ends with the new
//! core. Over real files the moved range survives a restart on its new core.

mod common;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::Document;
use teseladb_io::FileDisk;
use teseladb_raftlog::MemDisk;
use teseladb_server::node::{Node, NodeConfig, lock_dir};

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
        let r = c.run("db", Document::new().with("find", "c").with("batchSize", 1000));
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

#[test]
fn a_moved_range_survives_a_restart_on_its_new_core() {
    let dir = Dir(std::env::temp_dir().join(format!("teseladb-relocate-{}", std::process::id())));
    let _ = fs::remove_dir_all(&dir.0);
    let start = || {
        let path = dir.0.clone();
        Node::start(with_admin(), move || FileDisk::open(&path), Some(lock_dir(&dir.0).unwrap()))
            .unwrap()
    };
    let n = start();
    assert!(n.wait_ready(READY));
    let at = n.admin_addr().unwrap().to_string();
    let mut c = Client::new(&n);
    assert!(ok(&c.run("db", Document::new().with("insert", "c").with("documents", docs(0..40)))));
    let from = until("range 1", || core_of_range_1(&at));
    let (code, _, err) = admin(&at, &["range", "relocate", "1", "--core", &(1 - from).to_string()]);
    assert_eq!(code, 0, "{err}");
    until("the range on its new core", || (core_of_range_1(&at) == Some(1 - from)).then_some(()));
    assert_eq!(all(&mut c), 40);
    drop(c);
    n.stop();
    let n = start();
    assert!(n.wait_ready(READY));
    let at = n.admin_addr().unwrap().to_string();
    assert_eq!(until("range 1", || core_of_range_1(&at)), 1 - from);
    let mut c = Client::new(&n);
    assert_eq!(all(&mut c), 40);
    n.stop();
}
