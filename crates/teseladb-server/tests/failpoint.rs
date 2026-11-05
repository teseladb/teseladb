//! `configureFailPoint` on a running node started with `enableTestCommands` (006 T056, FR-034):
//! a closed connection, and a blocked one that holds only its own client.

mod common;

use std::io::Read;
use std::time::{Duration, Instant};

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::node::Node;
use teseladb_bson::{Document, Value};
use teseladb_raftlog::MemDisk;

fn fail(data: Document) -> Document {
    Document::new()
        .with("configureFailPoint", "failCommand")
        .with("mode", Document::new().with("times", 1))
        .with("data", data)
}

fn names(n: &str) -> Value {
    Value::Array(vec![Value::from(n)])
}

#[test]
fn a_failpoint_closes_or_holds_only_its_connection() {
    let mut cfg = config(Vec::new());
    if let Some(c) = cfg.client.as_mut() {
        c.test_commands = true;
    }
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let mut admin = Client::new(&node);
    let r = admin.run("app", Document::new().with("insert", "c").with("documents", docs(0..2)));
    assert!(ok(&r), "{r:?}");
    let close = Document::new().with("failCommands", names("find")).with("closeConnection", true);
    assert!(ok(&admin.run("admin", fail(close))));
    let mut victim = Client::new(&node);
    victim.send("app", Document::new().with("find", "c"));
    let mut b = [0u8; 1];
    assert!(matches!(victim.s.read(&mut b), Ok(0) | Err(_)));
    let block = Document::new()
        .with("failCommands", names("find"))
        .with("blockConnection", true)
        .with("blockTimeMS", 400);
    assert!(ok(&admin.run("admin", fail(block))));
    let mut held = Client::new(&node);
    let start = Instant::now();
    held.send("app", Document::new().with("find", "c"));
    // Another client goes on while that one waits.
    let r = until("a ping", || Some(admin.run("admin", Document::new().with("ping", 1))));
    assert!(ok(&r) && start.elapsed() < Duration::from_millis(400));
    let r = held.receive();
    assert!(start.elapsed() >= Duration::from_millis(400));
    assert_eq!(batch(&r, "firstBatch").len(), 2);
    // What the held client sends meanwhile follows the held request, in order.
    let block = Document::new()
        .with("failCommands", names("find"))
        .with("blockConnection", true)
        .with("blockTimeMS", 200);
    assert!(ok(&admin.run("admin", fail(block))));
    let mut queued = Client::new(&node);
    queued.send("app", Document::new().with("find", "c"));
    std::thread::sleep(Duration::from_millis(50));
    queued.send("admin", Document::new().with("ping", 1));
    assert_eq!(batch(&queued.receive(), "firstBatch").len(), 2);
    assert!(ok(&queued.receive()));
    // Held, then closed.
    let both = Document::new()
        .with("failCommands", names("find"))
        .with("blockConnection", true)
        .with("blockTimeMS", 200)
        .with("closeConnection", true);
    assert!(ok(&admin.run("admin", fail(both))));
    let mut closing = Client::new(&node);
    let start = Instant::now();
    closing.send("app", Document::new().with("find", "c"));
    assert!(matches!(closing.s.read(&mut b), Ok(0) | Err(_)));
    assert!(start.elapsed() >= Duration::from_millis(200));
    // A command the worker runs itself, held and then closed.
    let both = Document::new()
        .with("failCommands", names("ping"))
        .with("blockConnection", true)
        .with("blockTimeMS", 200)
        .with("closeConnection", true);
    assert!(ok(&admin.run("admin", fail(both))));
    let mut pinged = Client::new(&node);
    let start = Instant::now();
    pinged.send("admin", Document::new().with("ping", 1));
    assert!(matches!(pinged.s.read(&mut b), Ok(0) | Err(_)));
    assert!(start.elapsed() >= Duration::from_millis(200));
    // A held client that goes away is forgotten at the deadline.
    let block = Document::new()
        .with("failCommands", names("find"))
        .with("blockConnection", true)
        .with("blockTimeMS", 200);
    assert!(ok(&admin.run("admin", fail(block))));
    let mut gone = Client::new(&node);
    gone.send("app", Document::new().with("find", "c"));
    std::thread::sleep(Duration::from_millis(50));
    drop(gone);
    std::thread::sleep(Duration::from_millis(400));
    assert!(ok(&admin.run("admin", Document::new().with("ping", 1))));
    drop(node.stop());
}

