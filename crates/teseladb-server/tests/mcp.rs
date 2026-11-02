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
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
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
    let mut req = Request::new("POST", "/mcp").header("Host", "h").body(body.as_bytes().to_vec());
    if let Some(s) = session {
        req = req.header(SESSION_HEADER, s);
    }
    let mut s = TcpStream::connect(at).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    s.write_all(&req.encode().unwrap()).unwrap();
    let mut reader = ResponseReader::new(Limits::default());
    let mut buf = [0u8; 65536];
    loop {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0);
        if let Some(r) = reader.feed(&buf[..n]).unwrap() {
            let v = parse(&r.body).unwrap_or(Value::Null);
            return (r, v);
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
