//! `listDatabases` (FR-006, FR-027) as a `refrouter` 9.0.4 answers it (measured): `admin` and
//! `config` always listed, each database with its size and `empty`, the totals, and `filter`
//! applied to what `nameOnly` leaves.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_wire::commands::catalog::{BLOCK_BYTES, collection_size};

fn s(x: &str) -> Value {
    Value::from(x)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

fn list(m: &mut Mem, extra: &[(&str, Value)]) -> Document {
    let mut cmd = doc(&[("listDatabases", 1.into())]);
    for (k, v) in extra {
        cmd.push(*k, v.clone());
    }
    m.run("admin", cmd)
}

fn databases(r: &Document) -> Vec<Document> {
    match field(r, "databases") {
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Document(x) => x.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_collection_takes_whole_blocks_and_one_for_its_id_index() {
    assert_eq!(BLOCK_BYTES, 4096);
    assert_eq!(collection_size(0), 8192);
    assert_eq!(collection_size(1), 8192);
    assert_eq!(collection_size(4096), 8192);
    assert_eq!(collection_size(4097), 12_288);
}

#[test]
fn admin_and_config_are_always_there_with_the_databases_in_name_order() {
    let mut m = Mem::new();
    let r = list(&mut m, &[]);
    let names: Vec<Value> = databases(&r).iter().map(|x| field(x, "name").clone()).collect();
    assert_eq!(names, vec![s("admin"), s("config")]);
    for x in databases(&r) {
        assert_eq!(field(&x, "sizeOnDisk"), &Value::Int64(8192));
        assert_eq!(field(&x, "empty"), &Value::Boolean(false));
    }
    assert_eq!(field(&r, "totalSize"), &Value::Int64(16_384));
    assert_eq!(field(&r, "totalSizeMb"), &Value::Int64(0));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0));
}

#[test]
fn sizes_count_the_stored_documents_of_each_collection() {
    let mut m = Mem::new();
    m.run("zeta", doc(&[("create", s("e"))]));
    let big = "x".repeat(5000);
    let docs = (0..3).map(|i| d(&[("_id", Value::Int32(i)), ("b", s(&big))])).collect();
    m.run("beta", doc(&[("insert", s("c")), ("documents", Value::Array(docs))]));
    m.run("beta", doc(&[("insert", s("d")), ("documents", Value::Array(vec![d(&[])]))]));
    let r = list(&mut m, &[]);
    let dbs = databases(&r);
    let names: Vec<&Value> = dbs.iter().map(|x| field(x, "name")).collect();
    assert_eq!(names, vec![&s("admin"), &s("beta"), &s("config"), &s("zeta")]);
    let stored: u64 = m.stored(..).iter().map(|(k, v)| (k.len() + v.len()) as u64).sum();
    let d_bytes: u64 =
        m.data.iter().filter(|(_, v)| v.len() < 100).map(|(k, v)| (k.len() + v.len()) as u64).sum();
    let beta = collection_size(stored - d_bytes) + collection_size(d_bytes);
    assert_eq!(field(&dbs[1], "sizeOnDisk"), &Value::Int64(beta as i64));
    // An empty collection still takes its blocks, as in RefEngine.
    assert_eq!(field(&dbs[3], "sizeOnDisk"), &Value::Int64(8192));
    let total = beta + 3 * 8192;
    assert_eq!(field(&r, "totalSize"), &Value::Int64(total as i64));
}

#[test]
fn the_filter_sees_what_name_only_leaves() {
    let mut m = Mem::new();
    m.run("db", doc(&[("create", s("c"))]));
    let r = list(&mut m, &[("filter", d(&[("sizeOnDisk", d(&[("$gt", 0.into())]))]))]);
    assert_eq!(databases(&r).len(), 3);
    let r = list(
        &mut m,
        &[("nameOnly", true.into()), ("filter", d(&[("sizeOnDisk", d(&[("$gt", 0.into())]))]))],
    );
    assert!(databases(&r).is_empty());
    assert!(r.get("totalSize").is_none());
    let r = list(&mut m, &[("nameOnly", true.into()), ("filter", d(&[("name", s("db"))]))]);
    assert_eq!(databases(&r), vec![doc(&[("name", s("db"))])]);
}

#[test]
fn sizes_read_large_collections_in_chunks() {
    let mut m = Mem::new();
    let docs = (0..2_500).map(|i| d(&[("_id", Value::Int32(i))])).collect();
    m.run("db", doc(&[("insert", s("c")), ("documents", Value::Array(docs))]));
    let stored: u64 = m.stored(..).iter().map(|(k, v)| (k.len() + v.len()) as u64).sum();
    let r = list(&mut m, &[("filter", d(&[("name", s("db"))]))]);
    let size = collection_size(stored) as i64;
    assert_eq!(
        databases(&r),
        vec![doc(&[
            ("name", s("db")),
            ("sizeOnDisk", Value::Int64(size)),
            ("empty", Value::Boolean(false)),
            ("shards", d(&[("node1", Value::Int64(size))])),
        ])]
    );
}

#[test]
fn local_admin_and_config_follow_the_rules_of_a_refrouter() {
    use teseladb_wire::commands::names::{LOCAL_DB, check_db_reachable};
    let e = check_db_reachable(LOCAL_DB).expect_err("local");
    assert_eq!((e.code.code, e.errmsg.as_str()), (20, "Can't use 'local' database through refrouter"));
    assert!(check_db_reachable("db").is_ok());
    let mut m = Mem::new();
    for db in ["admin", "config"] {
        let r = m.run(db, doc(&[("drop", Value::from("c"))]));
        let want = format!("Cannot drop collection in '{db}' database in sharded cluster");
        assert_eq!(r.get("errmsg"), Some(&Value::from(want.as_str())));
        let r = m.run(db, doc(&[("dropDatabase", 1.into())]));
        let want = format!("Cannot drop the {db} database");
        assert_eq!(r.get("errmsg"), Some(&Value::from(want.as_str())));
    }
    let rename =
        doc(&[("renameCollection", Value::from("config.a")), ("to", Value::from("config.b"))]);
    let r = m.run("admin", rename);
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from("Can't rename a collection in the config database"))
    );
    let r = m.run("db", doc(&[("create", Value::from("a\0b"))]));
    assert_eq!(
        r.get("errmsg"),
        Some(&Value::from("namespaces cannot have embedded null characters"))
    );
}

