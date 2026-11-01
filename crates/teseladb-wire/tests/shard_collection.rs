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
