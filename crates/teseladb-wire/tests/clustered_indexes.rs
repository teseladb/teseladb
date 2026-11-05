//! The index of a clustered collection as the index commands see it (015 FR-009): `listIndexes`,
//! `createIndexes`, `dropIndexes`, `$indexStats`, `collStats` and the duplicate `_id`, against
//! what a `refrouter` 9.0.4 answered on 05-11-2026 (`clustered-collections` steps 22 to 44).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    m.run("m", json(cmd))
}

fn first_batch(r: &Document) -> Vec<Value> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.clone()
}

/// The indexes `listIndexes` gives for collection `name`.
fn list_indexes(m: &mut Mem, name: &str) -> Vec<Value> {
    first_batch(&m.run("m", doc(&[("listIndexes", Value::from(name))])))
}

fn int(r: &Document, k: &str) -> i64 {
    match r.get(k) {
        Some(Value::Int32(n)) => i64::from(*n),
        Some(Value::Int64(n)) => *n,
        other => panic!("{k}: {other:?} in {r:?}"),
    }
}

fn clustered(m: &mut Mem, name: &str, options: &str) {
    let r = run(
        m,
        &format!(
            r#"{{"create":"{name}","clusteredIndex":{{"key":{{"_id":1}},"unique":true{options}}}"#
        ),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

#[test]
fn list_indexes_shows_the_clustered_index_with_its_name_and_options() {
    let mut m = Mem::new();
    clustered(&mut m, "c0", "}");
    clustered(
        &mut m,
        "c17",
        r#","name":"both"},"collation":{"locale":"fr"},"expireAfterSeconds":30"#,
    );

    let c0 = list_indexes(&mut m, "c0");
    let c17 = list_indexes(&mut m, "c17");

    let expected = json(r#"{"v":2,"key":{"_id":1},"name":"_id_","unique":true,"clustered":true}"#);
    assert_eq!(c0, vec![Value::Document(expected)]);
    let Some(Value::Document(both)) = c17.first() else { panic!("{c17:?}") };
    let keys: Vec<&str> = both.iter().map(|(k, _)| k).collect();
    assert_eq!(
        keys,
        ["v", "key", "name", "unique", "expireAfterSeconds", "collation", "clustered"]
    );
    assert_eq!(both.get("name"), Some(&Value::from("both")));
    assert_eq!(both.get("expireAfterSeconds"), Some(&Value::Int32(30)));
}

#[test]
fn create_indexes_takes_a_clustered_spec_as_there_and_does_not_count_it() {
    let mut m = Mem::new();
    clustered(&mut m, "c0", "}");

    let added = run(&mut m, r#"{"createIndexes":"c0","indexes":[{"key":{"v":1},"name":"v_1"}]}"#);
    let same = run(
        &mut m,
        r#"{"createIndexes":"c0","indexes":[{"key":{"_id":1},"name":"other","clustered":true,"unique":true}]}"#,
    );

    assert_eq!((int(&added, "numIndexesBefore"), int(&added, "numIndexesAfter")), (0, 1));
    assert_eq!((int(&same, "numIndexesBefore"), int(&same, "numIndexesAfter")), (1, 1));
    assert_eq!(same.get("note"), Some(&Value::from("all indexes already exist")));
}

#[test]
fn a_clustered_spec_on_a_collection_that_is_not_clustered_is_still_refused() {
    let mut m = Mem::new();
    run(&mut m, r#"{"create":"plain"}"#);

    let r = run(
        &mut m,
        r#"{"createIndexes":"plain","indexes":[{"key":{"_id":1},"name":"_id_","clustered":true,"unique":true}]}"#,
    );

    assert_eq!(r.get("code"), Some(&Value::Int32(6_243_700)), "{r:?}");
}

#[test]
fn drop_indexes_refuses_the_clustered_index_and_does_not_count_it() {
    let mut m = Mem::new();
    clustered(&mut m, "c0", "}");
    run(&mut m, r#"{"createIndexes":"c0","indexes":[{"key":{"v":1},"name":"v_1"}]}"#);

    let refused = run(&mut m, r#"{"dropIndexes":"c0","index":"_id_"}"#);
    let all = run(&mut m, r#"{"dropIndexes":"c0","index":"*"}"#);

    assert_eq!(refused.get("code"), Some(&Value::Int32(5_979_800)), "{refused:?}");
    assert_eq!(
        refused.get("errmsg"),
        Some(&Value::from("It is illegal to drop the clusteredIndex"))
    );
    assert_eq!(int(&all, "nIndexesWas"), 1);
}

#[test]
fn index_stats_leaves_out_the_clustered_index_and_coll_stats_counts_it() {
    let mut m = Mem::new();
    clustered(&mut m, "c0", "}");
    run(&mut m, r#"{"insert":"c0","documents":[{"_id":1}]}"#);

    let stats = run(&mut m, r#"{"aggregate":"c0","pipeline":[{"$indexStats":{}}],"cursor":{}}"#);
    let coll = run(&mut m, r#"{"collStats":"c0"}"#);

    assert_eq!(first_batch(&stats), Vec::<Value>::new());
    assert_eq!(int(&coll, "nindexes"), 1);
    assert_eq!(coll.get("indexSizes"), Some(&Value::Document(Document::new())));
}

#[test]
fn a_duplicate_id_names_no_index_and_gives_the_found_value() {
    let mut m = Mem::new();
    clustered(&mut m, "c0", "}");
    run(&mut m, r#"{"insert":"c0","documents":[{"_id":3}]}"#);

    let r = run(&mut m, r#"{"insert":"c0","documents":[{"_id":3}]}"#);

    let Some(Value::Array(errors)) = r.get("writeErrors") else { panic!("{r:?}") };
    let Some(Value::Document(e)) = errors.first() else { panic!("{r:?}") };
    assert_eq!(
        e.get("errmsg"),
        Some(&Value::from(
            "E11000 duplicate key error collection: m.c0 dup key: { _id: 3 } found value: {}"
        ))
    );
    assert_eq!(e.get("foundValue"), Some(&Value::Document(Document::new())));
}
