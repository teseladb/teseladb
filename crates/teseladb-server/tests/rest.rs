//! The REST API on a running node (019 T009, T011, T013, T014, T016, FR-005 to FR-014): the
//! acceptance scenarios of story 1 over the HTTP port, run by the node's executor of the wire
//! protocol, and the requests without a credential on a node with authentication.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value, extjson};
use teseladb_http::http1::{Limits, Request, Response, ResponseReader};
use teseladb_http::server::Settings;
use teseladb_raftlog::MemDisk;
use teseladb_rest::api::{AFTER_CLUSTER_TIME, OPERATION_TIME};
use teseladb_server::client_listener::ClientConfig;
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};

fn send(addr: SocketAddr, req: Request) -> Response {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(&req.header("Host", "h").encode().unwrap()).unwrap();
    let mut reader = ResponseReader::new(Limits::default());
    let mut buf = [0u8; 65536];
    loop {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "closed before the answer");
        if let Some(r) = reader.feed(&buf[..n]).unwrap() {
            return r;
        }
    }
}

fn call(addr: SocketAddr, method: &str, target: &str, body: &str) -> (u16, Document, Response) {
    let r = send(addr, Request::new(method, target).body(body.as_bytes().to_vec()));
    let doc = extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap_or_default();
    (r.status, doc, r)
}

fn start(client: ClientConfig) -> Node<MemDisk> {
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: Settings::default(),
    };
    let base = config(Vec::new());
    let cfg = NodeConfig { http: Some(http), client: Some(client), ..base };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    node
}

fn plain_client() -> ClientConfig {
    config(Vec::new()).client.unwrap()
}

fn cursor(d: &Document) -> Document {
    d.get("cursor").and_then(Value::as_document).cloned().unwrap_or_else(|| panic!("{d:?}"))
}

const C: &str = "/v1/databases/shop/collections/items";

