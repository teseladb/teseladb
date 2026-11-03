//! `shardCollection` and `enableSharding` (018 FR-001, T004) as a `refrouter` 9.0.4 of one shard
//! answered them on 01-11-2026: the key's rules and their messages, the namespaces and
//! collections that cannot be sharded, the index it makes or needs, a repeated call, and the
//! entries of `config.collections`.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, db: &str, cmd: &str) -> Document {
    m.run(db, json(cmd))
}

fn shard(m: &mut Mem, cmd: &str) -> Document {
    run(m, "admin", cmd)
}

fn refused(r: &Document, code: i32, msg: &str) {
    assert_eq!(field(r, "code"), &Value::Int32(code), "{r:?}");
    assert_eq!(field(r, "errmsg"), &Value::from(msg), "{r:?}");
}

fn index_keys(m: &mut Mem, coll: &str) -> Vec<Document> {
    let r = run(m, "shk", &format!(r#"{{"listIndexes":"{coll}"}}"#));
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => {
                let mut out = Document::new();
                for k in ["key", "name", "unique"] {
                    if let Some(v) = d.get(k) {
                        out.push(k, v.clone());
                    }
                }
                out
            }
            other => panic!("{other:?}"),
        })
        .collect()
}

#[test]
fn a_new_collection_gets_its_key_and_its_index() {
    let mut m = Mem::new();
    let r = m.run("admin", doc(&[("enableSharding", Value::from("shk"))]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    for (coll, key, index) in [
        ("c", r#"{"tienda":1}"#, r#"{"key":{"tienda":1},"name":"tienda_1"}"#),
        ("h", r#"{"tienda":"hashed"}"#, r#"{"key":{"tienda":"hashed"},"name":"tienda_hashed"}"#),
        ("cmp", r#"{"a":1,"b":"hashed"}"#, r#"{"key":{"a":1,"b":"hashed"},"name":"a_1_b_hashed"}"#),
    ] {
        let r = shard(&mut m, &format!(r#"{{"shardCollection":"shk.{coll}","key":{key}}}"#));
        assert_eq!(field(&r, "collectionsharded"), &Value::from(format!("shk.{coll}")), "{r:?}");
        let id = json(r#"{"key":{"_id":1},"name":"_id_"}"#);
        assert_eq!(index_keys(&mut m, coll), vec![id, json(index)], "{coll}");
    }
    let r = shard(&mut m, r#"{"shardCollection":"shk.u","key":{"tienda":1},"unique":true}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let unique = json(r#"{"key":{"tienda":1},"name":"tienda_1","unique":true}"#);
    assert_eq!(index_keys(&mut m, "u").get(1), Some(&unique));
    // A key of `_id` ascending is served by the index of `_id`.
    let r = shard(&mut m, r#"{"shardCollection":"shk.id2","key":{"_id":1}}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(index_keys(&mut m, "id2").len(), 1);
    // `listCollections` shows no shard key among the options.
    let r = run(&mut m, "shk", r#"{"listCollections":1,"filter":{"name":"c"}}"#);
    assert_eq!(field(&r, "cursor.firstBatch.0.options"), &Value::Document(Document::new()));
    let r = run(
        &mut m,
        "config",
        r#"{"find":"collections","filter":{"_id":"shk.u"},"projection":{"key":1,"unique":1}}"#,
    );
    assert_eq!(
        field(&r, "cursor.firstBatch.0"),
        &Value::Document(json(r#"{"_id":"shk.u","key":{"tienda":1},"unique":true}"#)),
        "{r:?}"
    );
}

#[test]
fn a_repeated_call_answers_the_same_and_another_key_is_refused() {
    let mut m = Mem::new();
    let cmd = r#"{"shardCollection":"shk.c","key":{"tienda":1}}"#;
    assert_eq!(field(&shard(&mut m, cmd), "ok"), &Value::Double(1.0));
    assert_eq!(field(&shard(&mut m, cmd), "collectionsharded"), &Value::from("shk.c"));
    let r = shard(&mut m, r#"{"shardCollection":"shk.c","key":{"other":1}}"#);
    refused(
        &r,
        23,
        "Collection 'shk.c' already exists with a different shard key. Requested { other: 1 } but found { tienda: 1 }",
    );
}

#[test]
fn keys_that_break_the_rules_are_refused() {
    let mut m = Mem::new();
    let cases = [
        (r#""key":{}"#, 2, "Shard key is empty"),
        (
            r#""key":{"a":"text"}"#,
            2,
            r#"Shard key { a: "text" } is invalid. Non-hashed fields must be set to a value of 1 (ascending). Failed to parse field a"#,
        ),
        (
            r#""key":{"a":-1}"#,
            2,
            "Shard key { a: -1 } is invalid. Non-hashed fields must be set to a value of 1 (ascending). Failed to parse field a",
        ),
        (
            r#""key":{"a":"hashed","b":"hashed"}"#,
            2,
            r#"Shard key { a: "hashed", b: "hashed" } can contain at most one 'hashed' field. Failed to parse field b"#,
        ),
        (
            r#""key":{"a":"hashed"},"unique":true"#,
            72,
            "Hashed shard keys cannot be declared unique. It's possible to ensure uniqueness on the hashed field by declaring an additional (non-hashed) unique index on the field.",
        ),
        (r#""key":{"a.$b":1}"#, 2, "Field a.$b contains parts that start with '$'"),
        (
            r#""key":{"a":1},"collation":{"locale":"fr"}"#,
            2,
            r#"The collation for shardCollection must be {locale: 'simple'}, but found: { locale: "fr" }"#,
        ),
    ];
    for (rest, code, msg) in cases {
        let r = shard(&mut m, &format!(r#"{{"shardCollection":"shk.bad",{rest}}}"#));
        refused(&r, code, msg);
    }
    let r = shard(
        &mut m,
        r#"{"shardCollection":"shk.v","key":{"a":1},"collation":{"locale":"simple"}}"#,
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}

#[test]
fn namespaces_and_collections_that_cannot_be_sharded() {
    let mut m = Mem::new();
    refused(
        &shard(&mut m, r#"{"shardCollection":"nodb","key":{"a":1}}"#),
        73,
        "Invalid namespace specified 'nodb'",
    );
    for ns in ["admin.x", "config.x", "s2.system.x"] {
        let r = shard(&mut m, &format!(r#"{{"shardCollection":"{ns}","key":{{"a":1}}}}"#));
        refused(
            &r,
            20,
            &format!("Special collection '{ns}' can't be registered in the sharding catalog"),
        );
    }
    let r = run(&mut m, "shk", r#"{"shardCollection":"shk.z","key":{"a":1}}"#);
    refused(&r, 13, "shardCollection may only be run against the admin database.");
    run(&mut m, "s2", r#"{"insert":"e","documents":[{"_id":1,"a":1}]}"#);
    run(&mut m, "s2", r#"{"create":"vw","viewOn":"e","pipeline":[]}"#);
    let r = shard(&mut m, r#"{"shardCollection":"s2.vw","key":{"a":1}}"#);
    refused(
        &r,
        166,
        "Cannot shard collection 's2.vw' because a view already exists with the same namespace",
    );
    run(&mut m, "s2", r#"{"create":"cap","capped":true,"size":4096}"#);
    refused(
        &shard(&mut m, r#"{"shardCollection":"s2.cap","key":{"a":1}}"#),
        72,
        "can't shard a capped collection",
    );
}

#[test]
fn a_collection_with_documents_needs_its_index() {
    let mut m = Mem::new();
    run(&mut m, "s2", r#"{"insert":"e","documents":[{"_id":1,"a":1},{"_id":2,"a":2},{"_id":3}]}"#);
    let r = shard(&mut m, r#"{"shardCollection":"s2.e","key":{"a":1}}"#);
    refused(
        &r,
        72,
        "Please create an index that starts with the proposed shard key before sharding the collection. ",
    );
    run(&mut m, "s2", r#"{"createIndexes":"e","indexes":[{"key":{"a":1},"name":"a_1"}]}"#);
    let r = shard(&mut m, r#"{"shardCollection":"s2.e","key":{"a":1},"unique":true}"#);
    refused(
        &r,
        72,
        "can't shard collection s2.e, { a: 1 } index not unique, and unique index explicitly specified",
    );
    let r = shard(&mut m, r#"{"shardCollection":"s2.e","key":{"a":1}}"#);
    assert_eq!(field(&r, "collectionsharded"), &Value::from("s2.e"), "{r:?}");
    let r = run(&mut m, "s2", r#"{"find":"e","sort":{"_id":1}}"#);
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(b.len(), 3);
}

/// A unique key over an existing unique index and a hashed `_id` key are accepted (measured on
/// the same `refrouter`); the hashed key gets an index of its own, with RefDb's default name.
#[test]
fn a_unique_index_serves_a_unique_key_and_a_hashed_id_gets_its_index() {
    let mut m = Mem::new();
    run(&mut m, "shk", r#"{"insert":"u2","documents":[{"_id":1,"a":1}]}"#);
    let r = run(
        &mut m,
        "shk",
        r#"{"createIndexes":"u2","indexes":[{"key":{"a":1},"name":"a_1","unique":true}]}"#,
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = shard(&mut m, r#"{"shardCollection":"shk.u2","key":{"a":1},"unique":true}"#);
    assert_eq!(field(&r, "collectionsharded"), &Value::from("shk.u2"), "{r:?}");
    let r = shard(&mut m, r#"{"shardCollection":"shk.id","key":{"_id":"hashed"}}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let hashed = json(r#"{"key":{"_id":"hashed"},"name":"_id_hashed"}"#);
    assert_eq!(index_keys(&mut m, "id").get(1), Some(&hashed));
}

/// The arguments of `shardCollection` and `enableSharding` with the wrong type or missing, and
/// the numbers that say 1 in a shard key and in the index that serves it, as a `refrouter` 9.0.4
/// answers them (measured on 03-11-2026, `sh1.js`, `sh2.js`): any numeric 1, a decimal too, and
/// an index field of another number does not serve.
#[test]
fn arguments_and_the_numbers_of_a_key() {
    let mut m = Mem::new();
    refused(
        &shard(&mut m, r#"{"enableSharding":5}"#),
        14,
        "BSON field 'enableSharding.enableSharding' is the wrong type 'int', expected type 'string'",
    );
    refused(
        &shard(&mut m, r#"{"shardCollection":5,"key":{"a":1}}"#),
        73,
        "Collection name has invalid type int",
    );
    refused(
        &shard(&mut m, r#"{"shardCollection":".t","key":{"a":1}}"#),
        73,
        "Invalid namespace specified '.t'",
    );
    refused(
        &shard(&mut m, r#"{"shardCollection":"shk.k","key":5}"#),
        14,
        "BSON field '.key' is the wrong type 'int', expected type 'object'",
    );
    refused(
        &shard(&mut m, r#"{"shardCollection":"shk.k"}"#),
        40414,
        "BSON field '.key' is missing but a required field",
    );
    for (coll, key) in
        [("l", r#"{"$numberLong":"1"}"#), ("d", r#"{"$numberDecimal":"1"}"#), ("f", "1.0")]
    {
        let r =
            shard(&mut m, &format!(r#"{{"shardCollection":"shk.{coll}","key":{{"a":{key}}}}}"#));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{coll}: {r:?}");
    }
    for (coll, index, serves) in [
        ("i1", r#"{"$numberLong":"1"}"#, true),
        ("i2", r#"{"$numberDecimal":"1"}"#, true),
        ("i3", "2.0", false),
        ("i4", r#"{"$numberLong":"2"}"#, false),
    ] {
        let create = format!(
            r#"{{"createIndexes":"{coll}","indexes":[{{"key":{{"a":{index}}},"name":"a"}}]}}"#
        );
        assert_eq!(field(&run(&mut m, "shk", &create), "ok"), &Value::Double(1.0));
        run(&mut m, "shk", &format!(r#"{{"insert":"{coll}","documents":[{{"a":1}}]}}"#));
        let r = shard(&mut m, &format!(r#"{{"shardCollection":"shk.{coll}","key":{{"a":1}}}}"#));
        match serves {
            true => assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{coll}: {r:?}"),
            false => refused(
                &r,
                72,
                "Please create an index that starts with the proposed shard key before sharding the collection. ",
            ),
        }
    }
}

/// The indexes that start with the shard key but cannot serve it, as a `refrouter` 9.0.4 refuses
/// them (measured on 03-11-2026, `sh3.js`, `sh4.js`): each index in its order with its reason,
/// or the request for an index; the documents stay as they were.
#[test]
fn indexes_that_cannot_serve_the_key() {
    let mut m = Mem::new();
    let prefix = "can't shard collection because an index already exists on the shard key and it's a non shard key valid index. :: caused by :: couldn't find valid index for shard key\nIndex _id_ cannot be used for sharding because: Shard key is not a prefix of index key.";
    let please = "Please create an index that starts with the proposed shard key before sharding the collection. ";
    let cases: [(&str, &str, &str, String); 5] = [
        (
            "m1",
            r#"{"key":{"a":1},"name":"a_1"}"#,
            r#"[{"_id":1,"a":[1,2]},{"_id":2,"a":3}]"#,
            format!(
                "{prefix}\nIndex a_1 cannot be used for sharding because: Index key is multikey."
            ),
        ),
        (
            "m3",
            r#"{"key":{"a":1},"name":"a_1","sparse":true}"#,
            r#"[{"_id":1,"a":1}]"#,
            format!(
                "{please} Index a_1 cannot be used for sharding because [ Index key is sparse. ]"
            ),
        ),
        (
            "m4",
            r#"{"key":{"a":1},"name":"a_1","partialFilterExpression":{"a":{"$gt":0}}}"#,
            r#"[{"_id":1,"a":1}]"#,
            format!(
                "{prefix}\nIndex a_1 cannot be used for sharding because: Index key is partial."
            ),
        ),
        (
            "m6",
            r#"{"key":{"a":1,"b":1},"name":"a_1_b_1"}"#,
            r#"[{"_id":1,"a":1,"b":[1,2]}]"#,
            please.to_owned(),
        ),
        (
            "m7",
            r#"{"key":{"a":1},"name":"a_1","hidden":true}"#,
            r#"[{"_id":1,"a":1}]"#,
            prefix.to_owned(),
        ),
    ];
    for (coll, index, docs, msg) in cases {
        let create = format!(r#"{{"createIndexes":"{coll}","indexes":[{index}]}}"#);
        assert_eq!(field(&run(&mut m, "shk", &create), "ok"), &Value::Double(1.0), "{coll}");
        run(&mut m, "shk", &format!(r#"{{"insert":"{coll}","documents":{docs}}}"#));
        let r = shard(&mut m, &format!(r#"{{"shardCollection":"shk.{coll}","key":{{"a":1}}}}"#));
        refused(&r, 72, &msg);
        let r = run(&mut m, "shk", &format!(r#"{{"count":"{coll}"}}"#));
        let n = docs.matches("_id").count();
        assert_eq!(field(&r, "n"), &Value::Int32(i32::try_from(n).unwrap()), "{coll}");
    }
    // A multikey field after the shard key's does not stop another index that serves it.
    let both = r#"[{"key":{"a":1,"b":1},"name":"a_1_b_1"},{"key":{"a":1},"name":"a_1"}]"#;
    run(&mut m, "shk", &format!(r#"{{"createIndexes":"m5","indexes":{both}}}"#));
    run(&mut m, "shk", r#"{"insert":"m5","documents":[{"_id":1,"a":1,"b":[1,2]}]}"#);
    let r = shard(&mut m, r#"{"shardCollection":"shk.m5","key":{"a":1}}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}

/// Documents of a dotted shard key: a missing or scalar parent is a null key, an array on the
/// way is refused with each insert's error (measured on a `refrouter` 9.0.4 on 03-11-2026,
/// `sh7.js`); a collection dropped by another client while it is sharded fails the command.
#[test]
fn dotted_keys_and_a_collection_dropped_meanwhile() {
    use teseladb_meta::catalog::{CatalogCommand, Namespace};
    use teseladb_wire::commands::Backend;
    let mut m = Mem::new();
    let r = shard(&mut m, r#"{"shardCollection":"shk.d1","key":{"a.b":1}}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = run(
        &mut m,
        "shk",
        r#"{"insert":"d1","documents":[{"_id":1,"a":5},{"_id":2,"c":1},{"_id":3,"a":{"b":2}}]}"#,
    );
    assert_eq!(field(&r, "n"), &Value::Int32(3), "{r:?}");
    for doc in [r#"{"_id":4,"a":[{"b":1}]}"#, r#"{"_id":5,"a":{"b":[1]}}"#] {
        let r = run(&mut m, "shk", &format!(r#"{{"insert":"d1","documents":[{doc}]}}"#));
        assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(61), "{r:?}");
        assert_eq!(
            field(&r, "writeErrors.0.errmsg"),
            &Value::from("Shard key cannot contain array values or array descendants.")
        );
    }
    m.after_catalog_write = Some(Box::new(|m: &mut Mem, cmd: &CatalogCommand| {
        if let CatalogCommand::Create { ns, .. } = cmd {
            m.catalog_write(CatalogCommand::Drop { ns: ns.clone() }).expect("dropped");
        }
    }));
    let r = shard(&mut m, r#"{"shardCollection":"shk.gone","key":{"_id":1}}"#);
    refused(&r, 26, "the collection was dropped while it was being sharded");
    m.after_catalog_write = Some(Box::new(|m: &mut Mem, cmd: &CatalogCommand| {
        if let CatalogCommand::Modify { ns, .. } = cmd {
            m.catalog_write(CatalogCommand::Drop { ns: ns.clone() }).expect("dropped");
        }
    }));
    let r = shard(&mut m, r#"{"shardCollection":"shk.gone2","key":{"_id":1}}"#);
    refused(&r, 26, "the collection was dropped while it was being sharded");
    m.after_catalog_write = None;
    let ns = Namespace { db: "shk".into(), coll: "gone2".into() };
    assert_eq!(m.collection(&ns).expect("read"), None);
}

/// A write that fails while the documents take the shard key's keys fails the command and leaves
/// the collection as it was: its documents where they were and no shard key; the command can
/// then be run again.
#[test]
fn a_failed_write_while_keying_again() {
    let mut m = Mem::new();
    run(&mut m, "shk", r#"{"insert":"w","documents":[{"_id":1,"a":1},{"_id":2,"a":2}]}"#);
    run(&mut m, "shk", r#"{"createIndexes":"w","indexes":[{"key":{"a":1},"name":"a_1"}]}"#);
    let busy =
        teseladb_wire::error::CommandError::new(teseladb_wire::error::WRITE_CONFLICT, "busy");
    m.fail_write_at = Some((1, busy));
    let r = shard(&mut m, r#"{"shardCollection":"shk.w","key":{"a":1}}"#);
    assert_eq!(field(&r, "code"), &Value::Int32(112), "{r:?}");
    let r = run(&mut m, "shk", r#"{"find":"w","sort":{"_id":1}}"#);
    let Value::Array(docs) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(docs.len(), 2, "{r:?}");
    let r = m.run("config", json(r#"{"find":"collections","filter":{"_id":"shk.w"}}"#));
    assert_eq!(field(&r, "cursor.firstBatch"), &Value::Array(vec![]), "{r:?}");
    // Failing again after the first document moved puts it back too.
    let busy =
        teseladb_wire::error::CommandError::new(teseladb_wire::error::WRITE_CONFLICT, "busy");
    m.fail_write_at = Some((2, busy));
    let r = shard(&mut m, r#"{"shardCollection":"shk.w","key":{"a":1}}"#);
    assert_eq!(field(&r, "code"), &Value::Int32(112), "{r:?}");
    let r = run(&mut m, "shk", r#"{"find":"w","filter":{"a":2}}"#);
    let Value::Array(docs) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(docs.len(), 1, "{r:?}");
    let r = shard(&mut m, r#"{"shardCollection":"shk.w","key":{"a":1}}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = run(&mut m, "shk", r#"{"count":"w"}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(2), "{r:?}");
}
