//! Catalog changes in the first seconds of a cluster (009 FR-006, 006 FR-027): a three-node
//! cluster that has just started takes `create`, `createIndexes`, `collMod` and `drop` well inside
//! a command's deadline, also with the machine busy, and each node reports its routing table whole
//! (`getShardVersion`) instead of the part it has filled.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::cli::DevOpts;
use teseladb_server::node::Node;
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

/// Well inside the 30 s of a command: a change waits for Group 0 and the fence, not for a
/// command's deadline.
const PROMPT: Duration = Duration::from_secs(10);

/// Threads that keep every processor busy until dropped.
struct Load {
    stop: Arc<AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

impl Load {
    fn start() -> Load {
        let stop = Arc::new(AtomicBool::new(false));
        let n = std::thread::available_parallelism().map_or(4, |n| n.get()) * 2;
        let threads = (0..n)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let mut x = 1u64;
                    while !stop.load(Ordering::Relaxed) {
                        x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005) | 1);
                    }
                })
            })
            .collect();
        Load { stop, threads }
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

fn index(field: &str) -> Value {
    let key = Document::new().with(field, 1);
    Value::Array(vec![Value::Document(
        Document::new().with("key", key).with("name", format!("{field}_1")),
    )])
}

/// Runs `cmd` and checks it answers ok within [`PROMPT`].
fn prompt(c: &mut Client, cmd: Document) {
    prompt_on(c, "fresh", cmd);
}

/// Runs `cmd` on `db` and checks it answers ok within [`PROMPT`]: the answer.
fn prompt_on(c: &mut Client, db: &str, cmd: Document) -> Document {
    let what = format!("{cmd:?}");
    let t = Instant::now();
    let r = c.run(db, cmd);
    assert!(ok(&r), "{what}: {r:?} after {:?}", t.elapsed());
    assert!(t.elapsed() < PROMPT, "{what} took {:?}", t.elapsed());
    r
}

/// The routing table as each node reports it right away (`getShardVersion`): the whole of a
/// collection just sharded, one chunk from `MinKey` to `MaxKey`, never a part of it.
fn routes_on_every_node(nodes: &[Node<MemDisk>]) {
    for (k, n) in nodes.iter().enumerate() {
        let mut c = Client::new(n);
        let ns = format!("fresh.s{k}");
        prompt(&mut c, Document::new().with("create", format!("s{k}")));
        let key = Document::new().with("a", 1);
        prompt_on(
            &mut c,
            "admin",
            Document::new().with("shardCollection", ns.as_str()).with("key", key),
        );
        let version =
            Document::new().with("getShardVersion", ns.as_str()).with("fullMetadata", true);
        let r = prompt_on(&mut c, "admin", version);
        let Some(Value::Array(chunks)) = r.get("chunks") else { panic!("{r:?}") };
        assert_eq!(chunks.len(), 1, "{r:?}");
        prompt(&mut c, Document::new().with("drop", format!("s{k}")));
    }
}

/// The catalog changes of a collection of its own on each node, right away.
fn changes_on_every_node(nodes: &[Node<MemDisk>]) {
    for (k, n) in nodes.iter().enumerate() {
        let mut c = Client::new(n);
        let coll = format!("c{k}");
        let coll = coll.as_str();
        prompt(&mut c, Document::new().with("drop", coll));
        prompt(&mut c, Document::new().with("create", coll));
        prompt(&mut c, Document::new().with("createIndexes", coll).with("indexes", index("a")));
        prompt(&mut c, Document::new().with("insert", coll).with("documents", docs(0..5)));
        prompt(&mut c, Document::new().with("createIndexes", coll).with("indexes", index("v")));
        prompt(&mut c, Document::new().with("collMod", coll).with("validator", Document::new()));
        prompt(&mut c, Document::new().with("dropIndexes", coll).with("index", "a_1"));
        let r = c.run("fresh", Document::new().with("listIndexes", coll));
        let names: Vec<Value> = batch(&r, "firstBatch")
            .into_iter()
            .filter_map(|i| match i {
                Value::Document(d) => d.get("name").cloned(),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec![Value::from("_id_"), Value::from("v_1")], "{r:?}");
        prompt(&mut c, Document::new().with("drop", coll));
    }
}

/// As `teseladb dev` and the benchmarks start a cluster: the founder ready, the other two still
/// joining.
fn dev_cluster(loaded: bool) {
    let load = loaded.then(Load::start);
    let opts = DevOpts { nodes: 3, cores: 1, port: 0, test_commands: false };
    let nodes = teseladb_server::dev::start(&opts, None).unwrap();
    routes_on_every_node(&nodes);
    changes_on_every_node(&nodes);
    drop(load);
    drop(nodes.into_iter().map(Node::stop).collect::<Vec<_>>());
}

#[test]
fn a_dev_cluster_takes_catalog_changes_at_once() {
    dev_cluster(false);
}

#[test]
fn a_dev_cluster_on_a_busy_machine_takes_catalog_changes_at_once() {
    dev_cluster(true);
}

#[test]
fn a_cluster_whose_nodes_just_joined_takes_catalog_changes_at_once() {
    let _load = Load::start();
    let first = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(first.wait_ready(READY));
    let seed = vec![first.peer_addr().to_string()];
    let mut nodes = vec![first];
    for _ in 0..2 {
        let n = Node::start(config(seed.clone()), || Ok(MemDisk::new()), None).unwrap();
        until("a node that joined", || n.wait_ready(Duration::from_millis(100)).then_some(()));
        nodes.push(n);
    }
    routes_on_every_node(&nodes);
    changes_on_every_node(&nodes);
    drop(nodes.into_iter().map(Node::stop).collect::<Vec<_>>());
}
