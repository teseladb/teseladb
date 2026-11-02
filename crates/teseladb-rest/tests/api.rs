//! A REST request from route to answer against a recorded executor (019 T009, T011, T013,
//! T016, FR-005, FR-007, FR-008, FR-011): the command each route builds, how each reply becomes
//! an answer, cursor ids and the causal-consistency headers.

use teseladb_bson::{Document, Value, extjson};
use teseladb_http::http1::Request;
use teseladb_http::server::Response;
use teseladb_rest::api::{AFTER_CLUSTER_TIME, Executor, OPERATION_TIME, handle};
use teseladb_rest::media::{BSON, CANONICAL_JSON};
use teseladb_session::cursor::cursor_id;

const NODE: u64 = 3;

/// Records each command and answers with the replies queued.
#[derive(Default)]
struct Fake {
    ran: Vec<(String, Document)>,
    replies: Vec<Document>,
    cursors: Vec<(i64, String)>,
    forwarded: Vec<u64>,
}

impl Executor for Fake {
    fn run(&mut self, db: &str, command: Document) -> Document {
        self.ran.push((db.to_owned(), command));
        if self.replies.is_empty() { ok() } else { self.replies.remove(0) }
    }
    fn node(&self) -> u64 {
        NODE
    }
    fn cursor_ns(&mut self, id: i64) -> Option<String> {
        self.cursors.iter().find(|(c, _)| *c == id).map(|(_, ns)| ns.clone())
    }
    fn object_id(&mut self) -> [u8; 12] {
        [7; 12]
    }
    fn forward(&mut self, node: u64, _: &Request) -> Option<Response> {
        self.forwarded.push(node);
        (node == 4).then(|| Response { status: 299, headers: Vec::new(), body: Vec::new() })
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

fn ok() -> Document {
    Document::new().with("ok", 1.0)
}

fn json(r: &Response) -> Document {
    extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap()
}

fn header<'a>(r: &'a Response, name: &str) -> Option<&'a str> {
    r.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

fn call(f: &mut Fake, method: &str, target: &str, body: &str) -> Response {
    let req = Request::new(method, target).body(body.as_bytes().to_vec());
    handle(&req, f)
}

/// The command built for `method target body`, and its database.
fn built(method: &str, target: &str, body: &str) -> (String, Document) {
    let mut f = Fake::default();
    let r = call(&mut f, method, target, body);
    assert!(r.status < 500, "{method} {target}: {}", r.status);
    f.ran.pop().unwrap_or_else(|| panic!("{method} {target} ran nothing: {}", r.status))
}

fn doc(text: &str) -> Document {
    extjson::parse(text).unwrap()
}

const C: &str = "/v1/databases/shop/collections/items";

#[test]
fn each_route_builds_its_command() {
    let cases: &[(&str, &str, &str, &str, &str)] = &[
        (
            "GET",
            "/v1/databases?nameOnly=true&filter=%7B%22name%22%3A%22a%22%7D",
            "",
            "admin",
            r#"{"listDatabases":1,"filter":{"name":"a"},"nameOnly":true}"#,
        ),
        ("GET", "/v1/databases/shop/collections", "", "shop", r#"{"listCollections":1}"#),
        (
            "POST",
            "/v1/databases/shop/collections",
            r#"{"name":"c","capped":true,"size":10}"#,
            "shop",
            r#"{"create":"c","capped":true,"size":10}"#,
        ),
        ("DELETE", C, "", "shop", r#"{"drop":"items"}"#),
        (
            "POST",
            "/v1/databases/shop/collections/items/find",
            r#"{"filter":{"a":1},"limit":2}"#,
            "shop",
            r#"{"find":"items","filter":{"a":1},"limit":2}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/aggregate",
            r#"{"pipeline":[],"batchSize":5}"#,
            "shop",
            r#"{"aggregate":"items","pipeline":[],"cursor":{"batchSize":5}}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/aggregate",
            r#"{"pipeline":[],"cursor":{}}"#,
            "shop",
            r#"{"aggregate":"items","pipeline":[],"cursor":{}}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/countDocuments",
            r#"{"filter":{"a":1},"skip":1,"limit":5}"#,
            "shop",
            r#"{"aggregate":"items","pipeline":[{"$match":{"a":1}},{"$skip":1},{"$limit":5},{"$group":{"_id":1,"n":{"$sum":1}}}],"cursor":{}}"#,
        ),
        (
            "GET",
            "/v1/databases/shop/collections/items/estimatedDocumentCount",
            "",
            "shop",
            r#"{"count":"items"}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/distinct",
            r#"{"key":"a"}"#,
            "shop",
            r#"{"distinct":"items","key":"a"}"#,
        ),
        (
            "GET",
            "/v1/databases/shop/collections/items/documents/42",
            "",
            "shop",
            r#"{"find":"items","filter":{"_id":42},"limit":1,"singleBatch":true}"#,
        ),
        (
            "GET",
            "/v1/databases/shop/collections/items/documents/abc",
            "",
            "shop",
            r#"{"find":"items","filter":{"_id":"abc"},"limit":1,"singleBatch":true}"#,
        ),
        (
            "PUT",
            "/v1/databases/shop/collections/items/documents/%22k%22",
            r#"{"v":1}"#,
            "shop",
            r#"{"update":"items","updates":[{"q":{"_id":"k"},"u":{"v":1},"multi":false,"upsert":true}]}"#,
        ),
        (
            "PATCH",
            "/v1/databases/shop/collections/items/documents/1",
            r#"{"$set":{"v":2}}"#,
            "shop",
            r#"{"update":"items","updates":[{"q":{"_id":1},"u":{"$set":{"v":2}},"multi":false}]}"#,
        ),
        (
            "PATCH",
            "/v1/databases/shop/collections/items/documents/1",
            r#"[{"$set":{"v":2}}]"#,
            "shop",
            r#"{"update":"items","updates":[{"q":{"_id":1},"u":[{"$set":{"v":2}}],"multi":false}]}"#,
        ),
        (
            "DELETE",
            "/v1/databases/shop/collections/items/documents/1",
            "",
            "shop",
            r#"{"delete":"items","deletes":[{"q":{"_id":1},"limit":1}]}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/insertOne",
            r#"{"document":{"_id":1}}"#,
            "shop",
            r#"{"insert":"items","documents":[{"_id":1}]}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/insertMany",
            r#"{"documents":[{"_id":1},{"_id":2}],"ordered":false}"#,
            "shop",
            r#"{"insert":"items","ordered":false,"documents":[{"_id":1},{"_id":2}]}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/updateOne",
            r#"{"filter":{"a":1},"update":{"$inc":{"n":1}},"upsert":true,"comment":"c"}"#,
            "shop",
            r#"{"update":"items","comment":"c","updates":[{"q":{"a":1},"u":{"$inc":{"n":1}},"multi":false,"upsert":true}]}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/updateMany",
            r#"{"update":[{"$set":{"n":1}}]}"#,
            "shop",
            r#"{"update":"items","updates":[{"q":{},"u":[{"$set":{"n":1}}],"multi":true}]}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/replaceOne",
            r#"{"filter":{"a":1},"replacement":{"b":2}}"#,
            "shop",
            r#"{"update":"items","updates":[{"q":{"a":1},"u":{"b":2},"multi":false}]}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/deleteOne",
            r#"{"filter":{"a":1},"hint":"a_1"}"#,
            "shop",
            r#"{"delete":"items","deletes":[{"q":{"a":1},"limit":1,"hint":"a_1"}]}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/deleteMany",
            r#"{}"#,
            "shop",
            r#"{"delete":"items","deletes":[{"q":{},"limit":0}]}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/findOneAndUpdate",
            r#"{"filter":{"a":1},"update":{"$set":{"b":1}},"new":true,"projection":{"b":1}}"#,
            "shop",
            r#"{"findAndModify":"items","update":{"$set":{"b":1}},"new":true,"query":{"a":1},"fields":{"b":1}}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/findOneAndReplace",
            r#"{"filter":{"a":1},"replacement":{"b":1}}"#,
            "shop",
            r#"{"findAndModify":"items","query":{"a":1},"update":{"b":1}}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/findOneAndDelete",
            r#"{"filter":{"a":1}}"#,
            "shop",
            r#"{"findAndModify":"items","query":{"a":1},"remove":true}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/bulkWrite",
            r#"{"operations":[{"insertOne":{"document":{"_id":1}}},{"updateMany":{"filter":{},"update":{"$set":{"a":1}},"upsert":true}},{"replaceOne":{"filter":{"_id":2},"replacement":{"b":1}}},{"deleteOne":{"filter":{"_id":3}}}],"ordered":false}"#,
            "admin",
            r#"{"bulkWrite":1,"ops":[{"insert":0,"document":{"_id":1}},{"update":0,"filter":{},"updateMods":{"$set":{"a":1}},"multi":true,"upsert":true},{"update":0,"filter":{"_id":2},"updateMods":{"b":1}},{"delete":0,"filter":{"_id":3},"multi":false}],"nsInfo":[{"ns":"shop.items"}],"ordered":false}"#,
        ),
        (
            "GET",
            "/v1/databases/shop/collections/items/indexes",
            "",
            "shop",
            r#"{"listIndexes":"items"}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/indexes",
            r#"{"indexes":[{"key":{"a":1},"name":"a_1"}]}"#,
            "shop",
            r#"{"createIndexes":"items","indexes":[{"key":{"a":1},"name":"a_1"}]}"#,
        ),
        (
            "DELETE",
            "/v1/databases/shop/collections/items/indexes/a_1",
            "",
            "shop",
            r#"{"dropIndexes":"items","index":"a_1"}"#,
        ),
        (
            "GET",
            "/v1/databases/shop/collections/items/searchIndexes",
            "",
            "shop",
            r#"{"aggregate":"items","pipeline":[{"$listSearchIndexes":{}}],"cursor":{}}"#,
        ),
        (
            "GET",
            "/v1/databases/shop/collections/items/searchIndexes/s",
            "",
            "shop",
            r#"{"aggregate":"items","pipeline":[{"$listSearchIndexes":{"name":"s"}}],"cursor":{}}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/collections/items/searchIndexes",
            r#"{"indexes":[{"name":"s","definition":{}}]}"#,
            "shop",
            r#"{"createSearchIndexes":"items","indexes":[{"name":"s","definition":{}}]}"#,
        ),
        (
            "PATCH",
            "/v1/databases/shop/collections/items/searchIndexes/s",
            r#"{"definition":{}}"#,
            "shop",
            r#"{"updateSearchIndex":"items","definition":{},"name":"s"}"#,
        ),
        (
            "DELETE",
            "/v1/databases/shop/collections/items/searchIndexes/s",
            "",
            "shop",
            r#"{"dropSearchIndex":"items","name":"s"}"#,
        ),
        (
            "POST",
            "/v1/databases/shop/command",
            r#"{"collStats":"items"}"#,
            "shop",
            r#"{"collStats":"items"}"#,
        ),
    ];
    for (method, target, body, db, want) in cases {
        let (got_db, got) = built(method, target, body);
        assert_eq!((got_db.as_str(), &got), (*db, &doc(want)), "{method} {target}");
    }
}

#[test]
fn insert_without_id_gets_one_and_reports_it() {
    let mut f = Fake::default();
    f.replies.push(doc(r#"{"n":1,"ok":1}"#));
    let r = call(&mut f, "POST", &format!("{C}/insertOne"), r#"{"document":{"a":1}}"#);
    assert_eq!(r.status, 200);
    let sent = &f.ran[0].1;
    let Some(Value::Array(docs)) = sent.get("documents") else { panic!() };
    assert_eq!(
        docs[0].as_document().unwrap().iter().next().unwrap(),
        ("_id", &Value::ObjectId([7; 12]))
    );
    assert_eq!(json(&r).get("insertedId"), Some(&Value::ObjectId([7; 12])));
    assert_eq!(json(&r).get("ok"), None);

    f.replies.push(doc(r#"{"n":2,"ok":1}"#));
    let r = call(&mut f, "POST", &format!("{C}/insertMany"), r#"{"documents":[{"_id":5},{}]}"#);
    assert_eq!(
        json(&r).get("insertedIds"),
        Some(&Value::Array(vec![Value::Int32(5), Value::ObjectId([7; 12])]))
    );
}

#[test]
fn failed_single_writes_answer_their_error() {
    let mut f = Fake::default();
    let dup = r#"{"n":0,"writeErrors":[{"index":0,"code":11000,"errmsg":"E11000 dup","keyValue":{"_id":1}}],"ok":1}"#;
    f.replies.push(doc(dup));
    let r = call(&mut f, "POST", &format!("{C}/insertOne"), r#"{"document":{"_id":1}}"#);
    assert_eq!(r.status, 409);
    assert_eq!(header(&r, "Content-Type"), Some("application/problem+json"));
    let body = json(&r);
    assert_eq!(body.get("codeName"), Some(&Value::from("DuplicateKey")));
    assert!(matches!(body.get("writeErrors"), Some(Value::Array(_))));

    // Several operations answer 200 with their write errors, as the command.
    f.replies.push(doc(dup));
    let r =
        call(&mut f, "POST", &format!("{C}/insertMany"), r#"{"documents":[{"_id":1},{"_id":2}]}"#);
    assert_eq!(r.status, 200);

    f.replies.push(doc(r#"{"n":0,"writeConcernError":{"code":64,"codeName":"WriteConcernFailed","errmsg":"w"},"ok":1}"#));
    let r = call(&mut f, "POST", &format!("{C}/updateOne"), r#"{"update":{"$set":{"a":1}}}"#);
    assert_eq!(
        (r.status, json(&r).get("codeName")),
        (400, Some(&Value::from("WriteConcernFailed")))
    );

    f.replies.push(doc(dup));
    let r = call(&mut f, "POST", &format!("{C}/insertMany"), r#"{"documents":[{"_id":1}]}"#);
    assert_eq!(r.status, 409);
}

#[test]
fn documents_by_id_answer_404_when_missing() {
    let mut f = Fake::default();
    f.replies.push(doc(r#"{"cursor":{"id":0,"ns":"shop.items","firstBatch":[]},"ok":1}"#));
    assert_eq!(call(&mut f, "GET", &format!("{C}/documents/1"), "").status, 404);
    f.replies.push(doc(
        r#"{"cursor":{"id":0,"ns":"shop.items","firstBatch":[{"_id":1,"a":2}]},"ok":1}"#,
    ));
    let r = call(&mut f, "GET", &format!("{C}/documents/1"), "");
    assert_eq!((r.status, json(&r)), (200, doc(r#"{"_id":1,"a":2}"#)));
    f.replies.push(doc(r#"{"n":0,"nModified":0,"ok":1}"#));
    assert_eq!(
        call(&mut f, "PATCH", &format!("{C}/documents/1"), r#"{"$set":{"a":1}}"#).status,
        404
    );
    f.replies.push(doc(r#"{"n":0,"ok":1}"#));
    assert_eq!(call(&mut f, "DELETE", &format!("{C}/documents/1"), "").status, 404);
    f.replies.push(doc(r#"{"n":1,"ok":1}"#));
    assert_eq!(call(&mut f, "DELETE", &format!("{C}/documents/1"), "").status, 200);
    f.replies.push(doc(r#"{"n":0,"ok":1}"#));
    // PUT upserts, so it never answers 404.
    assert_eq!(call(&mut f, "PUT", &format!("{C}/documents/1"), r#"{"a":1}"#).status, 200);
}

#[test]
fn counts_answer_count() {
    let mut f = Fake::default();
    f.replies.push(doc(r#"{"cursor":{"id":0,"ns":"s.i","firstBatch":[{"_id":1,"n":7}]},"ok":1}"#));
    let r = call(&mut f, "POST", &format!("{C}/countDocuments"), "");
    assert_eq!(json(&r), doc(r#"{"count":7}"#));
    f.replies.push(doc(r#"{"cursor":{"id":0,"ns":"s.i","firstBatch":[]},"ok":1}"#));
    let r = call(&mut f, "POST", &format!("{C}/countDocuments"), "{}");
    assert_eq!(json(&r), doc(r#"{"count":0}"#));
    f.replies.push(doc(r#"{"n":12,"ok":1}"#));
    let r = call(&mut f, "GET", &format!("{C}/estimatedDocumentCount"), "");
    assert_eq!(json(&r), doc(r#"{"count":12}"#));
}

#[test]
fn query_cursor_and_its_routes() {
    let id = cursor_id(NODE, 99);
    let mut f = Fake { cursors: vec![(id, "shop.items".to_owned())], ..Fake::default() };
    f.replies.push(doc(&format!(
        r#"{{"cursor":{{"id":{{"$numberLong":"{id}"}},"ns":"shop.items","firstBatch":[{{"a":1}}]}},"ok":1}}"#
    )));
    let r = call(&mut f, "POST", &format!("{C}/find"), r#"{"batchSize":1}"#);
    let cursor = json(&r).get("cursor").and_then(Value::as_document).cloned().unwrap();
    let public = format!("{NODE}.{id}");
    assert_eq!(cursor.get("id"), Some(&Value::from(public.as_str())));
    assert_eq!(
        cursor.get("next"),
        Some(&Value::from(format!("/v1/cursors/{public}?batchSize=1").as_str()))
    );

    f.replies.push(doc(r#"{"cursor":{"id":0,"ns":"shop.items","nextBatch":[{"a":2}]},"ok":1}"#));
    let r = call(&mut f, "GET", &format!("/v1/cursors/{public}?batchSize=5&maxTimeMS=100"), "");
    assert_eq!(
        f.ran.last().unwrap(),
        &(
            "shop".to_owned(),
            Document::new()
                .with("getMore", id)
                .with("collection", "items")
                .with("batchSize", 5i64)
                .with("maxTimeMS", 100i64)
        )
    );
    let cursor = json(&r).get("cursor").and_then(Value::as_document).cloned().unwrap();
    assert_eq!((cursor.get("id"), cursor.get("next")), (Some(&Value::from("0")), None));

    call(&mut f, "DELETE", &format!("/v1/cursors/{public}"), "");
    assert_eq!(
        f.ran.last().unwrap().1,
        doc(&format!(r#"{{"killCursors":"items","cursors":[{{"$numberLong":"{id}"}}]}}"#))
    );

    // Malformed, mismatched, unknown and negative.
    for bad in ["x", "3.x", "4.5", &format!("9.{id}"), "3.0"] {
        let r = call(&mut f, "GET", &format!("/v1/cursors/{bad}"), "");
        assert_eq!((r.status, json(&r).get("code")), (404, Some(&Value::Int32(43))), "{bad}");
    }
    let unknown = cursor_id(NODE, 5);
    assert_eq!(call(&mut f, "GET", &format!("/v1/cursors/{NODE}.{unknown}"), "").status, 404);
    let r = call(&mut f, "GET", &format!("/v1/cursors/{public}?batchSize=-1"), "");
    assert_eq!(r.status, 400);

    // A cursor of another node goes to that node.
    let theirs = cursor_id(4, 1);
    assert_eq!(call(&mut f, "GET", &format!("/v1/cursors/4.{theirs}"), "").status, 299);
    let gone = cursor_id(5, 1);
    assert_eq!(call(&mut f, "GET", &format!("/v1/cursors/5.{gone}"), "").status, 404);
    assert_eq!(f.forwarded, vec![4, 5]);
}

#[test]
fn command_route_refuses_connection_commands() {
    let mut f = Fake::default();
    for name in [
        "hello",
        "isMaster",
        "saslStart",
        "saslContinue",
        "authenticate",
        "logout",
        "getMore",
        "killCursors",
        "startSession",
        "endSessions",
        "commitTransaction",
        "abortTransaction",
    ] {
        let r = call(&mut f, "POST", "/v1/databases/a/command", &format!(r#"{{"{name}":1}}"#));
        assert_eq!(
            (r.status, json(&r).get("codeName")),
            (400, Some(&Value::from("CommandNotSupported"))),
            "{name}"
        );
    }
    for field in ["lsid", "txnNumber", "autocommit"] {
        let r = call(
            &mut f,
            "POST",
            "/v1/databases/a/command",
            &format!(r#"{{"find":"c","{field}":1}}"#),
        );
        assert_eq!(r.status, 400, "{field}");
        let r = call(&mut f, "POST", &format!("{C}/find"), &format!(r#"{{"{field}":1}}"#));
        assert_eq!(r.status, 400, "{field}");
    }
    assert_eq!(call(&mut f, "POST", "/v1/databases/a/command", "{}").status, 400);
    assert!(f.ran.is_empty());
}

#[test]
fn command_errors_are_problems() {
    let mut f = Fake::default();
    f.replies.push(doc(r#"{"ok":0,"errmsg":"not authorized","code":13,"codeName":"Unauthorized","operationTime":{"$timestamp":{"t":5,"i":1}}}"#));
    let r = call(&mut f, "POST", &format!("{C}/find"), "{}");
    assert_eq!(r.status, 403);
    assert_eq!(header(&r, OPERATION_TIME), Some(r#"{"$timestamp":{"t":5,"i":1}}"#));
    let body = json(&r);
    assert_eq!(body.get("type"), Some(&Value::from("urn:teseladb:error:Unauthorized")));
    assert_eq!(body.get("operationTime"), None);
    f.replies.push(doc(r#"{"ok":0,"errmsg":"x","code":2}"#));
    let r = call(&mut f, "POST", &format!("{C}/find"), "{}");
    assert_eq!((r.status, json(&r).get("codeName")), (400, Some(&Value::from("BadValue"))));
    // Missing or wrong fields of a body.
    for (route, body) in [
        ("insertOne", "{}"),
        ("insertOne", r#"{"document":1}"#),
        ("insertMany", "{}"),
        ("insertMany", r#"{"documents":1}"#),
        ("insertMany", r#"{"documents":[1]}"#),
        ("updateOne", "{}"),
        ("updateOne", r#"{"update":1}"#),
        ("replaceOne", "{}"),
        ("deleteOne", r#"{"filter":1}"#),
        ("findOneAndReplace", "{}"),
        ("findOneAndUpdate", "{}"),
        ("bulkWrite", "{}"),
        ("bulkWrite", r#"{"operations":[1]}"#),
        ("bulkWrite", r#"{"operations":[{"x":{}}]}"#),
    ] {
        let r = call(&mut f, "POST", &format!("{C}/{route}"), body);
        assert_eq!(r.status, 400, "{route} {body}");
    }
    let r = call(&mut f, "POST", "/v1/databases/shop/collections", "{}");
    assert_eq!(r.status, 400);
    let r = call(&mut f, "GET", "/v1/databases?nameOnly=maybe", "");
    assert_eq!(r.status, 400);
    let r = call(&mut f, "GET", "/v1/databases?filter=%7B", "");
    assert_eq!(r.status, 400);
}

#[test]
fn after_cluster_time_goes_to_read_concern() {
    let mut f = Fake::default();
    let ts = r#"{"$timestamp":{"t":10,"i":2}}"#;
    let req = Request::new("POST", &format!("{C}/find"))
        .header(AFTER_CLUSTER_TIME, ts)
        .body(br#"{"readConcern":{"level":"majority"}}"#.to_vec());
    handle(&req, &mut f);
    let rc = doc(&format!(r#"{{"level":"majority","afterClusterTime":{ts}}}"#));
    assert_eq!(f.ran[0].1.get("readConcern"), Some(&Value::Document(rc)));
    let req =
        Request::new("GET", &format!("{C}/estimatedDocumentCount")).header(AFTER_CLUSTER_TIME, ts);
    handle(&req, &mut f);
    assert_eq!(
        f.ran[1].1.get("readConcern"),
        Some(&Value::Document(doc(&format!(r#"{{"afterClusterTime":{ts}}}"#))))
    );
    // Writes and commands that do not read leave it out.
    let req = Request::new("POST", &format!("{C}/deleteMany")).header(AFTER_CLUSTER_TIME, ts);
    handle(&req, &mut f);
    assert_eq!(f.ran[2].1.get("readConcern"), None);
    let req = Request::new("POST", "/v1/databases/a/command")
        .header(AFTER_CLUSTER_TIME, ts)
        .body(br#"{"ping":1}"#.to_vec());
    handle(&req, &mut f);
    assert_eq!(f.ran[3].1.get("readConcern"), None);
    let req = Request::new("POST", "/v1/databases/a/command")
        .header(AFTER_CLUSTER_TIME, ts)
        .body(br#"{"count":"c"}"#.to_vec());
    handle(&req, &mut f);
    assert!(f.ran[4].1.get("readConcern").is_some());
    for bad in ["5", "x", r#"{"$date":1}"#] {
        let req = Request::new("POST", &format!("{C}/find")).header(AFTER_CLUSTER_TIME, bad);
        assert_eq!(handle(&req, &mut f).status, 400, "{bad}");
    }
    let req = Request::new("POST", &format!("{C}/find"))
        .header(AFTER_CLUSTER_TIME, ts)
        .body(br#"{"readConcern":1}"#.to_vec());
    assert_eq!(handle(&req, &mut f).status, 400);
    assert_eq!(f.ran.len(), 5);
}

#[test]
fn routing_and_media_errors() {
    let mut f = Fake::default();
    for target in [
        "/",
        "/v2/databases",
        "/v1/nothing",
        "/v1/databases/a/collections/b/nothing",
        "/v1/databases//collections",
        "/v1/databases/%zz/collections",
    ] {
        let r = call(&mut f, "GET", target, "");
        assert_eq!(r.status, 404, "{target}");
        assert_eq!(header(&r, "Content-Type"), Some("application/problem+json"));
    }
    let r = call(&mut f, "PUT", "/v1/databases/a/collections", "");
    assert_eq!((r.status, header(&r, "Allow")), (405, Some("GET, POST")));
    let req = Request::new("POST", &format!("{C}/find"))
        .header("Content-Type", "text/plain")
        .body(b"{}".to_vec());
    assert_eq!(handle(&req, &mut f).status, 415);
    let req = Request::new("POST", &format!("{C}/find")).header("Accept", "text/html");
    assert_eq!(handle(&req, &mut f).status, 406);
    let r = call(&mut f, "POST", &format!("{C}/find"), "{");
    assert_eq!((r.status, json(&r).get("codeName")), (400, Some(&Value::from("BadValue"))));
    assert!(f.ran.is_empty());
    // HEAD is GET.
    f.replies.push(doc(r#"{"n":1,"ok":1}"#));
    assert_eq!(
        handle(&Request::new("HEAD", &format!("{C}/estimatedDocumentCount")), &mut f).status,
        200
    );
}

#[test]
fn formats_of_bodies_and_answers() {
    let mut f = Fake::default();
    f.replies.push(doc(r#"{"n":{"$numberLong":"5"},"ok":1}"#));
    let req = Request::new("GET", &format!("{C}/estimatedDocumentCount"))
        .header("Accept", CANONICAL_JSON);
    let r = handle(&req, &mut f);
    assert_eq!(header(&r, "Content-Type"), Some(CANONICAL_JSON));
    assert_eq!(std::str::from_utf8(&r.body).unwrap(), r#"{"count":{"$numberLong":"5"}}"#);

    let body = Document::new().with("filter", Document::new().with("a", 1i64)).to_bytes().unwrap();
    f.replies.push(doc(r#"{"ok":0,"code":13,"errmsg":"no"}"#));
    let req = Request::new("POST", &format!("{C}/find"))
        .header("Content-Type", BSON)
        .header("Accept", BSON)
        .body(body);
    let r = handle(&req, &mut f);
    assert_eq!(f.ran[1].1.get("filter"), Some(&Value::Document(Document::new().with("a", 1i64))));
    assert_eq!((r.status, header(&r, "Content-Type")), (403, Some(BSON)));
    let req =
        Request::new("POST", &format!("{C}/find")).header("Content-Type", BSON).body(vec![1, 2]);
    assert_eq!(handle(&req, &mut f).status, 400);
}

/// 019 FR-009: `POST /v1/transactions` answers its results, the problem of the operation that
/// failed with its index and `operationTime`, or the problem of a body it does not take.
#[test]
fn transactions_answer_results_or_the_operation_that_failed() {
    let body = r#"{"operations":[{"op":"insert","db":"a","coll":"c","documents":[{"_id":1}]}]}"#;
    let mut f = Fake::default();
    let r = call(&mut f, "POST", "/v1/transactions", body);
    assert_eq!(r.status, 200);
    assert!(json(&r).get("results").is_some());
    let mut f = Fake {
        replies: vec![doc(
            r#"{"ok":1.0,"writeErrors":[{"index":0,"code":11000,"errmsg":"dup"}],"operationTime":{"$timestamp":{"t":4,"i":1}}}"#,
        )],
        ..Fake::default()
    };
    let r = call(&mut f, "POST", "/v1/transactions", body);
    assert_eq!(r.status, 409);
    assert_eq!(json(&r).get("operationIndex"), Some(&Value::Int32(0)));
    assert!(header(&r, OPERATION_TIME).is_some());
    let mut f = Fake {
        replies: vec![doc(r#"{"ok":0.0,"code":2,"codeName":"BadValue","errmsg":"no"}"#)],
        ..Fake::default()
    };
    let r = call(&mut f, "POST", "/v1/transactions", body);
    assert_eq!((r.status, header(&r, OPERATION_TIME)), (400, None));
    let r = call(&mut Fake::default(), "POST", "/v1/transactions", r#"{"operations":[]}"#);
    assert_eq!(r.status, 400);
}

/// 019 FR-012: the change stream routes open their `aggregate`; other routes are not streams.
#[test]
fn watch_opens_the_change_stream_of_its_route() {
    use teseladb_rest::api::{problem_body, watch};
    let get = Request::new("GET", &format!("{C}/changeStream?fullDocument=updateLookup"))
        .header(teseladb_http::sse::LAST_EVENT_ID, r#"{"_data":"01"}"#);
    let (db, cmd) = watch(&get).unwrap().unwrap();
    assert_eq!((db.as_str(), cmd.get("aggregate")), ("shop", Some(&Value::from("items"))));
    let post = Request::new("POST", &format!("{C}/changeStream")).body(b"{\"x\":".to_vec());
    assert_eq!(watch(&post).unwrap().unwrap_err().status, 400);
    let bad = Request::new("GET", &format!("{C}/changeStream?nope=1"));
    assert_eq!(watch(&bad).unwrap().unwrap_err().status, 400);
    assert!(watch(&Request::new("GET", "/v1/databases")).is_none());
    assert!(watch(&Request::new("GET", "/nowhere")).is_none());
    let (st, d) =
        problem_body(&doc(r#"{"ok":0.0,"code":26,"codeName":"NamespaceNotFound","errmsg":"x"}"#));
    assert_eq!((st, d.get("code")), (404, Some(&Value::Int32(26))));
}

/// 019 FR-010: a write with `Idempotency-Key` runs once in its session through the route.
#[test]
fn a_write_with_an_idempotency_key_runs_in_its_session() {
    let mut f = Fake::default();
    let req = Request::new("POST", &format!("{C}/insertOne"))
        .header(teseladb_rest::idempotency::HEADER, "k1")
        .body(br#"{"document":{"_id":1}}"#.to_vec());
    let r = handle(&req, &mut f);
    assert!(r.status < 300, "{}", r.status);
    assert!(f.ran.iter().any(|(_, c)| c.get("lsid").is_some() && c.get("txnNumber").is_some()));
}

/// The edges of the answers: a transaction whose commit failed or whose writes report nothing,
/// an `operationTime` that is not a timestamp, the options of `getMore`, a `PATCH` with a
/// pipeline beside other fields, `listApiKeys` by user and the problems of a missing user or key.
#[test]
fn edges_of_answers_and_routes() {
    use teseladb_rest::api::problem_body;
    let body = r#"{"operations":[{"op":"insert","db":"a","coll":"c","documents":[]}]}"#;
    let mut f = Fake {
        replies: vec![
            doc(r#"{"ok":1.0,"writeErrors":[]}"#),
            doc(
                r#"{"ok":0.0,"code":50,"codeName":"MaxTimeMSExpired","errmsg":"t","operationTime":1}"#,
            ),
        ],
        ..Fake::default()
    };
    let r = call(&mut f, "POST", "/v1/transactions", body);
    assert_eq!(r.status, 504);
    assert!(json(&r).get("operationIndex").is_none());
    assert_eq!(header(&r, OPERATION_TIME), None);
    // A `getMore` without options asks for none.
    let id = cursor_id(NODE, 98);
    let mut f = Fake { cursors: vec![(id, "shop.items".to_owned())], ..Fake::default() };
    call(&mut f, "GET", &format!("/v1/cursors/{NODE}.{id}"), "");
    let more = &f.ran.last().unwrap().1;
    assert!(more.get("batchSize").is_none() && more.get("maxTimeMS").is_none(), "{more:?}");
    let (_, cmd) = built("PATCH", &format!("{C}/documents/1"), r#"{"pipeline":[],"x":1}"#);
    assert!(format!("{cmd:?}").contains("pipeline"), "{cmd:?}");
    let (_, cmd) = built("GET", "/v1/apiKeys?user=ana&db=admin", "");
    assert_eq!(cmd.get("user"), Some(&Value::from("ana")));
    let (_, cmd) = built("PUT", &format!("{C}/documents/1"), r#"{"a":1}"#);
    assert!(format!("{cmd:?}").contains("upsert"), "{cmd:?}");
    for (code, name) in [(11, "UserNotFound"), (211, "KeyNotFound")] {
        let reply = doc(&format!(r#"{{"ok":0.0,"code":{code},"codeName":"{name}","errmsg":"x"}}"#));
        assert_eq!(problem_body(&reply).0, 404, "{name}");
    }
    // A find whose `batchSize` is a fraction keeps the default batch.
    let r = call(&mut Fake::default(), "POST", &format!("{C}/find"), r#"{"batchSize":2.5}"#);
    assert!(r.status < 500);
}
