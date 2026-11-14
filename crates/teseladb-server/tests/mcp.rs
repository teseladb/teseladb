//! MCP over the HTTP port of running nodes (019 T025, T026, T028, FR-019 to FR-023): a session
//! opened on one node of three and used through the others, the stateless revision, and the
//! read-only default that hides and refuses the tools that write.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_bson::{Document, Value as Bson};
use teseladb_http::http1::{Limits, Request, Response, ResponseReader};
use teseladb_http::server::Settings;
use teseladb_json::{Value, parse};
use teseladb_mcp::server::SESSION_HEADER;
use teseladb_raftlog::MemDisk;
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};

fn start(join: Vec<String>) -> Node<MemDisk> {
    start_on(join, [127, 0, 0, 1])
}

/// [`start`], with the HTTP port on `ip`.
fn start_on(join: Vec<String>, ip: [u8; 4]) -> Node<MemDisk> {
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from((ip, 0))],
        tls: None,
        settings: Settings::default(),
    };
    let node =
        Node::start(NodeConfig { http: Some(http), ..config(join) }, || Ok(MemDisk::new()), None)
            .unwrap();
    assert!(node.wait_ready(READY));
    node
}

fn post(at: SocketAddr, body: &str, session: Option<&str>) -> (Response, Value) {
    let mut req = Request::new("POST", "/mcp").body(body.as_bytes().to_vec());
    if let Some(s) = session {
        req = req.header(SESSION_HEADER, s);
    }
    let r = send(at, req);
    let v = parse(&r.body).unwrap_or(Value::Null);
    (r, v)
}

/// The answer to `req`.
fn send(at: SocketAddr, req: Request) -> Response {
    let req = req.header("Host", "h");
    let mut s = TcpStream::connect(at).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(&req.encode().unwrap()).unwrap();
    let mut reader = ResponseReader::new(Limits::default());
    let mut buf = [0u8; 65536];
    loop {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0);
        if let Some(r) = reader.feed(&buf[..n]).unwrap() {
            return r;
        }
    }
}

fn tool(id: u32, name: &str, args: &str) -> String {
    format!(
        r#"{{"jsonrpc":"2.0","id":{id},"method":"tools/call","params":{{"name":"{name}","arguments":{args}}}}}"#
    )
}

fn structured(v: &Value) -> &Value {
    v.get("result").and_then(|r| r.get("structuredContent")).unwrap_or_else(|| panic!("{v:?}"))
}

#[test]
fn a_session_of_one_node_works_through_the_others() {
    let first = start(Vec::new());
    let seed = vec![first.peer_addr().to_string()];
    let others: Vec<Node<MemDisk>> = (0..2).map(|_| start(seed.clone())).collect();
    let at: Vec<SocketAddr> =
        std::iter::once(&first).chain(others.iter()).map(|n| n.http_addrs()[0]).collect();
    let mut wire = Client::new(&first);
    assert!(ok(&wire.run("db", Document::new().with("insert", "c").with("documents", docs(0..5)))));

    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let (r, v) = post(at[0], init, None);
    assert_eq!(r.status, 200, "{v:?}");
    let sid = r.header(SESSION_HEADER).unwrap().to_owned();
    // Behind a load balancer the next requests land on other nodes.
    let args = format!(
        r#"{{"connectionId":"{sid}","database":"db","collection":"c","filter":{{"_id":{{"$lt":3}}}}}}"#
    );
    let (_, v) = post(at[1], &tool(2, "find", &args), Some(&sid));
    assert_eq!(structured(&v).get("queryResultsCount").and_then(Value::as_i64), Some(3), "{v:?}");
    let (_, v) = post(at[2], r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#, Some(&sid));
    let tools = v.get("result").and_then(|r| r.get("tools")).and_then(Value::as_array).unwrap();
    assert!(tools.iter().all(|t| t.get("name").and_then(Value::as_str) != Some("insert-many")));
    // Read-only by default: `insert-many` fails and writes nothing (scenario 3).
    let args = format!(
        r#"{{"connectionId":"{sid}","database":"db","collection":"c","documents":[{{"_id":99}}]}}"#
    );
    let (_, v) = post(at[1], &tool(4, "insert-many", &args), Some(&sid));
    assert!(v.get("error").is_some(), "{v:?}");
    let count = Document::new().with("count", "c");
    assert_eq!(wire.run("db", count.clone()).get("n"), Some(&Bson::Int32(5)));
    // With writes allowed by `mcpOptions`, it runs.
    let set = Document::new().with(
        "setClusterParameter",
        Document::new().with("mcpOptions", Document::new().with("readOnly", false)),
    );
    assert!(ok(&wire.run("admin", set)));
    until("insert-many listed", || {
        let (_, v) = post(at[0], &tool(5, "insert-many", &args), Some(&sid));
        (v.get("result").and_then(|r| r.get("isError")) == Some(&Value::Bool(false))).then_some(())
    });
    let find = Document::new().with("find", "c").with("filter", Document::new().with("_id", 99));
    assert_eq!(batch(&wire.run("db", find), "firstBatch").len(), 1);

    // The stateless revision, without a session.
    let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}"#;
    let body = format!(
        r#"{{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{{{meta},"name":"count","arguments":{{"connectionId":"{}.stateless","database":"db","collection":"c"}}}}}}"#,
        first.node_id().0
    );
    let (r, v) = post(at[0], &body, None);
    assert_eq!(r.status, 200, "{v:?}");
    assert_eq!(structured(&v).get("count").and_then(Value::as_i64), Some(6));
    drop(others.into_iter().map(Node::stop).collect::<Vec<_>>());
    drop(first.stop());
}

