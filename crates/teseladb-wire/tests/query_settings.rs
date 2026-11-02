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
