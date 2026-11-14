//! Progress and cancellation of MCP's tools on a running node (019 T030, FR-026): a call with a
//! `progressToken` that accepts an event stream gets each `notifications/progress` as its batch
//! is read, in a piece of the stream of its own before the result; and `notifications/cancelled`
//! kills the operation of a call still running.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::Document;
use teseladb_http::server::Settings;
use teseladb_json::{Value, parse};
use teseladb_raftlog::MemDisk;
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};

const META: &str =
    r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","progressToken":"p1"}"#;

fn start() -> Node<MemDisk> {
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: Settings::default(),
    };
    let cfg = NodeConfig { http: Some(http), ..config(Vec::new()) };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    node
}

/// Sends `body` to `/mcp` accepting an event stream; the open connection.
fn open(at: SocketAddr, body: &str) -> TcpStream {
    let mut s = TcpStream::connect(at).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let head = format!(
        "POST /mcp HTTP/1.1\r\nHost: h\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    s.write_all(head.as_bytes()).unwrap();
    s.write_all(body.as_bytes()).unwrap();
    s
}

/// The head and the chunks of a chunked answer, read to its last chunk.
fn chunks(mut s: TcpStream) -> (String, Vec<Vec<u8>>) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 65536];
    while !raw.ends_with(b"\r\n0\r\n\r\n") {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "closed before the last chunk: {}", String::from_utf8_lossy(&raw));
        raw.extend_from_slice(&buf[..n]);
    }
    let at = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..at]).into_owned();
    let mut rest = &raw[at + 4..];
    let mut out = Vec::new();
    loop {
        let line = rest.windows(2).position(|w| w == b"\r\n").unwrap();
        let size = usize::from_str_radix(std::str::from_utf8(&rest[..line]).unwrap(), 16).unwrap();
        if size == 0 {
            return (head, out);
        }
        out.push(rest[line + 2..line + 2 + size].to_vec());
        rest = &rest[line + 2 + size + 2..];
    }
}

/// The JSON messages of the `data:` lines of an event-stream piece.
fn messages(piece: &[u8]) -> Vec<Value> {
    String::from_utf8_lossy(piece)
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .map(|d| parse(d.trim().as_bytes()).unwrap())
        .collect()
}

#[test]
fn each_batch_read_sends_its_progress_before_the_result() {
    let node = start();
    let at = node.http_addrs()[0];
    let mut wire = Client::new(&node);
    for part in 0..3 {
        let batch = docs(part * 1000..(part + 1) * 1000);
        assert!(ok(&wire.run("db", Document::new().with("insert", "c").with("documents", batch))));
    }
    let set = Document::new().with(
        "setClusterParameter",
        Document::new().with("mcpOptions", Document::new().with("maxDocuments", 5000)),
    );
    assert!(ok(&wire.run("admin", set)));
    let args = format!(
        r#"{{"connectionId":"{}.stateless","database":"db","collection":"c","limit":3000}}"#,
        node.node_id().0
    );
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{{{META},"name":"find","arguments":{args}}}}}"#
    );
    let (head, pieces) = chunks(open(at, &body));
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    assert!(head.to_ascii_lowercase().contains("content-type: text/event-stream"), "{head}");
    let sent: Vec<Vec<Value>> =
        pieces.iter().map(|p| messages(p)).filter(|m| !m.is_empty()).collect();
    // Each message went in its own piece, as it came: the progress ones first, the result last.
    assert!(sent.iter().all(|m| m.len() == 1), "{sent:?}");
    let all: Vec<&Value> = sent.iter().map(|m| &m[0]).collect();
    let (last, notes) = all.split_last().unwrap();
    assert!(notes.len() >= 2, "{all:?}");
    let mut before = 0;
    for n in notes {
        assert_eq!(n.get("method").and_then(Value::as_str), Some("notifications/progress"));
        let p = n.get("params").unwrap();
        assert_eq!(p.get("progressToken").and_then(Value::as_str), Some("p1"));
        let read = p.get("progress").and_then(Value::as_i64).unwrap();
        assert!(read > before, "{all:?}");
        before = read;
    }
    assert_eq!(before, 3000);
    let all_docs = wire.run("db", Document::new().with("find", "c").with("batchSize", 5000));
    assert_eq!(batch(&all_docs, "firstBatch").len(), 3000, "the call wrote nothing");
    let result = last.get("result").and_then(|r| r.get("structuredContent")).unwrap();
    assert_eq!(result.get("queryResultsCount").and_then(Value::as_i64), Some(3000));
    drop(node.stop());
}

#[test]
fn a_cancelled_call_has_its_operation_killed() {
    let node = start();
    let at = node.http_addrs()[0];
    let mut wire = Client::new(&node);
    assert!(ok(
        &wire.run("db", Document::new().with("insert", "c").with("documents", docs(0..200)))
    ));
    // Each document costs the filter a long loop: the call runs for a long time.
    let filter = r#"{"$where":"function() { for (var i = 0; i < 2000000; i++) {} return true; }"}"#;
    let args = format!(
        r#"{{"connectionId":"{}.stateless","database":"db","collection":"c","filter":{filter}}}"#,
        node.node_id().0
    );
    let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}"#;
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":"long","method":"tools/call","params":{{{meta},"name":"find","arguments":{args}}}}}"#
    );
    let started = Instant::now();
    let call = std::thread::spawn(move || {
        let mut s = open(at, &body);
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).unwrap();
        raw
    });
    // Once the operation runs, `notifications/cancelled` names its request.
    let mut running = || {
        let r = wire.run(
            "admin",
            Document::new()
                .with("currentOp", 1)
                .with("command.comment", Document::new().with("$regex", "^mcp:")),
        );
        matches!(r.get("inprog"), Some(teseladb_bson::Value::Array(a)) if !a.is_empty())
    };
    until("the call running", || running().then_some(()));
    let cancel = format!(
        r#"{{"jsonrpc":"2.0","method":"notifications/cancelled","params":{{{meta},"requestId":"long"}}}}"#
    );
    let mut s = open(at, &cancel);
    let mut answer = Vec::new();
    s.read_to_end(&mut answer).unwrap();
    assert!(
        String::from_utf8_lossy(&answer).starts_with("HTTP/1.1 202"),
        "{}",
        String::from_utf8_lossy(&answer)
    );
    let raw = call.join().unwrap();
    let text = String::from_utf8_lossy(&raw);
    assert!(text.contains("interrupted") || text.contains("Interrupted"), "{text}");
    assert!(started.elapsed() < READY);
    drop(node.stop());
}
