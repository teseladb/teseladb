//! The clients' port on io_uring rings (012 T033, FR-024), on Linux: many clients over few rings,
//! requests that arrive in pieces, closes, pipelined requests whose replies the client reads late
//! (backpressure), the awaitable `hello` with exhaust, the connection limit and faults.
#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use teseladb_server::client_listener::{
    ClientConfig, ClientListener, DEFAULT_MAX_CONNECTIONS, NodeState,
};
use teseladb_server::tls_port::{TlsMode, TlsPort};
use teseladb_wire::dispatch::{ClusterTime, NodeInfo, Topology};
use teseladb_wire::msg::{HEADER_LEN, Header, OpMsg, flags, opcode};
use teseladb_bson::{Document, RawDoc, Value};

const PID: [u8; 12] = [9; 12];

struct Fixed;

impl NodeState for Fixed {
    fn info(&self) -> NodeInfo {
        NodeInfo {
            topology: Topology { process_id: PID, counter: 0 },
            cluster_time: ClusterTime { time: 5, increment: 1 },
            now_ms: 1,
            keys: None,
            replica_set: None,
        }
    }
}

fn listener(max_connections: usize) -> ClientListener {
    let cfg = ClientConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        max_connections,
        rings: Some(2),
        ..ClientConfig::new(Vec::new())
    };
    let l = ClientListener::spawn(&cfg, Arc::new(Fixed)).unwrap();
    assert!(l.on_rings(), "the port runs on rings with io_uring");
    l
}

fn connect(l: &ClientListener) -> TcpStream {
    let s = TcpStream::connect(l.addrs()[0]).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s
}

fn msg(request_id: i32, flag_bits: u32, cmd: &Document) -> Vec<u8> {
    OpMsg::encode(request_id, 0, flag_bits, &cmd.to_bytes().unwrap())
}

fn send(s: &mut TcpStream, request_id: i32, flag_bits: u32, cmd: &Document) {
    s.write_all(&msg(request_id, flag_bits, cmd)).unwrap();
}

fn recv(s: &mut TcpStream) -> Option<(Header, u32, Document)> {
    let mut h = [0u8; HEADER_LEN];
    s.read_exact(&mut h).ok()?;
    let header = Header::parse(&h);
    let mut rest = vec![0u8; header.length as usize - HEADER_LEN];
    s.read_exact(&mut rest).ok()?;
    let mut whole = h.to_vec();
    whole.extend_from_slice(&rest);
    let bits = u32::from_le_bytes(whole[16..20].try_into().unwrap());
    let doc = Document::from(RawDoc::from_bytes(&whole[21..]).unwrap());
    Some((header, bits, doc))
}

fn closed(s: &mut TcpStream) -> bool {
    matches!(s.read(&mut [0u8; 1]), Ok(0) | Err(_))
}

fn hello() -> Document {
    Document::new().with("hello", 1).with("$db", "admin")
}

fn ping() -> Document {
    Document::new().with("ping", 1).with("$db", "admin")
}

fn wait_open(l: &ClientListener, n: usize) {
    let end = Instant::now() + Duration::from_secs(10);
    while l.open_connections() != n {
        assert!(Instant::now() < end, "{} open, not {n}", l.open_connections());
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn many_clients_on_two_rings_each_get_their_replies_in_order() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let addr = l.addrs()[0];
    let clients: Vec<_> = (0..64)
        .map(|k| {
            thread::spawn(move || {
                let mut s = TcpStream::connect(addr).unwrap();
                s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                send(&mut s, 1, 0, &hello());
                let (_, _, d) = recv(&mut s).unwrap();
                let Some(Value::Int32(id)) = d.get("connectionId") else { panic!("no id") };
                for r in 2..40 {
                    send(&mut s, r, 0, &ping());
                    let (h, _, d) = recv(&mut s).unwrap();
                    assert_eq!(h.response_to, r, "client {k}");
                    assert_eq!(d.get("ok"), Some(&Value::Double(1.0)));
                }
                *id
            })
        })
        .collect();
    let mut ids: Vec<i32> = clients.into_iter().map(|c| c.join().unwrap()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 64, "every connection has its own id");
    wait_open(&l, 0);
    l.stop();
}

#[test]
fn a_request_that_arrives_in_pieces_is_answered_once_whole() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let mut s = connect(&l);
    s.set_nodelay(true).unwrap();
    let m = msg(5, 0, &ping());
    for b in &m {
        s.write_all(&[*b]).unwrap();
        thread::sleep(Duration::from_millis(1));
    }
    // Two requests in one write, the second cut short, then its end.
    let mut two = msg(6, 0, &ping());
    let second = msg(7, 0, &hello());
    two.extend_from_slice(&second[..10]);
    s.write_all(&two).unwrap();
    assert_eq!(recv(&mut s).unwrap().0.response_to, 5);
    assert_eq!(recv(&mut s).unwrap().0.response_to, 6);
    s.write_all(&second[10..]).unwrap();
    assert_eq!(recv(&mut s).unwrap().0.response_to, 7);
    l.stop();
}

