//! Index scans of ranges far longer than one read of the index (023 FR-012): whatever the
//! planner picks, forward or backward, over one interval or several, with a residual filter, a
//! limit, a skip, a sort the index gives or `returnKey`, the documents and their order are those
//! of the collection scan, and `explain` counts the keys and documents the reference server's
//! index scan examines.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

/// Documents of the collection: `_id` i, `a` = i, `b` = i mod 7.
const COUNT: i32 = 3000;

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, cmd: &str) -> Document {
    let r = m.run("p", json(cmd));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{cmd}: {r:?}");
    r
}

fn loaded() -> Mem {
    let mut m = Mem::new();
    let docs: Vec<String> =
        (0..COUNT).map(|i| format!(r#"{{"_id":{i},"a":{i},"b":{}}}"#, i % 7)).collect();
    for chunk in docs.chunks(1000) {
        run(&mut m, &format!(r#"{{"insert":"c","documents":[{}]}}"#, chunk.join(",")));
    }
    for spec in [
        r#"{"key":{"a":1},"name":"a_1"}"#,
        r#"{"key":{"b":1},"name":"b_1"}"#,
        r#"{"key":{"b":1,"a":-1},"name":"b_1_a_-1"}"#,
    ] {
        run(&mut m, &format!(r#"{{"createIndexes":"c","indexes":[{spec}]}}"#));
    }
    m
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int32(n) => i64::from(*n),
        Value::Int64(n) => *n,
        other => panic!("{other:?}"),
    }
}

/// The documents a `find` gives, in its order, all in its first batch.
fn found(m: &mut Mem, find: &str) -> Vec<Document> {
    let cmd = format!(r#"{{"find":"c",{find},"batchSize":100000}}"#);
    let r = run(m, &cmd);
    let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    batch
        .iter()
        .map(|d| match d {
            Value::Document(d) => d.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn ids(docs: &[Document]) -> Vec<i32> {
    docs.iter().map(|d| i32::try_from(int(field(d, "_id"))).expect("an int")).collect()
}

/// The `_id` of the documents `keep` takes, by `_id`.
fn wanted(keep: impl Fn(i32) -> bool) -> Vec<i32> {
    (0..COUNT).filter(|i| keep(*i)).collect()
}

/// `totalKeysExamined` and `totalDocsExamined` of `find`.
fn examined(m: &mut Mem, find: &str) -> (i64, i64) {
    let cmd = format!(r#"{{"explain":{{"find":"c",{find}}},"verbosity":"executionStats"}}"#);
    let r = run(m, &cmd);
    let keys = int(field(&r, "executionStats.totalKeysExamined"));
    let docs = int(field(&r, "executionStats.totalDocsExamined"));
    (keys, docs)
}

#[test]
fn a_range_longer_than_a_read_gives_the_documents_of_the_collection_scan_in_order() {
    let mut m = loaded();
    let filter = r#""filter":{"a":{"$gte":100,"$lt":2900}}"#;
    let by_index = found(&mut m, &format!(r#"{filter},"hint":"a_1""#));
    let natural = found(&mut m, &format!(r#"{filter},"hint":{{"$natural":1}}"#));
    assert_eq!(ids(&by_index), wanted(|i| (100..2900).contains(&i)));
    // The collection scan goes by the buckets of the documents: the same documents, by `_id`.
    let mut natural = natural;
    natural.sort_by_key(|d| int(field(d, "_id")));
    assert_eq!(by_index, natural);
    assert_eq!(examined(&mut m, &format!(r#"{filter},"hint":"a_1""#)), (2800, 2800));
    // Backward, by the sort the index gives.
    let back = found(&mut m, &format!(r#"{filter},"sort":{{"a":-1}},"hint":"a_1""#));
    let mut reversed = wanted(|i| (100..2900).contains(&i));
    reversed.reverse();
    assert_eq!(ids(&back), reversed);
}

#[test]
fn a_skip_and_a_limit_take_their_part_of_a_long_range() {
    let mut m = loaded();
    let filter = r#""filter":{"a":{"$gte":10}}"#;
    let got = found(&mut m, &format!(r#"{filter},"sort":{{"a":1}},"skip":1200,"limit":1500"#));
    assert_eq!(ids(&got), wanted(|i| (1210..2710).contains(&i)));
    let back = found(&mut m, &format!(r#"{filter},"sort":{{"a":-1}},"skip":7,"limit":2000"#));
    let mut expected = wanted(|i| (993..2993).contains(&i));
    expected.reverse();
    assert_eq!(ids(&back), expected);
}

#[test]
fn a_residual_filter_over_a_long_range_keeps_what_the_collection_scan_keeps() {
    let mut m = loaded();
    let filter = r#""filter":{"a":{"$gte":0},"b":3}"#;
    let all = found(&mut m, &format!(r#"{filter},"hint":"a_1""#));
    assert_eq!(ids(&all), wanted(|i| i % 7 == 3));
    assert_eq!(examined(&mut m, &format!(r#"{filter},"hint":"a_1""#)), (3000, 3000));
    // With a limit the scan stops at the document that reaches it: the 300th is 3 + 299 · 7.
    let first = found(&mut m, &format!(r#"{filter},"hint":"a_1","limit":300"#));
    assert_eq!(ids(&first), wanted(|i| i % 7 == 3 && i <= 2096));
    let limited = format!(r#"{filter},"hint":"a_1","limit":300"#);
    assert_eq!(examined(&mut m, &limited), (2097, 2097));
    let few = found(&mut m, &format!(r#"{filter},"hint":"a_1","limit":5"#));
    assert_eq!(ids(&few), vec![3, 10, 17, 24, 31]);
}

#[test]
fn a_read_with_a_limit_reads_its_range_once_up_to_what_it_takes() {
    let mut m = loaded();
    // The 300th document of `b` 3 is at `a` 2096: 2097 entries and their documents, and what the
    // scan reads ahead of them, at most one pull and one batch of documents. Before, each read
    // that ran out of entries read the range again from its start with four times as many.
    m.scanned_rows = 0;
    let first = found(&mut m, r#""filter":{"a":{"$gte":0},"b":3},"hint":"a_1","limit":300"#);
    assert_eq!(first.len(), 300);
    let read = m.scanned_rows;
    assert!(read <= 2 * 2097 + 1024 + 512, "{read} rows read");
}

#[test]
fn several_long_intervals_come_in_the_order_of_the_index_both_ways() {
    let mut m = loaded();
    let filter = r#""filter":{"b":{"$in":[4,1]}}"#;
    let forward = found(&mut m, &format!(r#"{filter},"sort":{{"b":1}},"hint":"b_1""#));
    let expected: Vec<i32> = [1, 4].iter().flat_map(|b| wanted(move |i| i % 7 == *b)).collect();
    assert_eq!(ids(&forward), expected);
    // Each value of `b` holds 428 or 429 entries and the scan reads past each to the next value.
    let n = i64::try_from(expected.len()).expect("a count");
    assert_eq!(examined(&mut m, &format!(r#"{filter},"hint":"b_1""#)), (n + 2, n));
    let backward = found(&mut m, &format!(r#"{filter},"sort":{{"b":-1}},"hint":"b_1""#));
    let mut reversed = expected.clone();
    reversed.reverse();
    assert_eq!(ids(&backward), reversed);
    // A compound index in its own order within each value: `a` descending.
    let compound = found(&mut m, &format!(r#"{filter},"sort":{{"b":1,"a":-1}}"#));
    let mut by_b_then_a: Vec<i32> = expected.clone();
    by_b_then_a.sort_by_key(|i| (i % 7, -i));
    assert_eq!(ids(&compound), by_b_then_a);
}

#[test]
fn a_scan_that_checks_its_keys_over_a_long_range_walks_each_of_them() {
    let mut m = loaded();
    // A range of `b` and then `a`: the scan of `{b: 1, a: -1}` checks each key's `a`.
    let filter = r#""filter":{"b":{"$gte":2,"$lte":5},"a":{"$lt":1000}}"#;
    let hinted = format!(r#"{filter},"hint":"b_1_a_-1""#);
    let got = found(&mut m, &hinted);
    let mut expected = wanted(|i| (2..=5).contains(&(i % 7)) && i < 1000);
    expected.sort_by_key(|i| (i % 7, -i));
    assert_eq!(ids(&got), expected);
    // Each document, the first key of `b` 3, 4 and 5 (outside, a seek each) and the key of `b`
    // 6 that ends the scan.
    let n = i64::try_from(expected.len()).expect("a count");
    assert_eq!(examined(&mut m, &hinted), (n + 4, n));
    let backward = found(&mut m, &format!(r#"{filter},"sort":{{"b":-1,"a":1}},"hint":"b_1_a_-1""#));
    expected.reverse();
    assert_eq!(ids(&backward), expected);
    let few = found(&mut m, &format!(r#"{hinted},"limit":10"#));
    expected.reverse();
    assert_eq!(ids(&few), expected[..10].to_vec());
}

#[test]
fn a_long_range_of_id_reads_the_buckets_in_order_both_ways() {
    let mut m = loaded();
    let filter = r#""filter":{"_id":{"$gte":10,"$lt":2990}}"#;
    let forward = found(&mut m, &format!(r#"{filter},"sort":{{"_id":1}}"#));
    assert_eq!(ids(&forward), wanted(|i| (10..2990).contains(&i)));
    let backward = found(&mut m, &format!(r#"{filter},"sort":{{"_id":-1}},"limit":2500"#));
    let mut expected = wanted(|i| (490..2990).contains(&i));
    expected.reverse();
    assert_eq!(ids(&backward), expected);
}

#[test]
fn return_key_over_a_long_range_gives_each_entry_key() {
    let mut m = loaded();
    let got = found(
        &mut m,
        r#""filter":{"a":{"$gte":5,"$lt":2500}},"hint":"a_1","returnKey":true,"sort":{"a":1}"#,
    );
    let keys: Vec<i64> = got.iter().map(|d| int(field(d, "a"))).collect();
    assert_eq!(keys, (5..2500).collect::<Vec<i64>>());
    assert!(got.iter().all(|d| d.len() == 1), "{:?}", got.first());
}

#[test]
fn an_aggregation_and_a_count_over_a_long_range_add_up_what_the_documents_hold() {
    let mut m = loaded();
    let r = run(
        &mut m,
        r#"{"aggregate":"c","pipeline":[{"$match":{"a":{"$gte":100,"$lt":2900}}},
            {"$group":{"_id":"$b","n":{"$sum":1},"s":{"$sum":"$a"}}},{"$sort":{"_id":1}}],
            "cursor":{}}"#,
    );
    let Value::Array(groups) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(groups.len(), 7);
    for (b, g) in groups.iter().enumerate() {
        let Value::Document(g) = g else { panic!("{g:?}") };
        let b = i32::try_from(b).expect("a group");
        let members = wanted(|i| (100..2900).contains(&i) && i % 7 == b);
        assert_eq!(int(field(g, "n")), i64::try_from(members.len()).expect("a count"));
        assert_eq!(int(field(g, "s")), members.iter().map(|i| i64::from(*i)).sum::<i64>());
    }
    let query = json(r#"{"a":{"$gte":100,"$lt":2900},"b":{"$ne":0}}"#);
    let r = m.run("p", doc(&[("count", Value::from("c")), ("query", Value::Document(query))]));
    let n = wanted(|i| (100..2900).contains(&i) && i % 7 != 0).len();
    assert_eq!(int(field(&r, "n")), i64::try_from(n).expect("a count"));
}

#[test]
fn competing_plans_over_long_ranges_give_the_documents_of_the_collection_scan() {
    let mut m = loaded();
    // Both `a_1` and `b_1` serve: the trials read part of each range before one plan wins.
    let filter = r#""filter":{"a":{"$gte":200},"b":{"$in":[2,5]}}"#;
    let got = found(&mut m, filter);
    let natural = found(&mut m, &format!(r#"{filter},"hint":{{"$natural":1}}"#));
    let mut got_ids = ids(&got);
    got_ids.sort_unstable();
    let mut natural_ids = ids(&natural);
    natural_ids.sort_unstable();
    assert_eq!(got_ids, natural_ids);
    assert_eq!(got_ids, wanted(|i| i >= 200 && (i % 7 == 2 || i % 7 == 5)));
}
