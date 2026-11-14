//! A node stopped and started again in the same process over its data directory serves reads as
//! it serves writes (005 FR-012, 007): before, every `find` and `count` after the restart timed out
//! with "the range did not answer in time", also over an empty collection.

mod common;

use std::fs;
use std::path::PathBuf;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_io::FileDisk;
use teseladb_server::node::{Node, NodeConfig, lock_dir};
use teseladb_bson::{Document, Value};

const DB: &str = "restart";

/// A data directory removed when the test ends.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p =
            std::env::temp_dir().join(format!("teseladb-restart-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn start(dir: &Dir, cores: u16) -> Node<FileDisk> {
    let cfg =
        NodeConfig { data_dir: Some(dir.0.clone()), cores: Some(cores), ..config(Vec::new()) };
    let path = dir.0.clone();
    let lock = lock_dir(&dir.0).expect("locks the directory");
    let node = Node::start(cfg, || FileDisk::open(&path), Some(lock)).expect("starts");
    assert!(node.wait_ready(READY));
    node
}

fn count(c: &mut Client, coll: &str) -> Document {
    c.run(DB, Document::new().with("count", coll))
}

fn found(c: &mut Client, coll: &str) -> Vec<Value> {
    let r = c.run(DB, Document::new().with("find", coll));
    assert!(ok(&r), "{r:?}");
    batch(&r, "firstBatch")
}

fn restart_and_read(cores: u16) {
    let dir = Dir::new(&format!("cores-{cores}"));
    let node = start(&dir, cores);
    let mut c = Client::new(&node);
    let r = c.run(DB, Document::new().with("insert", "full").with("documents", docs(0..20)));
    assert!(ok(&r), "{r:?}");
    let r = c.run(DB, Document::new().with("create", "empty"));
    assert!(ok(&r), "{r:?}");
    drop(c);
    node.stop();

    let node = start(&dir, cores);
    let mut c = Client::new(&node);
    // The ranges elect their leaders again after the start.
    until("a find after the restart", || {
        let r = c.run(DB, Document::new().with("find", "full"));
        ok(&r).then_some(())
    });
    assert_eq!(found(&mut c, "full").len(), 20, "the documents written before the stop");
    assert!(found(&mut c, "empty").is_empty());
    let r = count(&mut c, "full");
    assert_eq!(r.get("n"), Some(&Value::Int32(20)), "{r:?}");
    let r = count(&mut c, "empty");
    assert_eq!(r.get("n"), Some(&Value::Int32(0)), "{r:?}");
    // And writes go on.
    let r = c.run(DB, Document::new().with("insert", "full").with("documents", docs(20..21)));
    assert!(ok(&r), "{r:?}");
    assert_eq!(found(&mut c, "full").len(), 21);
    drop(c);
    node.stop();
}

#[test]
fn a_restarted_node_of_one_core_serves_reads() {
    restart_and_read(1);
}

#[test]
fn a_restarted_node_of_two_cores_serves_reads() {
    restart_and_read(2);
}
