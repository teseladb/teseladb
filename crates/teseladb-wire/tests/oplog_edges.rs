//! The edges of `local.oplog.rs` over the in-memory node of `common` (018 FR-009, T015): the
//! lower bound a filter on `ts` gives, notes older than the history window, a rename that drops
//! its target, catalog changes of TeselaDB's own that a replica set does not log, a time series
//! with `expireAfterSeconds`, a clustered collection, an index option removed, and
//! the writes of a dropped collection.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_hlc::Hlc;
use teseladb_wire::commands::Backend;
use teseladb_wire::commands::oplog::lower_bound;

const DB: &str = "e";

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

fn oplog(m: &mut Mem, filter: Document) -> Vec<Document> {
    let find = Document::new().with("find", "oplog.rs").with("filter", filter);
    let r = m.run("local", find);
    docs(field(&r, "cursor.firstBatch"))
}

fn ts(time: u32, increment: u32) -> Value {
    Value::Timestamp { time, increment }
}

/// A top-level equality, `$gt` or `$gte` on a timestamp bounds the read; anything else does not.
#[test]
fn the_lower_bound_of_a_filter() {
    let at = |d: Document| lower_bound(&d);
    let eq = at(Document::new().with("ts", ts(10, 0))).expect("a bound");
    let gt = at(Document::new().with("ts", Document::new().with("$gt", ts(10, 0))));
    assert_eq!(gt, Some(eq));
    let both = Document::new().with("$gte", ts(5, 0)).with("$gt", ts(10, 0)).with("$lt", ts(20, 0));
    assert_eq!(at(Document::new().with("ts", both)), Some(eq), "the highest of the bounds");
    assert_eq!(at(Document::new().with("ts", Document::new().with("$lt", ts(9, 0)))), None);
    assert_eq!(at(Document::new().with("ts", 5)), None);
    assert_eq!(at(Document::new().with("op", "i")), None);
    assert!(eq < Hlc { physical: 10_000, logical: u32::MAX });
}

/// A filter on `ts` reads from there on.
#[test]
fn a_read_from_a_timestamp() {
    let mut m = Mem::new();
    let ins = |m: &mut Mem, id: i32| {
        let d = Document::new().with("_id", id);
        ok(&m.run(
            DB,
            Document::new().with("insert", "p").with("documents", vec![Value::Document(d)]),
        ));
    };
    ins(&mut m, 1);
    let first = oplog(&mut m, Document::new().with("ns", "e.p"));
    let Some(t) = first[0].get("ts").cloned() else { panic!("{first:?}") };
    ins(&mut m, 2);
    let after = oplog(&mut m, Document::new().with("ts", Document::new().with("$gt", t)));
    let ids: Vec<&Value> = after.iter().filter_map(|e| e.get("o")).collect();
    assert_eq!(ids, [&Value::Document(Document::new().with("_id", 2))], "{after:?}");
}

/// `appendOplogNote` forgets the notes below the history window.
#[test]
fn old_notes_are_forgotten() {
    let mut m = Mem::new();
    m.now = 1_000_000;
    let note = |m: &mut Mem, n: i32| {
        let cmd =
            Document::new().with("appendOplogNote", 1).with("data", Document::new().with("n", n));
        ok(&m.run("admin", cmd));
    };
    note(&mut m, 1);
    // A day and a bit later, the first note is below the window.
    m.now += 25 * 3600 * 1000;
    note(&mut m, 2);
    let notes = oplog(&mut m, Document::new().with("op", "n"));
    let left: Vec<&Value> = notes.iter().filter_map(|e| e.get("o")).collect();
    assert_eq!(left, [&Value::Document(Document::new().with("n", 2))], "{notes:?}");
}

/// A rename over an existing collection names it in `dropTarget`; a clustered collection and a
/// time series with `expireAfterSeconds` are created as a member writes them; removing an index
/// option logs its old value.
#[test]
fn catalog_edges() {
    let mut m = Mem::new();
    for c in ["a", "b"] {
        ok(&m.run(DB, Document::new().with("create", c)));
    }
    let rename =
        Document::new().with("renameCollection", "e.a").with("to", "e.b").with("dropTarget", true);
    ok(&m.run("admin", rename));
    let clustered = Document::new().with("create", "k").with(
        "clusteredIndex",
        Document::new().with("key", Document::new().with("_id", 1)).with("unique", true),
    );
    ok(&m.run(DB, clustered));
    let series = Document::new()
        .with("create", "t")
        .with("timeseries", Document::new().with("timeField", "at"))
        .with("expireAfterSeconds", 60);
    ok(&m.run(DB, series));
    let ttl = Document::new()
        .with("key", Document::new().with("x", 1))
        .with("name", "x_1")
        .with("expireAfterSeconds", 10);
    ok(&m.run(
        DB,
        Document::new().with("createIndexes", "b").with("indexes", vec![Value::Document(ttl)]),
    ));
    let change = Document::new().with("name", "x_1").with("expireAfterSeconds", 20);
    ok(&m.run(DB, Document::new().with("collMod", "b").with("index", change)));

    let entries = oplog(&mut m, Document::new().with("ns", "e.$cmd"));
    let first = |name: &str| {
        entries
            .iter()
            .find(|e| matches!(e.get("o"), Some(Value::Document(o)) if o.iter().next().map(|(k, _)| k) == Some(name)))
            .unwrap_or_else(|| panic!("{name}: {entries:?}"))
            .clone()
    };
    let rename = first("renameCollection");
    assert!(
        matches!(field(&rename, "o.dropTarget"), Value::Binary { subtype: 4, .. }),
        "{rename:?}"
    );
    let creates: Vec<&Document> = entries
        .iter()
        .filter(|e| matches!(field(e, "o"), Value::Document(o) if o.get("create").is_some()))
        .collect();
    let k = creates.iter().find(|e| field(e, "o.create") == &Value::from("k")).expect("k");
    assert!(matches!(field(k, "o"), Value::Document(o) if o.get("idIndex").is_none()), "{k:?}");
    let t = creates.iter().find(|e| field(e, "o.create") == &Value::from("t")).expect("t");
    let o: Vec<String> = match field(t, "o") {
        Value::Document(o) => o.iter().map(|(k, _)| k.to_owned()).collect(),
        other => panic!("{other:?}"),
    };
    assert_eq!(o, ["create", "clusteredIndex", "expireAfterSeconds", "timeseries"], "{t:?}");
    // The options before a `collMod` of a time series show it clustered.
    ok(&m.run(DB, Document::new().with("collMod", "t").with("expireAfterSeconds", 120)));
    let entries = oplog(&mut m, Document::new().with("o.collMod", "t"));
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(
        field(&entries[0], "o2.collectionOptions_old.clusteredIndex"),
        &Value::Boolean(true),
        "{entries:?}"
    );
    let modified = first("collMod");
    assert_eq!(field(&modified, "o.index.expireAfterSeconds"), &Value::Int32(20));
    assert_eq!(field(&modified, "o2.indexOptions_old.expireAfterSeconds"), &Value::Int32(10));
}

