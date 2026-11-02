//! `local.oplog.rs` over the in-memory node of `common` (018 FR-009, T011): its entries for
//! inserts, updates, replacements, deletes, transactions, catalog changes and notes, its
//! tailable cursor and its refusals, with the fields and order RefDb 9.0.4 writes (measured on
//! 02-11-2026 on a replica set of `ref:9.0`).

mod common;

use std::sync::Mutex;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_wire::commands::txn::resolve_queued;
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::msg::OpMsg;

const DB: &str = "v";

fn run(m: &mut Mem, db: &str, cmd: Document) -> Document {
    let info = NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: None,
    };
    let bytes = OpMsg::encode(1, 0, 0, &cmd.with("$db", db).to_bytes().expect("a command"));
    let mut conn = Connection::new(9);
    conn.receive(&bytes);
    let req: Request = conn.next_request().expect("a request").expect("a request");
    let shared = std::mem::take(&mut m.shared);
    let r = dispatch_with(&mut conn, &req, &info, Some((&mut *m, &shared)));
    resolve_queued(m, &shared);
    m.shared = shared;
    match r {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    }
}

fn node() -> Mem {
    let mut m = Mem::new();
    m.shared = Mutex::new(NodeSessions::new(Timeouts::default()));
    m
}

fn ok(r: &Document) {
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

fn docs(v: &Value) -> Vec<Document> {
    match v {
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// The entries of the oplog matching `filter`, as `find` gives them.
fn oplog(m: &mut Mem, filter: Document) -> Vec<Document> {
    let find = doc(&[("find", Value::from("oplog.rs")), ("filter", Value::Document(filter))]);
    let r = m.run("local", find);
    docs(field(&r, "cursor.firstBatch"))
}

fn keys(d: &Document) -> Vec<&str> {
    d.iter().map(|(k, _)| k).collect()
}

fn ints(v: &[i32]) -> Value {
    Value::Array(v.iter().copied().map(Value::Int32).collect())
}

fn ns_filter(ns: &str) -> Document {
    Document::new().with("ns", ns)
}

fn insert(m: &mut Mem, doc: Document) {
    let cmd = Document::new().with("insert", "p").with("documents", vec![Value::Document(doc)]);
    ok(&run(m, DB, cmd));
}

fn update(m: &mut Mem, q: Document, u: Document) {
    let one = Document::new().with("q", q).with("u", u);
    ok(&run(
        m,
        DB,
        Document::new().with("update", "p").with("updates", vec![Value::Document(one)]),
    ));
}

/// Inserts, updates by operators, a replacement and a delete, as `i`, `u` and `d` entries.
#[test]
fn document_changes() {
    let mut m = node();
    let first = Document::new()
        .with("_id", 1)
        .with("a", 1)
        .with("b", Document::new().with("c", 1).with("d", 2))
        .with("arr", ints(&[1, 2, 3, 4]))
        .with("s", "x");
    insert(&mut m, first.clone());
    let set =
        Document::new().with("a", 2).with("n", 5).with("b.c", 9).with("b.e", 1).with("arr.1", 7);
    let u = Document::new().with("$set", set).with("$unset", Document::new().with("s", 1));
    update(&mut m, Document::new().with("_id", 1), u);
    update(&mut m, Document::new().with("_id", 1), Document::new().with("_id", 1).with("r", 1));
    let del = Document::new().with("q", Document::new().with("_id", 1)).with("limit", 1);
    ok(&run(
        &mut m,
        DB,
        Document::new().with("delete", "p").with("deletes", vec![Value::Document(del)]),
    ));

    let entries = oplog(&mut m, ns_filter("v.p"));
    assert_eq!(entries.len(), 4, "{entries:?}");
    let ins = &entries[0];
    assert_eq!(keys(ins), ["op", "ns", "ui", "o", "o2", "ts", "t", "v", "wall"]);
    assert_eq!(ins.get("op"), Some(&Value::from("i")));
    assert_eq!(ins.get("o"), Some(&Value::Document(first)));
    assert_eq!(ins.get("o2"), Some(&Value::Document(Document::new().with("_id", 1))));
    assert_eq!(ins.get("t"), Some(&Value::Int64(1)));
    assert_eq!(ins.get("v"), Some(&Value::Int64(2)));

    let diff = Document::new()
        .with("d", Document::new().with("s", false))
        .with("u", Document::new().with("a", 2))
        .with("i", Document::new().with("n", 5))
        .with("sarr", Document::new().with("a", true).with("u1", 7))
        .with(
            "sb",
            Document::new()
                .with("u", Document::new().with("c", 9))
                .with("i", Document::new().with("e", 1)),
        );
    let upd = &entries[1];
    assert_eq!(upd.get("op"), Some(&Value::from("u")));
    assert_eq!(
        upd.get("o"),
        Some(&Value::Document(Document::new().with("$v", 2).with("diff", diff)))
    );
    assert_eq!(upd.get("o2"), Some(&Value::Document(Document::new().with("_id", 1))));

    let rep = &entries[2];
    assert_eq!(rep.get("op"), Some(&Value::from("u")));
    assert_eq!(rep.get("o"), Some(&Value::Document(Document::new().with("_id", 1).with("r", 1))));

    let d = &entries[3];
    assert_eq!(keys(d), ["op", "ns", "ui", "o", "ts", "t", "v", "wall"]);
    assert_eq!(d.get("o"), Some(&Value::Document(Document::new().with("_id", 1))));

    // In commit order, each `wall` the milliseconds of its `ts`.
    let ts: Vec<(u32, u32)> = entries
        .iter()
        .map(|e| match e.get("ts") {
            Some(Value::Timestamp { time, increment }) => (*time, *increment),
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(ts.windows(2).all(|w| w[0] < w[1]), "{ts:?}");
}

/// A transaction's writes are one `applyOps` entry on `admin.$cmd` with its `lsid` and
/// `txnNumber`, and a null `prevOpTime`.
#[test]
fn a_transaction_is_one_apply_ops_entry() {
    let mut m = node();
    insert(&mut m, Document::new().with("_id", 1));
    let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![3; 16] });
    let txn = |cmd: Document, start: bool| {
        let cmd = cmd.with("lsid", lsid.clone()).with("txnNumber", Value::Int64(2));
        let cmd = if start { cmd.with("startTransaction", true) } else { cmd };
        cmd.with("autocommit", false)
    };
    let two = Document::new().with("_id", 2);
    let ins = Document::new().with("insert", "p").with("documents", vec![Value::Document(two)]);
    ok(&run(&mut m, DB, txn(ins, true)));
    let one = Document::new()
        .with("q", Document::new().with("_id", 1))
        .with("u", Document::new().with("$set", Document::new().with("y", 1)));
    let upd = Document::new().with("update", "p").with("updates", vec![Value::Document(one)]);
    ok(&run(&mut m, DB, txn(upd, false)));
    ok(&run(&mut m, "admin", txn(Document::new().with("commitTransaction", 1), false)));

    let entries = oplog(&mut m, ns_filter("admin.$cmd"));
    assert_eq!(entries.len(), 1, "{entries:?}");
    let e = &entries[0];
    assert_eq!(
        keys(e),
        ["lsid", "txnNumber", "op", "ns", "o", "ts", "t", "v", "wall", "prevOpTime"]
    );
    assert_eq!(e.get("txnNumber"), Some(&Value::Int64(2)));
    let ops = docs(field(e, "o.applyOps"));
    assert_eq!(ops.len(), 2);
    assert_eq!(keys(&ops[0]), ["op", "ns", "ui", "o", "o2"]);
    assert_eq!(ops[0].get("op"), Some(&Value::from("i")));
    let diff = Document::new().with("i", Document::new().with("y", 1));
    assert_eq!(
        ops[1].get("o"),
        Some(&Value::Document(Document::new().with("$v", 2).with("diff", diff)))
    );
    let null =
        Document::new().with("ts", Value::Timestamp { time: 0, increment: 0 }).with("t", -1i64);
    assert_eq!(e.get("prevOpTime"), Some(&Value::Document(null)));
}

/// The catalog changes are `c` entries on `<db>.$cmd`.
#[test]
fn catalog_changes() {
    let mut m = node();
    let create = Document::new().with("create", "c").with("capped", true).with("size", 4096);
    ok(&run(&mut m, DB, create));
    let index = Document::new().with("key", Document::new().with("x", 1)).with("name", "x_1");
    let ci =
        Document::new().with("createIndexes", "c").with("indexes", vec![Value::Document(index)]);
    ok(&run(&mut m, DB, ci));
    ok(&run(&mut m, DB, Document::new().with("dropIndexes", "c").with("index", "x_1")));
    let rename = Document::new().with("renameCollection", "v.c").with("to", "v.c2");
    ok(&run(&mut m, "admin", rename));
    ok(&run(&mut m, DB, Document::new().with("drop", "c2")));
    ok(&run(&mut m, DB, Document::new().with("dropDatabase", 1)));

    let entries = oplog(&mut m, ns_filter("v.$cmd"));
    let first_keys: Vec<String> = entries
        .iter()
        .map(|e| match e.get("o") {
            Some(Value::Document(o)) => {
                o.iter().next().map(|(k, _)| k.to_owned()).unwrap_or_default()
            }
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        first_keys,
        ["create", "createIndexes", "dropIndexes", "renameCollection", "drop", "dropDatabase"],
        "{entries:?}"
    );
    let create = Document::new().with("create", "c").with("capped", true).with("size", 4096).with(
        "idIndex",
        Document::new()
            .with("v", 2)
            .with("key", Document::new().with("_id", 1))
            .with("name", "_id_"),
    );
    assert_eq!(field(&entries[0], "o"), &Value::Document(create));
    assert_eq!(field(&entries[3], "o.to"), &Value::from("v.c2"));
    assert_eq!(field(&entries[3], "o.stayTemp"), &Value::Boolean(false));
    assert_eq!(keys(&entries[5]), ["op", "ns", "o", "fromMigrate", "ts", "t", "v", "wall"]);
}

/// `appendOplogNote` leaves an `n` entry with an empty `ns` and its data as `o`.
#[test]
fn notes() {
    let mut m = node();
    let note =
        Document::new().with("appendOplogNote", 1).with("data", Document::new().with("hola", 1));
    ok(&run(&mut m, "admin", note));
    let entries = oplog(&mut m, Document::new().with("op", "n"));
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(keys(&entries[0]), ["op", "ns", "o", "ts", "t", "v", "wall"]);
    assert_eq!(entries[0].get("ns"), Some(&Value::from("")));
    assert_eq!(entries[0].get("o"), Some(&Value::Document(Document::new().with("hola", 1))));
}

/// A tailable cursor gives what was written after its last batch; `oplogReplay` is accepted.
#[test]
fn tailable_cursor_reads_on() {
    let mut m = node();
    insert(&mut m, Document::new().with("_id", 1));
    let find = Document::new()
        .with("find", "oplog.rs")
        .with("filter", ns_filter("v.p"))
        .with("tailable", true)
        .with("oplogReplay", true);
    let r = run(&mut m, "local", find);
    assert_eq!(docs(field(&r, "cursor.firstBatch")).len(), 1, "{r:?}");
    let id = field(&r, "cursor.id").clone();
    assert_ne!(id, Value::Int64(0));
    let more = |m: &mut Mem| {
        let r = run(
            m,
            "local",
            Document::new().with("getMore", id.clone()).with("collection", "oplog.rs"),
        );
        docs(field(&r, "cursor.nextBatch"))
    };
    assert!(more(&mut m).is_empty());
    insert(&mut m, Document::new().with("_id", 2));
    let next = more(&mut m);
    assert_eq!(next.len(), 1);
    assert_eq!(field(&next[0], "o._id"), &Value::Int32(2));
}

/// Writes and `drop` on the oplog fail as on RefDb 9.0.4.
#[test]
fn the_oplog_is_read_only() {
    let mut m = node();
    let ins = Document::new()
        .with("insert", "oplog.rs")
        .with("documents", vec![Value::Document(Document::new())]);
    let r = run(&mut m, "local", ins);
    assert_eq!(r.get("code"), Some(&Value::Int32(73)), "{r:?}");
    assert_eq!(r.get("errmsg"), Some(&Value::from("cannot write to local.oplog.rs")));
    let r = run(&mut m, "local", Document::new().with("drop", "oplog.rs"));
    assert_eq!(r.get("code"), Some(&Value::Int32(5_255_000)), "{r:?}");
    assert_eq!(r.get("errmsg"), Some(&Value::from("can't drop live oplog while replicating")));
}
