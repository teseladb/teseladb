//! Collections spread over partitions (003 FR-012, 009 FR-002, D-186): the same writes and reads
//! on a collection created with four partitions and on one created with one give the same
//! answers, in the same order where the command asks for one, and the partitioned one keeps its
//! documents and their index entries under its partitions' collection numbers.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_keys::KeyParts;
use teseladb_wire::commands::{Ctx, Shared, run};
use teseladb_wire::ops::{Guarded, Op, op_type};

/// Runs `cmd` as the node runs a client's command: through the operation's guard, which sends
/// the reads of a collection spread over partitions to each of them.
fn guarded(m: &mut Mem, cmd: Document) -> Document {
    guarded_in(m, &Shared::default(), cmd)
}

/// [`guarded`] with the node's `shared` state, which keeps the cursors between two commands.
fn guarded_in(m: &mut Mem, shared: &Shared, cmd: Document) -> Document {
    let op = Op {
        conn: 1,
        client: None,
        metadata: None,
        app_name: None,
        op: op_type("command"),
        ns: "db.c".to_owned(),
        command: Document::new(),
        started_ms: 0,
        deadline_ms: None,
        killed: false,
        interrupt: None,
        user: None,
        tenant: 0,
        privileges: None,
        roles: None,
        desc: None,
        progress: None,
        metrics: None,
    };
    let id = teseladb_wire::commands::hot::of(shared).ops.begin(op);
    let mut g = Guarded::new(m, shared, id);
    let r = run(&mut Ctx { backend: &mut g, shared, db: "db", cmd: &cmd })
        .expect("a command of the module");
    match r {
        Ok(d) => d,
        Err(e) => e.to_document(),
    }
}