/// The writes of a collection dropped since its writes are left out of the oplog once the drop
/// is read with them: its drop is the last entry it has.
#[test]
fn a_dropped_collection_keeps_its_entries_up_to_its_drop() {
    let mut m = Mem::new();
    let d = doc(&[("_id", Value::Int32(1))]);
    ok(&m.run(DB, Document::new().with("insert", "x").with("documents", vec![Value::Document(d)])));
    m.now += 1;
    ok(&m.run(DB, Document::new().with("drop", "x")));
    m.now += 1;
    let x = oplog(&mut m, Document::new().with("ns", "e.x"));
    assert_eq!(x.len(), 1, "{x:?}");
    let drops = oplog(&mut m, Document::new().with("o.drop", "x"));
    assert_eq!(drops.len(), 1, "{drops:?}");
}

/// A note whose mark another write of Group 0 took goes at the next one; with every mark taken
/// the command fails.
#[test]
fn a_note_takes_the_next_free_mark() {
    let mut m = Mem::new();
    let note = |m: &mut Mem| {
        let data = Document::new().with("x", 1);
        m.run("admin", Document::new().with("appendOplogNote", 1).with("data", data))
    };
    m.group0_conflicts = 1;
    ok(&note(&mut m));
    // The note is one logical tick past the cluster's mark: it shows once the mark passes it.
    assert_eq!(oplog(&mut m, Document::new().with("op", "n")).len(), 0);
    m.now += 1;
    assert_eq!(oplog(&mut m, Document::new().with("op", "n")).len(), 1);
    m.group0_conflicts = usize::MAX;
    let r = note(&mut m);
    assert_eq!(field(&r, "errmsg"), &Value::from("appendOplogNote found no free mark"), "{r:?}");
}

/// The `create` of a view has no `idIndex`; a collection whose catalog entry has no options
/// gives `create` alone; the `collMod` of an index names only the options that changed, an
/// option taken away with its default.
#[test]
fn more_catalog_entries() {
    let mut m = Mem::new();
    // As the server creates the collections of `config`: with no options at all.
    let bare = teseladb_meta::catalog::CatalogCommand::Create {
        ns: teseladb_meta::catalog::Namespace { db: DB.into(), coll: "bare".into() },
        uuid: [3; 16],
        options: Vec::new(),
        if_absent: false,
    };
    m.catalog_write(bare).expect("created");
    ok(&m.run(DB, Document::new().with("create", "p")));
    let view = Document::new()
        .with("create", "v")
        .with("viewOn", "p")
        .with("pipeline", Vec::<Value>::new());
    ok(&m.run(DB, view));
    let spec = Document::new()
        .with("key", Document::new().with("x", 1))
        .with("name", "x_1")
        .with("sparse", true)
        .with("hidden", true);
    let create =
        Document::new().with("createIndexes", "p").with("indexes", vec![Value::Document(spec)]);
    ok(&m.run(DB, create));
    let change = Document::new().with("name", "x_1").with("hidden", false);
    ok(&m.run(DB, Document::new().with("collMod", "p").with("index", change)));
    let entries = oplog(&mut m, Document::new().with("ns", "e.$cmd"));
    let named = |name: &str, coll: &str| {
        entries
            .iter()
            .find(|e| matches!(field(e, "o"), Value::Document(o) if o.get(name) == Some(&Value::from(coll))))
            .unwrap_or_else(|| panic!("{name} {coll}: {entries:?}"))
            .clone()
    };
    let v = named("create", "v");
    assert!(matches!(field(&v, "o"), Value::Document(o) if o.get("idIndex").is_none()), "{v:?}");
    let bare = named("create", "bare");
    assert_eq!(
        field(&bare, "o"),
        &Value::Document(
            Document::new()
                .with("create", "bare")
                .with("idIndex", field(&bare, "o.idIndex").clone())
        )
    );
    let modified = named("collMod", "p");
    assert_eq!(
        field(&modified, "o.index"),
        &Value::Document(Document::new().with("name", "x_1").with("hidden", false))
    );
    assert_eq!(
        field(&modified, "o2.indexOptions_old"),
        &Value::Document(Document::new().with("hidden", true))
    );
}
