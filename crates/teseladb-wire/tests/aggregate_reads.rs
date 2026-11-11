//! Reads of `aggregate` whose leading `$match` may go through the planner, and the namespace and
//! argument checks of the stages of the node itself, against what a `refnode` 9.0.4 answered on
//! 01-11-2026.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn run(m: &mut Mem, db: &str, cmd: &str) -> Document {
    m.run(db, json(cmd))
}

fn batch(r: &Document) -> Vec<Document> {
    let Value::Array(b) = field(r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|d| match d {
            Value::Document(d) => d.clone(),
            other => panic!("{other:?}"),
        })
        .collect()
}

fn loaded() -> Mem {
    let mut m = Mem::new();
    run(
        &mut m,
        "r",
        r#"{"insert":"c","documents":[{"_id":1,"a":1,"b":"x"},{"_id":2,"a":2,"b":"y"},{"_id":3,"a":3,"b":"x"}]}"#,
    );
    run(&mut m, "r", r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"}]}"#);
    m
}

/// A leading `$match` on a view, on a collection that does not exist and one that matches
/// nothing.
#[test]
fn a_leading_match_reads_views_missing_collections_and_empty_matches() {
    let mut m = loaded();
    run(&mut m, "r", r#"{"create":"v","viewOn":"c","pipeline":[{"$match":{"b":"x"}}]}"#);
    let r = run(
        &mut m,
        "r",
        r#"{"aggregate":"v","pipeline":[{"$match":{"a":{"$gt":1}}}],"cursor":{}}"#,
    );
    assert_eq!(batch(&r), vec![json(r#"{"_id":3,"a":3,"b":"x"}"#)], "{r:?}");
    let r = run(&mut m, "r", r#"{"aggregate":"nope","pipeline":[{"$match":{"a":1}}],"cursor":{}}"#);
    assert_eq!(batch(&r), Vec::<Document>::new(), "{r:?}");
    let empty_in = doc(&[("$in", Value::Array(Vec::new()))]);
    let pipeline = vec![Value::Document(doc(&[(
        "$match",
        Value::Document(doc(&[("a", Value::Document(empty_in))])),
    )]))];
    let r = m.run(
        "r",
        doc(&[
            ("aggregate", Value::from("c")),
            ("pipeline", Value::Array(pipeline)),
            ("cursor", Value::Document(Document::new())),
        ]),
    );
    assert_eq!(batch(&r), Vec::<Document>::new(), "{r:?}");
}

/// `$currentOp` runs only on `admin` with `{aggregate: 1}`.
#[test]
fn current_op_runs_only_on_admin_without_a_collection() {
    let mut m = loaded();
    let msg = "$currentOp must be run against the 'admin' database with {aggregate: 1}";
    for (db, target) in [("r", "1"), ("admin", r#""c""#)] {
        let r = run(
            &mut m,
            db,
            &format!(
                r#"{{"aggregate":{target},"pipeline":[{{"$currentOp":{{}}}}],"cursor":{{}}}}"#
            ),
        );
        assert_eq!(field(&r, "code"), &Value::Int32(73), "{db}: {r:?}");
        assert_eq!(field(&r, "errmsg"), &Value::from(msg));
    }
}

/// The users of `$listLocalSessions` are documents with a user and a database as strings.
#[test]
fn list_local_sessions_checks_its_users() {
    let mut m = loaded();
    let cases = [
        (
            r#"[{"user":"x","db":"admin"},{"user":1,"db":"a"}]"#,
            "BSON field '$listLocalSessions.users.user' is the wrong type 'int', expected type 'string'",
        ),
        (
            "[5]",
            "BSON field '$listLocalSessions.users.0' is the wrong type 'int', expected type 'object'",
        ),
    ];
    for (users, msg) in cases {
        let r = run(
            &mut m,
            "admin",
            &format!(
                r#"{{"aggregate":1,"pipeline":[{{"$listLocalSessions":{{"users":{users}}}}}],"cursor":{{}}}}"#
            ),
        );
        assert_eq!(field(&r, "code"), &Value::Int32(14), "{users}: {r:?}");
        assert_eq!(field(&r, "errmsg"), &Value::from(msg));
    }
    let r = run(
        &mut m,
        "admin",
        r#"{"aggregate":1,"pipeline":[{"$listLocalSessions":{"users":[{"user":"x","db":"admin"}]}}],"cursor":{}}"#,
    );
    assert_eq!(batch(&r), Vec::<Document>::new(), "{r:?}");
}

/// A `find` on a view whose pipeline starts with a stage of the node itself sees the node as an
/// aggregation does (measured on a `refrouter` 9.0.4 on 02-11-2026): the indexes of the collection
/// under the view, its count, its catalog entry and its geospatial index.
#[test]
fn a_view_over_a_stage_of_the_node_reads_the_collection_under_it() {
    let mut m = loaded();
    run(
        &mut m,
        "r",
        r#"{"update":"c","updates":[{"q":{},"u":{"$set":{"loc":[0,0]}},"multi":true}]}"#,
    );
    run(&mut m, "r", r#"{"createIndexes":"c","indexes":[{"key":{"loc":"2d"},"name":"loc_2d"}]}"#);
    let view = |m: &mut Mem, pipeline: &str| {
        run(m, "r", r#"{"drop":"v"}"#);
        let r = run(m, "r", &format!(r#"{{"create":"v","viewOn":"c","pipeline":{pipeline}}}"#));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{pipeline}: {r:?}");
        batch(&run(m, "r", r#"{"find":"v"}"#))
    };
    let mut names: Vec<String> =
        view(&mut m, r#"[{"$indexStats":{}},{"$project":{"name":1,"_id":0}}]"#)
            .iter()
            .map(|d| format!("{:?}", d.get("name")))
            .collect();
    names.sort();
    assert_eq!(names.len(), 3, "{names:?}");
    let counted = view(&mut m, r#"[{"$collStats":{"count":{}}},{"$project":{"count":1,"_id":0}}]"#);
    assert_eq!(counted, [json(r#"{"count":3}"#)]);
    assert!(view(&mut m, r#"[{"$planCacheStats":{}}]"#).is_empty());
    let catalog = view(&mut m, r#"[{"$listCatalog":{}},{"$project":{"name":1,"_id":0}}]"#);
    assert_eq!(catalog, [json(r#"{"name":"c"}"#)]);
    let near =
        view(&mut m, r#"[{"$geoNear":{"near":[0,0],"distanceField":"d"}},{"$project":{"d":1}}]"#);
    assert_eq!(near.len(), 3, "{near:?}");
}

/// A view whose pipeline starts with a stage of the node, read from another pipeline (`$lookup`,
/// `$unionWith`), by `aggregate` and inside an explained `$facet`, sees the node as a `find` on it
/// does (measured on a `refnode` 9.0.4 on 03-11-2026: the `_id_` index of the collection).
#[test]
fn a_view_over_a_stage_of_the_node_read_from_another_pipeline() {
    let mut m = loaded();
    let r = run(
        &mut m,
        "r",
        r#"{"create":"v","viewOn":"c","pipeline":[{"$indexStats":{}},{"$project":{"name":1,"_id":0}}]}"#,
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let names = |docs: Vec<Document>| {
        let mut n: Vec<String> = docs.iter().map(|d| format!("{:?}", d.get("name"))).collect();
        n.sort();
        n
    };
    let both = vec![r#"Some(String("_id_"))"#.to_owned(), r#"Some(String("a_1"))"#.to_owned()];
    let r = run(&mut m, "r", r#"{"aggregate":"v","pipeline":[],"cursor":{}}"#);
    assert_eq!(names(batch(&r)), both, "{r:?}");
    let r = run(
        &mut m,
        "r",
        r#"{"aggregate":"c","pipeline":[{"$limit":1},{"$lookup":{"from":"v","pipeline":[],"as":"x"}},{"$unwind":"$x"},{"$replaceWith":"$x"}],"cursor":{}}"#,
    );
    assert_eq!(names(batch(&r)), both, "{r:?}");
    let r = run(
        &mut m,
        "r",
        r#"{"aggregate":"c","pipeline":[{"$match":{"_id":0}},{"$unionWith":"v"}],"cursor":{}}"#,
    );
    assert_eq!(names(batch(&r)), both, "{r:?}");
    let r = run(
        &mut m,
        "r",
        r#"{"aggregate":"c","pipeline":[{"$limit":1},{"$lookup":{"from":"v","localField":"b","foreignField":"name","as":"x"}}],"cursor":{}}"#,
    );
    assert_eq!(batch(&r).len(), 1, "{r:?}");
}

/// The namespaces of `$listExtensions`, `$listCatalog` and `$listSampledQueries` that run
/// (measured on a `refnode` 9.0.4 on 03-11-2026): the first two on `admin` with `aggregate: 1`,
/// `$listCatalog` on a view with no stages lists the collection under it, `$listSampledQueries`
/// on a collection of `admin` is refused.
#[test]
fn the_namespaces_of_the_admin_stages() {
    let mut m = loaded();
    let r =
        run(&mut m, "admin", r#"{"aggregate":1,"pipeline":[{"$listExtensions":{}}],"cursor":{}}"#);
    assert_eq!(batch(&r), Vec::<Document>::new(), "{r:?}");
    let r = run(
        &mut m,
        "admin",
        r#"{"aggregate":1,"pipeline":[{"$listCatalog":{}},{"$match":{"db":"r","name":"c"}},{"$project":{"name":1,"db":1,"_id":0}}],"cursor":{}}"#,
    );
    assert_eq!(batch(&r), [json(r#"{"db":"r","name":"c"}"#)], "{r:?}");
    let r = run(
        &mut m,
        "admin",
        r#"{"aggregate":"c","pipeline":[{"$listSampledQueries":{}}],"cursor":{}}"#,
    );
    assert_eq!(field(&r, "code"), &Value::Int32(73), "{r:?}");
    let msg = "$listSampledQueries must be run against the 'admin' database with {aggregate: 1}";
    assert_eq!(field(&r, "errmsg"), &Value::from(msg));
    run(&mut m, "r", r#"{"create":"ve","viewOn":"c","pipeline":[]}"#);
    let r = run(
        &mut m,
        "r",
        r#"{"aggregate":"ve","pipeline":[{"$listCatalog":{}},{"$project":{"name":1,"type":1,"_id":0}}],"cursor":{}}"#,
    );
    assert_eq!(batch(&r), [json(r#"{"name":"c","type":"collection"}"#)], "{r:?}");
}

/// The storage of a collection with no document has no `avgObjSize`; a time series has no
/// `avgBucketSize` without buckets, and no counters before its first insert (measured on a
/// `refnode` 9.0.4 on 03-11-2026).
#[test]
fn the_storage_of_empty_collections_has_no_averages() {
    let mut m = Mem::new();
    let stats = |m: &mut Mem, coll: &str| {
        let cmd = format!(
            r#"{{"aggregate":"{coll}","pipeline":[{{"$collStats":{{"storageStats":{{}}}}}}],"cursor":{{}}}}"#
        );
        let r = run(m, "r", &cmd);
        let Value::Document(s) = field(&batch(&r)[0], "storageStats").clone() else { panic!() };
        s
    };
    run(&mut m, "r", r#"{"create":"e"}"#);
    let s = stats(&mut m, "e");
    assert_eq!((s.get("count"), s.get("avgObjSize")), (Some(&Value::Int32(0)), None), "{s:?}");
    run(&mut m, "r", r#"{"create":"ts","timeseries":{"timeField":"t"}}"#);
    let s = stats(&mut m, "ts");
    assert_eq!(
        s.get("timeseries"),
        Some(&Value::Document(json(r#"{"bucketsNs":"r.ts","bucketCount":0}"#))),
        "{s:?}"
    );
    run(
        &mut m,
        "r",
        r#"{"insert":"ts","documents":[{"t":{"$date":"2026-01-01T00:00:00Z"},"m":1}]}"#,
    );
    run(&mut m, "r", r#"{"delete":"ts","deletes":[{"q":{},"limit":0}]}"#);
    let s = stats(&mut m, "ts");
    let Some(Value::Document(t)) = s.get("timeseries") else { panic!("{s:?}") };
    assert_eq!(
        (t.get("bucketCount"), t.get("avgBucketSize")),
        (Some(&Value::Int32(0)), None),
        "{t:?}"
    );
    assert_eq!(t.get("numCommits"), Some(&Value::Int32(1)), "{t:?}");
}

/// The stages of the node on a time series: `$indexStats` lists the index on its meta and time
/// fields, `$listCatalog` its entry with that index, and `$listClusterCatalog` on `admin` lists
/// it with the other databases (measured on a `refnode` 9.0.4 on 03-11-2026).
#[test]
fn the_stages_of_the_node_on_a_time_series() {
    let mut m = Mem::new();
    run(&mut m, "t3", r#"{"create":"ts","timeseries":{"timeField":"t","metaField":"m"}}"#);
    run(
        &mut m,
        "t3",
        r#"{"insert":"ts","documents":[{"t":{"$date":"2026-01-01T00:00:00Z"},"m":1}]}"#,
    );
    let r = run(
        &mut m,
        "t3",
        r#"{"aggregate":"ts","pipeline":[{"$indexStats":{}},{"$project":{"name":1,"key":1,"spec":1,"_id":0}}],"cursor":{}}"#,
    );
    let want = r#"{"name":"m_1_t_1","key":{"m":1,"t":1},"spec":{"v":2,"key":{"m":1,"t":1},"name":"m_1_t_1"}}"#;
    assert_eq!(batch(&r), [json(want)], "{r:?}");
    let r = run(
        &mut m,
        "t3",
        r#"{"aggregate":"ts","pipeline":[{"$listCatalog":{}},{"$project":{"name":1,"type":1,"i":"$md.indexes.spec.name","c":"$md.options.clusteredIndex","t":"$md.options.timeseries","x":"$md.timeseriesBucketsMayHaveMixedSchemaData","_id":0}}],"cursor":{}}"#,
    );
    let want = r#"{"name":"ts","type":"timeseries","i":["m_1_t_1"],"c":true,"t":{"timeField":"t","metaField":"m","granularity":"seconds","bucketMaxSpanSeconds":3600,"fixedBucketing":true},"x":false}"#;
    assert_eq!(batch(&r), [json(want)], "{r:?}");
    let r = run(
        &mut m,
        "admin",
        r#"{"aggregate":1,"pipeline":[{"$listClusterCatalog":{}},{"$match":{"db":"t3"}},{"$project":{"ns":1,"type":1,"_id":0}}],"cursor":{}}"#,
    );
    assert_eq!(batch(&r), [json(r#"{"ns":"t3.ts","type":"timeseries"}"#)], "{r:?}");
}

/// `$geoNear` on a collection that does not exist, and on one whose other indexes are not
/// geospatial (measured on a `refnode` 9.0.4 on 03-11-2026).
#[test]
fn geo_near_needs_a_collection_and_its_geospatial_index() {
    let mut m = loaded();
    let near = r#"[{"$geoNear":{"near":[0,0],"distanceField":"d"}},{"$project":{"d":1}}]"#;
    let r = run(&mut m, "r", &format!(r#"{{"aggregate":"nope","pipeline":{near},"cursor":{{}}}}"#));
    assert_eq!(field(&r, "code"), &Value::Int32(26), "{r:?}");
    let msg = "$geoNear needs a collection to run, but r.nope does not exist";
    assert_eq!(field(&r, "errmsg"), &Value::from(msg));
    run(
        &mut m,
        "r",
        r#"{"update":"c","updates":[{"q":{},"u":{"$set":{"loc":[1,1]}},"multi":true}]}"#,
    );
    run(
        &mut m,
        "r",
        r#"{"createIndexes":"c","indexes":[{"key":{"b":"hashed"},"name":"b_h"},{"key":{"loc":"2d"},"name":"l"}]}"#,
    );
    let r = run(&mut m, "r", &format!(r#"{{"aggregate":"c","pipeline":{near},"cursor":{{}}}}"#));
    assert_eq!(batch(&r).len(), 3, "{r:?}");
}

/// An `$expr` comparison compares across types with or without an index, and a multikey index
/// does not answer it, since the expression compares a whole array (measured on a `refrouter`
/// 9.0.4 on 11-11-2026).
#[test]
fn expr_comparisons_cross_types_through_an_index() {
    let ids = |r: &Document| {
        let mut v: Vec<i32> = batch(r)
            .iter()
            .map(|d| match d.get("_id") {
                Some(Value::Int32(n)) => *n,
                other => panic!("{other:?}"),
            })
            .collect();
        v.sort_unstable();
        v
    };
    let docs = r#"[{"_id":0,"v":1},{"_id":1,"v":2},{"_id":2,"v":"a"},{"_id":3,"v":"b"},
        {"_id":4,"v":null},{"_id":5,"v":{"x":1}},{"_id":6,"v":true},{"_id":99}]"#;
    for multikey in [false, true] {
        let mut m = Mem::new();
        run(&mut m, "r", &format!(r#"{{"insert":"c","documents":{docs}}}"#));
        if multikey {
            run(&mut m, "r", r#"{"insert":"c","documents":[{"_id":50,"v":[0]}]}"#);
        }
        run(&mut m, "r", r#"{"createIndexes":"c","indexes":[{"key":{"v":1},"name":"v_1"}]}"#);
        let cases: [(&str, &[i32]); 4] = [
            (r#"{"$gt":["$v",1]}"#, if multikey { &[1, 2, 3, 5, 6, 50] } else { &[1, 2, 3, 5, 6] }),
            (r#"{"$lt":["$v","a"]}"#, &[0, 1, 4, 99]),
            (r#"{"$lte":["$v",2]}"#, &[0, 1, 4, 99]),
            (r#"{"$eq":["$v",null]}"#, &[4]),
        ];
        for (expr, want) in cases {
            let r = run(&mut m, "r", &format!(r#"{{"find":"c","filter":{{"$expr":{expr}}}}}"#));
            assert_eq!(ids(&r), want, "{expr} multikey {multikey}");
        }
    }
}
