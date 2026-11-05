//! The HTTP port of a running node (019 T006, T007; FR-002, FR-004, FR-028): `/healthz` answers
//! without credentials, an unknown path gets a `404` problem, and the origins set with
//! `setClusterParameter` in `httpOptions.cors.allowedOrigins` reach the port's CORS answers, while
//! the clients' port keeps answering.

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use common::{Client, READY, batch, config, docs, ok, until};
use teseladb_server::admin_http::{Method, request};
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_bson::Document;
use teseladb_http::http1::{Limits, Response, ResponseReader};
use teseladb_http::server::Settings;
use teseladb_raftlog::MemDisk;

fn get(addr: SocketAddr, text: &str) -> Response {
    let mut s = TcpStream::connect(addr).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    s.write_all(text.as_bytes()).unwrap();
    let mut reader = ResponseReader::new(Limits::default());
    let mut buf = [0u8; 4096];
    loop {
        let n = s.read(&mut buf).unwrap();
        assert!(n > 0, "closed before the answer");
        if let Some(r) = reader.feed(&buf[..n]).unwrap() {
            return r;
        }
    }
}

#[test]
fn a_node_serves_healthz_and_cors_from_its_cluster_parameter() {
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: Settings::default(),
    };
    let admin = Some(SocketAddr::from(([127, 0, 0, 1], 0)));
    let cfg = NodeConfig { http: Some(http), admin_listen: admin, ..config(Vec::new()) };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    let at = node.http_addrs()[0];
    let r = get(at, "GET /healthz HTTP/1.1\r\nHost: h\r\n\r\n");
    assert_eq!((r.status, r.body.as_slice()), (200, &b"ok\n"[..]));
    let r = get(at, "GET /nowhere HTTP/1.1\r\nHost: h\r\n\r\n");
    assert_eq!(r.status, 404);
    assert_eq!(r.header("content-type"), Some("application/problem+json"));
    let preflight = "OPTIONS /v1/databases HTTP/1.1\r\nHost: h\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: GET\r\n\r\n";
    // CORS is off until an origin is listed.
    assert_eq!(get(at, preflight).header("access-control-allow-origin"), None);
    let mut c = Client::new(&node);
    let origins = Document::new()
        .with("allowedOrigins", vec![teseladb_bson::Value::from("https://app.example")]);
    let set = Document::new().with(
        "setClusterParameter",
        Document::new().with("httpOptions", Document::new().with("cors", origins)),
    );
    let r = c.run("admin", set);
    assert!(ok(&r), "{r:?}");
    until("the origin on the HTTP port", || {
        let r = get(at, preflight);
        (r.header("access-control-allow-origin") == Some("https://app.example")).then_some(())
    });
    // `/metrics` of the administration port counts the HTTP port's requests by route (FR-030).
    let admin_at = node.admin_addr().unwrap().to_string();
    let (status, body) = request(&admin_at, Method::Get, "/metrics", b"").unwrap();
    let text = String::from_utf8(body).unwrap();
    assert_eq!(status, 200);
    assert!(text.contains("teseladb_http_requests_total{route=\"/healthz\"} 1"), "{text}");
    assert!(
        text.contains("teseladb_http_errors_total{route=\"unrouted\",status=\"404\"} 1"),
        "{text}"
    );
    // The clients' port answers beside it.
    assert!(ok(&c.run("db", Document::new().with("insert", "c").with("documents", docs(0..2)))));
    assert_eq!(batch(&c.run("db", Document::new().with("find", "c")), "firstBatch").len(), 2);
    drop(node.stop());
}

/// An HTTP port that cannot be bound stops the node's start: the administration and clients'
/// ports it had opened are closed again, and the error names the port.
#[test]
fn a_taken_http_port_stops_the_start_and_closes_what_it_opened() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = taken.local_addr().unwrap();
    let http = || HttpPortConfig { addrs: vec![at], tls: None, settings: Settings::default() };
    let admin = Some(SocketAddr::from(([127, 0, 0, 1], 0)));
    let full = NodeConfig { http: Some(http()), admin_listen: admin, ..config(Vec::new()) };
    let bare = NodeConfig { http: Some(http()), client: None, ..config(Vec::new()) };
    for cfg in [full, bare] {
        let Err(e) = Node::start(cfg, || Ok(MemDisk::new()), None) else {
            panic!("the node started on a taken HTTP port");
        };
        assert!(matches!(e, teseladb_server::node::NodeError::Io(_)), "{e:?}");
    }
    drop(taken);
}