#[test]
fn story_one_over_the_http_port() {
    let node = start(plain_client());
    let at = node.http_addrs()[0];

    // Scenario 1: insertOne, then the same document by its id.
    let (st, d, r) = call(at, "POST", &format!("{C}/insertOne"), r#"{"document":{"a":1}}"#);
    assert_eq!(st, 200, "{d:?}");
    assert!(r.header(OPERATION_TIME).is_some_and(|t| t.starts_with("{\"$timestamp\"")));
    let Some(Value::ObjectId(oid)) = d.get("insertedId") else { panic!("{d:?}") };
    let hex: String = oid.iter().map(|b| format!("{b:02x}")).collect();
    let id = format!("%7B%22%24oid%22%3A%22{hex}%22%7D");
    let (st, d, _) = call(at, "GET", &format!("{C}/documents/{id}"), "");
    assert_eq!((st, d.get("a")), (200, Some(&Value::Int32(1))), "{d:?}");
    assert_eq!(d.get("_id"), Some(&Value::ObjectId(*oid)));
    let (st, ..) = call(at, "GET", &format!("{C}/documents/nothing"), "");
    assert_eq!(st, 404);

    // Scenario 2: 1,000 results in batches of 100, followed through `next`.
    let items: Vec<String> = (0..1000).map(|k| format!("{{\"_id\":{k}}}")).collect();
    let body = format!("{{\"documents\":[{}]}}", items.join(","));
    let (st, d, _) = call(at, "POST", "/v1/databases/big/collections/c/insertMany", &body);
    assert_eq!((st, d.get("n")), (200, Some(&Value::Int32(1000))), "{d:?}");
    let (st, d, _) = call(
        at,
        "POST",
        "/v1/databases/big/collections/c/find",
        r#"{"batchSize":100,"sort":{"_id":1}}"#,
    );
    assert_eq!(st, 200, "{d:?}");
    let mut c = cursor(&d);
    let mut seen = match c.get("firstBatch") {
        Some(Value::Array(b)) => b.len(),
        _ => panic!("{c:?}"),
    };
    let mut batches = 1;
    while let Some(Value::String(next)) = c.get("next").cloned() {
        let (st, d, _) = call(at, "GET", &next, "");
        assert_eq!(st, 200, "{d:?}");
        c = cursor(&d);
        if let Some(Value::Array(b)) = c.get("nextBatch") {
            seen += b.len();
        }
        batches += 1;
    }
    assert_eq!(seen, 1000);
    assert!(batches >= 10, "{batches}");
    assert_eq!(c.get("id"), Some(&Value::from("0")));

    // A killed cursor is gone.
    let (_, d, _) = call(at, "POST", "/v1/databases/big/collections/c/find", r#"{"batchSize":1}"#);
    let Some(Value::String(next)) = cursor(&d).get("next").cloned() else { panic!() };
    let (st, d, _) = call(at, "DELETE", &next, "");
    assert_eq!(st, 200, "{d:?}");
    let (st, d, _) = call(at, "GET", &next, "");
    assert_eq!((st, d.get("code")), (404, Some(&Value::Int32(43))));

    // Counts, a causal read after the insert's operation time, and errors.
    let (_, d, r) = call(
        at,
        "POST",
        "/v1/databases/big/collections/c/countDocuments",
        r#"{"filter":{"_id":{"$lt":10}}}"#,
    );
    assert_eq!(d.get("count"), Some(&Value::Int32(10)));
    let time = r.header(OPERATION_TIME).unwrap().to_owned();
    let req = Request::new("GET", "/v1/databases/big/collections/c/estimatedDocumentCount")
        .header(AFTER_CLUSTER_TIME, &time);
    let r = send(at, req);
    assert_eq!(r.status, 200, "{}", String::from_utf8_lossy(&r.body));
    let (st, d, _) = call(at, "POST", &format!("{C}/insertOne"), r#"{"document":{"_id":1}}"#);
    assert_eq!(st, 200, "{d:?}");
    let (st, d, r) = call(at, "POST", &format!("{C}/insertOne"), r#"{"document":{"_id":1}}"#);
    assert_eq!((st, d.get("codeName")), (409, Some(&Value::from("DuplicateKey"))), "{d:?}");
    assert_eq!(r.header("content-type"), Some("application/problem+json"));

    // Any command of the inventory, except those of a connection.
    let (st, d, _) = call(at, "POST", "/v1/databases/shop/command", r#"{"ping":1}"#);
    assert_eq!((st, d.get("ok")), (200, None), "{d:?}");
    let (st, d, _) = call(at, "POST", "/v1/databases/shop/command", r#"{"find":"items"}"#);
    assert_eq!(st, 200, "{d:?}");
    assert_eq!(cursor(&d).get("id"), Some(&Value::from("0")));
    let (st, d, _) = call(at, "POST", "/v1/databases/shop/command", r#"{"hello":1}"#);
    assert_eq!((st, d.get("code")), (400, Some(&Value::Int32(115))));

    let (st, d, _) = call(at, "GET", "/v1/databases?nameOnly=true", "");
    assert_eq!(st, 200, "{d:?}");
    assert!(matches!(d.get("databases"), Some(Value::Array(_))));
    // The same data through the wire protocol (research R1): the documents REST wrote, and
    // REST reads what the wire protocol writes.
    let mut wire = Client::new(&node);
    let find = Document::new()
        .with("find", "c")
        .with("filter", Document::new().with("_id", Document::new().with("$lt", 3)));
    let ids: Vec<Value> = (0..3).map(|k| Value::Document(Document::new().with("_id", k))).collect();
    assert_eq!(batch(&wire.run("big", find), "firstBatch"), ids);
    assert!(
        ok(&wire.run("big", Document::new().with("insert", "d").with("documents", docs(5..7))))
    );
    until("the wire protocol's insert over REST", || {
        let (_, d, _) = call(at, "POST", "/v1/databases/big/collections/d/countDocuments", "");
        (d.get("count") == Some(&Value::Int32(2))).then_some(())
    });
    let (st, _, r) = call(at, "GET", "/v1/openapi.json", "");
    assert_eq!((st, r.header("content-type")), (200, Some("application/json")));
    drop(node.stop());
}

#[test]
fn a_node_with_authentication_refuses_requests_without_a_credential() {
    let node = start(ClientConfig { auth: true, ..plain_client() });
    let at = node.http_addrs()[0];
    let (st, _, r) = call(at, "GET", "/v1/databases", "");
    assert_eq!(st, 401);
    assert!(r.header("www-authenticate").is_some_and(|c| c.starts_with("Bearer")));
    let req = Request::new("GET", "/v1/databases").header("Authorization", "Bearer nothing");
    assert_eq!(send(at, req).status, 401);
    // `/healthz` needs none.
    assert_eq!(call(at, "GET", "/healthz", "").0, 200);
    drop(node.stop());
}
