//! 007 T050 (FR-031, FR-036, SC-009): the storage quota of a tenant on three nodes. Its writes fail
//! with 14031 `OutOfDiskSpace` and the words of the contract once its data reaches
//! `storageBytes`; reads, deletes, `drop` and an `update` that only `$unset`s go on; deleting
//! frees it at the next read of its data; the default tenant is never held; and `maxCollections`,
//! `maxDatabases` and `maxSearchIndexes` refuse what goes past them, also a collection created by
//! writing to it.

mod common;
mod secured;

use std::time::Duration;

use common::{Client, batch, docs, ok, until};
use secured::{PASSWORD, ROOT_PASSWORD, admin_out as admin, login, secured as start};
use teseladb_server::node::Node;
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

const OUT_OF_DISK_SPACE: i32 = 14031;

fn code(r: &Document) -> Option<i32> {
    match r.get("code") {
        Some(Value::Int32(c)) => Some(*c),
        _ => None,
    }
}

fn errmsg(r: &Document) -> String {
    match r.get("errmsg") {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn cluster() -> (Vec<Node<MemDisk>>, Client) {
    let first = start(Vec::new());
    let seed = vec![first.peer_addr().to_string()];
    let mut nodes = vec![first];
    nodes.extend((0..2).map(|_| start(seed.clone())));
    admin(&nodes[0], &["tenant", "create", "acme"]);
    let mut root = Client::new(&nodes[0]);
    assert!(ok(&login(&mut root, "root", ROOT_PASSWORD)));
    let create = Document::new()
        .with("createUser", "ann")
        .with("pwd", PASSWORD)
        .with(
            "roles",
            Value::Array(vec![
                Value::from("readWriteAnyDatabase"),
                Value::from("dbAdminAnyDatabase"),
            ]),
        )
        .with("mechanisms", Value::Array(vec![Value::from("SCRAM-SHA-256")]))
        .with("$tenant", "acme");
    assert!(ok(&root.run("admin", create)));
    (nodes, root)
}

fn ann(node: &Node<MemDisk>) -> Client {
    until("Ann signs in", || {
        let mut c = Client::new(node);
        ok(&login(&mut c, "ann", PASSWORD)).then_some(c)
    })
}

fn big(from: i32, n: i32) -> Document {
    let docs: Vec<Value> = (from..from + n)
        .map(|i| Value::Document(Document::new().with("_id", i).with("f", "x".repeat(4_000))))
        .collect();
    Document::new().with("insert", "c").with("documents", Value::Array(docs))
}

fn stop(nodes: Vec<Node<MemDisk>>) {
    for n in nodes.into_iter().rev() {
        n.stop();
    }
}

#[test]
fn writes_stop_at_the_storage_quota_and_deleting_frees_it() {
    let (nodes, mut root) = cluster();
    let mut c = ann(&nodes[1]);
    assert!(ok(&c.run("app", big(0, 5))));
    // A limit below what it uses holds every write that makes it grow, at the next read of it.
    admin(&nodes[0], &["tenant", "limits", "acme", "--storage-bytes", "1000"]);
    let refused = until("the quota", || {
        let r = c.run("app", big(100, 1));
        let first = match r.get("writeErrors") {
            Some(Value::Array(e)) => match &e[0] {
                Value::Document(d) => Some(d.clone()),
                _ => None,
            },
            _ if !ok(&r) => Some(r),
            _ => None,
        };
        first
            .filter(|e| errmsg(e).ends_with(" of 1000 bytes") && errmsg(e).contains(": 2"))
            .or_else(|| {
                std::thread::sleep(Duration::from_millis(200));
                None
            })
    });
    assert_eq!(code(&refused), Some(OUT_OF_DISK_SPACE), "{refused:?}");
    assert!(errmsg(&refused).starts_with("tenant acme is over its storage quota: "), "{refused:?}");
    // Reads go on, an update that only `$unset`s too, and one that sets does not.
    let r = c.run("app", Document::new().with("find", "c").with("batchSize", 1));
    assert_eq!(batch(&r, "firstBatch").len(), 1, "{r:?}");
    let unset = Document::new()
        .with("q", Document::new())
        .with("u", Document::new().with("$unset", Document::new().with("f", 1)))
        .with("multi", true);
    assert!(ok(&c.run(
        "app",
        Document::new()
            .with("update", "c")
            .with("updates", Value::Array(vec![Value::Document(unset)]))
    )));
    let set = Document::new()
        .with("q", Document::new())
        .with("u", Document::new().with("$set", Document::new().with("g", 1)));
    let r = c.run(
        "app",
        Document::new()
            .with("update", "c")
            .with("updates", Value::Array(vec![Value::Document(set)])),
    );
    assert_eq!(code(&r), Some(OUT_OF_DISK_SPACE), "{r:?}");
    // The default tenant is never held.
    assert!(ok(
        &root.run("app", Document::new().with("insert", "root").with("documents", docs(0..3)))
    ));
    // Dropping frees the data, and writes come back at the next read of it on every node.
    assert!(ok(&c.run("app", Document::new().with("drop", "c"))));
    for (i, node) in (0_i32..).zip(&nodes) {
        let mut w = ann(node);
        until("the freed quota", || {
            let r =
                w.run("app", Document::new().with("insert", "d").with("documents", docs(i..i + 1)));
            (ok(&r) && r.get("writeErrors").is_none()).then_some(())
        });
    }
    stop(nodes);
}

#[test]
fn collections_databases_and_search_indexes_stop_at_their_limits() {
    let (nodes, _root) = cluster();
    admin(
        &nodes[0],
        &[
            "tenant",
            "limits",
            "acme",
            "--max-collections",
            "2",
            "--max-databases",
            "1",
            "--max-search-indexes",
            "1",
        ],
    );
    let mut c = ann(&nodes[2]);
    let r = until("the limits on the node", || {
        assert!(ok(&c.run("app", Document::new().with("create", "a"))));
        assert!(ok(&c.run("app", Document::new().with("create", "b"))));
        let r = c.run("app", Document::new().with("create", "x"));
        (!ok(&r)).then_some(r)
    });
    assert_eq!(code(&r), Some(OUT_OF_DISK_SPACE), "{r:?}");
    assert_eq!(errmsg(&r), "tenant acme is over its collection quota: 2 of 2");
    // Writing to a new collection creates it, and is refused the same.
    let r = c.run("app", Document::new().with("insert", "implicit").with("documents", docs(0..1)));
    let text = format!("{r:?}");
    assert!(text.contains("14031") && text.contains("collection quota"), "{r:?}");
    // Another database, past `maxDatabases`.
    assert!(ok(&c.run("app", Document::new().with("drop", "b"))));
    let r = c.run("other", Document::new().with("create", "c"));
    assert_eq!(errmsg(&r), "tenant acme is over its database quota: 1 of 1", "{r:?}");
    // A second search index.
    let index = |name: &str| {
        Value::Document(Document::new().with("name", name).with(
            "definition",
            Document::new().with("mappings", Document::new().with("dynamic", true)),
        ))
    };
    let create = |name: &str| {
        Document::new()
            .with("createSearchIndexes", "a")
            .with("indexes", Value::Array(vec![index(name)]))
    };
    let r = c.run("app", create("one"));
    assert!(ok(&r), "{r:?}");
    let r = c.run("app", create("two"));
    assert_eq!(code(&r), Some(OUT_OF_DISK_SPACE), "{r:?}");
    assert_eq!(errmsg(&r), "tenant acme is over its search index quota: 1 of 1");
    stop(nodes);
}