#[test]
fn closes_from_either_side_free_the_connection() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let mut a = connect(&l);
    send(&mut a, 1, 0, &hello());
    assert!(recv(&mut a).is_some());
    let b = connect(&l);
    wait_open(&l, 2);
    drop(b);
    wait_open(&l, 1);
    // A request the port refuses closes its connection after the replies before it.
    let mut bad = msg(2, 0, &ping());
    bad.extend_from_slice(&msg(3, 1 << 5, &ping()));
    a.write_all(&bad).unwrap();
    assert_eq!(recv(&mut a).unwrap().0.response_to, 2);
    assert!(closed(&mut a));
    wait_open(&l, 0);
    l.stop();
}

#[test]
fn requests_that_pile_up_stop_the_reading_and_all_get_their_replies() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let mut s = connect(&l);
    // An awaitable `hello` holds the connection's requests for a second, while about 40 MB of
    // requests come behind it: past the mark the ring stops reading the connection, so the
    // client's writes stall in the full socket instead of piling up in the node.
    let tv = Document::new().with("processId", Value::ObjectId(PID)).with("counter", 0i64);
    let wait = hello().with("topologyVersion", tv).with("maxAwaitTimeMS", 1000);
    send(&mut s, 1, 0, &wait);
    let big = "x".repeat(16 * 1024);
    let n = 2400;
    let written = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let mut w = s.try_clone().unwrap();
        let written = written.clone();
        thread::spawn(move || {
            for r in 2..=n {
                let cmd = Document::new().with("ping", 1).with("comment", big.as_str());
                w.write_all(&msg(r, 0, &cmd.with("$db", "a"))).unwrap();
            }
            written.store(true, std::sync::atomic::Ordering::SeqCst);
        })
    };
    thread::sleep(Duration::from_millis(600));
    assert!(!written.load(std::sync::atomic::Ordering::SeqCst), "the writes stall");
    for r in 1..=n {
        let (h, _, d) = recv(&mut s).unwrap();
        assert_eq!(h.response_to, r);
        assert_eq!(d.get("ok"), Some(&Value::Double(1.0)));
    }
    writer.join().unwrap();
    l.stop();
}

#[test]
fn awaitable_hello_with_exhaust_streams_from_the_ring() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let tv = Document::new().with("processId", Value::ObjectId(PID)).with("counter", 0i64);
    let wait = hello().with("topologyVersion", tv).with("maxAwaitTimeMS", 200);
    let mut c = connect(&l);
    let t = Instant::now();
    send(&mut c, 1, 0, &wait);
    send(&mut c, 2, 0, &ping());
    let (h, bits, _) = recv(&mut c).unwrap();
    assert!(t.elapsed() >= Duration::from_millis(190));
    assert_eq!((h.response_to, bits), (1, 0));
    assert_eq!(recv(&mut c).unwrap().0.response_to, 2);
    let mut e = connect(&l);
    send(&mut e, 11, flags::EXHAUST_ALLOWED, &wait);
    let mut previous = 11;
    for _ in 0..3 {
        let (h, bits, _) = recv(&mut e).unwrap();
        assert_eq!(bits, flags::MORE_TO_COME);
        assert_eq!(h.response_to, previous);
        previous = h.request_id;
    }
    // A client that goes while its `hello` waits leaves nothing behind.
    drop(e);
    drop(c);
    wait_open(&l, 0);
    l.stop();
}

#[test]
fn the_connection_limit_and_faults_close_only_their_connection() {
    let l = listener(2);
    let mut good = connect(&l);
    send(&mut good, 1, 0, &hello());
    assert!(recv(&mut good).is_some());
    let mut bad = connect(&l);
    let mut third = connect(&l);
    assert!(closed(&mut third), "over --maxConns");
    let mut h = Vec::new();
    Header { length: 48_000_001, request_id: 1, response_to: 0, op_code: opcode::OP_MSG }
        .write(&mut h);
    bad.write_all(&h).unwrap();
    assert!(closed(&mut bad));
    send(&mut good, 2, 0, &ping());
    assert!(recv(&mut good).is_some());
    l.stop();
    assert!(closed(&mut good), "stopping closes the connections");
}

#[test]
fn a_port_with_tls_keeps_a_thread_per_connection() {
    let cfg = ClientConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        rings: Some(2),
        tls: TlsPort::new(TlsMode::AllowTls, None),
        ..ClientConfig::new(Vec::new())
    };
    let l = ClientListener::spawn(&cfg, Arc::new(Fixed)).unwrap();
    assert!(!l.on_rings());
    let plain = ClientListener::spawn(&ClientConfig { rings: None, ..cfg }, Arc::new(Fixed));
    assert!(!plain.unwrap().on_rings());
    l.stop();
}
