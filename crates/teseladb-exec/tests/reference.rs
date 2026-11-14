//! The reference evaluator (010 FR-020, T003): a pipeline over a snapshot held in memory, its
//! stages one after another without moving or splitting any, as the oracle of the `queries`
//! load compares the distributed result with it.

use teseladb_bson::{Document, Value, extjson};
use teseladb_exec::reference::Reference;

fn doc(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn pipeline(text: &str) -> Vec<Document> {
    match doc(&format!(r#"{{"p":{text}}}"#)).get("p") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("not a stage: {other:?}"),
            })
            .collect(),
        other => panic!("not a pipeline: {other:?}"),
    }
}

fn shown(docs: &[Document]) -> Vec<String> {
    docs.iter()
        .map(|d| {
            let bytes = d.to_bytes().expect("small");
            extjson::to_canonical(teseladb_bson::RawDoc::from_bytes(&bytes).expect("a document"))
        })
        .collect()
}

fn snapshot() -> Reference {
    Reference::new(7)
        .with_collection(
            "c",
            vec![
                doc(r#"{"_id":1,"g":"a","n":5}"#),
                doc(r#"{"_id":2,"g":"b","n":3}"#),
                doc(r#"{"_id":3,"g":"a","n":7}"#),
                doc(r#"{"_id":4,"g":"b","n":1}"#),
            ],
        )
        .with_collection("o", vec![doc(r#"{"_id":10,"k":"a","name":"one"}"#)])
}

#[test]
fn stages_run_in_order_over_the_snapshot() {
    let r = snapshot();
    let got = r
        .evaluate("c", &pipeline(r#"[{"$match":{"n":{"$gt":2}}},{"$group":{"_id":"$g","t":{"$sum":"$n"}}},{"$sort":{"_id":1}}]"#))
        .expect("runs");
    assert_eq!(
        shown(&got),
        [r#"{"_id":"a","t":{"$numberInt":"12"}}"#, r#"{"_id":"b","t":{"$numberInt":"3"}}"#]
    );
}

#[test]
fn lookups_read_the_other_collections_of_the_snapshot() {
    let r = snapshot();
    let got = r
        .evaluate(
            "c",
            &pipeline(
                r#"[{"$match":{"_id":1}},{"$lookup":{"from":"o","localField":"g","foreignField":"k","as":"j"}},{"$project":{"j.name":1}}]"#,
            ),
        )
        .expect("runs");
    assert_eq!(shown(&got), [r#"{"_id":{"$numberInt":"1"},"j":[{"name":"one"}]}"#]);
    let missing = r.evaluate("zz", &pipeline(r#"[{"$count":"n"}]"#)).expect("runs");
    assert!(missing.is_empty());
}

#[test]
fn expressions_and_variables_follow_the_engine() {
    let r = snapshot().with_vars(doc(r#"{"lim":4}"#));
    let got = r
        .evaluate(
            "c",
            &pipeline(r#"[{"$match":{"$expr":{"$lt":["$n","$$lim"]}}},{"$project":{"_id":1}}]"#),
        )
        .expect("runs");
    assert_eq!(shown(&got), [r#"{"_id":{"$numberInt":"2"}}"#, r#"{"_id":{"$numberInt":"4"}}"#]);
    let err = r.evaluate("c", &pipeline(r#"[{"$bogus":1}]"#)).expect_err("unknown stage");
    assert_eq!(err.code.code, 40_324);
    let err = r
        .evaluate("c", &pipeline(r#"[{"$project":{"x":{"$divide":["$n",0]}}}]"#))
        .expect_err("division by zero");
    assert_eq!(err.code.code, 2);
}

#[test]
fn samples_repeat_with_the_seed() {
    let a = snapshot().evaluate("c", &pipeline(r#"[{"$sample":{"size":2}}]"#)).expect("runs");
    let b = snapshot().evaluate("c", &pipeline(r#"[{"$sample":{"size":2}}]"#)).expect("runs");
    assert_eq!(shown(&a), shown(&b));
    assert_eq!(a.len(), 2);
    let other = Reference::new(8)
        .with_collection("c", snapshot().collection_of("c"))
        .evaluate("c", &pipeline(r#"[{"$sample":{"size":4}},{"$sort":{"_id":1}}]"#))
        .expect("runs");
    assert_eq!(other.len(), 4);
}
