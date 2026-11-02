//! Documents of a collection with a shard key (018 FR-001, T004): each one keyed by its shard
//! key and its `_id` (`enc(shard key) · enc(_id)`), with `_id` unique across the collection in
//! an index of its own, as a `refrouter` 9.0.4 of one shard answered on 02-11-2026.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, RawDoc, Value, extjson};
use teseladb_keys::PrimaryKey;

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run("shk", json(cmd))
}

fn ok(r: &Document) {
    assert_eq!(field(r, "ok"), &Value::Double(1.0), "{r:?}");
}

fn batch(r: &Document) -> Vec<Value> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.clone()
}

/// A sharded collection `c` with key `key`, and documents `docs`.
fn sharded(key: &str, docs: &str) -> Mem {
    let mut m = Mem::new();
    ok(&m.run("admin", json(r#"{"enableSharding":"shk"}"#)));
    ok(&m.run("admin", json(&format!(r#"{{"shardCollection":"shk.c","key":{key}}}"#))));
    if docs != "[]" {
        ok(&run(&mut m, &format!(r#"{{"insert":"c","documents":{docs}}}"#)));
    }
    m
}

/// Whether a document key of the store ends in primary key `pk`, without the bucket of a
/// collection without a shard key.
fn keyed(m: &Mem, pk: &PrimaryKey) -> bool {
    m.data.keys().any(|k| {
        k.first() == Some(&teseladb_keys::space::DATA)
            && k.get(9..13) == Some(&teseladb_keys::DOCUMENTS.to_be_bytes()[..])
            && k.get(13..) == Some(pk.as_bytes())
    })
}

#[test]
fn a_document_is_keyed_by_its_shard_key_and_its_id() {
    let m = sharded(r#"{"a":1}"#, r#"[{"_id":1,"a":5,"v":1},{"_id":2,"v":2}]"#);
    let five = Value::Int32(5).to_raw_bytes();
    let one = Value::Int32(1).to_raw_bytes();
    let pk = PrimaryKey::sharded(&[raw(&five)], raw(&one));
    assert!(keyed(&m, &pk), "the key of _id 1");
    // A missing field of the shard key is null in the key.
    let null = Value::Null.to_raw_bytes();
    let two = Value::Int32(2).to_raw_bytes();
    assert!(keyed(&m, &PrimaryKey::sharded(&[raw(&null)], raw(&two))), "the key of _id 2");
}

/// The raw value of a one-field document's bytes.
fn raw(holder: &[u8]) -> teseladb_bson::RawValue<'_> {
    RawDoc::from_bytes(holder).expect("a document").get("v").expect("its value")
}

trait ToRaw {
    fn to_raw_bytes(&self) -> Vec<u8>;
}

impl ToRaw for Value {
    fn to_raw_bytes(&self) -> Vec<u8> {
        Document::new().with("v", self.clone()).to_bytes().expect("bytes")
    }
}

#[test]
fn reads_writes_and_indexes_find_sharded_documents() {
    let mut m = sharded(
        r#"{"a":1,"b":"hashed"}"#,
        r#"[{"_id":1,"a":5,"b":"x","v":1},{"_id":2,"a":3,"b":"y","v":2},{"_id":3,"v":3}]"#,
    );
    let r = run(&mut m, r#"{"find":"c","filter":{"_id":1}}"#);
    assert_eq!(batch(&r), vec![Value::Document(json(r#"{"_id":1,"a":5,"b":"x","v":1}"#))]);
    let r = run(&mut m, r#"{"find":"c","filter":{"_id":{"$gte":2}},"sort":{"_id":1}}"#);
    assert_eq!(batch(&r).len(), 2, "{r:?}");
    ok(&run(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"v":1},"name":"v_1"}]}"#));
    let r = run(
        &mut m,
        r#"{"find":"c","filter":{"v":{"$gte":2}},"sort":{"v":1},"projection":{"_id":1}}"#,
    );
    assert_eq!(
        batch(&r),
        vec![Value::Document(json(r#"{"_id":2}"#)), Value::Document(json(r#"{"_id":3}"#))]
    );
    let r = run(&mut m, r#"{"update":"c","updates":[{"q":{"_id":2},"u":{"$set":{"v":20}}}]}"#);
    assert_eq!(field(&r, "nModified"), &Value::Int32(1), "{r:?}");
    let r = run(&mut m, r#"{"find":"c","filter":{"v":20},"projection":{"_id":1}}"#);
    assert_eq!(batch(&r), vec![Value::Document(json(r#"{"_id":2}"#))]);
    let r = run(&mut m, r#"{"delete":"c","deletes":[{"q":{"_id":1},"limit":1}]}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = run(&mut m, r#"{"count":"c"}"#);
    assert_eq!(field(&r, "n"), &Value::Int32(2), "{r:?}");
    // An upsert keys its document by its shard key too.
    let r = run(
        &mut m,
        r#"{"update":"c","updates":[{"q":{"_id":9,"a":4},"u":{"$set":{"v":9}},"upsert":true}]}"#,
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = run(&mut m, r#"{"find":"c","filter":{"_id":9}}"#);
    assert_eq!(batch(&r), vec![Value::Document(json(r#"{"_id":9,"a":4,"v":9}"#))]);
}

#[test]
fn the_id_stays_unique_across_shard_key_values() {
    let mut m = sharded(r#"{"a":1}"#, r#"[{"_id":2,"v":1}]"#);
    let r = run(&mut m, r#"{"insert":"c","documents":[{"_id":2,"a":1}]}"#);
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(11000), "{r:?}");
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::from(
            "E11000 duplicate key error collection: shk.c index: _id_ dup key: { _id: 2 }"
        )
    );
    assert_eq!(field(&r, "writeErrors.0.keyPattern"), &Value::Document(json(r#"{"_id":1}"#)));
}

#[test]
fn a_shard_key_holds_no_array() {
    let mut m = sharded(r#"{"x.y":1}"#, "[]");
    for doc in [r#"{"_id":4,"x":{"y":[1]}}"#, r#"{"_id":5,"x":[{"y":1}]}"#] {
        let r = run(&mut m, &format!(r#"{{"insert":"c","documents":[{doc}]}}"#));
        assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(61), "{r:?}");
        assert_eq!(
            field(&r, "writeErrors.0.errmsg"),
            &Value::from("Shard key cannot contain array values or array descendants.")
        );
    }
}

#[test]
fn a_change_event_keys_its_document_by_the_shard_key_fields_it_has() {
    let mut m = sharded(r#"{"x.y":1,"b":"hashed"}"#, "[]");
    let r = run(&mut m, r#"{"aggregate":"c","pipeline":[{"$changeStream":{}}],"cursor":{}}"#);
    ok(&r);
    let Value::Int64(id) = field(&r, "cursor.id") else { panic!("{r:?}") };
    let id = *id;
    ok(&run(
        &mut m,
        r#"{"insert":"c","documents":[{"_id":1,"v":1},{"_id":2,"x":{"y":3},"b":{"z":1}},{"_id":3,"x":5}]}"#,
    ));
    let r = m.run("shk", Document::new().with("getMore", Value::Int64(id)).with("collection", "c"));
    let Value::Array(events) = field(&r, "cursor.nextBatch") else { panic!("{r:?}") };
    let keys: Vec<&Value> = events
        .iter()
        .map(|e| match e {
            Value::Document(e) => field(e, "documentKey"),
            other => panic!("{other:?}"),
        })
        .collect();
    let want = [r#"{"_id":1}"#, r#"{"x.y":3,"b":{"z":1},"_id":2}"#, r#"{"_id":3}"#]
        .map(|k| Value::Document(json(k)));
    assert_eq!(keys, want.iter().collect::<Vec<_>>());
}

#[test]
fn enable_sharding_refuses_an_invalid_database_name() {
    let mut m = Mem::new();
    for (name, msg) in [
        ("bad.name", "'.' is an invalid character in a db name: bad.name"),
        ("a$b", "Database a$b could not be created :: caused by :: Invalid db name specified: a$b"),
        ("", "Database  could not be created :: caused by :: Invalid db name specified: "),
        ("x y", "Database x y could not be created :: caused by :: Invalid db name specified: x y"),
    ] {
        let r = m.run("admin", doc(&[("enableSharding", Value::from(name))]));
        assert_eq!(field(&r, "code"), &Value::Int32(73), "{name}: {r:?}");
        assert_eq!(field(&r, "errmsg"), &Value::from(msg), "{name}");
    }
}

#[test]
fn an_update_cannot_change_the_shard_key() {
    let mut m = sharded(r#"{"a":1,"b":"hashed"}"#, r#"[{"_id":1,"a":5,"b":"x","v":1}]"#);
    for q in [r#"{"_id":1}"#, r#"{"_id":1,"a":5}"#] {
        let r = run(
            &mut m,
            &format!(r#"{{"update":"c","updates":[{{"q":{q},"u":{{"$set":{{"a":6}}}}}}]}}"#),
        );
        assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(20), "{r:?}");
        assert_eq!(
            field(&r, "writeErrors.0.errmsg"),
            &Value::from(
                "Plan executor error during update :: caused by :: Must run update to shard key field in a multi-statement transaction or with retryWrites: true."
            )
        );
    }
    let r = run(&mut m, r#"{"find":"c","filter":{}}"#);
    assert_eq!(batch(&r), vec![Value::Document(json(r#"{"_id":1,"a":5,"b":"x","v":1}"#))]);
}

#[test]
fn the_documents_already_there_are_keyed_again() {
    let mut m = Mem::new();
    ok(&run(&mut m, r#"{"insert":"c","documents":[{"_id":1,"a":5},{"_id":2,"a":3}]}"#));
    ok(&run(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"}]}"#));
    ok(&m.run("admin", json(r#"{"shardCollection":"shk.c","key":{"a":1}}"#)));
    let five = Value::Int32(5).to_raw_bytes();
    let one = Value::Int32(1).to_raw_bytes();
    assert!(keyed(&m, &PrimaryKey::sharded(&[raw(&five)], raw(&one))));
    let r = run(&mut m, r#"{"find":"c","filter":{"_id":2}}"#);
    assert_eq!(batch(&r), vec![Value::Document(json(r#"{"_id":2,"a":3}"#))]);
    let r = run(&mut m, r#"{"find":"c","filter":{"a":{"$gte":4}},"projection":{"_id":1}}"#);
    assert_eq!(batch(&r), vec![Value::Document(json(r#"{"_id":1}"#))]);
    let r = run(&mut m, r#"{"insert":"c","documents":[{"_id":1,"a":9}]}"#);
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(11000), "{r:?}");
}

#[test]
fn a_split_point_of_a_sharded_collection_comes_before_its_values() {
    use teseladb_meta::catalog::CollectionEntry;
    use teseladb_wire::commands::sharding::{SHARD_KEY, document_key, split_point};
    let key = json(r#"{"a":1}"#);
    let options = Document::new()
        .with(SHARD_KEY, Document::new().with("key", key.clone()).with("unique", false))
        .to_bytes()
        .expect("bytes");
    let c = CollectionEntry { id: 7, uuid: [0; 16], options, created_index: 1, tenant: 0 };
    let at = split_point(&c, &json(r#"{"a":5}"#)).expect("a key");
    let doc = document_key(&c, &key, &json(r#"{"_id":1,"a":5}"#)).expect("a key");
    let before = document_key(&c, &key, &json(r#"{"_id":1,"a":4}"#)).expect("a key");
    assert!(before < at && at < doc);
    let with_id = split_point(&c, &json(r#"{"a":5,"_id":1}"#)).expect("a key");
    assert_eq!(with_id, doc);
    let plain = CollectionEntry { options: Document::new().to_bytes().expect("bytes"), ..c };
    let r = split_point(&plain, &json(r#"{"a":5}"#));
    assert_eq!(r.map_err(|e| e.errmsg), Err("--at needs an _id".to_owned()));
}

/// The session fields of a retryable write with `txnNumber` `n`.
fn session(n: u32) -> String {
    let lsid = r#""lsid":{"id":{"$binary":{"base64":"AgICAgICAgICAgICAgICAg==","subType":"04"}}}"#;
    format!(r#"{lsid},"txnNumber":{{"$numberLong":"{n}"}}"#)
}

fn found(m: &mut Mem) -> Vec<Value> {
    batch(&run(m, r#"{"find":"c","filter":{},"sort":{"_id":1}}"#))
}

/// A retryable write moves the document to the key of its new shard key values, and a retry
/// answers from the first send (measured on a `refrouter` 9.0.4 on 02-11-2026).
#[test]
fn a_retryable_update_moves_its_document_to_its_new_shard_key() {
    let mut m = sharded(r#"{"a":1}"#, r#"[{"_id":1,"a":5,"x":1},{"_id":2,"a":7}]"#);
    let cmd = format!(
        r#"{{"update":"c","updates":[{{"q":{{"_id":1}},"u":{{"$set":{{"a":6}}}}}}],{}}}"#,
        session(1)
    );
    let r = run(&mut m, &cmd);
    assert_eq!((field(&r, "n"), field(&r, "nModified")), (&Value::Int32(1), &Value::Int32(1)));
    let r = run(&mut m, &cmd);
    assert_eq!(field(&r, "retriedStmtIds"), &Value::Array(vec![Value::Int32(0)]), "{r:?}");
    let six = Value::Int32(6).to_raw_bytes();
    let one = Value::Int32(1).to_raw_bytes();
    assert!(keyed(&m, &PrimaryKey::sharded(&[raw(&six)], raw(&one))), "the new key");
    let five = Value::Int32(5).to_raw_bytes();
    assert!(!keyed(&m, &PrimaryKey::sharded(&[raw(&five)], raw(&one))), "the old key is gone");
    // By `_id`, by the new value and by a replacement and `findAndModify`.
    let r = run(&mut m, r#"{"find":"c","filter":{"_id":1}}"#);
    assert_eq!(batch(&r), vec![Value::Document(json(r#"{"_id":1,"a":6,"x":1}"#))]);
    let replace = format!(
        r#"{{"update":"c","updates":[{{"q":{{"a":7}},"u":{{"a":8,"z":1}}}}],{}}}"#,
        session(2)
    );
    ok(&run(&mut m, &replace));
    let fam = format!(
        r#"{{"findAndModify":"c","query":{{"_id":1}},"update":{{"$set":{{"a":9}}}},"new":true,{}}}"#,
        session(3)
    );
    let r = run(&mut m, &fam);
    assert_eq!(field(&r, "value"), &Value::Document(json(r#"{"_id":1,"a":9,"x":1}"#)), "{r:?}");
    let want = [r#"{"_id":1,"a":9,"x":1}"#, r#"{"_id":2,"a":8,"z":1}"#];
    assert_eq!(found(&mut m), want.map(|d| Value::Document(json(d))).to_vec());
    let r = run(&mut m, r#"{"find":"c","filter":{"a":{"$gte":8}},"sort":{"a":1}}"#);
    assert_eq!(batch(&r).len(), 2);
}

/// A multi-update cannot change the shard key, and a retryable write cannot be a multi-update
/// or a delete of many (measured on a `refrouter` and a `refnode` 9.0.4 on 02-11-2026).
#[test]
fn multi_writes_and_the_shard_key() {
    let mut m = sharded(r#"{"a":1}"#, r#"[{"_id":1,"a":5}]"#);
    let multi = r#"{"update":"c","updates":[{"q":{"_id":1},"u":{"$set":{"a":6}},"multi":true}]}"#;
    let r = run(&mut m, multi);
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(72), "{r:?}");
    assert_eq!(
        field(&r, "writeErrors.0.errmsg"),
        &Value::from(
            "Plan executor error during update :: caused by :: Multi-update operations are not allowed when updating the shard key field."
        )
    );
    // Keeping the values is fine.
    let same =
        r#"{"update":"c","updates":[{"q":{"_id":1},"u":{"$set":{"a":5,"y":1}},"multi":true}]}"#;
    assert_eq!(field(&run(&mut m, same), "nModified"), &Value::Int32(1));
    for (cmd, what) in [
        (
            r#""update":"c","updates":[{"q":{"_id":1},"u":{"$set":{"b":1}},"multi":true}]"#,
            "multi=true",
        ),
        (r#""delete":"c","deletes":[{"q":{"_id":1},"limit":0}]"#, "limit=0"),
    ] {
        let r = run(&mut m, &format!("{{{cmd},{}}}", session(7)));
        assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(72), "{r:?}");
        let msg = format!("Cannot use (or request) retryable writes with {what}");
        assert_eq!(field(&r, "writeErrors.0.errmsg"), &Value::from(msg.as_str()));
    }
    assert_eq!(found(&mut m), vec![Value::Document(json(r#"{"_id":1,"a":5,"y":1}"#))]);
}

/// A moved document gives one `update` event keyed by the document before the change, as a
/// `refrouter` 9.0.4 of one shard gives it (measured on 02-11-2026); a replacement, a `replace`.
#[test]
fn a_moved_document_gives_one_event_keyed_by_its_old_values() {
    let mut m = sharded(r#"{"a":1}"#, r#"[{"_id":1,"a":5,"x":1},{"_id":2,"a":7}]"#);
    let r = run(&mut m, r#"{"aggregate":"c","pipeline":[{"$changeStream":{}}],"cursor":{}}"#);
    let Value::Int64(id) = field(&r, "cursor.id") else { panic!("{r:?}") };
    let id = *id;
    for (n, body) in [
        (1, r#""updates":[{"q":{"_id":1},"u":{"$set":{"a":6}}}]"#),
        (2, r#""updates":[{"q":{"_id":2},"u":{"a":8}}]"#),
    ] {
        ok(&run(&mut m, &format!(r#"{{"update":"c",{body},{}}}"#, session(n))));
    }
    let r = m.run("shk", Document::new().with("getMore", Value::Int64(id)).with("collection", "c"));
    let Value::Array(events) = field(&r, "cursor.nextBatch") else { panic!("{r:?}") };
    let seen: Vec<(Value, Value)> = events
        .iter()
        .map(|e| match e {
            Value::Document(e) => {
                (field(e, "operationType").clone(), field(e, "documentKey").clone())
            }
            other => panic!("{other:?}"),
        })
        .collect();
    let want = [("update", r#"{"a":5,"_id":1}"#), ("replace", r#"{"a":7,"_id":2}"#)]
        .map(|(t, k)| (Value::from(t), Value::Document(json(k))));
    assert_eq!(seen, want.to_vec());
    let Value::Document(first) = &events[0] else { panic!() };
    assert_eq!(
        field(first, "updateDescription.updatedFields"),
        &Value::Document(json(r#"{"a":6}"#))
    );
}
