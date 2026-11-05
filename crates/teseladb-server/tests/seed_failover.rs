//! Stopping the node the others joined through (005 US3, SC-004; 007 FR-010): once its ranges
//! have a voter on each node, the two that stay elect new leaders for Group 0 and for the ranges
//! and answer reads and writes within the failover budget, without the seed they joined through.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use teseladb_server::client_listener::ClientConfig;
use teseladb_server::http_listener::HttpPortConfig;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_bson::{Document, Value, extjson};
use teseladb_http::http1::{Limits, Request, Response, ResponseReader};
use teseladb_http::server::Settings;
use teseladb_raftlog::MemDisk;

const READY: Duration = Duration::from_secs(30);
/// What a request may wait for its answer before the test counts it as unanswered.
const ANSWER: Duration = Duration::from_secs(5);
/// The time the two nodes left may take to answer reads and writes again: a leader replaced in
/// under a second (005 SC-004) plus the retry within server selection, 2 s (005 FR-019).
const FAILOVER: Duration = Duration::from_secs(3);
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
        client: Some(client),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(READY));
    node
}

/// The answer to one request, or `None` when none comes within [`ANSWER`].
fn send(addr: SocketAddr, method: &str, target: &str, body: &str) -> Option<Response> {
    let mut s = TcpStream::connect_timeout(&addr, ANSWER).ok()?;
    s.set_read_timeout(Some(ANSWER)).ok()?;
    let req = Request::new(method, target).header("Host", "h").body(body.as_bytes().to_vec());
    s.write_all(&req.encode().ok()?).ok()?;
    let mut reader = ResponseReader::new(Limits::default());
    let mut buf = [0u8; 65536];
    loop {
        let n = s.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        if let Some(r) = reader.feed(&buf[..n]).ok()? {
            return Some(r);
        }
    }
}

fn doc(r: &Response) -> Document {
    extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap_or_default()
}

/// The time from `since` until `f` answers `200`, or a panic naming the last answer once
/// [`FAILOVER`] has passed.
fn answered_within(
    since: Instant,
    what: &str,
    mut f: impl FnMut() -> Option<Response>,
) -> Duration {
    loop {
        let r = f();
        if r.as_ref().is_some_and(|r| r.status == 200) {
            return since.elapsed();
        }
        assert!(
            since.elapsed() < FAILOVER,
            "{what}: no answer within {FAILOVER:?}; last {:?}",
            r.map(|r| (r.status, doc(&r)))
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Whether Group 0 and each range `node` routes have a voter on each of the three nodes, waiting
/// at most [`READY`].
fn replicated(node: &Node<MemDisk>) -> bool {
    let end = Instant::now() + READY;
    let three = |v: &[teseladb_types::NodeId]| v.len() == 3;
    while Instant::now() < end {
        let group0 = node.view(ANSWER).is_some_and(|v| three(&v.voters));
        let ranges = node.ranges().iter().all(|e| {
            node.range_status(e.group, ANSWER)
                .is_some_and(|s| three(&s.voters) && s.learners.is_empty())
        });
        if group0 && ranges {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn the_two_nodes_left_answer_reads_and_writes_soon_after_the_seed_stops() {
    let first = start(Vec::new());
    let seed = vec![first.peer_addr().to_string()];
    let others: Vec<Node<MemDisk>> = (0..2).map(|_| start(seed.clone())).collect();
    let at: Vec<SocketAddr> = others.iter().map(|n| n.http_addrs()[0]).collect();
    let made =
        send(first.http_addrs()[0], "POST", "/v1/databases/app/collections", r#"{"name":"c"}"#)
            .unwrap();
    assert_eq!(made.status, 200, "{:?}", doc(&made));
    let w = send(at[0], "POST", &format!("{C}/insertOne"), r#"{"document":{"_id":1}}"#).unwrap();
    assert_eq!(w.status, 200, "{:?}", doc(&w));

    // Every range and Group 0 reach a voter on each node (the balancer looks as soon as a node
    // joins or starts, and again as each change settles).
    assert!(replicated(&first), "{:?}", first.ranges());
    let stopped = Instant::now();
    drop(first.stop());
    for (i, node) in at.iter().enumerate() {
        let id = 10 + i32::try_from(i).unwrap();
        let write = format!(r#"{{"document":{{"_id":{id}}}}}"#);
        answered_within(stopped, "write", || {
            send(*node, "POST", &format!("{C}/insertOne"), &write)
        });
        answered_within(stopped, "read", || {
            send(*node, "POST", &format!("{C}/find"), r#"{"filter":{"_id":1}}"#)
        });
    }
    let r = send(at[1], "POST", &format!("{C}/find"), r#"{"sort":{"_id":1}}"#).unwrap();
    let ids: Vec<Value> = match doc(&r)
        .get("cursor")
        .and_then(Value::as_document)
        .and_then(|c| c.get("firstBatch"))
    {
        Some(Value::Array(b)) => {
            b.iter().filter_map(|d| d.as_document()?.get("_id").cloned()).collect()
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(ids, vec![Value::Int32(1), Value::Int32(10), Value::Int32(11)]);
    drop(others.into_iter().map(Node::stop).collect::<Vec<_>>());
}
