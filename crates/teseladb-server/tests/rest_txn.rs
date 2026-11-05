//! `POST /v1/transactions` on a running node (019 T017, FR-009): every operation or none, the
//! result of each, and the retries of `withTransaction` with errors injected by `failCommand`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use teseladb_server::client_listener::ClientConfig;
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_bson::{Document, Value, extjson};
use teseladb_http::http1::{Limits, Request, ResponseReader};
use teseladb_http::server::Settings;
use teseladb_raftlog::MemDisk;

const READY: Duration = Duration::from_secs(30);
const TXN: &str = "/v1/transactions";
const COMMAND: &str = "/v1/databases/admin/command";

fn call(addr: SocketAddr, method: &str, target: &str, body: &str) -> (u16, Document) {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let req = Request::new(method, target).header("Host", "h").body(body.as_bytes().to_vec());
    s.write_all(&req.encode().unwrap()).unwrap();
    let mut reader = ResponseReader::new(Limits::default());
    let mut buf = [0u8; 65536];
    loop {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "closed before the answer");
        if let Some(r) = reader.feed(&buf[..n]).unwrap() {
            let text = std::str::from_utf8(&r.body).unwrap();
            let doc = extjson::parse(text).unwrap_or_else(|_| Document::new().with("raw", text));
            return (r.status, doc);
        }
    }
}

fn start() -> Node<MemDisk> {
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: Settings::default(),
    };
    let client = ClientConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        test_commands: true,
        ..ClientConfig::new(Vec::new())
    };
    let cfg = NodeConfig {
        cores: Some(2),
        http: Some(http),
        client: Some(client),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    node
}

/// The documents of `db.coll`, by `_id`.
fn all(at: SocketAddr, db: &str, coll: &str) -> Vec<Value> {
    let target = format!("/v1/databases/{db}/collections/{coll}/find");
    let (st, d) = call(at, "POST", &target, r#"{"sort":{"_id":1}}"#);
    assert_eq!(st, 200, "{d:?}");
    match d.get("cursor").and_then(Value::as_document).and_then(|c| c.get("firstBatch")) {
        Some(Value::Array(b)) => b.clone(),
        _ => panic!("{d:?}"),
    }
}

fn fail(at: SocketAddr, command: &str, code: i32, label: Option<&str>) {
    let labels = label.map_or(String::new(), |l| format!(r#","errorLabels":["{l}"]"#));
    let body = format!(
        r#"{{"configureFailPoint":"failCommand","mode":{{"times":1}},"data":{{"failCommands":["{command}"],"errorCode":{code}{labels}}}}}"#
    );
    let (st, d) = call(at, "POST", COMMAND, &body);
    assert_eq!(st, 200, "{d:?}");
}

fn id(n: i32) -> Value {
    Value::Document(Document::new().with("_id", n))
}

#[test]
fn a_transaction_applies_every_operation_or_none_and_retries_like_with_transaction() {
    let node = start();
    let at = node.http_addrs()[0];
    for (db, coll) in [("a", "c"), ("b", "d")] {
        let (st, d) = call(
            at,
            "POST",
            &format!("/v1/databases/{db}/collections"),
            &format!(r#"{{"name":"{coll}"}}"#),
        );
        assert_eq!(st, 200, "{d:?}");
    }
    let ops = |n: i32| {
        format!(
            r#"{{"operations":[{{"op":"insert","db":"a","coll":"c","documents":[{{"_id":{n}}}]}},{{"op":"update","db":"b","coll":"d","updates":[{{"q":{{"_id":{n}}},"u":{{"$set":{{"v":{n}}}}},"upsert":true}}]}},{{"op":"find","db":"a","coll":"c","filter":{{}},"batchSize":1}}],"maxCommitTimeMS":5000}}"#
        )
    };
    // Every operation, with the result of each; the `find` sees the transaction's own insert
    // and follows its cursor inside the transaction.
    let (st, d) = call(at, "POST", TXN, &ops(1));
    assert_eq!(st, 200, "{d:?}");
    let Some(Value::Array(results)) = d.get("results") else { panic!("{d:?}") };
    assert_eq!(results.len(), 3);
    assert_eq!(results[2], Value::Array(vec![id(1)]));
    assert_eq!(all(at, "a", "c"), vec![id(1)]);
    // A transient error of the second operation runs the whole transaction again, once.
    fail(at, "update", 112, Some("TransientTransactionError"));
    let (st, d) = call(at, "POST", TXN, &ops(2));
    assert_eq!(st, 200, "{d:?}");
    assert_eq!(all(at, "a", "c"), vec![id(1), id(2)]);
    // A commit whose result is unknown is committed again.
    fail(at, "commitTransaction", 91, Some("UnknownTransactionCommitResult"));
    let (st, d) = call(at, "POST", TXN, &ops(3));
    assert_eq!(st, 200, "{d:?}");
    assert_eq!(all(at, "a", "c"), vec![id(1), id(2), id(3)]);
    // Any other error, and a write error, leave nothing: the insert before it is gone too.
    fail(at, "update", 2, None);
    let (st, d) = call(at, "POST", TXN, &ops(4));
    assert_eq!((st, d.get("operationIndex")), (400, Some(&Value::Int32(1))), "{d:?}");
    let (st, d) =
        call(at, "POST", TXN, &ops(5).replace(r#""q":{"_id":5}"#, r#""q":{"_id":{"$bad":1}}"#));
    assert_eq!(st, 400, "{d:?}");
    let dup = r#"{"operations":[{"op":"insert","db":"a","coll":"c","documents":[{"_id":6}]},{"op":"insert","db":"a","coll":"c","documents":[{"_id":1}]}]}"#;
    let (st, d) = call(at, "POST", TXN, dup);
    assert_eq!(
        (st, d.get("code"), d.get("operationIndex")),
        (409, Some(&Value::Int32(11000)), Some(&Value::Int32(1))),
        "{d:?}"
    );
    assert_eq!(all(at, "a", "c"), vec![id(1), id(2), id(3)]);
    let (_, d) = call(at, "POST", "/v1/databases/b/collections/d/countDocuments", "{}");
    assert_eq!(d.get("count"), Some(&Value::Int32(3)), "{d:?}");
    // Bodies a transaction does not take.
    for bad in [
        r#"{}"#,
        r#"{"operations":[]}"#,
        r#"{"operations":[{"op":"drop","db":"a","coll":"c"}]}"#,
        r#"{"operations":[{"op":"find","db":"a"}]}"#,
        r#"{"operations":[{"op":"find","db":"a","coll":"c","lsid":{}}]}"#,
        r#"{"operations":[{"op":"find","db":"a","coll":"c","readConcern":{}}]}"#,
        r#"{"operations":[1]}"#,
        r#"{"operations":1}"#,
        r#"{"operations":[{"op":"find","db":"a","coll":"c"}],"other":1}"#,
    ] {
        assert_eq!(call(at, "POST", TXN, bad).0, 400, "{bad}");
    }
    drop(node.stop());
}