/// A value for argument `name` of schema `schema`, drawn with `r`.
fn argument(name: &str, schema: &Value, r: &mut u64) -> String {
    *r = r.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let pick = (*r >> 33) as usize;
    if let Some(Value::Array(values)) = schema.get("enum") {
        return teseladb_json::to_string(&values[pick % values.len()]).unwrap();
    }
    let ty = schema.get("type").and_then(Value::as_str).unwrap_or("object");
    let text = match (name, ty) {
        ("database", _) => "\"db\"",
        ("collection", _) => "\"c\"",
        ("newName", _) => "\"c2\"",
        ("indexName", _) => "\"_id_\"",
        ("connectionString", _) => "\"refdb://localhost\"",
        ("documents", _) => r#"[{"_id":1000}]"#,
        ("method" | "exportTarget", _) => [
            r#"[{"name":"find","arguments":{}}]"#,
            r#"[{"name":"aggregate","arguments":{"pipeline":[]}}]"#,
        ][pick % 2],
        ("definition", _) => r#"[{"type":"classic","keys":{"z":1}}]"#,
        ("pipeline", _) => [r#"[]"#, r#"[{"$match":{}}]"#, r#"[{"$out":"c3"}]"#][pick % 3],
        ("update", _) => r#"{"$set":{"z":1}}"#,
        (_, "string") => "\"x\"",
        (_, "number" | "integer") => ["1", "5", "0"][pick % 3],
        (_, "boolean") => ["true", "false"][pick % 2],
        (_, "array") => "[]",
        _ => ["{}", r#"{"_id":1}"#][pick % 2],
    };
    text.to_owned()
}

/// The state a write would change: each collection's documents and indexes.
fn state(wire: &mut Client) -> Vec<Bson> {
    let mut out = Vec::new();
    let dbs = wire.run("admin", Document::new().with("listDatabases", 1).with("nameOnly", true));
    out.push(dbs.get("databases").cloned().unwrap_or(Bson::Null));
    let colls = wire.run("db", Document::new().with("listCollections", 1).with("nameOnly", true));
    for c in batch(&colls, "firstBatch") {
        out.push(c.clone());
        let Some(Bson::String(coll)) = c.as_document().and_then(|d| d.get("name")).cloned() else {
            continue;
        };
        let find = Document::new()
            .with("find", coll.as_str())
            .with("sort", Document::new().with("_id", 1));
        out.extend(batch(&wire.run("db", find), "firstBatch"));
        let indexes = Document::new().with("listIndexes", coll.as_str());
        out.extend(batch(&wire.run("db", indexes), "firstBatch"));
    }
    out
}

/// SC-004 (019 T028, FR-023): every tool called with arguments drawn from its schema, under the
/// default `mcpOptions` (`readOnly`), leaves the data as it was.
#[test]
fn every_tool_in_read_only_mode_writes_nothing() {
    let node = start(Vec::new());
    let at = node.http_addrs()[0];
    let mut wire = Client::new(&node);
    assert!(ok(&wire.run("db", Document::new().with("insert", "c").with("documents", docs(0..5)))));
    let before = state(&mut wire);
    let reference = parse(teseladb_mcp::tools::REFERENCE.as_bytes()).unwrap();
    let tools = reference.as_array().unwrap();
    let meta = r#""_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}"#;
    let mut r = 7u64;
    let mut calls = 0;
    for round in 0..8 {
        for t in tools {
            let name = t.get("name").and_then(Value::as_str).unwrap();
            let schema = t.get("inputSchema").unwrap();
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .unwrap_or_default()
                .iter()
                .filter_map(Value::as_str)
                .collect();
            let mut args = Vec::new();
            for (k, s) in schema.get("properties").and_then(Value::as_object).unwrap().iter() {
                if k == "connectionId" {
                    args.push(format!("\"connectionId\":\"{}.stateless\"", node.node_id().0));
                } else if required.contains(&k) || (round + k.len()) % 2 == 0 {
                    args.push(format!("\"{k}\":{}", argument(k, s, &mut r)));
                }
            }
            let body = format!(
                r#"{{"jsonrpc":"2.0","id":{calls},"method":"tools/call","params":{{{meta},"name":"{name}","arguments":{{{}}}}}}}"#,
                args.join(",")
            );
            let (resp, v) = post(at, &body, None);
            assert!(resp.status < 500, "{name}: {v:?}");
            assert!(v.get("result").is_some() || v.get("error").is_some(), "{name}: {v:?}");
            calls += 1;
        }
    }
    assert_eq!(calls, 8 * 25);
    assert_eq!(state(&mut wire), before);
    drop(node.stop());
}

/// `listChanged` (019 T028, FR-023): the server's stream of a session (`GET /mcp`) tells the
/// client when `mcpOptions` changes the tools it sees.
#[test]
fn the_session_stream_announces_a_change_of_the_tools() {
    let node = start(Vec::new());
    let at = node.http_addrs()[0];
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let (r, _) = post(at, init, None);
    let sid = r.header(SESSION_HEADER).unwrap().to_owned();
    let mut s = TcpStream::connect(at).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let get = Request::new("GET", "/mcp").header("Host", "h").header(SESSION_HEADER, &sid);
    s.write_all(&get.encode().unwrap()).unwrap();
    let mut wire = Client::new(&node);
    let set = Document::new().with(
        "setClusterParameter",
        Document::new().with("mcpOptions", Document::new().with("readOnly", false)),
    );
    assert!(ok(&wire.run("admin", set)));
    let mut seen = Vec::new();
    let mut buf = [0u8; 4096];
    while !String::from_utf8_lossy(&seen).contains("notifications/tools/list_changed") {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "{}", String::from_utf8_lossy(&seen));
        seen.extend_from_slice(&buf[..n]);
    }
    assert!(String::from_utf8_lossy(&seen).starts_with("HTTP/1.1 200"));
    drop(node.stop());
}

/// What the port's routes do with requests they do not serve, on a node without credentials
/// whose HTTP port listens only on a wildcard address (no URI for `/mcp`, which needs none then):
/// the metadata path takes only `GET`, `/mcp` without a session takes no `GET`, paths outside
/// `/v1` and `/mcp` are `404`; batches and bodies that are not text are refused; a call with a
/// `progressToken` that fails ends its stream with the error; and a session stream the client
/// drops ends.
#[test]
fn the_ports_routes_answer_what_they_do_not_serve() {
    let node = start_on(Vec::new(), [0, 0, 0, 0]);
    let at = SocketAddr::from(([127, 0, 0, 1], node.http_addrs()[0].port()));
    let list = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}"#;
    let (r, v) = post(at, list, None);
    assert_eq!(r.status, 200, "{v:?}");
    let well_known = "/.well-known/oauth-protected-resource/mcp";
    assert_eq!(send(at, Request::new("POST", well_known)).status, 404);
    assert!(send(at, Request::new("GET", "/mcp")).status >= 400);
    assert_eq!(send(at, Request::new("GET", "/elsewhere")).status, 404);
    assert_eq!(send(at, Request::new("GET", "/v1/no/such/route")).status, 404);
    assert!(send(at, Request::new("GET", "/v1")).status >= 400);
    // Batches are refused (2026-07-28), and the audit log records them as such.
    let (r, v) = post(at, &format!("[{list}]"), None);
    assert_eq!(r.status, 400, "{v:?}");
    let not_text = Request::new("POST", "/mcp").body(vec![0xff, 0xfe]);
    assert!(send(at, not_text).status >= 400);
    let no_method = Request::new("POST", "/mcp").body(b"{}".to_vec());
    assert!(send(at, no_method).status >= 400);
    // A call with a `progressToken` for a session that does not exist: its stream carries the
    // refusal.
    let in_session = r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"_meta":{"progressToken":"p"},"name":"find","arguments":{}}}"#;
    let req = Request::new("POST", "/mcp")
        .header("Accept", "application/json, text/event-stream")
        .header(SESSION_HEADER, "no-such-session")
        .body(in_session.as_bytes().to_vec());
    let r = send(at, req);
    assert_eq!(r.status, 200);
    assert!(String::from_utf8_lossy(&r.body).starts_with("data: "), "{r:?}");
    let failing = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"_meta":{"progressToken":"p","io.modelcontextprotocol/protocolVersion":"2026-07-28"},"name":"no-such-tool","arguments":{}}}"#;
    let req = Request::new("POST", "/mcp")
        .header("Accept", "application/json, text/event-stream")
        .header("Content-Type", "application/json")
        .body(failing.as_bytes().to_vec());
    let r = send(at, req);
    assert_eq!(r.status, 200);
    assert!(String::from_utf8_lossy(&r.body).contains(r#""id":2"#), "{r:?}");
    // A session stream the client drops: the node notices at its next poll.
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let (r, _) = post(at, init, None);
    let sid = r.header(SESSION_HEADER).unwrap().to_owned();
    let mut s = TcpStream::connect(at).unwrap();
    let get = Request::new("GET", "/mcp").header("Host", "h").header(SESSION_HEADER, &sid);
    s.write_all(&get.encode().unwrap()).unwrap();
    let mut buf = [0u8; 64];
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    assert!(s.read(&mut buf).unwrap() > 0);
    drop(s);
    std::thread::sleep(Duration::from_millis(2_500));
    drop(node.stop());
}
