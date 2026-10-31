//! Online index builds (009 T012, FR-006): the index goes through `deleteOnly` and `writeOnly`
//! before its backfill, the backfill's writes wait for the `commitQuorum`, `currentOp` shows its
//! progress, `killOp` stops it, and a failed build leaves nothing behind. Replies and errors as a
//! `refnode` and a `refrouter` 9.0.4 give them (measured).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Decimal128, Document, Value};
use teseladb_index::build::{BUILD_DESC, Progress};
use teseladb_keys::KeyBuilder;
use teseladb_meta::indexes::{IndexCommand, IndexState};
use teseladb_range::ack::W;
use teseladb_wire::commands::{Ctx, Shared, lock, run};
use teseladb_wire::ops::{Op, op_type};

const DB: &str = "b";
const DOCS: i32 = 2_500;

/// A node with collection `c` holding `n` documents `{_id: i, a: i % 7}`.
fn seeded(n: i32) -> Mem {
    let mut m = Mem::new();
    let docs: Vec<Value> =
        (0..n).map(|i| Value::Document(Document::new().with("_id", i).with("a", i % 7))).collect();
    let r = m.run(DB, Document::new().with("insert", "c").with("documents", Value::Array(docs)));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    m
}

fn create(key: &str, extra: Option<(&str, Value)>) -> Document {
    let mut spec = Document::new().with("key", Document::new().with(key, 1)).with("name", key);
    let mut cmd = Document::new().with("createIndexes", "c");
    if let Some((k, v)) = extra {
        if k == "commitQuorum" {
            cmd.push(k, v);
        } else {
            spec.push(k, v);
        }
    }
    cmd.with("indexes", Value::Array(vec![Value::Document(spec)]))
}

fn collection(m: &Mem) -> u32 {
    m.catalog.collections.values().next().expect("c").id
}

/// The keys under index `id` of collection `c` and under its guards.
fn leftovers(m: &Mem, c: u32, id: u32) -> usize {
    [id, teseladb_index::prepared::guards_of(id)]
        .into_iter()
        .map(|i| {
            let start = KeyBuilder::index_prefix(0, c, i).as_bytes().to_vec();
            let end = KeyBuilder::index_prefix(0, c, i + 1).as_bytes().to_vec();
            m.data.range(start..end).count()
        })
        .sum()
}

