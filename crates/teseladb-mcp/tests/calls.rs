//! The tools' calls against a scripted executor (019 FR-021, FR-022, FR-024): the replies of
//! the node each tool reads (cursors across `getMore`s, every BSON type in a schema, logs,
//! write errors), the limits of a result, and the arguments and replies a call refuses.

use std::collections::VecDeque;

use teseladb_bson::{Document, extjson};
use teseladb_http::http1::Request;
use teseladb_http::server::Response;
use teseladb_json::{Object, Value, parse};
use teseladb_mcp::calls::{Ctx, Limits, Output, call, is_ok};
use teseladb_rest::api::Executor;

/// Answers each command with the next scripted reply of its name, or `ok: 1`; records them.
#[derive(Default)]
struct Fake {
    script: VecDeque<(&'static str, Document)>,
    ran: Vec<Document>,
}

fn d(t: &str) -> Document {
    extjson::parse(t).unwrap()
}

fn name(cmd: &Document) -> String {
    cmd.iter().next().map(|(k, _)| k.to_owned()).unwrap_or_default()
}

impl Executor for Fake {
    fn run(&mut self, _: &str, command: Document) -> Document {
        let n = name(&command);
        self.ran.push(command);
        match self.script.iter().position(|(k, _)| *k == n) {
            Some(i) => self.script.remove(i).map(|(_, r)| r).unwrap_or_default(),
            None => d(r#"{"ok":1.0}"#),
        }
    }
    fn node(&self) -> u64 {
        7
    }
    fn cursor_ns(&mut self, _: i64) -> Option<String> {
        None
    }
    fn object_id(&mut self) -> [u8; 12] {
        [0xab; 12]
    }
    fn forward(&mut self, _: u64, _: &Request) -> Option<Response> {
        None
    }
    fn random(&mut self, out: &mut [u8]) {
        out.fill(1);
    }
    fn now_ms(&self) -> u64 {
        0
    }
    fn principal(&self) -> String {
        String::new()
    }
    fn run_internal(&mut self, db: &str, command: Document) -> Document {
        self.run(db, command)
    }
}

const LIMITS: Limits = Limits { max_documents: 100, max_bytes: 1 << 20, max_time_ms: 500 };

fn args(json: &str) -> Object {
    match parse(json.as_bytes()).unwrap() {
        Value::Object(o) => o,
        other => panic!("{other:?}"),
    }
}

/// Calls `tool` with `json` and the replies of `script`: the outcome and the commands run.
fn run_with(
    script: Vec<(&'static str, Document)>,
    limits: Limits,
    tool: &str,
    json: &str,
) -> (Result<Output, String>, Vec<Document>) {
    let mut f = Fake { script: script.into(), ..Fake::default() };
    let mut seen = Vec::new();
    let mut tell = |n: u64| seen.push(n);
    let mut c = Ctx {
        exec: &mut f,
        limits,
        connection_id: "7.s",
        comment: Some("mcp:7.s:1".to_owned()),
        on_batch: Some(&mut tell),
    };
    let out = call(&mut c, tool, &args(json));
    drop(c);
    (out, f.ran)
}

fn ok_call(script: Vec<(&'static str, Document)>, tool: &str, json: &str) -> Output {
    let (out, _) = run_with(script, LIMITS, tool, json);
    out.unwrap_or_else(|e| panic!("{tool}: {e}"))
}

fn err_call(script: Vec<(&'static str, Document)>, tool: &str, json: &str) -> String {
    run_with(script, LIMITS, tool, json).0.unwrap_err()
}

const DC: &str = r#""database":"db","collection":"c""#;

#[test]
fn a_cursor_is_read_across_its_get_mores_and_killed_once_it_holds_enough() {
    let first = d(
        r#"{"cursor":{"id":{"$numberLong":"9"},"ns":"db.c","firstBatch":[{"name":"a"}]},"ok":1.0}"#,
    );
    let next = d(r#"{"cursor":{"id":{"$numberLong":"0"},"nextBatch":[{"name":"b"}]},"ok":1}"#);
    let (out, ran) = run_with(
        vec![("listCollections", first.clone()), ("getMore", next)],
        LIMITS,
        "list-collections",
        r#"{"database":"db"}"#,
    );
    assert_eq!(out.unwrap().structured.get("totalCount").and_then(Value::as_i64), Some(2));
    assert_eq!(ran.iter().map(name).collect::<Vec<_>>(), ["listCollections", "getMore"]);
    assert_eq!(ran[0].get("comment"), Some(&teseladb_bson::Value::from("mcp:7.s:1")));
    assert!(ran[0].get("maxTimeMS").is_some() && ran[1].get("maxTimeMS").is_none());
    // A `find` that already has more than its limit kills the cursor instead of reading on.
    let big = d(
        r#"{"cursor":{"id":{"$numberLong":"9"},"firstBatch":[{"_id":1},{"_id":2},{"_id":3}]},"ok":1.0}"#,
    );
    let limits = Limits { max_documents: 1, ..LIMITS };
    let (out, ran) =
        run_with(vec![("find", big)], limits, "find", &format!(r#"{{{DC},"limit":10}}"#));
    let out = out.unwrap();
    assert!(ran.iter().any(|c| name(c) == "killCursors"), "{ran:?}");
    assert!(!ran.iter().any(|c| name(c) == "getMore"), "{ran:?}");
    let applied = out.structured.get("appliedLimits").cloned();
    assert_eq!(applied, Some(Value::Array(vec![Value::from("config.maxDocumentsPerQuery")])));
}

#[test]
fn the_bytes_of_a_result_are_limited_by_the_call_or_the_configuration() {
    let docs = d(
        r#"{"cursor":{"id":0,"firstBatch":[{"_id":1,"t":"xxxxxxxxxxxxxxxxxxxx"},{"_id":2}]},"ok":1.0}"#,
    );
    let out = ok_call(
        vec![("find", docs.clone())],
        "find",
        &format!(r#"{{{DC},"responseBytesLimit":10}}"#),
    );
    let applied = out.structured.get("appliedLimits").cloned();
    assert_eq!(applied, Some(Value::Array(vec![Value::from("tool.responseBytesLimit")])));
    let limits = Limits { max_bytes: 10, ..LIMITS };
    let (out, _) = run_with(vec![("find", docs)], limits, "find", &format!("{{{DC}}}"));
    let applied = out.unwrap().structured.get("appliedLimits").cloned();
    assert_eq!(applied, Some(Value::Array(vec![Value::from("config.maxBytesPerQuery")])));
}

#[test]
fn a_schema_names_every_bson_type_of_each_field() {
    let docs = d(r#"{"cursor":{"id":0,"firstBatch":[
        {"a":1.5,"b":"s","c":{},"d":[],"e":{"$binary":{"base64":"","subType":"00"}},"f":{"$undefined":true},
         "g":{"$oid":"0102030405060708090a0b0c"},"h":true,"i":{"$date":{"$numberLong":"0"}},"j":null,
         "k":{"$regularExpression":{"pattern":"x","options":""}},
         "l":{"$dbPointer":{"$ref":"c","$id":{"$oid":"0102030405060708090a0b0c"}}},
         "m":{"$code":"f()"},"n":{"$symbol":"s"},"o":{"$code":"g()","$scope":{}},"p":1,
         "q":{"$timestamp":{"t":1,"i":1}},"r":{"$numberLong":"1"},"s":{"$numberDecimal":"1"},
         "t":{"$minKey":1},"u":{"$maxKey":1}},
        {"a":2.5,"p":"now a string"}]},"ok":1.0}"#);
    let out = ok_call(vec![("aggregate", docs)], "collection-schema", &format!("{{{DC}}}"));
    let text = format!("{:?}", out.structured);
    for t in [
        "double",
        "string",
        "object",
        "array",
        "binData",
        "undefined",
        "objectId",
        "bool",
        "date",
        "null",
        "regex",
        "dbPointer",
        "javascript",
        "symbol",
        "javascriptWithScope",
        "int",
        "timestamp",
        "long",
        "decimal",
        "minKey",
        "maxKey",
    ] {
        assert!(text.contains(&format!("\"{t}\"")), "{t}: {text}");
    }
}

#[test]
fn reads_of_the_node_become_their_tools_answers() {
    let dbs = d(
        r#"{"databases":[{"name":"a","sizeOnDisk":{"$numberLong":"5"}},{"name":"b","sizeOnDisk":2.0}],"ok":{"$numberLong":"1"}}"#,
    );
    let out = ok_call(vec![("listDatabases", dbs)], "list-databases", "{}");
    assert_eq!(out.structured.get("totalCount").and_then(Value::as_i64), Some(2));
    let out = ok_call(vec![("listDatabases", d(r#"{"ok":1}"#))], "list-databases", "{}");
    assert_eq!(out.structured.get("totalCount").and_then(Value::as_i64), Some(0));
    let logs = d(r#"{"log":["one",2,"three"],"ok":1.0}"#);
    let out = ok_call(vec![("getLog", logs)], teseladb_names::MCP_TOOL_LOGS, r#"{"limit":2}"#);
    let shown = format!("{:?}", out.structured);
    assert!(shown.contains("\"three\"") && !shown.contains("\"one\""), "{shown}");
    let out = ok_call(vec![("getLog", d(r#"{"ok":1.0}"#))], teseladb_names::MCP_TOOL_LOGS, "{}");
    assert!(out.summary.contains('0'), "{}", out.summary);
    // `collection-indexes` keeps the classic ones when `$listSearchIndexes` fails.
    let fail = d(r#"{"ok":0.0,"errmsg":"no search"}"#);
    let out = ok_call(vec![("aggregate", fail)], "collection-indexes", &format!("{{{DC}}}"));
    assert_eq!(out.structured.get("searchIndexesCount").and_then(Value::as_i64), Some(0));
}

#[test]
fn explain_and_export_take_each_method() {
    for (m, inner) in [("aggregate", r#"{"pipeline":[]}"#), ("count", r#"{"query":{}}"#)] {
        let json = format!(r#"{{{DC},"method":[{{"name":"{m}","arguments":{inner}}}]}}"#);
        let out = ok_call(Vec::new(), "explain", &json);
        assert_eq!(out.structured.get("method").and_then(Value::as_str), Some(m));
    }
    let json = format!(r#"{{{DC},"method":[{{"name":"aggregate"}}]}}"#);
    assert!(ok_call(Vec::new(), "explain", &json).summary.contains("aggregate"));
    let json = format!(r#"{{{DC},"method":[{{"name":"distinct"}}]}}"#);
    assert!(err_call(Vec::new(), "explain", &json).contains("cannot be explained"));
    let json = format!(r#"{{{DC},"method":[{{}}]}}"#);
    assert!(err_call(Vec::new(), "explain", &json).contains("method must name"));
    let target = |t: &str| format!(r#"{{{DC},"exportTitle":"x","exportTarget":[{t}]}}"#);
    let out = ok_call(
        Vec::new(),
        "export",
        &target(r#"{"name":"aggregate","arguments":{"pipeline":[{"$match":{}}]}}"#),
    );
    assert!(out.export.is_some());
    assert!(ok_call(Vec::new(), "export", &target(r#"{"name":"aggregate"}"#)).export.is_some());
    let out = err_call(
        Vec::new(),
        "export",
        &target(r#"{"name":"aggregate","arguments":{"pipeline":[{"$out":"x"}]}}"#),
    );
    assert!(out.contains("$out and $merge"));
    assert!(
        err_call(Vec::new(), "export", &target(r#"{"name":"count"}"#)).contains("exportTarget")
    );
}

#[test]
fn writes_report_their_ids_and_their_errors() {
    let json = format!(r#"{{{DC},"documents":[{{"a":1}},{{"_id":5}},3]}}"#);
    let (out, ran) =
        run_with(vec![("insert", d(r#"{"n":2,"ok":1.0}"#))], LIMITS, "insert-many", &json);
    assert!(format!("{:?}", out.unwrap()).contains("abababab"));
    assert!(format!("{:?}", ran[0]).contains("ObjectId"));
    let dup = d(r#"{"ok":1.0,"writeErrors":[{"code":11000,"errmsg":"dup key"}]}"#);
    assert!(err_call(vec![("insert", dup)], "insert-many", &json).contains("dup key"));
    let bare = d(r#"{"ok":1.0,"writeErrors":[1]}"#);
    assert_eq!(err_call(vec![("insert", bare)], "insert-many", &json), "the insert failed");
    for (id, text) in [(r#""k""#, "k"), ("4", "4")] {
        let reply =
            format!(r#"{{"n":1,"nModified":0,"upserted":[{{"index":0,"_id":{id}}}],"ok":1.0}}"#);
        let json = format!(r#"{{{DC},"update":{{"$set":{{"a":1}}}},"upsert":true}}"#);
        let out = ok_call(vec![("update", d(&reply))], "update-many", &json);
        assert_eq!(out.structured.get("upsertedId").and_then(Value::as_str), Some(text));
    }
    // An update that inserts nothing has no `upsertedId`.
    let json = format!(r#"{{{DC},"update":{{"$set":{{"a":1}}}}}}"#);
    let out =
        ok_call(vec![("update", d(r#"{"n":2,"nModified":2,"ok":1.0}"#))], "update-many", &json);
    assert!(out.structured.get("upsertedId").is_none());
    // An index listed without its key keeps what it has.
    let bare = d(r#"{"cursor":{"id":0,"firstBatch":[{"name":"x","v":2}]},"ok":1.0}"#);
    let out = ok_call(vec![("listIndexes", bare)], "collection-indexes", &format!("{{{DC}}}"));
    assert_eq!(
        format!("{:?}", out.structured.get("classicIndexes")),
        r#"Some(Array([Object(Object { entries: [("name", String("x"))] })]))"#
    );
    // An index without a name is named after its keys.
    let keys = r#"{"a":1,"b":"text","c":-1.0}"#;
    let json = format!(r#"{{{DC},"definition":[{{"type":"classic","keys":{keys}}}]}}"#);
    let out = ok_call(Vec::new(), "create-index", &json);
    assert!(out.summary.contains("a_1_b_text_c_"), "{}", out.summary);
    let json = format!(r#"{{{DC},"indexName":"s","type":"search"}}"#);
    let (_, ran) = run_with(Vec::new(), LIMITS, "drop-index", &json);
    assert_eq!(name(&ran[0]), "dropSearchIndex");
}

#[test]
fn what_a_call_refuses() {
    assert!(err_call(Vec::new(), "find", r#"{"connectionId":"other"}"#).contains("not found"));
    assert!(err_call(Vec::new(), "nothing", "{}").contains("Tool nothing not found"));
    let json = format!(r#"{{{DC},"filter":3}}"#);
    assert!(err_call(Vec::new(), "find", &json).contains("filter must be an object"));
    let json = format!(r#"{{{DC},"pipeline":{{}}}}"#);
    assert!(err_call(Vec::new(), "aggregate", &json).contains("pipeline must be an array"));
    let json = format!(r#"{{{DC},"pipeline":[{{"$merge":"x"}}]}}"#);
    assert!(err_call(Vec::new(), "aggregate", &json).contains("$out and $merge"));
    let failed = d(r#"{"ok":0.0}"#);
    assert_eq!(
        err_call(vec![("count", failed)], "count", &format!("{{{DC}}}")),
        "the command failed"
    );
    assert!(is_ok(&d(r#"{"ok":1}"#)) && is_ok(&d(r#"{"ok":{"$numberLong":"1"}}"#)));
    assert!(!is_ok(&d(r#"{"ok":"1"}"#)) && !is_ok(&d(r#"{"ok":0.0}"#)));
}
