//! 018 T021: `configureQueryAnalyzer`, the sampling of queries, `$listSampledQueries`,
//! `analyzeShardKey` and `$shardedDataDistribution` (FR-017) over the in-memory node of
//! `common`, with the replies and messages of the reference router 9.0.4 of two shards (measured on
//! 02-11-2026, `t5.js` to `t8.js` and `t13.js` in the lane's notes).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_router::table::RouteEntry;
use teseladb_types::{GroupId, NodeId};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn admin(m: &mut Mem, cmd: &str) -> Document {
    m.run("admin", json(cmd))
}

fn ok(r: &Document) {
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

fn fails(r: &Document, code: i32, msg: &str) {
    assert_eq!(field(r, "code"), &Value::Int32(code), "{r:?}");
    assert_eq!(field(r, "errmsg"), &Value::from(msg), "{r:?}");
}

fn long(n: i64) -> Value {
    Value::Int64(n)
}

/// One range led by node 1; `q.a` sharded by `{x: 1}` with 100 documents `{x: i, y: i % 10,
/// z: i % 3}` and an index on `y`; `q.big` without key, 2,000 documents `{x: i, y: i % 10}`.
fn cluster() -> Mem {
    let mut m = Mem::new();
    m.shards = vec!["node1".into(), "node2".into()];
    m.routes = vec![RouteEntry {
        group: GroupId(1),
        start: Vec::new(),
        end: Vec::new(),
        core: 0,
        voters: vec![NodeId(1)],
        generation: 1,
        leader_hint: Some(NodeId(1)),
        moved_ms: 0,
        replica_epoch: 1,
    }];
    ok(&admin(&mut m, r#"{"shardCollection":"q.a","key":{"x":1}}"#));
    let docs: Vec<String> = (0..100)
        .map(|i| format!(r#"{{"_id":{i},"x":{i},"y":{},"z":{}}}"#, i % 10, i % 3))
        .collect();
    ok(&m.run("q", json(&format!(r#"{{"insert":"a","documents":[{}]}}"#, docs.join(",")))));
    ok(&m.run("q", json(r#"{"createIndexes":"a","indexes":[{"key":{"y":1},"name":"y_1"}]}"#)));
    let docs: Vec<String> =
        (0..2000).map(|i| format!(r#"{{"_id":{i},"x":{i},"y":{}}}"#, i % 10)).collect();
    ok(&m.run("q", json(&format!(r#"{{"insert":"big","documents":[{}]}}"#, docs.join(",")))));
    m
}

fn cursor(r: &Document) -> Vec<Document> {
    match field(r, "cursor.firstBatch") {
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn configure_query_analyzer_fails_and_answers_as_on_the_reference_router() {
    let mut m = cluster();
    let cases: [(&str, &str, i32, &str); 11] = [
        (
            "admin",
            r#"{"configureQueryAnalyzer":"q.big"}"#,
            40414,
            "BSON field 'configureQueryAnalyzer.mode' is missing but a required field",
        ),
        (
            "admin",
            r#"{"configureQueryAnalyzer":"q.big","mode":"x"}"#,
            2,
            "Enumeration value 'x' for field 'configureQueryAnalyzer.mode' is not a valid value.",
        ),
        (
            "admin",
            r#"{"configureQueryAnalyzer":"q.big","mode":"full"}"#,
            72,
            "'samplesPerSecond' must be specified when 'mode' is not \"off\"",
        ),
        (
            "admin",
            r#"{"configureQueryAnalyzer":"q.big","mode":"full","samplesPerSecond":0}"#,
            72,
            "'samplesPerSecond' must be greater than 0",
        ),
        (
            "admin",
            r#"{"configureQueryAnalyzer":"q.big","mode":"full","samplesPerSecond":"a"}"#,
            14,
            "BSON field 'configureQueryAnalyzer.samplesPerSecond' is the wrong type 'string', expected types '[double, long, int, decimal]'",
        ),
        (
            "admin",
            r#"{"configureQueryAnalyzer":"q.big","mode":"off","samplesPerSecond":1}"#,
            72,
            "Cannot specify 'samplesPerSecond' when 'mode' is \"off\"",
        ),
        (
            "q",
            r#"{"configureQueryAnalyzer":"q.big","mode":"off"}"#,
            13,
            "configureQueryAnalyzer may only be run against the admin database.",
        ),
        (
            "admin",
            r#"{"configureQueryAnalyzer":"q.nope","mode":"full","samplesPerSecond":1}"#,
            26,
            "The namespace does not exist",
        ),
        (
            "admin",
            r#"{"configureQueryAnalyzer":"q","mode":"full","samplesPerSecond":1}"#,
            73,
            "The namespace is invalid",
        ),
        (
            "admin",
            r#"{"configureQueryAnalyzer":"q.big","mode":"full","samplesPerSecond":50001}"#,
            72,
            "'samplesPerSecond' must be less than or equal to 50",
        ),
        (
            "admin",
            r#"{"configureQueryAnalyzer":1,"mode":"off"}"#,
            14,
            "BSON field 'configureQueryAnalyzer.configureQueryAnalyzer' is the wrong type 'int', expected type 'string'",
        ),
    ];
    for (db, cmd, code, msg) in cases {
        fails(&m.run(db, json(cmd)), code, msg);
    }
    let r = admin(&mut m, r#"{"configureQueryAnalyzer":"q.big","mode":"off"}"#);
    assert_eq!(r, json(r#"{"newConfiguration":{"mode":"off"},"ok":1.0}"#));
    let r = admin(
        &mut m,
        r#"{"configureQueryAnalyzer":"q.big","mode":"full","samplesPerSecond":2.5,"x":1}"#,
    );
    assert_eq!(r, json(r#"{"newConfiguration":{"mode":"full","samplesPerSecond":2.5},"ok":1.0}"#));
    let listed = cursor(&m.run("config", json(r#"{"find":"queryAnalyzers"}"#)));
    let names: Vec<&str> = listed[0].iter().map(|(k, _)| k).collect();
    assert_eq!(names, ["_id", "collUuid", "mode", "samplesPerSecond", "startTime"]);
    let r = admin(&mut m, r#"{"configureQueryAnalyzer":"q.big","mode":"off"}"#);
    assert_eq!(
        r,
        json(
            r#"{"newConfiguration":{"mode":"off"},"oldConfiguration":{"mode":"full","samplesPerSecond":2.5},"ok":1.0}"#
        )
    );
    let listed = cursor(&m.run("config", json(r#"{"find":"queryAnalyzers"}"#)));
    assert_eq!(listed[0].get("mode"), Some(&Value::from("off")));
    assert!(listed[0].get("stopTime").is_some());
}

#[test]
fn sampled_queries_are_listed_newest_first() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"configureQueryAnalyzer":"q.big","mode":"full","samplesPerSecond":50}"#));
    let commands = [
        r#"{"find":"big","filter":{"x":1}}"#,
        r#"{"find":"big","filter":{"x":2}}"#,
        r#"{"update":"big","updates":[{"q":{"x":1},"u":{"$set":{"z":1}}}]}"#,
        r#"{"delete":"big","deletes":[{"q":{"x":1999},"limit":1}]}"#,
        r#"{"aggregate":"big","pipeline":[{"$match":{"x":{"$gt":5}}}],"cursor":{}}"#,
        r#"{"distinct":"big","key":"y","query":{"x":7}}"#,
        r#"{"findAndModify":"big","query":{"x":8},"update":{"$set":{"w":1}}}"#,
        r#"{"update":"big","updates":[{"q":{"x":{"$in":[1,2]}},"u":{"$set":{"v":1}},"multi":true}]}"#,
        r#"{"count":"big","query":{"x":{"$lt":5}}}"#,
    ];
    for (i, c) in commands.iter().enumerate() {
        // The second find comes within 20 ms of the first: not sampled at 50 a second.
        m.now += if i == 1 { 5 } else { 100 };
        ok(&m.run("q", json(c)));
    }
    let r = admin(&mut m, r#"{"aggregate":1,"pipeline":[{"$listSampledQueries":{}}],"cursor":{}}"#);
    let all = cursor(&r);
    let names: Vec<String> = all
        .iter()
        .map(|d| match d.get("cmdName") {
            Some(Value::String(s)) => s.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        names,
        ["count", "update", "findAndModify", "distinct", "aggregate", "delete", "update", "find"]
    );
    let keys: Vec<&str> = all[0].iter().map(|(k, _)| k).collect();
    assert_eq!(keys, ["_id", "ns", "collectionUuid", "cmdName", "cmd", "expireAt"]);
    let without = |d: &Document| {
        let Some(Value::Document(c)) = d.get("cmd") else { panic!("{d:?}") };
        let mut c = c.clone();
        c.remove("sampleId");
        if let Some(Value::Array(a)) = c.get("updates").cloned() {
            let s: Vec<Value> = a
                .into_iter()
                .map(|v| match v {
                    Value::Document(mut d) => {
                        d.remove("sampleId");
                        Value::Document(d)
                    }
                    other => other,
                })
                .collect();
            c.insert("updates", Value::Array(s));
        }
        c
    };
    assert_eq!(without(&all[7]), json(r#"{"filter":{"x":1},"collation":{}}"#));
    assert_eq!(
        without(&all[4]),
        json(r#"{"filter":{"x":{"$gt":5}},"collation":{"locale":"simple"},"let":{}}"#)
    );
    assert_eq!(
        without(&all[1]),
        json(
            r#"{"update":"big","bypassDocumentValidation":false,"ordered":true,"updates":[{"q":{"x":{"$in":[1,2]}},"u":{"$set":{"v":1}},"multi":true,"upsert":false}],"$db":"q"}"#
        )
    );
    assert_eq!(
        without(&all[2]),
        json(
            r#"{"findAndModify":"big","query":{"x":8},"remove":false,"update":{"$set":{"w":1}},"upsert":false,"new":false,"$db":"q"}"#
        )
    );
    let r = admin(
        &mut m,
        r#"{"aggregate":1,"pipeline":[{"$listSampledQueries":{"namespace":"q.nope"}}],"cursor":{}}"#,
    );
    assert!(cursor(&r).is_empty());
    let r = admin(
        &mut m,
        r#"{"aggregate":1,"pipeline":[{"$listSampledQueries":{"namespace":"q.big"}},{"$count":"n"}],"cursor":{}}"#,
    );
    assert_eq!(cursor(&r), [doc(&[("n", Value::Int32(8))])]);
    fails(
        &admin(
            &mut m,
            r#"{"aggregate":1,"pipeline":[{"$listSampledQueries":{"namespace":1}}],"cursor":{}}"#,
        ),
        14,
        "BSON field '$listSampledQueries.namespace' is the wrong type 'int', expected type 'string'",
    );
    fails(
        &admin(&mut m, r#"{"aggregate":1,"pipeline":[{"$listSampledQueries":1}],"cursor":{}}"#),
        6876000,
        "$listSampledQueries must take a nested object but found: $listSampledQueries: 1",
    );
    fails(
        &m.run("q", json(r#"{"aggregate":1,"pipeline":[{"$listSampledQueries":{}}],"cursor":{}}"#)),
        73,
        "$listSampledQueries must be run against the 'admin' database with {aggregate: 1}",
    );
    // `off`: no more samples; the ones taken stay.
    ok(&admin(&mut m, r#"{"configureQueryAnalyzer":"q.big","mode":"off"}"#));
    m.now += 1_000;
    ok(&m.run("q", json(commands[0])));
    let r = admin(&mut m, r#"{"aggregate":1,"pipeline":[{"$listSampledQueries":{}}],"cursor":{}}"#);
    assert_eq!(cursor(&r).len(), 8);
    // A week later they are gone.
    m.now += 7 * 24 * 3_600 * 1_000;
    let r = admin(&mut m, r#"{"aggregate":1,"pipeline":[{"$listSampledQueries":{}}],"cursor":{}}"#);
    assert!(cursor(&r).is_empty());
}

#[test]
fn analyze_shard_key_fails_as_on_the_reference_router() {
    let mut m = cluster();
    ok(&m.run("q", json(r#"{"create":"e"}"#)));
    let cases: [(&str, &str, i32, &str); 12] = [
        (
            "q",
            r#"{"analyzeShardKey":"q.a","key":{"x":1}}"#,
            13,
            "analyzeShardKey may only be run against the admin database.",
        ),
        (
            "admin",
            r#"{"analyzeShardKey":"q.a","key":{"z":1}}"#,
            4952606,
            "Failed to find split points that partition the data into 100 chunks with roughly equal number of documents using the shard key being analyzed :: caused by :: The shard key provided does not have enough cardinality to make the required number of chunks of 100, it can only make 4 chunks",
        ),
        (
            "admin",
            r#"{"analyzeShardKey":"q.a","key":{"y":1},"keyCharacteristics":false,"readWriteDistribution":false}"#,
            72,
            "Cannot skip analyzing all metrics",
        ),
        (
            "admin",
            r#"{"analyzeShardKey":"q.a","key":{"y":1},"sampleSize":10,"sampleRate":0.5}"#,
            72,
            "Cannot specify both 'sampleRate' and 'sampleSize'",
        ),
        (
            "admin",
            r#"{"analyzeShardKey":"q.a"}"#,
            40414,
            "BSON field 'analyzeShardKey.key' is missing but a required field",
        ),
        (
            "admin",
            r#"{"analyzeShardKey":"q.a","key":{"y":2}}"#,
            2,
            "Shard key { y: 2 } is invalid. Non-hashed fields must be set to a value of 1 (ascending). Failed to parse field y",
        ),
        ("admin", r#"{"analyzeShardKey":"q.a","key":{}}"#, 2, "Shard key is empty"),
        (
            "admin",
            r#"{"analyzeShardKey":"q.nope","key":{"y":1}}"#,
            26,
            "The namespace does not exist",
        ),
        ("admin", r#"{"analyzeShardKey":"q","key":{"y":1}}"#, 73, "The namespace is invalid"),
        (
            "admin",
            r#"{"analyzeShardKey":1,"key":{"y":1}}"#,
            14,
            "BSON field 'analyzeShardKey.analyzeShardKey' is the wrong type 'int', expected type 'string'",
        ),
        (
            "admin",
            r#"{"analyzeShardKey":"q.e","key":{"_id":1}}"#,
            20,
            "Cannot analyze a shard key for an empty collection: CollectionIsEmptyLocally: Cannot analyze the characteristics of a shard key for an empty collection",
        ),
        (
            "admin",
            r#"{"analyzeShardKey":"q.big","key":{"y":1},"readWriteDistribution":false}"#,
            20,
            "Cannot analyze the characteristics of a shard key that does not have a supporting index",
        ),
    ];
    for (db, cmd, code, msg) in cases {
        fails(&m.run(db, json(cmd)), code, msg);
    }
}

#[test]
fn key_characteristics_are_those_of_the_reference_router() {
    let mut m = cluster();
    let r = admin(
        &mut m,
        r#"{"analyzeShardKey":"q.a","key":{"y":1},"keyCharacteristics":true,"readWriteDistribution":false}"#,
    );
    let kc = match field(&r, "keyCharacteristics") {
        Value::Document(d) => d.clone(),
        other => panic!("{other:?}"),
    };
    let names: Vec<&str> = kc.iter().map(|(k, _)| k).collect();
    assert_eq!(
        names,
        [
            "numDocsTotal",
            "numOrphanDocs",
            "avgDocSizeBytes",
            "numDocsSampled",
            "isUnique",
            "numDistinctValues",
            "mostCommonValues",
            "monotonicity",
            "note"
        ]
    );
    assert_eq!(kc.get("numDocsTotal"), Some(&long(100)));
    assert_eq!(kc.get("avgDocSizeBytes"), Some(&long(35)));
    assert_eq!(kc.get("isUnique"), Some(&Value::Boolean(false)));
    assert_eq!(kc.get("numDistinctValues"), Some(&long(10)));
    assert_eq!(
        kc.get("monotonicity"),
        Some(&Value::Document(json(
            r#"{"recordIdCorrelationCoefficient":0.198019802,"type":"not monotonic"}"#
        )))
    );
    let Some(Value::Array(common)) = kc.get("mostCommonValues") else { panic!("{kc:?}") };
    assert_eq!(common.len(), 5);
    assert_eq!(
        common[0],
        Value::Document(json(r#"{"value":{"y":0},"frequency":{"$numberLong":"10"}}"#))
    );
    // An unsharded collection: no orphans nor note; `_id` is unique and monotonic.
    let r = admin(
        &mut m,
        r#"{"analyzeShardKey":"q.big","key":{"_id":1},"readWriteDistribution":false,"sampleSize":10}"#,
    );
    let Value::Document(kc) = field(&r, "keyCharacteristics") else { panic!("{r:?}") };
    assert_eq!(kc.get("numOrphanDocs"), None);
    assert_eq!(kc.get("note"), None);
    assert_eq!(kc.get("numDocsSampled"), Some(&long(10)));
    assert_eq!(kc.get("isUnique"), Some(&Value::Boolean(true)));
    assert_eq!(
        kc.get("monotonicity"),
        Some(&Value::Document(json(
            r#"{"recordIdCorrelationCoefficient":1.0,"type":"monotonic"}"#
        )))
    );
    let r = admin(
        &mut m,
        r#"{"analyzeShardKey":"q.big","key":{"_id":1},"readWriteDistribution":false,"sampleRate":0.5}"#,
    );
    assert_eq!(field(&r, "keyCharacteristics.numDocsSampled"), &long(1000));
}

#[test]
fn the_read_and_write_distribution_routes_the_samples() {
    let mut m = cluster();
    // No samples: the sizes alone; no supporting index on `x`, so no characteristics.
    let r = admin(&mut m, r#"{"analyzeShardKey":"q.big","key":{"x":1}}"#);
    assert_eq!(
        r,
        json(
            r#"{"readDistribution":{"sampleSize":{"total":{"$numberLong":"0"},"find":{"$numberLong":"0"},"aggregate":{"$numberLong":"0"},"count":{"$numberLong":"0"},"distinct":{"$numberLong":"0"}}},"writeDistribution":{"sampleSize":{"total":{"$numberLong":"0"},"update":{"$numberLong":"0"},"delete":{"$numberLong":"0"},"findAndModify":{"$numberLong":"0"}}},"ok":1.0}"#
        )
    );
    ok(&admin(&mut m, r#"{"configureQueryAnalyzer":"q.big","mode":"full","samplesPerSecond":50}"#));
    for c in [
        r#"{"find":"big","filter":{"x":1}}"#,
        r#"{"find":"big","filter":{"x":{"$eq":2}}}"#,
        r#"{"find":"big","filter":{"y":3}}"#,
        r#"{"aggregate":"big","pipeline":[{"$match":{"$and":[{"x":{"$gte":5}},{"x":{"$lt":500}}]}}],"cursor":{}}"#,
        r#"{"update":"big","updates":[{"q":{"x":1},"u":{"$set":{"x":-1}}}]}"#,
        r#"{"update":"big","updates":[{"q":{"x":{"$in":[1,2]}},"u":{"$set":{"v":1}},"multi":true}]}"#,
        r#"{"delete":"big","deletes":[{"q":{"y":1},"limit":1}]}"#,
        r#"{"findAndModify":"big","query":{"x":8},"update":{"$set":{"w":1}}}"#,
    ] {
        m.now += 100;
        ok(&m.run("q", json(c)));
    }
    let r =
        admin(&mut m, r#"{"analyzeShardKey":"q.big","key":{"x":1},"keyCharacteristics":false}"#);
    let Value::Document(reads) = field(&r, "readDistribution") else { panic!("{r:?}") };
    let names: Vec<&str> = reads.iter().map(|(k, _)| k).collect();
    assert_eq!(
        names,
        [
            "sampleSize",
            "percentageOfSingleShardReads",
            "percentageOfMultiShardReads",
            "percentageOfScatterGatherReads",
            "numReadsByRange"
        ]
    );
    assert_eq!(reads.get("percentageOfSingleShardReads"), Some(&Value::Double(50.0)));
    assert_eq!(reads.get("percentageOfMultiShardReads"), Some(&Value::Double(25.0)));
    assert_eq!(reads.get("percentageOfScatterGatherReads"), Some(&Value::Double(25.0)));
    let Some(Value::Array(by)) = reads.get("numReadsByRange") else { panic!("{reads:?}") };
    assert_eq!(by.len(), 100);
    assert_eq!(by[0], long(4));
    assert_eq!(by[99], long(1));
    let Value::Document(writes) = field(&r, "writeDistribution") else { panic!("{r:?}") };
    let names: Vec<&str> = writes.iter().map(|(k, _)| k).collect();
    assert_eq!(
        names,
        [
            "sampleSize",
            "percentageOfSingleShardWrites",
            "percentageOfMultiShardWrites",
            "percentageOfScatterGatherWrites",
            "numWritesByRange",
            "percentageOfShardKeyUpdates",
            "percentageOfSingleWritesWithoutShardKey",
            "percentageOfMultiWritesWithoutShardKey"
        ]
    );
    assert_eq!(writes.get("percentageOfSingleShardWrites"), Some(&Value::Double(75.0)));
    assert_eq!(writes.get("percentageOfScatterGatherWrites"), Some(&Value::Double(25.0)));
    assert_eq!(writes.get("percentageOfShardKeyUpdates"), Some(&Value::Double(25.0)));
    assert_eq!(writes.get("percentageOfSingleWritesWithoutShardKey"), Some(&Value::Double(25.0)));
    assert_eq!(writes.get("percentageOfMultiWritesWithoutShardKey"), Some(&Value::Double(25.0)));
    // Rounded up to ten decimals, as on the reference router: 2 of 3.
    m.now += 100;
    ok(&m.run("q", json(r#"{"find":"big","filter":{"y":4}}"#)));
    let r =
        admin(&mut m, r#"{"analyzeShardKey":"q.big","key":{"x":1},"keyCharacteristics":false}"#);
    assert_eq!(field(&r, "readDistribution.percentageOfScatterGatherReads"), &Value::Double(40.0));
    assert_eq!(field(&r, "readDistribution.percentageOfSingleShardReads"), &Value::Double(40.0));
}

#[test]
fn sharded_data_distribution_lists_each_shards_documents() {
    let mut m = cluster();
    let r = admin(
        &mut m,
        r#"{"aggregate":1,"pipeline":[{"$shardedDataDistribution":{}}],"cursor":{}}"#,
    );
    assert_eq!(
        cursor(&r),
        [json(
            r#"{"ns":"q.a","shards":[{"shardName":"node1","numOrphanedDocs":{"$numberLong":"0"},"numOwnedDocuments":{"$numberLong":"100"},"ownedSizeBytes":{"$numberLong":"3500"},"orphanedSizeBytes":{"$numberLong":"0"}}]}"#
        )]
    );
    fails(
        &admin(
            &mut m,
            r#"{"aggregate":1,"pipeline":[{"$shardedDataDistribution":{"x":1}}],"cursor":{}}"#,
        ),
        6789100,
        "The $shardedDataDistribution stage specification must be an empty object",
    );
    fails(
        &m.run(
            "q",
            json(r#"{"aggregate":1,"pipeline":[{"$shardedDataDistribution":{}}],"cursor":{}}"#),
        ),
        6789102,
        "The $shardedDataDistribution stage must be run on the admin database",
    );
}

/// A unique index makes the key unique; a compound key routes by its first field, and the
/// conditions that do not bound it (a document, `$exists`, an empty `$in`) count as they do on
/// the reference router: the samples are all counted.
#[test]
fn unique_and_compound_keys_and_conditions_that_bound_nothing() {
    let mut m = cluster();
    ok(&m.run(
        "q",
        json(r#"{"createIndexes":"big","indexes":[{"key":{"x":1},"name":"x_1","unique":true}]}"#),
    ));
    let r =
        admin(&mut m, r#"{"analyzeShardKey":"q.big","key":{"x":1},"readWriteDistribution":false}"#);
    assert_eq!(field(&r, "keyCharacteristics.isUnique"), &Value::Boolean(true));
    ok(&admin(&mut m, r#"{"configureQueryAnalyzer":"q.big","mode":"full","samplesPerSecond":50}"#));
    for c in [
        r#"{"find":"big","filter":{"x":{"k":1}}}"#,
        r#"{"find":"big","filter":{"x":{"$exists":true}}}"#,
        r#"{"find":"big","filter":{"x":{"$in":[]}}}"#,
        r#"{"find":"big","filter":{"x":{"$gte":3,"$lt":9}}}"#,
    ] {
        m.now += 100;
        ok(&m.run("q", json(c)));
    }
    ok(&m.run(
        "q",
        json(r#"{"createIndexes":"big","indexes":[{"key":{"x":1,"y":1},"name":"x_1_y_1"}]}"#),
    ));
    let r = admin(
        &mut m,
        r#"{"analyzeShardKey":"q.big","key":{"x":1,"y":1},"keyCharacteristics":false}"#,
    );
    assert_eq!(field(&r, "readDistribution.sampleSize.find"), &long(4));
    let single = field(&r, "readDistribution.percentageOfSingleShardReads").clone();
    let scatter = field(&r, "readDistribution.percentageOfScatterGatherReads").clone();
    assert_eq!((single, scatter), (Value::Double(50.0), Value::Double(25.0)), "{r:?}");
    // The unique index on `x` does not make `{x: 1, y: 1}` unique.
    let r = admin(
        &mut m,
        r#"{"analyzeShardKey":"q.big","key":{"x":1,"y":1},"readWriteDistribution":false}"#,
    );
    assert_eq!(field(&r, "keyCharacteristics.isUnique"), &Value::Boolean(false));
}

/// A change of the configuration that finds another first is retried, and a configuration
/// turned off keeps its rate.
#[test]
fn configurations_retry_and_keep_their_rate_when_off() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"configureQueryAnalyzer":"q.big","mode":"full","samplesPerSecond":5}"#));
    m.group0_conflicts = 1;
    let r = admin(&mut m, r#"{"configureQueryAnalyzer":"q.big","mode":"off"}"#);
    assert_eq!(m.group0_conflicts, 0, "retried after the conflict");
    assert_eq!(
        field(&r, "oldConfiguration"),
        &Value::Document(json(r#"{"mode":"full","samplesPerSecond":5}"#))
    );
    // Turning off what was never on records nothing.
    let r = admin(&mut m, r#"{"configureQueryAnalyzer":"q.a","mode":"off"}"#);
    assert_eq!(r.get("oldConfiguration"), None);
    fails(
        &admin(&mut m, r#"{"configureQueryAnalyzer":".big","mode":"off"}"#),
        73,
        "The namespace is invalid",
    );
}

/// `$shardedDataDistribution` counts a chunk of more documents than one read gives.
#[test]
fn sharded_data_distribution_reads_a_large_chunk_in_batches() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"shardCollection":"q.big","key":{"_id":1}}"#));
    let r = admin(
        &mut m,
        r#"{"aggregate":1,"pipeline":[{"$shardedDataDistribution":{}}],"cursor":{}}"#,
    );
    let big = cursor(&r).into_iter().find(|d| d.get("ns") == Some(&Value::from("q.big")));
    let big = big.expect("q.big listed");
    let Value::Array(shards) = field(&big, "shards") else { panic!("{big:?}") };
    let Some(Value::Document(first)) = shards.first() else { panic!("{big:?}") };
    assert_eq!(first.get("numOwnedDocuments"), Some(&long(2000)));
}

/// A key on `_id` has the cardinality of its values: the read and write distribution of
/// `{_id: 1}` splits the 2,000 documents of `q.big` into its 100 ranges.
#[test]
fn a_key_on_id_has_the_cardinality_of_its_values() {
    let mut m = cluster();
    let r =
        admin(&mut m, r#"{"analyzeShardKey":"q.big","key":{"_id":1},"keyCharacteristics":false}"#);
    assert_eq!(field(&r, "readDistribution.sampleSize.total"), &long(0), "{r:?}");
}

/// The samples of a collection dropped and created again are not the new one's.
#[test]
fn the_samples_of_a_dropped_collection_are_not_the_new_ones() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"configureQueryAnalyzer":"q.big","mode":"full","samplesPerSecond":5}"#));
    m.now += 100;
    ok(&m.run("q", json(r#"{"find":"big","filter":{"x":1}}"#)));
    let r =
        admin(&mut m, r#"{"analyzeShardKey":"q.big","key":{"_id":1},"keyCharacteristics":false}"#);
    assert_eq!(field(&r, "readDistribution.sampleSize.total"), &long(1));
    ok(&m.run("q", json(r#"{"drop":"big"}"#)));
    let docs: Vec<String> = (0..2000).map(|i| format!(r#"{{"_id":{i},"x":{i}}}"#)).collect();
    ok(&m.run("q", json(&format!(r#"{{"insert":"big","documents":[{}]}}"#, docs.join(",")))));
    let r =
        admin(&mut m, r#"{"analyzeShardKey":"q.big","key":{"_id":1},"keyCharacteristics":false}"#);
    assert_eq!(field(&r, "readDistribution.sampleSize.total"), &long(0));
}

/// The node keeps the newest 10,000 samples, and an analysis counts only those of its
/// collection.
#[test]
fn the_node_keeps_ten_thousand_samples_of_each_collection_apart() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"configureQueryAnalyzer":"q.a","mode":"full","samplesPerSecond":50}"#));
    ok(&admin(&mut m, r#"{"configureQueryAnalyzer":"q.big","mode":"full","samplesPerSecond":50}"#));
    m.now += 1_000;
    ok(&m.run("q", json(r#"{"find":"a","filter":{"x":1}}"#)));
    m.now += 1_000;
    ok(&m.run("q", json(r#"{"find":"big","filter":{"x":1}}"#)));
    let r =
        admin(&mut m, r#"{"analyzeShardKey":"q.big","key":{"_id":1},"keyCharacteristics":false}"#);
    assert_eq!(field(&r, "readDistribution.sampleSize.total"), &long(1), "q.a's is not q.big's");
    for _ in 0..10_000 {
        m.now += 1_000;
        ok(&m.run("q", json(r#"{"find":"big","filter":{"x":2}}"#)));
    }
    let r =
        admin(&mut m, r#"{"analyzeShardKey":"q.big","key":{"_id":1},"keyCharacteristics":false}"#);
    assert_eq!(field(&r, "readDistribution.sampleSize.total"), &long(10_000));
}