fn s(x: &str) -> Value {
    Value::from(x)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

fn field_of(v: &Value, name: &str) -> Value {
    match v {
        Value::Document(d) => d.get(name).cloned().unwrap_or(Value::Null),
        other => panic!("not a document: {other:?}"),
    }
}

fn batch(r: &Document) -> Vec<Value> {
    match field(r, "cursor.firstBatch") {
        Value::Array(a) => a.clone(),
        other => panic!("no batch in {r:?}: {other:?}"),
    }
}

/// Creates collection `coll` spread over `parts` partitions, with an index on `v` and 3,000
/// documents: more than a scan reads at once, so the scans go on from where they stopped.
fn fill(m: &mut Mem, coll: &str, parts: u8) {
    m.partitions = parts;
    let r = guarded(m, doc(&[("create", s(coll))]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    m.partitions = 1;
    let index = d(&[("key", d(&[("v", 1.into())])), ("name", s("v_1"))]);
    // A catalog change, which does not read the partitions: as the module runs it alone.
    let r = m.run("db", doc(&[("createIndexes", s(coll)), ("indexes", Value::Array(vec![index]))]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let docs: Vec<Value> = (0..3000)
        .map(|i: i32| d(&[("_id", i.into()), ("v", (i % 17).into()), ("g", (i % 5).into())]))
        .collect();
    let r = guarded(m, doc(&[("insert", s(coll)), ("documents", Value::Array(docs))]));
    assert_eq!(field(&r, "n"), &Value::Int32(3000), "{r:?}");
}

/// The answers of a battery of commands on `coll`.
fn answers(m: &mut Mem, coll: &str) -> Vec<Value> {
    let find = |m: &mut Mem, filter: Value, sort: Value, limit: i32| {
        let cmd = doc(&[
            ("find", s(coll)),
            ("filter", filter),
            ("sort", sort),
            ("limit", limit.into()),
            ("batchSize", 1000.into()),
        ]);
        Value::Array(batch(&guarded(m, cmd)))
    };
    let range = d(&[("v", d(&[("$gte", 3.into()), ("$lt", 9.into())]))]);
    let by_v = d(&[("v", 1.into()), ("_id", 1.into())]);
    let mut out = vec![
        find(m, d(&[("_id", 7.into())]), d(&[]), 0),
        find(m, range, by_v.clone(), 0),
        find(m, d(&[]), d(&[("_id", (-1).into())]), 10),
        find(m, d(&[("v", 5.into())]), d(&[("v", 1.into())]), 0),
    ];
    // A filter on a field without an index: every document examined, the accepted ones in
    // whatever order the scan reads them, compared by `_id`.
    let cmd = doc(&[
        ("find", s(coll)),
        ("filter", d(&[("g", 1.into()), ("v", d(&[("$lt", 4.into())]))])),
        ("projection", d(&[("_id", 1.into())])),
        ("batchSize", 100_000.into()),
    ]);
    let mut ids: Vec<i32> = batch(&guarded(m, cmd))
        .iter()
        .map(|d| match field_of(d, "_id") {
            Value::Int32(i) => i,
            other => panic!("{other:?}"),
        })
        .collect();
    ids.sort_unstable();
    out.push(Value::Array(ids.into_iter().map(Value::Int32).collect()));
    // An unordered insert over several partitions, two of its documents already there: the
    // others go in, and the errors name the two by their place in the command.
    let docs: Vec<Value> = [5000, 7, 5001, 5002, 11, 5003]
        .into_iter()
        .map(|i: i32| d(&[("_id", i.into()), ("v", 1.into()), ("g", 9.into())]))
        .collect();
    let cmd =
        doc(&[("insert", s(coll)), ("documents", Value::Array(docs)), ("ordered", false.into())]);
    let r = guarded(m, cmd);
    out.push(field(&r, "n").clone());
    let places: Vec<Value> = match r.get("writeErrors") {
        Some(Value::Array(e)) => e.iter().map(|x| field_of(x, "index")).collect(),
        other => panic!("no write errors: {other:?}"),
    };
    out.push(Value::Array(places));
    let count = guarded(m, doc(&[("count", s(coll)), ("query", d(&[("g", 2.into())]))]));
    out.push(field(&count, "n").clone());
    let group = d(&[("$group", d(&[("_id", s("$g")), ("n", d(&[("$sum", 1.into())]))]))]);
    let sort = d(&[("$sort", d(&[("_id", 1.into())]))]);
    let cmd = doc(&[
        ("aggregate", s(coll)),
        ("pipeline", Value::Array(vec![group, sort])),
        ("cursor", d(&[])),
    ]);
    out.push(Value::Array(batch(&guarded(m, cmd))));
    let inc = d(&[("$inc", d(&[("v", 100.into())]))]);
    let update = d(&[("q", d(&[("g", 3.into())])), ("u", inc), ("multi", true.into())]);
    let r = guarded(m, doc(&[("update", s(coll)), ("updates", Value::Array(vec![update]))]));
    out.push(field(&r, "nModified").clone());
    out.push(find(m, d(&[("v", d(&[("$gt", 100.into())]))]), by_v, 0));
    let delete = d(&[("q", d(&[("g", 4.into())])), ("limit", 0.into())]);
    let r = guarded(m, doc(&[("delete", s(coll)), ("deletes", Value::Array(vec![delete]))]));
    out.push(field(&r, "n").clone());
    let count = guarded(m, doc(&[("count", s(coll))]));
    out.push(field(&count, "n").clone());
    out
}

/// Every document `find` gives for `filter` and `sort` on `coll`, by batches of `size`: its first
/// batch and then each `getMore`, until the cursor ends.
fn found_by_batches(
    m: &mut Mem,
    coll: &str,
    (filter, sort): (Value, Value),
    size: i32,
) -> Vec<Value> {
    let shared = Shared::default();
    let find =
        doc(&[("find", s(coll)), ("filter", filter), ("sort", sort), ("batchSize", size.into())]);
    let r = guarded_in(m, &shared, find.clone());
    let mut out = batch(&r);
    let mut id = field(&r, "cursor.id").clone();
    while id != Value::Int64(0) {
        let cmd = doc(&[("getMore", id), ("collection", s(coll)), ("batchSize", size.into())]);
        let r = guarded_in(m, &shared, cmd);
        let Some(Value::Document(cursor)) = r.get("cursor") else {
            panic!("a getMore of {find:?} after {} documents answered {r:?}", out.len());
        };
        match cursor.get("nextBatch") {
            Some(Value::Array(a)) => out.extend(a.iter().cloned()),
            other => panic!("no next batch in {r:?}: {other:?}"),
        }
        id = field(&r, "cursor.id").clone();
    }
    out
}

/// A `find` over one index that a cursor reads by batches (010 FR-035) gives on a collection
/// spread over partitions every document it gives in one batch, in the same order: its entries
/// are under the partitions' numbers, and its `getMore` goes on from the last one in the
/// interval of the collection's own number (25-11-2026, 0.2.1: the cursor compared the
/// partition's key with the collection's intervals and ended after its first batch, or read the
/// whole collection into one reply). A read backward that went past a chunk of entries took the
/// rest of the partition's keys below the partition's own last key instead of each one's.
#[test]
fn a_cursor_over_four_partitions_reads_by_batches_what_one_batch_reads() {
    let mut m = Mem::new();
    fill(&mut m, "spread", 4);
    fill(&mut m, "plain", 1);
    let reads = [
        (d(&[("v", 5.into())]), d(&[])),
        (d(&[("v", d(&[("$gte", 3.into()), ("$lt", 9.into())]))]), d(&[("v", 1.into())])),
        (d(&[("v", d(&[("$gte", 3.into())]))]), d(&[("v", (-1).into())])),
        (d(&[]), d(&[("_id", 1.into())])),
        (d(&[]), d(&[("_id", (-1).into())])),
    ];
    for (filter, sort) in reads {
        let read = (filter.clone(), sort.clone());
        let whole = found_by_batches(&mut m, "plain", read.clone(), 100_000);
        assert!(whole.len() > 10, "{filter:?}: {} documents", whole.len());
        // A read in one batch of the spread one goes past a scan's chunk too, backward as well.
        for (coll, size) in [("spread", 10), ("plain", 10), ("spread", 100_000)] {
            let by_batches = found_by_batches(&mut m, coll, read.clone(), size);
            assert_eq!(by_batches.len(), whole.len(), "{coll} {size} {filter:?} {sort:?}");
            assert_eq!(by_batches, whole, "{coll} {size} {filter:?} {sort:?}");
        }
    }
}

#[test]
fn a_collection_over_four_partitions_answers_as_one_over_one() {
    let mut m = Mem::new();
    fill(&mut m, "spread", 4);
    fill(&mut m, "plain", 1);
    let spread =
        m.catalog.collections.values().find(|c| c.partitions().is_ok_and(|p| p.count() == 4));
    let parts = spread.expect("a collection over four partitions").partitions().unwrap();
    // Its documents and their entries are under the four numbers, and under each of them.
    let numbers: std::collections::BTreeSet<u32> = m
        .data
        .keys()
        .filter_map(|k| match KeyParts::of(k) {
            Ok(KeyParts::Data { collection, .. }) if parts.contains(collection) => Some(collection),
            _ => None,
        })
        .collect();
    assert_eq!(numbers.len(), 4, "{numbers:?}");
    let (spread, plain) = (answers(&mut m, "spread"), answers(&mut m, "plain"));
    assert_eq!(spread.len(), plain.len());
    for (i, (a, b)) in spread.iter().zip(&plain).enumerate() {
        assert_eq!(a, b, "answer {i}");
    }
    // The option is the catalog's own: `listCollections` does not show it.
    let cmd = doc(&[("listCollections", 1.into()), ("filter", d(&[("name", s("spread"))]))]);
    let listed = format!("{:?}", batch(&guarded(&mut m, cmd)));
    assert!(!listed.contains("partitions"), "{listed}");
}

#[test]
fn a_dropped_collection_over_four_partitions_leaves_no_key_in_any() {
    // The drop deletes the documents and the index entries under every number of the
    // collection, not only its first: the catalog no longer has the collection when its data
    // goes, and a read of its first number alone saw a quarter of it. The history versions next
    // to each document (014) are not counted: the range's GC drops them past its threshold,
    // which this backend does not run.
    let mut m = Mem::new();
    fill(&mut m, "spread", 4);
    let parts = m
        .catalog
        .collections
        .values()
        .find_map(|c| c.partitions().ok().filter(|p| p.count() == 4))
        .expect("a collection over four partitions");
    let left = |m: &Mem| {
        m.data
            .keys()
            .filter(|k| m.types.get(*k) != Some(&teseladb_range::HISTORY_VALUE))
            .filter(|k| match KeyParts::of(k) {
                Ok(KeyParts::Data { collection, .. }) => parts.contains(collection),
                _ => false,
            })
            .count()
    };
    assert_eq!(left(&m), 6000, "3,000 documents and their 3,000 entries of v_1");
    let r = guarded(&mut m, doc(&[("drop", s("spread"))]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(left(&m), 0, "keys of the dropped collection left");
}

#[test]
fn a_retryable_unordered_insert_with_indexes_goes_to_its_partitions_at_once_and_once() {
    // A driver's insert_many is a retryable write, and the load's collections have indexes: such
    // an insert went part after part, each awaited before the next (002 run p36: insert_many p50
    // 11.9 ms against 3.4 for the reference). Its parts now go at once, after one read of the
    // results earlier sends left across ranges; sent again, it answers the same and adds nothing.
    let mut m = Mem::new();
    fill(&mut m, "spread", 4);
    let lsid = Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![9; 16] });
    let docs: Vec<Value> =
        (5000..5008).map(|i: i32| d(&[("_id", i.into()), ("v", (i % 17).into())])).collect();
    let cmd = doc(&[("insert", s("spread")), ("documents", Value::Array(docs))])
        .with("ordered", false)
        .with("lsid", lsid)
        .with("txnNumber", Value::Int64(1));
    let before = m.batches_at_once;
    let r = guarded(&mut m, cmd.clone());
    assert_eq!(field(&r, "n"), &Value::Int32(8), "{r:?}");
    assert!(m.batches_at_once > before, "the parts went one after another");
    let again = guarded(&mut m, cmd);
    assert_eq!(field(&again, "n"), &Value::Int32(8), "{again:?}");
    assert!(again.get("writeErrors").is_none(), "the retry wrote again: {again:?}");
    let count = guarded(&mut m, doc(&[("count", s("spread"))]));
    assert_eq!(field(&count, "n"), &Value::Int32(3008), "{count:?}");
}

#[test]
fn an_index_built_over_four_partitions_writes_them_at_once_and_answers_as_over_one() {
    // The build of an index on a filled collection writes each chunk's entries a part per
    // partition, all at once (D-186), and the index answers as on a collection of one.
    let mut m = Mem::new();
    fill(&mut m, "spread", 4);
    fill(&mut m, "plain", 1);
    let before = m.batches_at_once;
    for coll in ["spread", "plain"] {
        let index = d(&[("key", d(&[("g", 1.into())])), ("name", s("g_1"))]);
        let cmd = doc(&[("createIndexes", s(coll)), ("indexes", Value::Array(vec![index]))]);
        let r = guarded(&mut m, cmd);
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    assert!(m.batches_at_once > before, "the build wrote no chunk over its partitions at once");
    let by_g = |m: &mut Mem, coll: &str| {
        let cmd = doc(&[
            ("find", s(coll)),
            ("filter", d(&[("g", 3.into())])),
            ("hint", s("g_1")),
            ("batchSize", 5000.into()),
        ]);
        let mut ids: Vec<String> =
            batch(&guarded(m, cmd)).iter().map(|v| format!("{:?}", field_of(v, "_id"))).collect();
        ids.sort();
        ids
    };
    let (spread, plain) = (by_g(&mut m, "spread"), by_g(&mut m, "plain"));
    assert_eq!(spread.len(), 600);
    assert_eq!(spread, plain);
}
