//! Write paths measured on a `refnode` 9.0.4 on 01-11-2026: an ordered insert longer than one chunk
//! that stops at a duplicate key in the first chunk, and an upsert with `upsertSupplied` that
//! inserts the document of its constant `new`, and a `collMod` of an index and an option at once.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run("u", json(cmd))
}

#[test]
fn an_ordered_insert_stops_in_its_first_chunk() {
    let mut m = Mem::new();
    let docs: Vec<String> =
        (0..1500).map(|i| format!(r#"{{"_id":{}}}"#, if i == 500 { 0 } else { i })).collect();
    let r = run(&mut m, &format!(r#"{{"insert":"c","documents":[{}]}}"#, docs.join(",")));
    assert_eq!(field(&r, "n"), &Value::Int32(500), "{r:?}");
    assert_eq!(field(&r, "writeErrors.0.index"), &Value::Int32(500), "{r:?}");
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(11000), "{r:?}");
    let r = m.run("u", doc(&[("count", Value::from("c"))]));
    assert_eq!(field(&r, "n"), &Value::Int32(500), "{r:?}");
}

#[test]
fn an_upsert_with_upsert_supplied_inserts_its_new_document() {
    let mut m = Mem::new();
    let r = run(
        &mut m,
        r#"{"update":"c","updates":[{"q":{"a":1},"u":[{"$set":{"b":"$$new.b"}}],"upsert":true,
            "upsertSupplied":true,"c":{"new":{"_id":5,"b":7}}}]}"#,
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    assert_eq!(field(&r, "nModified"), &Value::Int32(0), "{r:?}");
    assert_eq!(field(&r, "upserted.0._id"), &Value::Int32(5), "{r:?}");
    let r = run(&mut m, r#"{"find":"c"}"#);
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(b, &vec![Value::Document(json(r#"{"_id":5,"b":7}"#))]);
}

/// `collMod` with an index change and a collection option makes both and answers with the
/// index's change.
#[test]
fn coll_mod_changes_an_index_and_an_option_together() {
    let mut m = Mem::new();
    run(&mut m, r#"{"insert":"c","documents":[{"a":1}]}"#);
    let r = run(&mut m, r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"}]}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = run(
        &mut m,
        r#"{"collMod":"c","index":{"name":"a_1","hidden":true},"validationLevel":"moderate"}"#,
    );
    assert_eq!(field(&r, "hidden_old"), &Value::Boolean(false), "{r:?}");
    assert_eq!(field(&r, "hidden_new"), &Value::Boolean(true), "{r:?}");
    let r = run(&mut m, r#"{"listCollections":1,"filter":{"name":"c"}}"#);
    assert_eq!(
        field(&r, "cursor.firstBatch.0.options"),
        &Value::Document(json(r#"{"validationLevel":"moderate","validationAction":"error"}"#)),
        "{r:?}"
    );
}

/// `aggregate` with `explain: true` on a view explains the view's pipeline over its collection,
/// and an explain that runs over a missing collection reads nothing.
#[test]
fn explains_on_a_view_and_on_a_missing_collection() {
    let mut m = Mem::new();
    run(&mut m, r#"{"insert":"c","documents":[{"a":1}]}"#);
    let r = run(&mut m, r#"{"create":"v","viewOn":"c","pipeline":[{"$match":{"a":1}}]}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = run(&mut m, r#"{"aggregate":"v","pipeline":[],"explain":true}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(field(&r, "command.aggregate"), &Value::from("c"), "{r:?}");
    // The shard's plan, as a `refrouter` 9.0.4 shows it on 01-11-2026.
    for (cmd, router, inner) in [
        (r#"{"count":"missing","query":{"a":1}}"#, "SINGLE_SHARD", "COUNT"),
        (r#"{"count":"missing"}"#, "SINGLE_SHARD", "COUNT"),
        (r#"{"distinct":"missing","key":"a"}"#, "SINGLE_SHARD", "EOF"),
        (r#"{"delete":"missing","deletes":[{"q":{"a":1},"limit":0}]}"#, "SHARD_WRITE", "EOF"),
        (
            r#"{"update":"missing","updates":[{"q":{"a":1},"u":{"$set":{"b":1}}}]}"#,
            "SHARD_WRITE",
            "EOF",
        ),
    ] {
        let r = run(&mut m, &format!(r#"{{"explain":{cmd},"verbosity":"executionStats"}}"#));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
        assert_eq!(field(&r, "executionStats.nReturned"), &Value::Int32(0), "{cmd}: {r:?}");
        assert_eq!(field(&r, "executionStats.totalDocsExamined"), &Value::Int32(0), "{r:?}");
        assert_eq!(
            field(&r, "executionStats.executionStages.stage"),
            &Value::from(router),
            "{r:?}"
        );
        let shard = field(&r, "executionStats.executionStages.shards.0.executionStages");
        let Value::Document(shard) = shard else { panic!("{r:?}") };
        assert_eq!(field(shard, "stage"), &Value::from(inner), "{cmd}: {shard:?}");
        let eof = if inner == "COUNT" {
            field(shard, "inputStage")
        } else {
            &Value::Document(shard.clone())
        };
        let Value::Document(eof) = eof else { panic!("{shard:?}") };
        assert_eq!(field(eof, "type"), &Value::from("nonExistentNamespace"), "{cmd}: {shard:?}");
    }
}

/// The upsert of a pipeline update reads the command's `let`, and `upsertSupplied` without a
/// document to insert fails its statement (measured on a `refnode` 9.0.4).
#[test]
fn upserts_read_their_variables_and_need_their_supplied_document() {
    let mut m = Mem::new();
    let r = run(
        &mut m,
        r#"{"update":"c","let":{"x":4},"updates":[{"q":{"a":1},"u":[{"$set":{"b":"$$x"}}],"upsert":true}]}"#,
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = run(&mut m, r#"{"find":"c","projection":{"_id":0}}"#);
    assert_eq!(field(&r, "cursor.firstBatch.0"), &Value::Document(json(r#"{"a":1,"b":4}"#)));
    let r = run(
        &mut m,
        r#"{"update":"c","updates":[{"q":{"a":3},"u":[{"$set":{"b":1}}],"upsert":true,
            "upsertSupplied":true,"c":{"new":{"b":7,"a":3}}}]}"#,
    );
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    let r = run(&mut m, r#"{"find":"c","filter":{"a":3}}"#);
    let Value::Document(d) = field(&r, "cursor.firstBatch.0") else { panic!("{r:?}") };
    let keys: Vec<&str> = d.iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["_id", "b", "a"], "{d:?}");
    for c in ["", r#","c":{"x":1}"#] {
        let r = run(
            &mut m,
            &format!(
                r#"{{"update":"c","updates":[{{"q":{{"a":4}},"u":[{{"$set":{{"b":1}}}}],"upsert":true,"upsertSupplied":true{c}}}]}}"#
            ),
        );
        assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(9), "{r:?}");
        assert_eq!(
            field(&r, "writeErrors.0.errmsg"),
            &Value::from(
                "the parameter 'upsertSupplied' is set to 'true', but no document was supplied"
            )
        );
    }
}
