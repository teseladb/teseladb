//! Databases in the catalog (D-077, FR-006, FR-027), as the reference router 9.0.4 keeps them in
//! `config.databases` (measured): a write records its database even when it writes nothing, a
//! read does not, a database outlives its last collection until `dropDatabase`, two databases
//! may not differ only in case (13297), and commands on a missing database name it.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

fn s(x: &str) -> Value {
    Value::from(x)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

fn arr(items: Vec<Value>) -> Value {
    Value::Array(items)
}

fn names(m: &mut Mem) -> Vec<String> {
    let r = m.run("admin", doc(&[("listDatabases", 1.into()), ("nameOnly", true.into())]));
    match field(&r, "databases") {
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Document(x) => match field(x, "name") {
                    Value::String(n) => n.clone(),
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn error(r: &Document) -> (i32, String) {
    match (field(r, "code"), field(r, "errmsg")) {
        (Value::Int32(c), Value::String(m)) => (*c, m.clone()),
        other => panic!("{other:?} in {r:?}"),
    }
}

const CLASH: &str = "Database UP could not be created :: caused by :: can't have 2 databases that \
                     just differ on case  have: up want to add: UP";

fn routing(msg: &str) -> String {
    format!("Failed to create RoutingContext in WriteBatchScheduler :: caused by :: {msg}")
}

fn update(q: Value, upsert: bool) -> Document {
    let stmt =
        d(&[("q", q), ("u", d(&[("$set", d(&[("a", 1.into())]))])), ("upsert", upsert.into())]);
    doc(&[("update", s("z")), ("updates", arr(vec![stmt]))])
}

#[test]
fn writes_record_their_database_and_reads_do_not() {
    let mut m = Mem::new();
    m.run("w1", update(d(&[]), false));
    m.run(
        "w2",
        doc(&[
            ("delete", s("z")),
            ("deletes", arr(vec![d(&[("q", d(&[])), ("limit", 0.into())])])),
        ]),
    );
    m.run("w3", doc(&[("findAndModify", s("z")), ("query", d(&[])), ("remove", true.into())]));
    let ops = arr(vec![d(&[("delete", 0.into()), ("filter", d(&[]))])]);
    m.run(
        "admin",
        doc(&[
            ("bulkWrite", 1.into()),
            ("ops", ops),
            ("nsInfo", arr(vec![d(&[("ns", s("w4.z"))])])),
        ]),
    );
    m.run("r1", doc(&[("find", s("z"))]));
    m.run("r2", doc(&[("count", s("z"))]));
    m.run("r3", doc(&[("distinct", s("z")), ("key", s("a"))]));
    assert_eq!(names(&mut m), ["admin", "config", "w1", "w2", "w3", "w4"]);
    // Nothing was created in them.
    assert!(m.catalog.collections.is_empty());
}

#[test]
fn databases_that_differ_only_in_case_fail_the_whole_command() {
    let mut m = Mem::new();
    m.run("up", doc(&[("insert", s("c")), ("documents", arr(vec![d(&[])]))]));
    let insert = doc(&[("insert", s("z")), ("documents", arr(vec![d(&[]), d(&[])]))]);
    let index = d(&[("key", d(&[("a", 1.into())])), ("name", s("a_1"))]);
    let plain = [
        doc(&[("create", s("z"))]),
        insert,
        update(d(&[]), true),
        update(d(&[]), false),
        doc(&[
            ("delete", s("z")),
            ("deletes", arr(vec![d(&[("q", d(&[])), ("limit", 0.into())])])),
        ]),
        doc(&[("createIndexes", s("z")), ("indexes", arr(vec![index]))]),
    ];
    for cmd in plain {
        let r = m.run("UP", cmd.clone());
        assert_eq!(error(&r), (13297, CLASH.to_owned()), "{cmd:?}");
        assert_eq!(field(&r, "codeName"), &s("DatabaseDifferCase"));
        // The router refuses the database: no shard's reply under `raw`, the fields as any error
        // (the reference router 9.0.4's `createIndexes`, battery's crud-database-entries on
        // 04-11-2026).
        let keys: Vec<&str> = r.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["ok", "errmsg", "code", "codeName"], "{cmd:?}");
    }
    let rename = doc(&[("renameCollection", s("up.c")), ("to", s("UP.c"))]);
    assert_eq!(error(&m.run("admin", rename)), (13297, CLASH.to_owned()));
    let fam = doc(&[("findAndModify", s("z")), ("query", d(&[])), ("remove", true.into())]);
    assert_eq!(error(&m.run("UP", fam)), (13297, routing(CLASH)));
    // `bulkWrite` fails before the operations on the other namespaces run.
    let ops = arr(vec![
        d(&[("insert", 0.into()), ("document", d(&[("_id", 1.into())]))]),
        d(&[("insert", 1.into()), ("document", d(&[]))]),
    ]);
    let ns_info = arr(vec![d(&[("ns", s("up.c"))]), d(&[("ns", s("UP.z"))])]);
    let bulk = doc(&[("bulkWrite", 1.into()), ("ops", ops), ("nsInfo", ns_info)]);
    assert_eq!(error(&m.run("admin", bulk)), (13297, routing(CLASH)));
    let count = m.run("up", doc(&[("count", s("c"))]));
    assert_eq!(field(&count, "n"), &Value::Int32(1));
    // Reads of the other spelling answer as an empty database.
    assert_eq!(field(&m.run("UP", doc(&[("find", s("z"))])), "ok"), &Value::Double(1.0));
    assert_eq!(names(&mut m), ["admin", "config", "up"]);
    // Once `up` is dropped, `UP` may be created.
    m.run("up", doc(&[("dropDatabase", 1.into())]));
    assert_eq!(field(&m.run("UP", doc(&[("create", s("z"))])), "ok"), &Value::Double(1.0));
}

#[test]
fn commands_on_a_missing_database_name_it() {
    let mut m = Mem::new();
    m.run("empty", doc(&[("create", s("c"))]));
    m.run("empty", doc(&[("drop", s("c"))]));
    let cases: [(&str, Document, &str); 5] = [
        ("collMod", doc(&[("collMod", s("z"))]), "namespace does not exist: empty.z"),
        ("listIndexes", doc(&[("listIndexes", s("z"))]), "ns does not exist: empty.z"),
        ("dropIndexes", doc(&[("dropIndexes", s("z")), ("index", s("*"))]), "ns not found empty.z"),
        ("collStats", doc(&[("collStats", s("z"))]), ""),
        (
            "rename",
            doc(&[("renameCollection", s("empty.z")), ("to", s("empty.y"))]),
            "Collection empty.z doesn't exist.",
        ),
    ];
    for (name, cmd, in_empty) in cases {
        let db = |x: &str| if name == "rename" { "admin".to_owned() } else { x.to_owned() };
        let missing = if name == "rename" {
            doc(&[("renameCollection", s("none.z")), ("to", s("none.y"))])
        } else {
            cmd.clone()
        };
        let r = m.run(&db("none"), missing);
        assert_eq!(error(&r), (26, "database none not found".to_owned()), "{name}");
        // The router answers it, with the fields of any error (the reference router 9.0.4's
        // `dropIndexes`, battery's crud-database-entries on 04-11-2026).
        let keys: Vec<&str> = r.iter().map(|(k, _)| k).collect();
        assert_eq!(keys, ["ok", "errmsg", "code", "codeName"], "{name}");
        let r = m.run(&db("empty"), cmd);
        if in_empty.is_empty() {
            assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{name}");
        } else {
            assert_eq!(error(&r), (26, in_empty.to_owned()), "{name}");
        }
    }
    let drop = |m: &mut Mem, db: &str| m.run(db, doc(&[("drop", s("z"))]));
    assert_eq!(
        drop(&mut m, "none"),
        doc(&[("info", s("database does not exist")), ("ok", 1.0.into())])
    );
    assert_eq!(drop(&mut m, "empty"), doc(&[("ok", 1.0.into())]));
}
