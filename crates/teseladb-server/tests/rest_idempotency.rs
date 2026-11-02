//! `Idempotency-Key` on three nodes (019 T018, FR-010): the same write sent again, to another
//! node and after the node that ran it stopped (its ranges elect another leader), answers the
//! first answer and applies once; another request with the key gets `422`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use teseladb_bson::{Document, Value, extjson};
use teseladb_http::http1::{Limits, Request, Response, ResponseReader};
use teseladb_http::server::Settings;
use teseladb_raftlog::MemDisk;
use teseladb_server::client_listener::ClientConfig;
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};

const READY: Duration = Duration::from_secs(30);
const C: &str = "/v1/databases/app/collections/c";

fn start(join: Vec<String>) -> Node<MemDisk> {
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: Settings::default(),
    };
    let client = ClientConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        ..ClientConfig::new(Vec::new())
    };
    let cfg = NodeConfig {
        cores: Some(2),
        join,
        http: Some(http),
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        client: Some(client),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    node
}

fn send(addr: SocketAddr, method: &str, target: &str, body: &str, key: Option<&str>) -> Response {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let mut req = Request::new(method, target).header("Host", "h");
    if let Some(k) = key {
        req = req.header("Idempotency-Key", k);
    }
    s.write_all(&req.body(body.as_bytes().to_vec()).encode().unwrap()).unwrap();
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

fn doc(r: &Response) -> Document {
    extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap_or_default()
}

/// `f` until it answers below 500, for at most [`READY`].
fn settled(mut f: impl FnMut() -> Response) -> Response {
    let end = Instant::now() + READY;
    loop {
        let r = f();
        if r.status < 500 || Instant::now() >= end {
            return r;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn counts(at: SocketAddr) -> Vec<Value> {
    let r =
        send(at, "POST", &format!("{C}/find"), r#"{"sort":{"v":1},"projection":{"_id":0}}"#, None);
    let d = doc(&r);
    match d.get("cursor").and_then(Value::as_document).and_then(|c| c.get("firstBatch")) {
        Some(Value::Array(b)) => b.clone(),
        _ => panic!("{d:?}"),
    }
}

#[test]
fn a_write_sent_again_with_its_key_applies_once_across_nodes_and_leaders() {
    let first = start(Vec::new());
    let seed = vec![first.peer_addr().to_string()];
    let others: Vec<Node<MemDisk>> = (0..2).map(|_| start(seed.clone())).collect();
    let at: Vec<SocketAddr> =
        std::iter::once(&first).chain(others.iter()).map(|n| n.http_addrs()[0]).collect();
    let (st, d) = {
        let r = send(at[0], "POST", "/v1/databases/app/collections", r#"{"name":"c"}"#, None);
        (r.status, doc(&r))
    };
    assert_eq!(st, 200, "{d:?}");
    // A retryable write: the ObjectId it gave and its answer come back the same from any node.
    let one = r#"{"document":{"v":1,"n":0}}"#;
    let a = send(at[0], "POST", &format!("{C}/insertOne"), one, Some("k-insert"));
    assert_eq!(a.status, 200, "{:?}", doc(&a));
    for node in [at[1], at[2], at[0]] {
        let b = send(node, "POST", &format!("{C}/insertOne"), one, Some("k-insert"));
        assert_eq!((b.status, doc(&b)), (a.status, doc(&a)));
    }
    let two = r#"{"document":{"v":2,"n":0}}"#;
    assert_eq!(send(at[1], "POST", &format!("{C}/insertOne"), two, None).status, 200);
    // A write RefDb does not retry runs as a transaction of the key's session.
    let many = r#"{"filter":{},"update":{"$inc":{"n":1}}}"#;
    let m = send(at[2], "POST", &format!("{C}/updateMany"), many, Some("k-many"));
    assert_eq!(m.status, 200, "{:?}", doc(&m));
    assert_eq!(doc(&m).get("nModified"), Some(&Value::Int32(2)), "{:?}", doc(&m));
    // Another request with a key already used.
    let other = send(at[0], "POST", &format!("{C}/insertOne"), two, Some("k-insert"));
    assert_eq!(other.status, 422);
    for bad in ["", &"x".repeat(256), "with space"] {
        assert_eq!(send(at[0], "POST", &format!("{C}/insertOne"), one, Some(bad)).status, 400);
    }
    // The collection's range moves to the other core of its node, under a new leader; the
    // retries still answer the first answers without applying anything.
    relocate(&first, "app.c");
    let again = settled(|| send(at[1], "POST", &format!("{C}/updateMany"), many, Some("k-many")));
    assert_eq!((again.status, doc(&again)), (m.status, doc(&m)));
    let again = settled(|| send(at[2], "POST", &format!("{C}/insertOne"), one, Some("k-insert")));
    assert_eq!((again.status, doc(&again)), (a.status, doc(&a)));
    let want = vec![
        Value::Document(Document::new().with("v", 1).with("n", 1)),
        Value::Document(Document::new().with("v", 2).with("n", 1)),
    ];
    assert_eq!(counts(at[1]), want);
    drop(others.into_iter().map(Node::stop).collect::<Vec<_>>());
    drop(first.stop());
}

/// `teseladb admin` on `node`'s administration port: its output.
fn admin(node: &Node<MemDisk>, args: &[&str]) -> String {
    let none = std::env::temp_dir().join("teseladb-idempotency-no-pki");
    let mut all = vec!["--node".to_owned(), node.admin_addr().unwrap().to_string()];
    all.extend(["--dbpath".to_owned(), none.to_str().unwrap().to_owned()]);
    all.extend(args.iter().map(|a| (*a).to_owned()));
    let o = teseladb_server::admin::parse(&all).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::admin::run(&o, &mut out, &mut err);
    assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
    String::from_utf8(out).unwrap()
}

/// Moves each range of collection `ns` to the other core of its node (007 FR-011): its Raft
/// group starts again there and elects a new leader.
fn relocate(node: &Node<MemDisk>, ns: &str) {
    let text = admin(node, &["--format", "json", "ranges", "--collection", ns]);
    let d = extjson::parse(&text).unwrap();
    let Some(Value::Array(ranges)) = d.get("ranges") else { panic!("{text}") };
    for r in ranges {
        let r = r.as_document().unwrap();
        let id = match r.get("_id") {
            Some(Value::Int64(n)) => n.to_string(),
            Some(Value::Int32(n)) => n.to_string(),
            other => panic!("{other:?}"),
        };
        let core = match r.get("core") {
            Some(Value::Int32(c)) => 1 - c,
            other => panic!("{other:?}"),
        };
        admin(node, &["range", "relocate", &id, "--core", &core.to_string()]);
    }
}