fn has_empty(m: &mut Mem, extra: &[(&str, Value)]) -> bool {
    databases(&list(m, extra)).iter().any(|x| field(x, "name") == &s("gone"))
}

#[test]
fn a_database_without_collections_is_listed_empty_until_drop_database() {
    // D-077, measured on a `refrouter` 9.0.4: the database of a dropped last collection stays with
    // `sizeOnDisk: 0` and `empty: true`, and counts nothing in `totalSize`.
    let mut m = Mem::new();
    m.run("gone", doc(&[("create", s("c"))]));
    m.run("gone", doc(&[("drop", s("c"))]));
    let r = list(&mut m, &[]);
    let dbs = databases(&r);
    assert_eq!(
        dbs[2],
        doc(&[("name", s("gone")), ("sizeOnDisk", Value::Int64(0)), ("empty", true.into())])
    );
    assert_eq!(field(&r, "totalSize"), &Value::Int64(16_384));
    let names = list(&mut m, &[("nameOnly", true.into())]);
    assert_eq!(databases(&names)[2], doc(&[("name", s("gone"))]));
    let lc = m.run("gone", doc(&[("listCollections", 1.into())]));
    assert_eq!(field(&lc, "ok"), &Value::Double(1.0));
    // `refrouter` adds such a database only with no filter or a filter of one positive predicate
    // on `name`.
    let shown = [
        d(&[("name", s("gone"))]),
        d(&[("name", d(&[("$in", Value::Array(vec![s("gone")]))]))]),
        d(&[("name", d(&[("$gt", s("d"))]))]),
        d(&[("name", d(&[("$exists", true.into())]))]),
        d(&[("name", d(&[("$type", s("string"))]))]),
        d(&[("name", d(&[("$regex", s("^go"))]))]),
    ];
    for f in shown {
        assert!(has_empty(&mut m, &[("filter", f.clone())]), "{f:?}");
    }
    let hidden = [
        d(&[]),
        d(&[("empty", true.into())]),
        d(&[("sizeOnDisk", 0.into())]),
        d(&[("name", s("gone")), ("empty", true.into())]),
        d(&[("name", d(&[("$ne", s("zz"))]))]),
        d(&[("name", d(&[("$nin", Value::Array(vec![s("zz")]))]))]),
        d(&[("name", d(&[("$gte", s("gone")), ("$lte", s("gone"))]))]),
        d(&[("$and", Value::Array(vec![d(&[("name", s("gone"))])]))]),
        d(&[("$or", Value::Array(vec![d(&[("name", s("gone"))])]))]),
        d(&[("name", s("other"))]),
    ];
    for f in hidden {
        assert!(!has_empty(&mut m, &[("filter", f.clone())]), "{f:?}");
    }
    // `dropDatabase` removes it.
    assert_eq!(
        field(&m.run("gone", doc(&[("dropDatabase", 1.into())])), "ok"),
        &Value::Double(1.0)
    );
    assert!(!has_empty(&mut m, &[]));
}

/// Each database with data names where it is (measured on a `refrouter` 9.0.4, DV-031, D-102):
/// `admin` and `config` on the config server, the others on the node; the filter does not see
/// it, and an empty database and `nameOnly` have none.
#[test]
fn each_database_names_its_shard_after_the_filter() {
    let mut m = Mem::new();
    let one = Value::Array(vec![d(&[("a", 1.into())])]);
    m.run("d", doc(&[("insert", s("c")), ("documents", one)]));
    m.run("e", doc(&[("create", s("c"))]));
    m.run("e", doc(&[("drop", s("c"))]));
    let r = list(&mut m, &[]);
    let shards: Vec<(Value, Option<Value>)> = databases(&r)
        .iter()
        .map(|x| {
            let on = x.get("shards").cloned();
            (field(x, "name").clone(), on)
        })
        .collect();
    let size = |name: &str| -> Value {
        let found = databases(&r).into_iter().find(|x| field(x, "name") == &s(name));
        field(&found.unwrap(), "sizeOnDisk").clone()
    };
    let at = |shard: &str, name: &str| Some(d(&[(shard, size(name))]));
    let want = vec![
        (s("admin"), at("config", "admin")),
        (s("config"), at("config", "config")),
        (s("d"), at("node1", "d")),
        (s("e"), None),
    ];
    assert_eq!(shards, want);
    let r = list(&mut m, &[("filter", d(&[("shards.node1", d(&[("$exists", true.into())]))]))]);
    assert!(databases(&r).is_empty(), "{r:?}");
    let r = list(&mut m, &[("nameOnly", true.into())]);
    assert!(databases(&r).iter().all(|x| x.get("shards").is_none()), "{r:?}");
}