#[test]
fn a_build_goes_through_delete_only_and_write_only_before_its_backfill() {
    let mut m = seeded(DOCS);
    let r = m.run(DB, create("a", None));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let id = m.indexes.indexes.values().next().expect("index").id;
    let states: Vec<String> = m
        .index_log
        .iter()
        .map(|c| match c {
            IndexCommand::Create { .. } => "create".to_owned(),
            IndexCommand::SetState { id: i, state } if *i == id => format!("{state:?}"),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(states, ["create", "Building", "Ready"]);
    assert_eq!(m.indexes.indexes[&id].state, IndexState::Ready);
    let c = collection(&m);
    assert_eq!(leftovers(&m, c, id), DOCS as usize, "one entry per document");
}

#[test]
fn the_backfill_waits_for_the_commit_quorum() {
    let cases = [
        (None, W::Nodes(3)),
        (Some(Value::from("votingMembers")), W::Nodes(3)),
        (Some(Value::from("majority")), W::Majority),
        (Some(Value::Int32(0)), W::Majority),
        (Some(Value::Int32(2)), W::Majority),
        (Some(Value::Int32(3)), W::Nodes(3)),
    ];
    for (quorum, want) in cases {
        let mut m = seeded(DOCS);
        m.concerns.clear();
        let r = m.run(DB, create("a", quorum.clone().map(|q| ("commitQuorum", q))));
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{quorum:?}: {r:?}");
        assert!(!m.concerns.is_empty(), "{quorum:?}");
        assert!(m.concerns.iter().all(|wc| wc.w == want), "{quorum:?}: {:?}", m.concerns);
    }
}

#[test]
fn commit_quorum_answers_as_on_a_refnode() {
    let decimal = |s: &str| Value::Decimal128(s.parse::<Decimal128>().expect("decimal"));
    let shown = [
        (Value::Int32(0), Value::Int32(1)),
        (Value::Int32(1), Value::Int32(1)),
        (Value::Int32(2), Value::Int32(2)),
        (Value::Int32(3), Value::Int32(3)),
        (Value::Double(1.5), Value::Int32(1)),
        (Value::Double(2.9), Value::Int32(2)),
        (Value::Double(0.5), Value::Int32(1)),
        (Value::Double(f64::NAN), Value::Int32(1)),
        (Value::Int64(2), Value::Int32(2)),
        (decimal("2.7"), Value::Int32(3)),
        (Value::from("majority"), Value::from("majority")),
        (Value::from("votingMembers"), Value::from("votingMembers")),
        (Value::Null, Value::from("votingMembers")),
    ];
    for (q, want) in shown {
        let mut m = seeded(3);
        let r = m.run(DB, create("a", Some(("commitQuorum", q.clone()))));
        assert_eq!(r.get("commitQuorum"), Some(&want), "{q:?}: {r:?}");
    }
    let range = "commitQuorum has to be a non-negative number and not greater than 50";
    let refused = [
        (Value::Int32(4), 278, "Not enough data-bearing nodes to satisfy commit quorum"),
        (Value::Int32(50), 278, "Not enough data-bearing nodes to satisfy commit quorum"),
        (Value::Int32(51), 9, range),
        (Value::Int32(-1), 9, range),
        (Value::Double(f64::INFINITY), 9, range),
        (Value::Double(1e300), 9, range),
        (Value::from(""), 9, "commitQuorum can't be an empty string"),
        (
            Value::from("bogus"),
            79,
            "No write concern mode named 'bogus' found in replica set configuration",
        ),
        (
            Value::Boolean(true),
            14,
            "BSON field 'createIndexes.commitQuorum' is the wrong type 'bool', expected types \
             '[long, double, decimal, int, string]'",
        ),
        (
            Value::Document(Document::new()),
            14,
            "BSON field 'createIndexes.commitQuorum' is the wrong type 'object', expected types \
             '[long, double, decimal, int, string]'",
        ),
    ];
    for (q, code, msg) in refused {
        let mut m = seeded(3);
        let r = m.run(DB, create("a", Some(("commitQuorum", q.clone()))));
        assert_eq!(r.get("code"), Some(&Value::Int32(code)), "{q:?}: {r:?}");
        assert_eq!(r.get("errmsg"), Some(&Value::from(msg)), "{q:?}");
        assert!(m.indexes.indexes.is_empty(), "{q:?}: nothing is built");
    }
    // An unsatisfiable quorum is refused even when the index already exists (measured on a
    // `refrouter`).
    let mut m = seeded(3);
    let r = m.run(DB, create("a", None));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = m.run(DB, create("a", Some(("commitQuorum", Value::Int32(5)))));
    assert_eq!(r.get("code"), Some(&Value::Int32(278)), "{r:?}");
}

/// A shared table of operations a scan hook can read while the command runs.
fn leaked() -> &'static Shared {
    Box::leak(Box::default())
}

fn run_on(m: &mut Mem, shared: &'static Shared, cmd: &Document) -> Document {
    run_on_db(m, shared, DB, cmd)
}

fn run_on_db(m: &mut Mem, shared: &'static Shared, db: &str, cmd: &Document) -> Document {
    let mut ctx = Ctx { backend: m, shared, db, cmd };
    match run(&mut ctx).expect("a command of the module") {
        Ok(d) => d,
        Err(e) => e.to_document(),
    }
}

/// The build's operation, while it runs.
fn build_op(shared: &Shared) -> Option<(u32, Op)> {
    lock(shared)
        .ops
        .iter()
        .find(|(_, o)| o.desc.as_deref().is_some_and(|d| d.starts_with(BUILD_DESC)))
        .map(|(id, o)| (id, o.clone()))
}

#[test]
fn current_op_shows_the_progress_of_the_backfill() {
    let mut m = seeded(DOCS);
    let shared = leaked();
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::<(Op, Progress)>::new()));
    let log = seen.clone();
    m.on_scan = Some(Box::new(move || {
        // While the backfill counts the documents, the build has no progress yet.
        if let Some((_, op)) = build_op(shared)
            && let Some(p) = op.progress
        {
            log.borrow_mut().push((op, p));
        }
    }));
    let r = run_on(&mut m, shared, &create("a", None));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let seen = seen.borrow();
    assert!(seen.len() >= 2, "the backfill reads in several chunks: {}", seen.len());
    let (op, _) = &seen[0];
    assert_eq!(op.ns, "b.c");
    assert_eq!(op.op, op_type("createIndexes"));
    let Some(Value::Array(specs)) = op.command.get("indexes") else { panic!("{op:?}") };
    let Value::Document(spec) = &specs[0] else { panic!() };
    assert_eq!(spec.iter().next().map(|(k, _)| k), Some("v"), "the stored specification");
    assert!(seen.iter().all(|(_, p)| p.total == DOCS as u64));
    let done: Vec<u64> = seen.iter().map(|(_, p)| p.done).collect();
    assert!(done.windows(2).all(|w| w[0] <= w[1]), "{done:?}");
    assert!(done.iter().any(|d| *d > 0 && *d < DOCS as u64), "{done:?}");
    assert!(build_op(shared).is_none(), "the operation ends with the build");
}

