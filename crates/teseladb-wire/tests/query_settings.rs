//! Query settings and `$queryStats` (010 T040, FR-023) as a `refrouter` 9.0.4 answers them
//! (measured on 01-11-2026 with `setQuerySettings`, `removeQuerySettings`, `$querySettings` and
//! `$queryStats` on database `qsh`).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

/// `queryShapeHash` of `{aggregate: "c", pipeline: [{$match: {a: <number>}}]}` on `qsh`.
const MATCH_HASH: &str = "90C79CCC0A5DBEF8ED895DECFDF91690CA4B2B0B1367EF1395D5155745FF33E9";
/// `queryShapeHash` of `{aggregate: "c", pipeline: []}` on `qsh`.
const EMPTY_HASH: &str = "0F8B5091E18615FF2DB199066882F6EF5E3D81FAE81DA9D7AB09ED7032862F33";
const MATCH_QUERY: &str = r#"{"aggregate":"c","pipeline":[{"$match":{"a":1}}],"$db":"qsh"}"#;

fn cmd(text: &str) -> Document {
    extjson::parse(text).expect("a command")
}

fn error(r: &Document) -> (i32, String) {
    match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(m))) => (*c, m.clone()),
        _ => panic!("not an error: {r:?}"),
    }
}

fn set(m: &mut Mem, target: &str, settings: &str) -> Document {
    m.run("admin", cmd(&format!(r#"{{"setQuerySettings":{target},"settings":{settings}}}"#)))
}

fn listed(m: &mut Mem, debug: bool) -> Vec<Value> {
    let stage = format!(r#"{{"$querySettings":{{"showDebugQueryShape":{debug}}}}}"#);
    let r =
        m.run("admin", cmd(&format!(r#"{{"aggregate":1,"pipeline":[{stage}],"cursor":{{}}}}"#)));
    match field(&r, "cursor.firstBatch") {
        Value::Array(b) => b.clone(),
        other => panic!("{other:?} in {r:?}"),
    }
}

fn seeded() -> Mem {
    let mut m = Mem::new();
    m.run("qsh", cmd(r#"{"insert":"c","documents":[{"_id":1,"a":1},{"_id":2,"a":5}]}"#));
    m
}

#[test]
fn set_query_settings_refuses_what_refdb_refuses() {
    let mut m = seeded();
    let cases: [(&str, &str, i32, &str); 9] = [
        (
            MATCH_QUERY,
            r#"{"bogus":1}"#,
            8_727_502,
            "settings field in setQuerySettings command cannot be empty",
        ),
        (
            MATCH_QUERY,
            "{}",
            8_727_502,
            "settings field in setQuerySettings command cannot be empty",
        ),
        (
            MATCH_QUERY,
            r#"{"reject":false}"#,
            7_746_604,
            "the resulting settings cannot be empty or contain only default values",
        ),
        (r#""zz""#, r#"{"reject":true}"#, 2, "Hash input is not a hex string"),
        (r#""ABCD""#, r#"{"reject":true}"#, 16, "Unsupported SHA256Block hash length: 2"),
        (
            r#"{"aggregate":"c","pipeline":[]}"#,
            r#"{"reject":true}"#,
            40414,
            "BSON field 'aggregateCommandRequest.$db' is missing but a required field",
        ),
        (
            r#"{"insert":"c","documents":[],"$db":"qsh"}"#,
            r#"{"reject":true}"#,
            7_746_402,
            r#"QueryShape can not be computed for command: { insert: "c", documents: [], $db: "qsh" }"#,
        ),
        (
            MATCH_QUERY,
            r#"{"queryFramework":"bogus"}"#,
            2,
            "Invalid value for 'queryFramework': expected classic or sbe, but got bogus",
        ),
        (
            MATCH_QUERY,
            r#"{"queryKnobs":{"x":1}}"#,
            12_194_501,
            "failed to parse query knob x: query knob not settable via QuerySettings: x",
        ),
    ];
    for (target, settings, code, msg) in cases {
        let r = set(&mut m, target, settings);
        assert_eq!(error(&r), (code, msg.to_owned()), "{target} {settings}");
    }
    let r = m.run("qsh", cmd(r#"{"setQuerySettings":"zz","settings":{"reject":true}}"#));
    assert_eq!(error(&r).0, 13, "{r:?}");
    let r = set(&mut m, MATCH_QUERY, r#"{"maxTimeMS":-1}"#);
    assert_eq!(
        error(&r),
        (2, "BSON field 'maxTimeMS' value must be >= 0, actual value '-1'".to_owned())
    );
    let r =
        set(&mut m, MATCH_QUERY, r#"{"indexHints":{"ns":{"db":"qsh"},"allowedIndexes":["a_1"]}}"#);
    assert_eq!(error(&r), (8_727_501, "invalid index hint: 'ns.coll' field is missing".to_owned()));
}

#[test]
fn settings_merge_list_reject_and_remove() {
    let mut m = seeded();
    let r = set(&mut m, MATCH_QUERY, r#"{"reject":true}"#);
    assert_eq!(field(&r, "queryShapeHash"), &Value::from(MATCH_HASH), "{r:?}");
    assert_eq!(field(&r, "representativeQuery"), &Value::Document(cmd(MATCH_QUERY)));
    // An update merges and does not repeat the query.
    let r = set(&mut m, &format!(r#""{MATCH_HASH}""#), r#"{"maxTimeMS":2.5,"comment":"hey"}"#);
    let settings = cmd(r#"{"reject":true,"comment":"hey","maxTimeMS":{"$numberLong":"2"}}"#);
    assert_eq!(field(&r, "settings"), &Value::Document(settings.clone()), "{r:?}");
    assert!(r.get("representativeQuery").is_none(), "{r:?}");
    let all = listed(&mut m, true);
    assert_eq!(all.len(), 1);
    let Value::Document(entry) = &all[0] else { panic!() };
    assert_eq!(entry.get("settings"), Some(&Value::Document(settings)));
    let debug = cmd(
        r#"{"cmdNs":{"db":"qsh","coll":"c"},"command":"aggregate","pipeline":[{"$match":{"a":{"$eq":"?number"}}}]}"#,
    );
    assert_eq!(entry.get("debugQueryShape"), Some(&Value::Document(debug)));
    let Value::Document(plain) = &listed(&mut m, false)[0] else { panic!() };
    assert!(plain.get("debugQueryShape").is_none());
    // The shape is rejected, whatever its literal; another shape is not.
    let r = m.run("qsh", cmd(r#"{"aggregate":"c","pipeline":[{"$match":{"a":7}}],"cursor":{}}"#));
    assert_eq!(error(&r), (411, "Query rejected by admin query settings".to_owned()));
    let r = m.run("qsh", cmd(r#"{"aggregate":"c","pipeline":[{"$match":{"a":"x"}}],"cursor":{}}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    // Back to defaults only is refused; removing works by hash, in any case of the digits.
    let r = set(&mut m, &format!(r#""{MATCH_HASH}""#), r#"{"reject":false,"maxTimeMS":0}"#);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let lower = Value::from(MATCH_HASH.to_lowercase());
    let r = m.run("admin", doc(&[("removeQuerySettings", lower)]));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert!(listed(&mut m, false).is_empty());
    let r = m.run("qsh", cmd(r#"{"aggregate":"c","pipeline":[{"$match":{"a":7}}],"cursor":{}}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}

#[test]
fn a_command_takes_settings_for_one_run() {
    let mut m = seeded();
    let r = m.run("qsh", cmd(r#"{"find":"c","filter":{"a":5},"querySettings":{"reject":true}}"#));
    assert_eq!(error(&r).0, 411);
    let r = m.run(
        "qsh",
        cmd(r#"{"aggregate":"c","pipeline":[],"cursor":{},"querySettings":{"reject":true}}"#),
    );
    assert_eq!(error(&r).0, 411);
    let r = m.run("qsh", cmd(r#"{"find":"c","filter":{"a":5},"querySettings":{"bogus":1}}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}

#[test]
fn query_stats_count_each_shape_and_explain_shows_its_hash() {
    let mut m = seeded();
    let r = m.run("admin", cmd(r#"{"setParameter":1,"internalQueryStatsRateLimit":-1}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = m.run("admin", cmd(r#"{"setParameter":1,"internalQueryStatsSampleRate":0}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    for _ in 0..2 {
        let r = m.run("qsh", cmd(r#"{"aggregate":"c","pipeline":[],"cursor":{}}"#));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    let r = m.run("admin", cmd(r#"{"aggregate":1,"pipeline":[{"$queryStats":{}}],"cursor":{}}"#));
    let Value::Array(batch) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    let entry = batch
        .iter()
        .find_map(|e| match e {
            Value::Document(d) if d.get("queryShapeHash") == Some(&Value::from(EMPTY_HASH)) => {
                Some(d)
            }
            _ => None,
        })
        .expect("the empty pipeline's entry");
    assert_eq!(field(entry, "metrics.execCount"), &Value::Int64(2));
    assert_eq!(field(entry, "metrics.queryExec.docsReturned.sum"), &Value::Int64(4));
    let Value::String(key_hash) = field(entry, "keyHash") else { panic!() };
    assert_eq!(key_hash.len(), 44);
    let shape = cmd(r#"{"cmdNs":{"db":"qsh","coll":"c"},"command":"aggregate","pipeline":[]}"#);
    assert_eq!(field(entry, "key.queryShape"), &Value::Document(shape));
    let r = m.run(
        "admin",
        cmd(r#"{"aggregate":1,"pipeline":[{"$queryStats":{"bogus":1}}],"cursor":{}}"#),
    );
    assert_eq!(error(&r).0, 40415);
    let r = m.run("qsh", cmd(r#"{"explain":{"aggregate":"c","pipeline":[],"cursor":{}}}"#));
    assert_eq!(field(&r, "queryShapeHash"), &Value::from(EMPTY_HASH), "{r:?}");
}

/// The index the winning plan of `explain` reads, if any.
fn winning_index(r: &Document) -> Option<String> {
    let text = format!("{:?}", field(r, "queryPlanner.winningPlan"));
    ["a_1", "b_1"].iter().find(|n| text.contains(&format!("\"{n}\""))).map(|n| (*n).to_owned())
}

#[test]
fn index_hints_limit_the_indexes_a_shape_plans_with() {
    let mut m = seeded();
    for k in ["a", "b"] {
        let spec =
            format!(r#"{{"createIndexes":"c","indexes":[{{"key":{{"{k}":1}},"name":"{k}_1"}}]}}"#);
        let r = m.run("qsh", cmd(&spec));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    let find = r#"{"explain":{"find":"c","filter":{"a":7,"b":7}},"verbosity":"queryPlanner"}"#;
    let target = r#"{"aggregate":"c","pipeline":[{"$match":{"a":1,"b":1}}],"$db":"qsh"}"#;
    let r = set(
        &mut m,
        target,
        r#"{"indexHints":{"ns":{"db":"qsh","coll":"c"},"allowedIndexes":["b_1"]}}"#,
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let agg = r#"{"explain":{"aggregate":"c","pipeline":[{"$match":{"a":7,"b":7}}],"cursor":{}},"verbosity":"queryPlanner"}"#;
    let r = m.run("qsh", cmd(agg));
    assert!(format!("{r:?}").contains("\"b_1\"") && !format!("{r:?}").contains("\"a_1\""), "{r:?}");
    // The find has another shape: both indexes stay in its plans.
    let r = m.run("qsh", cmd(find));
    assert!(winning_index(&r).is_some(), "{r:?}");
}

/// Runs `text` on `qsh` as a client's command, so that the indexes it reads count in
/// `$indexStats`.
fn dispatched(m: &mut Mem, text: &str) -> Document {
    use teseladb_wire::connection::Connection;
    use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
    let info = NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 0 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
    };
    let body = cmd(text).with("$db", "qsh").to_bytes().unwrap();
    let mut c = Connection::new(9);
    c.receive(&teseladb_wire::msg::OpMsg::encode(1, 0, 0, &body));
    let req = c.next_request().unwrap().unwrap();
    let shared = std::mem::take(&mut m.shared);
    let r = match dispatch_with(&mut Connection::new(1), &req, &info, Some((m, &shared))) {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    };
    m.shared = shared;
    r
}

/// `accesses.ops` of the indexes of `coll`, by name.
fn index_ops(m: &mut Mem, coll: &str) -> Vec<(String, i64)> {
    let stats = r#"[{"$indexStats":{}},{"$project":{"_id":0,"name":1,"o":"$accesses.ops"}},{"$sort":{"name":1}}]"#;
    let r = dispatched(m, &format!(r#"{{"aggregate":"{coll}","pipeline":{stats},"cursor":{{}}}}"#));
    let Value::Array(b) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    b.iter()
        .map(|d| {
            let Value::Document(d) = d else { panic!("{d:?}") };
            let Value::String(n) = field(d, "name") else { panic!("{d:?}") };
            let Value::Int64(o) = field(d, "o") else { panic!("{d:?}") };
            (n.clone(), *o)
        })
        .collect()
}

/// Index hints name one collection: a `$lookup` of the shape still reads the other one with its
/// own indexes (measured on a `refnode` 9.0.4 on 02-11-2026: `b_1` of `c` and `a_1` of `d` read
/// once each, `a_1` of `c` never).
#[test]
fn index_hints_leave_the_indexes_of_other_collections_alone() {
    let mut m = Mem::new();
    m.shared = std::sync::Mutex::new(teseladb_wire::commands::NodeSessions::new(
        teseladb_wire::commands::Timeouts::default(),
    ));
    for text in [
        r#"{"insert":"c","documents":[{"_id":1,"a":1,"b":1},{"_id":2,"a":5,"b":5}]}"#,
        r#"{"insert":"d","documents":[{"_id":1,"a":1},{"_id":2,"a":5}]}"#,
        r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"},{"key":{"b":1},"name":"b_1"}]}"#,
        r#"{"createIndexes":"d","indexes":[{"key":{"a":1},"name":"a_1"}]}"#,
    ] {
        let r = m.run("qsh", cmd(text));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    let pipeline = r#"[{"$match":{"a":1,"b":1}},{"$lookup":{"from":"d","localField":"a","foreignField":"a","as":"j"}}]"#;
    let target = format!(r#"{{"aggregate":"c","pipeline":{pipeline},"$db":"qsh"}}"#);
    let hints = r#"{"indexHints":{"ns":{"db":"qsh","coll":"c"},"allowedIndexes":["b_1"]}}"#;
    let r = set(&mut m, &target, hints);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r =
        dispatched(&mut m, &format!(r#"{{"aggregate":"c","pipeline":{pipeline},"cursor":{{}}}}"#));
    let joined = json_of(r#"[{"_id":1,"a":1,"b":1,"j":[{"_id":1,"a":1}]}]"#);
    assert_eq!(field(&r, "cursor.firstBatch"), &joined, "{r:?}");
    let named =
        |l: &[(&str, i64)]| l.iter().map(|(n, o)| ((*n).to_owned(), *o)).collect::<Vec<_>>();
    assert_eq!(index_ops(&mut m, "c"), named(&[("_id_", 0), ("a_1", 0), ("b_1", 1)]));
    assert_eq!(index_ops(&mut m, "d"), named(&[("_id_", 0), ("a_1", 1)]));
}

fn json_of(text: &str) -> Value {
    cmd(&format!(r#"{{"v":{text}}}"#)).get("v").cloned().expect("a value")
}

/// The value checks of each setting (measured on a `refnode` 9.0.4 replica set on 02-11-2026,
/// with the settings of the shape removed before each): `indexHints` that is not an object or an
/// array, an index hint without `ns`, `ns.db` or `allowedIndexes`, `queryFramework` and
/// `maxTimeMS` of the wrong type, `maxTimeMS` above 2³¹ − 1 (a double out of the 64-bit range
/// counts as the nearest end, NaN as 0) and `queryKnobs` that is not an object.
#[test]
fn settings_values_are_checked_as_refdb_checks_them() {
    let refused: [(&str, i32, &str); 15] = [
        (r#"{"indexHints":5}"#, 9, "'indexHints' must be an object or an array"),
        (r#"{"indexHints":"x"}"#, 9, "'indexHints' must be an object or an array"),
        (r#"{"indexHints":null}"#, 9, "'indexHints' must be an object or an array"),
        (r#"{"indexHints":[5]}"#, 10065, "invalid parameter: expected an object (0)"),
        (
            r#"{"queryFramework":true}"#,
            14,
            "BSON field 'setQuerySettings.settings.queryFramework' is the wrong type 'bool', \
             expected type 'string'",
        ),
        (
            r#"{"maxTimeMS":null}"#,
            8_727_502,
            "settings field in setQuerySettings command cannot be empty",
        ),
        (
            r#"{"indexHints":[{"allowedIndexes":["a"]}]}"#,
            40414,
            "BSON field 'IndexHintSpec.ns' is missing but a required field",
        ),
        (
            r#"{"indexHints":[{"ns":{"coll":"c"},"allowedIndexes":["a"]}]}"#,
            8_727_500,
            "invalid index hint: 'ns.db' field is missing",
        ),
        (
            r#"{"indexHints":[{"ns":{"db":"qsh","coll":"c"}}]}"#,
            40414,
            "BSON field 'IndexHintSpec.allowedIndexes' is missing but a required field",
        ),
        (
            r#"{"queryFramework":5}"#,
            14,
            "BSON field 'setQuerySettings.settings.queryFramework' is the wrong type 'int', \
             expected type 'string'",
        ),
        (r#"{"reject":1}"#, 14, "Field 'reject' should be a boolean value, but found: int"),
        (
            r#"{"maxTimeMS":"x"}"#,
            14,
            "BSON field 'setQuerySettings.settings.maxTimeMS' is the wrong type 'string', \
             expected types '[decimal, double, int, long]'",
        ),
        (
            r#"{"maxTimeMS":2147483648}"#,
            2,
            "BSON field 'maxTimeMS' value must be <= 2147483647, actual value '2147483648'",
        ),
        (
            r#"{"maxTimeMS":1e30}"#,
            2,
            "BSON field 'maxTimeMS' value must be <= 2147483647, actual value \
             '9223372036854775807'",
        ),
        (r#"{"queryKnobs":1}"#, 10065, "invalid parameter: expected an object (queryKnobs)"),
    ];
    for (settings, code, msg) in refused {
        let mut m = seeded();
        let r = set(&mut m, MATCH_QUERY, settings);
        assert_eq!(error(&r), (code, msg.to_owned()), "{settings}");
    }
    let mut m = seeded();
    let r = set(&mut m, MATCH_QUERY, r#"{"maxTimeMS":{"$numberDouble":"-Infinity"}}"#);
    assert_eq!(
        error(&r),
        (
            2,
            "BSON field 'maxTimeMS' value must be >= 0, actual value '-9223372036854775808'".into()
        )
    );
    let accepted = [
        (
            r#"{"indexHints":[{"ns":{"db":"qsh","coll":"c"},"allowedIndexes":[]}],"reject":true}"#,
            r#"{"reject":true}"#,
        ),
        (
            r#"{"indexHints":{"ns":{"db":"qsh","coll":"c"},"allowedIndexes":["a_1"]}}"#,
            r#"{"indexHints":[{"ns":{"db":"qsh","coll":"c"},"allowedIndexes":["a_1"]}]}"#,
        ),
        (r#"{"queryFramework":"sbe"}"#, r#"{"queryFramework":"sbe"}"#),
        (r#"{"reject":true,"maxTimeMS":{"$numberDouble":"NaN"}}"#, r#"{"reject":true}"#),
        (r#"{"queryKnobs":{},"reject":true}"#, r#"{"reject":true}"#),
        (r#"{"reject":true,"other":1}"#, r#"{"reject":true}"#),
        (r#"{"queryFramework":null,"maxTimeMS":5}"#, r#"{"maxTimeMS":{"$numberLong":"5"}}"#),
        (r#"{"maxTimeMS":{"$numberDecimal":"2.7"}}"#, r#"{"maxTimeMS":{"$numberLong":"3"}}"#),
        (r#"{"maxTimeMS":{"$numberDecimal":"2.5"}}"#, r#"{"maxTimeMS":{"$numberLong":"2"}}"#),
        (r#"{"maxTimeMS":{"$numberDecimal":"3.5"}}"#, r#"{"maxTimeMS":{"$numberLong":"4"}}"#),
        (r#"{"reject":true,"maxTimeMS":{"$numberDecimal":"NaN"}}"#, r#"{"reject":true}"#),
    ];
    for (settings, want) in accepted {
        let mut m = seeded();
        let r = set(&mut m, MATCH_QUERY, settings);
        assert_eq!(field(&r, "settings"), &Value::Document(cmd(want)), "{settings}: {r:?}");
    }
}

/// A target given by its hash alone keeps no representative query; removing settings that do
/// not exist answers `ok`; the `$querySettings` stage refuses what is not a document (7746800),
/// a `showDebugQueryShape` that is not a boolean (14) and other fields (40415). Measured on a
/// `refnode` 9.0.4 replica set on 02-11-2026.
#[test]
fn settings_by_hash_removal_and_the_stage_answer_as_refdb() {
    let mut m = seeded();
    let hash = format!(r#""{}""#, EMPTY_HASH.to_lowercase());
    let r = set(&mut m, &hash, r#"{"reject":true}"#);
    assert_eq!(field(&r, "queryShapeHash"), &Value::from(EMPTY_HASH), "{r:?}");
    assert!(r.get("representativeQuery").is_none(), "{r:?}");
    let Value::Document(entry) = &listed(&mut m, true)[0] else { panic!() };
    assert!(entry.get("representativeQuery").is_none() && entry.get("debugQueryShape").is_none());
    for _ in 0..2 {
        let r = m.run("admin", doc(&[("removeQuerySettings", Value::from(EMPTY_HASH))]));
        assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    }
    assert!(listed(&mut m, false).is_empty());
    for (stage, code, msg) in [
        ("5", 7_746_800, "$querySettings stage expects a document as argument"),
        (r#"{"bogus":1}"#, 40415, "BSON field '$querySettings.bogus' is an unknown field."),
        (
            r#"{"showDebugQueryShape":1}"#,
            14,
            "Field 'showDebugQueryShape' should be a boolean value, but found: int",
        ),
    ] {
        let text =
            format!(r#"{{"aggregate":1,"pipeline":[{{"$querySettings":{stage}}}],"cursor":{{}}}}"#);
        let r = m.run("admin", cmd(&text));
        assert_eq!(error(&r), (code, msg.to_owned()), "{stage}");
    }
}

/// A `maxTimeMS` of the settings applies to a run that has none and does not stop a quick one,
/// and index hints for another namespace leave the shape's plans alone.
#[test]
fn settings_time_limit_and_hints_of_other_namespaces() {
    let mut m = seeded();
    let r = m.run("qsh", cmd(r#"{"createIndexes":"c","indexes":[{"key":{"a":1},"name":"a_1"}]}"#));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let hints = r#"{"maxTimeMS":5000,"indexHints":[{"ns":{"db":"other","coll":"c"},"allowedIndexes":["_id_"]},{"ns":{"db":"qsh","coll":"x"},"allowedIndexes":["_id_"]}]}"#;
    let r = set(&mut m, MATCH_QUERY, hints);
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = m.run("qsh", cmd(r#"{"aggregate":"c","pipeline":[{"$match":{"a":5}}],"cursor":{}}"#));
    assert_eq!(field(&r, "cursor.firstBatch"), &json_of(r#"[{"_id":2,"a":5}]"#), "{r:?}");
    let r = m.run(
        "qsh",
        cmd(r#"{"explain":{"aggregate":"c","pipeline":[{"$match":{"a":5}}],"cursor":{}}}"#),
    );
    let plan = format!("{:?}", field(&r, "shards.node1.queryPlanner.winningPlan"));
    assert!(plan.contains(r#"("indexName", String("a_1"))"#), "{plan}");
}

/// `$queryStats` with a specification that is not a document fails with 9 (measured on a
/// `refnode` 9.0.4 replica set on 02-11-2026).
#[test]
fn query_stats_stage_needs_an_object() {
    let mut m = seeded();
    let r = m.run("admin", cmd(r#"{"aggregate":1,"pipeline":[{"$queryStats":5}],"cursor":{}}"#));
    assert_eq!(error(&r), (9, "$queryStats value must be an object. Found: int".to_owned()));
}
