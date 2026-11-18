//! `Idempotency-Key` on three nodes (019 T018, FR-010): the same write sent again, to another
//! node, after its range moved under a new leader and after the node that ran it stopped (its
//! ranges elect another leader), answers the first answer and applies once; another request with
//! the key gets `422`.

#[cfg(target_os = "linux")]
#[path = "common/procs.rs"]
mod procs;

#[cfg(target_os = "linux")]
use std::io::{Read, Write};
#[cfg(target_os = "linux")]
use std::net::{SocketAddr, TcpStream};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
use teseladb_bson::{Document, Value, extjson};
#[cfg(target_os = "linux")]
use teseladb_http::http1::{Limits, Request, Response, ResponseReader};
#[cfg(target_os = "linux")]
use teseladb_http::server::Settings;
#[cfg(target_os = "linux")]
use teseladb_server::client_listener::ClientConfig;
#[cfg(target_os = "linux")]
use teseladb_server::http_listener::HttpPortConfig;
#[cfg(target_os = "linux")]
use teseladb_server::node::NodeConfig;
#[cfg(target_os = "linux")]
use teseladb_server::procs::{NodeProc, start_self};

#[cfg(target_os = "linux")]
fn main() {
    procs::main(
        &[(
            "a_write_sent_again_with_its_key_applies_once_across_nodes_and_leaders",
            a_write_sent_again_with_its_key_applies_once_across_nodes_and_leaders,
        )],
        &[("node", |a| node_config(a.join.clone()))],
    );
}

#[cfg(target_os = "linux")]
const READY: Duration = Duration::from_secs(30);
/// What a node may take to say how its replicas are.
#[cfg(target_os = "linux")]
const ANSWER: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const C: &str = "/v1/databases/app/collections/c";

/// A node process of [`node_config`], ready.
#[cfg(target_os = "linux")]
fn start(join: Vec<String>) -> NodeProc {
    let node = start_self("node", &join, None, &[]);
    assert!(node.wait_ready(READY));
    node
}

/// A node of two cores with the HTTP, administration and clients' ports.
#[cfg(target_os = "linux")]
fn node_config(join: Vec<String>) -> NodeConfig {
    let http = HttpPortConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        tls: None,
        settings: Settings::default(),
    };
    let client = ClientConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        ..ClientConfig::new(Vec::new())
    };
    NodeConfig {
        cores: Some(2),
        join,
        http: Some(http),
        admin_listen: Some(SocketAddr::from(([127, 0, 0, 1], 0))),
        client: Some(client),
        ..NodeConfig::default()
    }
}

#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn doc(r: &Response) -> Document {
    extjson::parse(std::str::from_utf8(&r.body).unwrap()).unwrap_or_default()
}

/// `f` until it answers below 500, for at most [`READY`].
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
fn counts(at: SocketAddr) -> Vec<Value> {
    let r =
        send(at, "POST", &format!("{C}/find"), r#"{"sort":{"v":1},"projection":{"_id":0}}"#, None);
    let d = doc(&r);
    match d.get("cursor").and_then(Value::as_document).and_then(|c| c.get("firstBatch")) {
        Some(Value::Array(b)) => b.clone(),
        _ => panic!("{d:?}"),
    }
}

#[cfg(target_os = "linux")]
fn a_write_sent_again_with_its_key_applies_once_across_nodes_and_leaders() {
    let first = start(Vec::new());
    let seed = vec![first.peer_addr().to_string()];
    let others: Vec<NodeProc> = (0..2).map(|_| start(seed.clone())).collect();
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
    // A write the reference server does not retry runs as a transaction of the key's session.
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
    // The node that ran both writes stops once every range has a voter on each node: the two
    // left elect new leaders, and the retries sent to them still answer the first answers.
    assert!(replicated(&first), "{}", replica_report(&first, &others));
    assert!(first.stop().success());
    let again = settled(|| send(at[2], "POST", &format!("{C}/insertOne"), one, Some("k-insert")));
    assert_eq!((again.status, doc(&again)), (a.status, doc(&a)));
    let again = settled(|| send(at[1], "POST", &format!("{C}/updateMany"), many, Some("k-many")));
    assert_eq!((again.status, doc(&again)), (m.status, doc(&m)));
    assert_eq!(counts(at[2]), want);
    assert!(others.into_iter().map(NodeProc::stop).all(|s| s.success()));
}

/// Whether Group 0 and each range `node` routes have a voter on each of the three nodes, waiting
/// at most [`READY`].
#[cfg(target_os = "linux")]
fn replicated(node: &NodeProc) -> bool {
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

/// What Group 0 and each node say of the ranges, for a cluster whose ranges did not get their
/// three voters in time: the descriptors, each node's routes and each replica's configuration.
#[cfg(target_os = "linux")]
fn replica_report(first: &NodeProc, others: &[NodeProc]) -> String {
    let (code, descs, err) = admin_run(first, &["--format", "json", "ranges"]);
    let mut out = format!("ranges of Group 0 (exit {code}): {descs}{err}");
    for (i, node) in std::iter::once(first).chain(others).enumerate() {
        for e in node.ranges() {
            let status = node.range_status(e.group, ANSWER);
            out.push_str(&format!("\nnode {}: {e:?}\n  its replica: {status:?}", i + 1));
        }
    }
    out
}

/// `teseladb admin` on `node`'s administration port: its exit code, its output and its errors.
#[cfg(target_os = "linux")]
fn admin_run(node: &NodeProc, args: &[&str]) -> (i32, String, String) {
    let none = std::env::temp_dir().join("teseladb-idempotency-no-pki");
    let mut all = vec!["--node".to_owned(), node.admin_addr().unwrap().to_string()];
    all.extend(["--dbpath".to_owned(), none.to_str().unwrap().to_owned()]);
    all.extend(args.iter().map(|a| (*a).to_owned()));
    let o = teseladb_server::admin::parse(&all).unwrap();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = teseladb_server::admin::run(&o, &mut out, &mut err);
    (code, String::from_utf8(out).unwrap(), String::from_utf8_lossy(&err).into_owned())
}

/// `teseladb admin` on `node`'s administration port: its output.
#[cfg(target_os = "linux")]
fn admin(node: &NodeProc, args: &[&str]) -> String {
    let (code, out, err) = admin_run(node, args);
    assert_eq!(code, 0, "{err}");
    out
}

/// [`admin`] for a change of a range, asked again while Group 0 answers that the range is busy
/// with another change (a replica it is still adding, say), for at most [`READY`].
#[cfg(target_os = "linux")]
fn admin_once_free(node: &NodeProc, args: &[&str]) -> String {
    let busy = format!("{:?}", teseladb_meta::group0::Group0Error::RangeBusy);
    let end = Instant::now() + READY;
    loop {
        let (code, out, err) = admin_run(node, args);
        if code == 0 || !err.contains(&busy) || Instant::now() >= end {
            assert_eq!(code, 0, "{err}");
            return out;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Moves each range of collection `ns` to the other core of its node (007 FR-011): its Raft
/// group starts again there and elects a new leader.
#[cfg(target_os = "linux")]
fn relocate(node: &NodeProc, ns: &str) {
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
        admin_once_free(node, &["range", "relocate", &id, "--core", &core.to_string()]);
    }
}

/// The tests run on Linux only (D-177): elsewhere the program does nothing.
#[cfg(not(target_os = "linux"))]
fn main() {}