#[test]
fn current_op_lists_a_build_as_a_refnode_does() {
    let mut m = Mem::new();
    let shared = leaked();
    let command = Document::new().with("createIndexes", "c");
    let op = Op {
        conn: 0,
        client: None,
        metadata: None,
        app_name: None,
        op: op_type("createIndexes"),
        ns: "b.c".to_owned(),
        command: command.clone(),
        started_ms: m.now,
        deadline_ms: None,
        killed: false,
        user: None,
        privileges: None,
        roles: None,
        desc: Some(format!("{BUILD_DESC}3")),
        progress: Some(Progress { done: 500, total: 1000 }),
    };
    lock(shared).ops.begin(op);
    let r = run_on_db(&mut m, shared, "admin", &doc(&[("currentOp", Value::Int32(1))]));
    let Some(Value::Array(ops)) = r.get("inprog") else { panic!("{r:?}") };
    let Value::Document(d) = &ops[0] else { panic!() };
    assert_eq!(d.get("desc"), Some(&Value::from("IndexBuildsCoordinatorRefNode-3")));
    assert_eq!(d.get("active"), Some(&Value::Boolean(false)));
    assert_eq!(d.get("connectionId"), None);
    assert_eq!(
        d.get("msg"),
        Some(&Value::from(
            "Index Build: scanning collection Index Build: scanning collection: 500/1000 50%"
        ))
    );
    let progress = Document::new().with("done", 500_i64).with("total", 1000_i64);
    assert_eq!(d.get("progress"), Some(&Value::Document(progress)));
    // As on a `refnode`: `msg` and `progress` right after `command`.
    let keys: Vec<&str> = d.iter().map(|(k, _)| k).collect();
    let at = keys.iter().position(|k| *k == "command").expect("command");
    assert_eq!(&keys[at + 1..at + 3], ["msg", "progress"]);
}

#[test]
fn kill_op_stops_a_build_and_leaves_nothing() {
    let mut m = seeded(DOCS);
    let c = collection(&m);
    let shared = leaked();
    m.on_scan = Some(Box::new(move || {
        if let Some((id, op)) = build_op(shared)
            && op.progress.is_some_and(|p| p.done > 0)
        {
            lock(shared).ops.kill(id);
        }
    }));
    let r = run_on(&mut m, shared, &create("a", None));
    assert_eq!(r.get("code"), Some(&Value::Int32(11601)), "{r:?}");
    let Some(Value::String(msg)) = r.get("errmsg") else { panic!("{r:?}") };
    assert!(msg.starts_with("Index build failed: "), "{msg}");
    assert!(msg.contains(":: caused by :: collection scan stopped. totalRecords: "), "{msg}");
    assert!(msg.contains("; phase: collection scan; collectionScanPosition: RecordId("), "{msg}");
    assert!(
        msg.ends_with("readSource: kMajorityCommitted :: caused by :: operation was interrupted")
    );
    let id = m.indexes.last_index;
    assert!(m.indexes.of(c).next().is_none(), "the index is gone");
    assert_eq!(leftovers(&m, c, id), 0, "and its entries");
    assert!(build_op(shared).is_none());
}

#[test]
fn a_duplicate_fails_a_unique_build_without_leftovers() {
    let mut m = seeded(DOCS);
    let c = collection(&m);
    let r = m.run(DB, create("a", Some(("unique", Value::Boolean(true)))));
    assert_eq!(field(&r, "code"), &Value::Int32(11000), "{r:?}");
    let id = m.indexes.last_index;
    assert!(m.indexes.of(c).next().is_none());
    assert_eq!(leftovers(&m, c, id), 0);
    // The drop went through `dropping`, as the barrier asks.
    assert!(m.index_log.contains(&IndexCommand::SetState { id, state: IndexState::Dropping }));
    assert_eq!(m.index_log.last(), Some(&IndexCommand::Drop { id }));
}