/// `POST target` with `body` on the HTTP port at `at`: status and body.
fn rest(at: std::net::SocketAddr, target: &str, body: &str) -> (u16, String) {
    let req = teseladb_http::http1::Request::new("POST", target).body(body.as_bytes().to_vec());
    answer(&mut sent(at, req))
}

/// A connection to `at` that sent `req`.
fn sent(at: std::net::SocketAddr, req: teseladb_http::http1::Request) -> std::net::TcpStream {
    use std::io::Write;
    let mut s = std::net::TcpStream::connect(at).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(&req.header("Host", "h").encode().unwrap()).unwrap();
    s
}

/// The answer that comes on `s`: status and body.
fn answer(s: &mut std::net::TcpStream) -> (u16, String) {
    use teseladb_http::http1::{Limits, ResponseReader};
    let mut reader = ResponseReader::new(Limits::default());
    let mut buf = [0u8; 65536];
    loop {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "closed before the answer");
        if let Some(r) = reader.feed(&buf[..n]).unwrap() {
            return (r.status, String::from_utf8_lossy(&r.body).into_owned());
        }
    }
}

/// `failCommand` on a request of the REST API (019): `blockConnection` holds it and then runs
/// it; `closeConnection`, which has no connection to close there, fails it.
#[test]
fn a_failpoint_holds_or_fails_a_rest_request() {
    let mut cfg = config(Vec::new());
    if let Some(c) = cfg.client.as_mut() {
        c.test_commands = true;
    }
    cfg.http = Some(teseladb_server::http_listener::HttpPortConfig {
        addrs: vec![std::net::SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: teseladb_http::server::Settings::default(),
    });
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let at = node.http_addrs()[0];
    let mut admin = Client::new(&node);
    let r = admin.run("app", Document::new().with("insert", "c").with("documents", docs(0..2)));
    assert!(ok(&r), "{r:?}");
    let find = "/v1/databases/app/collections/c/find";
    let block = Document::new()
        .with("failCommands", names("find"))
        .with("blockConnection", true)
        .with("blockTimeMS", 300);
    assert!(ok(&admin.run("admin", fail(block))));
    let start = Instant::now();
    let (st, body) = rest(at, find, r#"{"filter":{}}"#);
    assert_eq!(st, 200, "{body}");
    assert!(start.elapsed() >= Duration::from_millis(300));
    let close = Document::new().with("failCommands", names("find")).with("closeConnection", true);
    assert!(ok(&admin.run("admin", fail(close))));
    let (st, body) = rest(at, find, r#"{"filter":{}}"#);
    assert!(st >= 500 && body.contains("the command closed the request"), "{st} {body}");

    // An MCP call with a `progressToken` held longer than the stream's poll: its stream waits for
    // it, and a client that gives it up ends the wait.
    let call = format!(
        r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"_meta":{{"progressToken":"p","io.modelcontextprotocol/protocolVersion":"2026-07-28"}},"name":"find","arguments":{{"connectionId":"{}.stateless","database":"app","collection":"c","filter":{{}}}}}}}}"#,
        node.node_id().0
    );
    let progress = || {
        teseladb_http::http1::Request::new("POST", "/mcp")
            .header("Accept", "application/json, text/event-stream")
            .header("Content-Type", "application/json")
            .body(call.as_bytes().to_vec())
    };
    let block = Document::new()
        .with("failCommands", names("find"))
        .with("blockConnection", true)
        .with("blockTimeMS", 600);
    assert!(ok(&admin.run("admin", fail(block.clone()))));
    let (st, body) = answer(&mut sent(at, progress()));
    assert!(st == 200 && body.contains("queryResultsCount"), "{st} {body}");
    assert!(ok(&admin.run("admin", fail(block))));
    let given_up = sent(at, progress());
    std::thread::sleep(Duration::from_millis(100));
    drop(given_up);
    std::thread::sleep(Duration::from_millis(900));
    drop(node.stop());
}