#[test]
fn a_document_the_index_refuses_fails_the_build_as_on_a_refnode() {
    let mut m = seeded(DOCS);
    let c = collection(&m);
    let parallel = Document::new()
        .with("_id", "parallel")
        .with("a", Value::Array(vec![1.into(), 2.into()]))
        .with("b", Value::Array(vec![3.into(), 4.into()]));
    let r = m.run(
        DB,
        Document::new()
            .with("insert", "c")
            .with("documents", Value::Array(vec![Value::Document(parallel)])),
    );
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    // The node holds only documents yet: the scan reads them in key order.
    let before = m
        .data
        .values()
        .position(|v| {
            let d = Document::from(teseladb_bson::RawDoc::from_bytes(v).expect("a document"));
            d.get("_id") == Some(&Value::from("parallel"))
        })
        .expect("the document");
    let key = Document::new().with("a", 1).with("b", 1);
    let spec = Document::new().with("key", key).with("name", "ab");
    let cmd = Document::new()
        .with("createIndexes", "c")
        .with("indexes", Value::Array(vec![Value::Document(spec)]));
    let r = m.run(DB, cmd);
    assert_eq!(r.get("code"), Some(&Value::Int32(171)), "{r:?}");
    let Some(Value::String(msg)) = r.get("errmsg") else { panic!("{r:?}") };
    // Measured on a `refnode` and a `refrouter` 9.0.4: the documents before it, and its position.
    assert!(msg.starts_with("Index build failed: "), "{msg}");
    let scan = format!(
        ":: caused by :: collection scan stopped. totalRecords: {before}; durationMillis: "
    );
    assert!(msg.contains(&scan), "{msg}");
    let tail = format!(
        "ms; phase: collection scan; collectionScanPosition: RecordId({}); readSource: \
         kMajorityCommitted :: caused by :: cannot index parallel arrays [b] [a]",
        before + 1
    );
    assert!(msg.ends_with(&tail), "{msg}");
    let id = m.indexes.last_index;
    assert!(m.indexes.of(c).next().is_none());
    assert_eq!(leftovers(&m, c, id), 0, "the entries of the documents before it are gone");
}

#[test]
fn a_build_starts_only_with_a_creation() {
    let mut m = seeded(1);
    let c = collection(&m);
    let not_create = IndexCommand::Drop { id: 1 };
    let e = teseladb_wire::commands::index_build::start(&mut m, c, not_create).unwrap_err();
    assert_eq!(e.code.code, 1, "{e:?}");
}

#[test]
fn rules_with_a_bad_delete_only_flag_are_malformed() {
    use teseladb_wire::commands::index_maintenance::read_rules;
    let rule = Document::new()
        .with("id", 1_i64)
        .with("spec", Document::new())
        .with("buckets", 0)
        .with("deleteOnly", 1);
    let doc = Document::new().with("r", Value::Array(vec![Value::Document(rule)]));
    let bytes = doc.to_bytes().expect("bytes");
    let raw = teseladb_bson::RawDoc::from_bytes(&bytes).expect("raw");
    let e = read_rules(raw.get("r")).unwrap_err();
    assert_eq!(e.errmsg, "malformed index rules");
}

#[test]
fn the_guarded_backend_passes_voters_and_ends_the_operation() {
    let mut m = seeded(1);
    let shared = leaked();
    let id = lock(shared).ops.begin(Op {
        conn: 1,
        client: None,
        metadata: None,
        app_name: None,
        op: op_type("insert"),
        ns: "b.c".to_owned(),
        command: Document::new(),
        started_ms: m.now,
        deadline_ms: None,
        killed: false,
        user: None,
        privileges: None,
        roles: None,
        desc: None,
        progress: None,
    });
    let ended = m.ended;
    {
        let mut g = teseladb_wire::ops::Guarded::new(&mut m, shared, id);
        use teseladb_wire::commands::Backend;
        assert_eq!(g.voters(&[1], &[2]), Ok(3));
        g.end_operation();
    }
    assert_eq!(m.ended, ended + 2, "once asked, once when the operation's backend goes");
}
