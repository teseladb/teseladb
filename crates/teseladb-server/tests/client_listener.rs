//! The clients' port (006 T009, FR-005): connections spread over the workers, the handshake and
//! `ping` over real sockets, compression, the awaitable `hello` with and without exhaust, the
//! connection limit, faults that close a connection without touching the others, the options
//! `--maxConns` and `--networkMessageCompressors`, and a node that opens the port.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use teseladb_server::cli::{Command, DEFAULT_COMPRESSORS, parse_with};
use teseladb_server::client_listener::{
    ClientConfig, ClientListener, DEFAULT_MAX_CONNECTIONS, NodeClockState, NodePresence, NodeState,
};
use teseladb_server::node::{Node, NodeConfig};
use teseladb_wire::compressed::{self, Compressor};
use teseladb_wire::dispatch::{ClusterTime, NodeInfo, Topology};
use teseladb_wire::msg::{HEADER_LEN, Header, OpMsg, flags, opcode};
use teseladb_bson::{Document, RawDoc, Value};
use teseladb_raftlog::MemDisk;

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

fn config(max_connections: usize) -> ClientConfig {
    ClientConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        max_connections,
        ..ClientConfig::new(Vec::new())
    }
}

fn listener(max: usize) -> ClientListener {
    ClientListener::spawn(&config(max), Arc::new(Fixed)).unwrap()
}

fn connect(l: &ClientListener) -> TcpStream {
    let s = TcpStream::connect(l.addrs()[0]).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s
}

fn send(s: &mut TcpStream, request_id: i32, flag_bits: u32, cmd: &Document) {
    let msg = OpMsg::encode(request_id, 0, flag_bits, &cmd.to_bytes().unwrap());
    s.write_all(&msg).unwrap();
}

/// The next message: its header, its OP_MSG flags and its document (decompressed if needed).
fn recv(s: &mut TcpStream) -> Option<(Header, u32, Document)> {
    let mut h = [0u8; HEADER_LEN];
    s.read_exact(&mut h).ok()?;
    let header = Header::parse(&h);
    let mut rest = vec![0u8; header.length as usize - HEADER_LEN];
    s.read_exact(&mut rest).ok()?;
    let mut whole = h.to_vec();
    whole.extend_from_slice(&rest);
    if header.op_code == opcode::OP_COMPRESSED {
        whole = compressed::decompress(&whole).unwrap().0;
    }
    let bits = u32::from_le_bytes(whole[16..20].try_into().unwrap());
    let doc = Document::from(RawDoc::from_bytes(&whole[21..]).unwrap());
    Some((header, bits, doc))
}

fn closed(s: &mut TcpStream) -> bool {
    let mut b = [0u8; 1];
    matches!(s.read(&mut b), Ok(0) | Err(_))
}

fn hello() -> Document {
    Document::new().with("hello", 1).with("$db", "admin")
}

#[test]
fn handshake_and_ping_over_sockets() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let mut clients: Vec<TcpStream> = (0..6).map(|_| connect(&l)).collect();
    let mut ids = Vec::new();
    for (k, c) in clients.iter_mut().enumerate() {
        send(c, k as i32 + 1, 0, &hello());
        let (h, _, d) = recv(c).unwrap();
        assert_eq!(h.response_to, k as i32 + 1);
        assert_eq!(d.get("msg"), Some(&Value::from("isdbgrid")));
        let Some(Value::Int32(id)) = d.get("connectionId") else { panic!("no connectionId") };
        ids.push(*id);
        send(c, 100, 0, &Document::new().with("ping", 1).with("$db", "admin"));
        assert_eq!(recv(c).unwrap().2.get("ok"), Some(&Value::Double(1.0)));
    }
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 6, "every connection has its own id");
    assert_eq!(l.open_connections(), 6);
    // A moreToCome request gets no reply; the next one does.
    let c = &mut clients[0];
    send(c, 7, flags::MORE_TO_COME, &Document::new().with("ping", 1).with("$db", "admin"));
    send(c, 8, 0, &Document::new().with("ping", 1).with("$db", "admin"));
    assert_eq!(recv(c).unwrap().0.response_to, 8);
    drop(clients);
    let end = Instant::now() + Duration::from_secs(5);
    while l.open_connections() > 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(l.open_connections(), 0);
    l.stop();
}

#[test]
fn compressed_requests_get_compressed_replies() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let mut c = connect(&l);
    let list = vec![Value::from("zstd"), Value::from("zlib")];
    send(&mut c, 1, 0, &hello().with("compression", list));
    let (_, _, d) = recv(&mut c).unwrap();
    assert_eq!(
        d.get("compression"),
        Some(&Value::Array(vec![Value::from("zstd"), Value::from("zlib")]))
    );
    let ping = OpMsg::encode(
        2,
        0,
        0,
        &Document::new().with("ping", 1).with("$db", "a").to_bytes().unwrap(),
    );
    c.write_all(&compressed::compress(&ping, Compressor::Zstd)).unwrap();
    let mut h = [0u8; HEADER_LEN];
    c.read_exact(&mut h).unwrap();
    assert_eq!(Header::parse(&h).op_code, opcode::OP_COMPRESSED);
    l.stop();
}

#[test]
fn awaitable_hello_and_exhaust() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let tv = Document::new().with("processId", Value::ObjectId(PID)).with("counter", 0i64);
    let wait = hello().with("topologyVersion", tv).with("maxAwaitTimeMS", 200);
    // Without exhaust: one reply after the wait, then the connection serves the next request.
    let mut c = connect(&l);
    let t = Instant::now();
    send(&mut c, 1, 0, &wait);
    send(&mut c, 2, 0, &Document::new().with("ping", 1).with("$db", "admin"));
    let (h, bits, _) = recv(&mut c).unwrap();
    assert!(t.elapsed() >= Duration::from_millis(190));
    assert_eq!((h.response_to, bits), (1, 0));
    assert_eq!(recv(&mut c).unwrap().0.response_to, 2);
    // With exhaust: replies every 200 ms with moreToCome, each answering the previous one.
    let mut e = connect(&l);
    send(&mut e, 11, flags::EXHAUST_ALLOWED, &wait);
    let mut previous = 11;
    for _ in 0..3 {
        let (h, bits, d) = recv(&mut e).unwrap();
        assert_eq!(bits, flags::MORE_TO_COME);
        assert_eq!(h.response_to, previous);
        assert_eq!(d.get("isWritablePrimary"), Some(&Value::Boolean(true)));
        previous = h.request_id;
    }
    l.stop();
}

#[test]
fn connection_limit_closes_new_connections() {
    let l = listener(2);
    let mut a = connect(&l);
    let mut b = connect(&l);
    for s in [&mut a, &mut b] {
        send(s, 1, 0, &hello());
        assert!(recv(s).is_some());
    }
    let mut third = connect(&l);
    assert!(closed(&mut third));
    assert_eq!(l.open_connections(), 2);
    drop(a);
    let end = Instant::now() + Duration::from_secs(5);
    let mut fourth = loop {
        let mut s = connect(&l);
        send(&mut s, 1, 0, &hello());
        if recv(&mut s).is_some() {
            break s;
        }
        assert!(Instant::now() < end, "a slot never freed");
        std::thread::sleep(Duration::from_millis(10));
    };
    send(&mut fourth, 2, 0, &Document::new().with("ping", 1).with("$db", "a"));
    assert!(recv(&mut fourth).is_some());
    l.stop();
}

#[test]
fn faults_close_only_their_connection() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let mut good = connect(&l);
    let mut bad = connect(&l);
    // A message length over the limit, and a required flag nobody knows.
    let mut h = Vec::new();
    Header { length: 48_000_001, request_id: 1, response_to: 0, op_code: opcode::OP_MSG }
        .write(&mut h);
    bad.write_all(&h).unwrap();
    assert!(closed(&mut bad));
    let mut flagged = connect(&l);
    send(&mut flagged, 1, 1 << 5, &hello());
    assert!(closed(&mut flagged));
    send(&mut good, 1, 0, &hello());
    assert!(recv(&mut good).is_some());
    l.stop();
    // Stopping closes the connections.
    assert!(closed(&mut good));
}

#[test]
fn spawn_fails_on_a_busy_address() {
    let first = listener(1);
    let cfg = ClientConfig { addrs: first.addrs().to_vec(), ..config(1) };
    assert!(ClientListener::spawn(&cfg, Arc::new(Fixed)).is_err());
    first.stop();
}

fn start(s: &str) -> teseladb_server::cli::StartOpts {
    let args: Vec<String> = s.split_whitespace().map(String::from).collect();
    match parse_with(&args, &|_| {
        Ok(b"net:\n  maxIncomingConnections: 7\n  compression:\n    compressors: zlib\n".to_vec())
    }) {
        Ok(Command::Start(o)) => *o,
        other => panic!("{s}: {other:?}"),
    }
}

#[test]
fn client_options() {
    let o = start("start --insecure");
    assert_eq!(
        (o.max_conns, o.compressors.clone()),
        (DEFAULT_MAX_CONNECTIONS, DEFAULT_COMPRESSORS.to_vec())
    );
    let o = start("start --insecure --maxConns 5 --networkMessageCompressors snappy,zlib");
    assert_eq!(
        (o.max_conns, o.compressors.clone()),
        (5, vec![Compressor::Snappy, Compressor::Zlib])
    );
    // Port 0: the system chooses it, as `refnode --port 0` does.
    assert_eq!(start("start --insecure --port 0").port, 0);
    let o = start("start --insecure --networkMessageCompressors disabled");
    assert!(o.compressors.is_empty());
    // From the configuration file, overridden by the command line.
    let o = start("start --insecure --config c.yaml --maxConns 9");
    assert_eq!((o.max_conns, o.compressors.clone()), (9, vec![Compressor::Zlib]));
    for bad in [
        "--maxConns 0",
        "--maxConns x",
        "--networkMessageCompressors lz4",
        "--networkMessageCompressors noop",
    ] {
        let args: Vec<String> =
            format!("start --insecure {bad}").split_whitespace().map(String::from).collect();
        assert!(parse_with(&args, &|_| Ok(Vec::new())).is_err(), "{bad}");
    }
    let cfg = teseladb_server::client_config(&start(
        "start --insecure --insecure-allow-remote --bind_ip 127.0.0.1,localhost,nonexistent.invalid --port 27999",
    ));
    assert!(cfg.addrs.len() >= 2, "{:?}", cfg.addrs);
    assert!(cfg.addrs.iter().all(|a| a.ip().is_loopback()));
    // Every address of the list must be loopback without --insecure-allow-remote.
    let args: Vec<String> = "start --insecure --bind_ip 127.0.0.1,localhost"
        .split_whitespace()
        .map(String::from)
        .collect();
    assert!(parse_with(&args, &|_| Ok(Vec::new())).is_ok());
    let args: Vec<String> = "start --insecure --bind_ip 127.0.0.1,10.0.0.1"
        .split_whitespace()
        .map(String::from)
        .collect();
    assert!(parse_with(&args, &|_| Ok(Vec::new())).is_err());
}

#[test]
fn a_node_opens_the_clients_port() {
    let cfg = NodeConfig {
        cores: Some(2),
        client: Some(config(DEFAULT_MAX_CONNECTIONS)),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(Duration::from_secs(10)));
    let mut c = TcpStream::connect(node.client_addrs()[0]).unwrap();
    c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    send(&mut c, 1, 0, &hello());
    let (_, _, d) = recv(&mut c).unwrap();
    let Some(Value::Document(ct)) = d.get("$clusterTime") else { panic!("no $clusterTime") };
    assert!(matches!(ct.get("clusterTime"), Some(Value::Timestamp { time, .. }) if *time > 0));
    // The diagnostic commands see the port's counters and the process (006 FR-023).
    let status = Document::new().with("serverStatus", 1).with("$db", "admin");
    send(&mut c, 2, 0, &status);
    let (_, _, d) = recv(&mut c).unwrap();
    let Some(Value::Document(conns)) = d.get("connections") else { panic!("{d:?}") };
    assert_eq!(conns.get("current"), Some(&Value::Int32(1)));
    assert_eq!(d.get("pid"), Some(&Value::Int64(i64::from(std::process::id()))));
    let Some(Value::Document(net)) = d.get("network") else { panic!("{d:?}") };
    assert!(matches!(net.get("numRequests"), Some(Value::Int64(n)) if *n >= 2));
    drop(node.stop());
    assert!(closed(&mut c));
    // Without a clients' port, there are no addresses.
    let plain = Node::start(
        NodeConfig { cores: Some(1), ..NodeConfig::default() },
        || Ok(MemDisk::new()),
        None,
    )
    .unwrap();
    assert!(plain.client_addrs().is_empty());
    drop(plain.stop());
}

#[test]
fn a_slow_reader_gets_every_reply() {
    // Thousands of pipelined pings without reading: the replies fill the socket, the worker
    // waits until it can write, and every reply arrives in order.
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    assert!(format!("{l:?}").contains("ClientListener"));
    let mut c = connect(&l);
    let n = 20_000;
    let ping = Document::new().with("ping", 1).with("$db", "admin").to_bytes().unwrap();
    let mut batch = Vec::new();
    for k in 0..n {
        batch.extend_from_slice(&OpMsg::encode(k + 1, 0, 0, &ping));
    }
    let mut writer = c.try_clone().unwrap();
    let sender = std::thread::spawn(move || writer.write_all(&batch).unwrap());
    std::thread::sleep(Duration::from_millis(300));
    for k in 0..n {
        assert_eq!(recv(&mut c).unwrap().0.response_to, k + 1);
    }
    sender.join().unwrap();
    l.stop();
}

#[test]
fn clients_that_vanish_are_closed() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let tv = Document::new().with("processId", Value::ObjectId(PID)).with("counter", 0i64);
    let wait = hello().with("topologyVersion", tv).with("maxAwaitTimeMS", 100);
    // An exhaust stream and a plain wait whose clients go away before the answer.
    for bits in [flags::EXHAUST_ALLOWED, 0] {
        let mut e = connect(&l);
        send(&mut e, 1, bits, &wait);
        drop(e);
    }
    let end = Instant::now() + Duration::from_secs(5);
    while l.open_connections() > 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(l.open_connections(), 0);
    // A connection still waiting when the listener stops is closed.
    let mut w = connect(&l);
    send(
        &mut w,
        1,
        0,
        &hello()
            .with(
                "topologyVersion",
                Document::new().with("processId", Value::ObjectId(PID)).with("counter", 0i64),
            )
            .with("maxAwaitTimeMS", 60_000),
    );
    std::thread::sleep(Duration::from_millis(50));
    l.stop();
    assert!(closed(&mut w));
}

#[test]
fn dev_cluster_with_a_busy_clients_port_fails() {
    use teseladb_server::cli::DevOpts;
    // Node 2 of a cluster on base port P takes P + 1, which is already in use.
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = busy.local_addr().unwrap().port() - 1;
    let r = teseladb_server::dev::start(
        &DevOpts { nodes: 2, cores: 1, port: base, test_commands: false },
        Some(3),
    );
    assert!(r.is_err());
}

#[test]
fn dev_cluster_with_test_commands_answers_configure_fail_point() {
    use teseladb_server::cli::DevOpts;
    // The drivers' specification tests need the failpoints (002 T043).
    let fail_point = Document::new()
        .with("configureFailPoint", "failCommand")
        .with("mode", "off")
        .with("$db", "admin");
    for (test_commands, ok) in [(true, 1.0), (false, 0.0)] {
        let o = DevOpts { nodes: 1, cores: 1, port: 0, test_commands };
        let nodes = teseladb_server::dev::start(&o, Some(5)).unwrap();
        let mut c = TcpStream::connect(nodes[0].client_addrs()[0]).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        send(&mut c, 1, 0, &fail_point);
        let (_, _, reply) = recv(&mut c).expect("a reply");
        assert_eq!(reply.get("ok"), Some(&Value::Double(ok)), "{reply:?}");
        nodes.into_iter().for_each(|n| drop(n.stop()));
    }
}

#[test]
fn a_waiting_client_that_hangs_up_is_closed_before_its_deadline() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let tv = Document::new().with("processId", Value::ObjectId(PID)).with("counter", 0i64);
    let mut w = connect(&l);
    send(&mut w, 1, 0, &hello().with("topologyVersion", tv).with("maxAwaitTimeMS", 400));
    std::thread::sleep(Duration::from_millis(50));
    drop(w);
    let end = Instant::now() + Duration::from_millis(300);
    while l.open_connections() > 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(l.open_connections(), 0, "closed on the hang-up, not at the deadline");
    // The stale deadline passes without effect.
    std::thread::sleep(Duration::from_millis(400));
    let mut c = connect(&l);
    send(&mut c, 2, 0, &hello());
    assert!(recv(&mut c).is_some());
    l.stop();
}

#[test]
fn serving_a_node_without_clients_port() {
    use teseladb_server::log::{Format, Level, Logger};
    let node = Node::start(
        NodeConfig { cores: Some(1), ..NodeConfig::default() },
        || Ok(MemDisk::new()),
        None,
    )
    .unwrap();
    let log = Logger::new(Level::Info, Format::Text, Box::new(std::io::sink()));
    let mut out = Vec::new();
    let code = teseladb_server::serve(
        node,
        Format::Text,
        &log,
        &mut out,
        &|| true,
        Duration::from_secs(10),
    );
    assert_eq!(code, 0);
    assert!(String::from_utf8(out).unwrap().starts_with("ready "));
}

#[test]
fn a_client_that_closes_with_replies_unread_is_closed() {
    // The client pipelines requests and closes without reading: the worker's writes fail
    // (connection reset) and it closes the connection.
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    for _ in 0..3 {
        let mut c = connect(&l);
        let ping = Document::new().with("ping", 1).with("$db", "admin").to_bytes().unwrap();
        let mut batch = Vec::new();
        for k in 0..5_000 {
            batch.extend_from_slice(&OpMsg::encode(k + 1, 0, 0, &ping));
        }
        c.write_all(&batch).unwrap();
        drop(c);
    }
    let end = Instant::now() + Duration::from_secs(5);
    while l.open_connections() > 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(l.open_connections(), 0);
    l.stop();
}

#[test]
fn configuration_file_keys_of_the_clients_port() {
    use teseladb_server::config::yaml_args;
    let args = yaml_args(b"net:\n  port: 0\n  maxIncomingConnections: 7\n  compression:\n    compressors: snappy,zlib\n").unwrap();
    assert_eq!(args, ["--port=0", "--maxConns=7", "--networkMessageCompressors=snappy,zlib"]);
    // Verbosity past 5 is refused, like the rest of the file's ranges.
    assert!(yaml_args(b"systemLog:\n  verbosity: 9\n").is_err());
    assert_eq!(yaml_args(b"systemLog:\n  verbosity: 3\n").unwrap(), ["--verbose"]);
}

#[test]
fn a_fault_behind_a_waiting_hello_closes_the_connection_after_the_answer() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let tv = Document::new().with("processId", Value::ObjectId(PID)).with("counter", 0i64);
    let mut w = connect(&l);
    send(&mut w, 1, 0, &hello().with("topologyVersion", tv).with("maxAwaitTimeMS", 500));
    // A required flag nobody knows, behind the waiting `hello`: it waits too.
    send(&mut w, 2, 1 << 5, &hello());
    // Another client of the same worker is served while the `hello` waits.
    let mut other = connect(&l);
    send(&mut other, 1, 0, &hello());
    assert!(recv(&mut other).is_some());
    // At the deadline the `hello` is answered, then the fault closes the connection.
    assert!(recv(&mut w).is_some());
    assert!(closed(&mut w));
    l.stop();
}

#[test]
fn the_replies_before_a_fault_reach_the_client() {
    let l = listener(DEFAULT_MAX_CONNECTIONS);
    let mut c = connect(&l);
    let mut both = OpMsg::encode(1, 0, 0, &hello().to_bytes().unwrap());
    both.extend(OpMsg::encode(2, 0, 1 << 5, &hello().to_bytes().unwrap()));
    c.write_all(&both).unwrap();
    assert!(recv(&mut c).is_some());
    assert!(closed(&mut c));
    l.stop();
}

/// A node's state as core 0 publishes it (007 FR-015): a draining node shows a new
/// `topologyVersion` counter, and a drained one closes its clients' port: the open connections
/// get `ShutdownInProgress` and close, and new ones are dropped.
#[test]
fn a_drained_node_closes_its_clients_port() {
    let presence = Arc::new(NodePresence::default());
    let clock = teseladb_multiraft::NodeClock::for_node(teseladb_types::NodeId(1));
    let state = NodeClockState::new(clock, 7).with_presence(presence.clone());
    assert_eq!(state.info().topology.counter, 0);
    let l = ClientListener::spawn(&config(DEFAULT_MAX_CONNECTIONS), Arc::new(state)).unwrap();
    let mut s = connect(&l);
    send(&mut s, 1, 0, &hello());
    let (_, _, d) = recv(&mut s).unwrap();
    let counter = |d: &Document| match d.get("topologyVersion") {
        Some(Value::Document(t)) => t.get("counter").cloned(),
        _ => None,
    };
    assert_eq!(counter(&d), Some(Value::Int64(0)), "{d:?}");
    // Draining: a new counter, and the port still serves.
    presence.set(1, false);
    send(&mut s, 2, 0, &hello());
    let (_, _, d) = recv(&mut s).unwrap();
    assert_eq!(counter(&d), Some(Value::Int64(1)), "{d:?}");
    // Drained: the next request gets ShutdownInProgress and the connection closes.
    presence.set(1, true);
    send(&mut s, 3, 0, &Document::new().with("ping", 1).with("$db", "admin"));
    let (_, _, d) = recv(&mut s).unwrap();
    assert_eq!(d.get("code"), Some(&Value::Int32(91)), "{d:?}");
    assert_eq!(d.get("codeName"), Some(&Value::String("ShutdownInProgress".into())));
    assert!(closed(&mut s));
    // A new connection is dropped.
    let mut late = connect(&l);
    send(&mut late, 4, 0, &hello());
    assert!(closed(&mut late));
    l.stop();
}

/// A drained node's `ShutdownInProgress` carries the label of D-148: a
/// retryable write gets `RetryableWriteError`, so the driver retries it on another node instead
/// of failing it (e2e of dedicated clusters, 10-11-2026: writes lost while a node was drained
/// during a resize); a statement of a transaction gets `TransientTransactionError`.
#[test]
fn a_drained_node_labels_its_shutdown_as_the_driver_retries_it() {
    let presence = Arc::new(NodePresence::default());
    let clock = teseladb_multiraft::NodeClock::for_node(teseladb_types::NodeId(1));
    let state = NodeClockState::new(clock, 7).with_presence(presence.clone());
    let l = ClientListener::spawn(&config(DEFAULT_MAX_CONNECTIONS), Arc::new(state)).unwrap();
    // A drained node drops new connections and closes each open one after its answer: one
    // connection per command, opened while the node still serves.
    let mut open: Vec<_> = (0..3)
        .map(|i| {
            let mut s = connect(&l);
            send(&mut s, i, 0, &hello());
            recv(&mut s).unwrap();
            s
        })
        .collect();
    presence.set(1, true);
    let mut labels = |cmd: Document| {
        let mut s = open.pop().unwrap();
        send(&mut s, 9, 0, &cmd);
        let (_, _, d) = recv(&mut s).unwrap();
        assert_eq!(d.get("code"), Some(&Value::Int32(91)), "{d:?}");
        match d.get("errorLabels") {
            Some(Value::Array(a)) => a.clone(),
            _ => Vec::new(),
        }
    };
    let insert = Document::new()
        .with("insert", "c")
        .with("documents", vec![Value::Document(Document::new().with("_id", 1))])
        .with("txnNumber", Value::Int64(1))
        .with("$db", "app");
    assert_eq!(labels(insert), [Value::String("RetryableWriteError".into())]);
    let in_txn = Document::new()
        .with("find", "c")
        .with("txnNumber", Value::Int64(2))
        .with("autocommit", false)
        .with("$db", "app");
    assert_eq!(labels(in_txn), [Value::String("TransientTransactionError".into())]);
    assert!(labels(Document::new().with("ping", 1).with("$db", "admin")).is_empty());
    l.stop();
}

/// A running node answers the `hello`s waiting on its `topologyVersion` as soon as it applies a
/// change of `topologyPresentation` (018 FR-015, T016): the awaitable `hello` of a driver's
/// monitor returns well before its `maxAwaitTimeMS` with the replica set presentation and a
/// higher counter.
#[test]
fn a_presentation_change_wakes_the_waiting_hellos() {
    // The member's host is the client address the node announces.
    let cfg = NodeConfig {
        cores: Some(1),
        client: Some(config(DEFAULT_MAX_CONNECTIONS)),
        native_addr: "member.test:27017".into(),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(Duration::from_secs(10)));
    let mut admin = TcpStream::connect(node.client_addrs()[0]).unwrap();
    admin.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut monitor = TcpStream::connect(node.client_addrs()[0]).unwrap();
    monitor.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    send(&mut monitor, 1, 0, &hello());
    let (_, _, first) = recv(&mut monitor).unwrap();
    let Some(Value::Document(tv)) = first.get("topologyVersion").cloned() else {
        panic!("{first:?}")
    };
    let counter = |d: &Document| match d.get("topologyVersion") {
        Some(Value::Document(t)) => match t.get("counter") {
            Some(Value::Int64(n)) => *n,
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    };
    let wait =
        |tv: &Document| hello().with("topologyVersion", tv.clone()).with("maxAwaitTimeMS", 15_000);
    send(&mut monitor, 2, 0, &wait(&tv));
    let started = Instant::now();
    let present = Document::new()
        .with(
            "setClusterParameter",
            Document::new().with(
                "topologyPresentation",
                Document::new().with("mode", "replicaset").with("setName", "rs0"),
            ),
        )
        .with("$db", "admin");
    // The node's client address reaches `config.shards` once it is announced to Group 0.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        send(&mut admin, 3, 0, &present);
        let (_, _, set) = recv(&mut admin).unwrap();
        if set.get("ok") == Some(&Value::Double(1.0)) {
            break;
        }
        assert!(Instant::now() < deadline, "{set:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
    let (h, _, woken) = recv(&mut monitor).unwrap();
    assert_eq!(h.response_to, 2);
    assert!(started.elapsed() < Duration::from_secs(5), "woken after {:?}", started.elapsed());
    assert_eq!(woken.get("setName"), Some(&Value::from("rs0")), "{woken:?}");
    assert_eq!(woken.get("isWritablePrimary"), Some(&Value::Boolean(true)), "{woken:?}");
    assert_eq!(woken.get("me"), Some(&Value::from("member.test:27017")));
    assert!(counter(&woken) > counter(&first));
    drop(node.stop());
}

/// The replica set port (018 FR-016, T018): opened on the running node when Group 0 says so,
/// it answers as a member of the replica set at that port while the main port keeps answering
/// as a `refrouter`, one application on each at the same time; 0 closes it.
#[test]
fn the_replica_set_port_opens_and_closes_without_a_restart() {
    let cfg = NodeConfig {
        cores: Some(1),
        client: Some(config(DEFAULT_MAX_CONNECTIONS)),
        native_addr: "member.test:27017".into(),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(Duration::from_secs(10)));
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut admin = TcpStream::connect(node.client_addrs()[0]).unwrap();
    admin.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let set = |admin: &mut TcpStream, id: i32, port: i32| {
        let value = Document::new()
            .with("mode", "refrouter")
            .with("setName", "rs0")
            .with("replicaSetPort", port);
        let cmd = Document::new()
            .with("setClusterParameter", Document::new().with("topologyPresentation", value))
            .with("$db", "admin");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            send(admin, id, 0, &cmd);
            let (_, _, r) = recv(admin).unwrap();
            if r.get("ok") == Some(&Value::Double(1.0)) {
                break;
            }
            assert!(Instant::now() < deadline, "{r:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    set(&mut admin, 1, i32::from(port));
    let started = Instant::now();
    let mut member = loop {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
            break s;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "the port never opened");
        std::thread::sleep(Duration::from_millis(20));
    };
    member.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    send(&mut member, 2, 0, &hello());
    let (_, _, h) = recv(&mut member).unwrap();
    let me = format!("member.test:{port}");
    assert_eq!(h.get("setName"), Some(&Value::from("rs0")), "{h:?}");
    assert_eq!(h.get("me"), Some(&Value::from(me.as_str())));
    assert_eq!(h.get("hosts"), Some(&Value::Array(vec![Value::from(me.as_str())])));
    assert_eq!(h.get("isWritablePrimary"), Some(&Value::Boolean(true)));
    send(&mut admin, 3, 0, &hello());
    let (_, _, main) = recv(&mut admin).unwrap();
    assert_eq!(main.get("msg"), Some(&Value::from("isdbgrid")), "{main:?}");
    // Both applications write and read the same data.
    let ins = Document::new()
        .with("insert", "c")
        .with("documents", vec![Value::Document(Document::new().with("_id", 1))])
        .with("$db", "dual");
    send(&mut member, 4, 0, &ins);
    let (_, _, r) = recv(&mut member).unwrap();
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    let count = Document::new().with("count", "c").with("$db", "dual");
    send(&mut admin, 5, 0, &count);
    let (_, _, r) = recv(&mut admin).unwrap();
    assert_eq!(r.get("n"), Some(&Value::Int32(1)), "{r:?}");
    set(&mut admin, 6, 0);
    let started = Instant::now();
    while TcpStream::connect(("127.0.0.1", port)).is_ok() {
        assert!(started.elapsed() < Duration::from_secs(5), "the port never closed");
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(node.stop());
}

/// A replica set port that another program holds cannot open: the node goes on, and the next
/// port Group 0 asks for opens (018 FR-016).
#[test]
fn a_replica_set_port_that_is_taken_waits_for_another() {
    let cfg = NodeConfig {
        cores: Some(1),
        client: Some(config(DEFAULT_MAX_CONNECTIONS)),
        native_addr: "member.test:27017".into(),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(Duration::from_secs(10)));
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let busy = taken.local_addr().unwrap().port();
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let mut admin = TcpStream::connect(node.client_addrs()[0]).unwrap();
    admin.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    let mut id = 0;
    let mut set = |admin: &mut TcpStream, port: u16| {
        let value = Document::new()
            .with("mode", "refrouter")
            .with("setName", "rs0")
            .with("replicaSetPort", i32::from(port));
        let cmd = Document::new()
            .with("setClusterParameter", Document::new().with("topologyPresentation", value))
            .with("$db", "admin");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            id += 1;
            send(admin, id, 0, &cmd);
            let (_, _, r) = recv(admin).unwrap();
            if r.get("ok") == Some(&Value::Double(1.0)) {
                break;
            }
            assert!(Instant::now() < deadline, "{r:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    set(&mut admin, busy);
    // The keeper says why the port is closed (written by the node's loop, or drained here).
    use teseladb_server::log::{Format, Level, Logger, drain, recent};
    let why = format!("cannot open the replica set port {busy}");
    let started = Instant::now();
    while !recent().1.iter().any(|l| l.contains(&why)) {
        drain(&Logger::new(Level::Warn, Format::Text, Box::new(std::io::sink())));
        assert!(started.elapsed() < Duration::from_secs(5), "no warning for the taken port");
        std::thread::sleep(Duration::from_millis(20));
    }
    send(&mut admin, 100, 0, &hello());
    let (_, _, main) = recv(&mut admin).unwrap();
    assert_eq!(main.get("msg"), Some(&Value::from("isdbgrid")), "{main:?}");
    set(&mut admin, free);
    let started = Instant::now();
    let mut member = loop {
        if let Ok(s) = TcpStream::connect(("127.0.0.1", free)) {
            break s;
        }
        assert!(started.elapsed() < Duration::from_secs(5), "the free port never opened");
        std::thread::sleep(Duration::from_millis(20));
    };
    member.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    send(&mut member, 101, 0, &hello());
    let (_, _, h) = recv(&mut member).unwrap();
    assert_eq!(h.get("setName"), Some(&Value::from("rs0")), "{h:?}");
    drop(taken);
    drop(node.stop());
}

/// Each connection's thread is woken at a change of `topologyVersion` (or of the tenants'
/// states); a connection that closes leaves the list, and so do those of a port that stops, so
/// opening and closing connections or the replica set port again and again keeps nothing (018
/// FR-016, H-013).
#[test]
fn a_closed_connection_is_no_longer_woken() {
    let presence = Arc::new(NodePresence::default());
    let clock = teseladb_multiraft::NodeClock::for_node(teseladb_types::NodeId(1));
    let state = Arc::new(NodeClockState::new(clock, 7).with_presence(presence.clone()));
    let l = ClientListener::spawn(&config(DEFAULT_MAX_CONNECTIONS), state).unwrap();
    assert_eq!(presence.watchers(), 0, "no connection, no thread");
    let mut a = connect(&l);
    let mut b = connect(&l);
    for (id, s) in [(1, &mut a), (2, &mut b)] {
        send(s, id, 0, &hello());
        assert!(recv(s).is_some());
    }
    assert_eq!(presence.watchers(), 2);
    drop(a);
    let settled = |want: usize| {
        let until = Instant::now() + Duration::from_secs(5);
        while presence.watchers() != want && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        presence.watchers()
    };
    assert_eq!(settled(1), 1, "the closed connection's thread ended");
    presence.set(1, false);
    l.stop();
    assert_eq!(settled(0), 0, "the port's stop ends the others");
    drop(b);
}

/// The limiter of new connections (006 FR-036, measured on a `refnode` 9.0.4): with a bucket of
/// two tokens refilled at one a second and a queue of one, of five connections that greet at once
/// two are answered now, one a second later and two are closed without a reply. A connection that
/// has sent nothing has taken no token, and is answered when it greets.
#[test]
fn the_limiter_of_new_connections_queues_then_closes_the_greetings_past_its_bucket() {
    use teseladb_wire::ingress::Setting;
    let mut cfg = config(DEFAULT_MAX_CONNECTIONS);
    cfg.ingress = vec![
        Setting::RatePerSec(1),
        Setting::BurstSecs(2.0),
        Setting::QueueDepth(1),
        Setting::Enabled(true),
    ];
    let l = ClientListener::spawn(&cfg, Arc::new(Fixed)).unwrap();
    let mut silent = connect(&l);
    let start = Instant::now();
    let mut clients: Vec<TcpStream> = (0..5).map(|_| connect(&l)).collect();
    for (k, c) in clients.iter_mut().enumerate() {
        send(c, k as i32 + 1, 0, &hello());
    }
    let mut answered: Vec<Duration> = Vec::new();
    let mut shut = 0;
    std::thread::scope(|scope| {
        let waits: Vec<_> = clients
            .iter_mut()
            .map(|c| scope.spawn(move || recv(c).map(|_| start.elapsed())))
            .collect();
        for w in waits {
            match w.join().unwrap() {
                Some(at) => answered.push(at),
                None => shut += 1,
            }
        }
    });
    answered.sort_unstable();
    assert_eq!((answered.len(), shut), (3, 2), "{answered:?}");
    assert!(answered[1] < Duration::from_millis(500), "{answered:?}");
    assert!(answered[2] >= Duration::from_millis(900), "{answered:?}");
    send(&mut silent, 9, 0, &hello());
    assert!(recv(&mut silent).is_some(), "the silent connection is answered");
    l.stop();
}

/// The limiter of new connections on a node (006 FR-036): `setParameter` turns it on, a client
/// that leaves while it waits in the queue counts as interrupted when its turn comes, one that
/// finds the queue full is closed, and `serverStatus` counts both.
#[test]
fn a_node_s_limiter_counts_the_closed_connections_and_those_that_left_the_queue() {
    use teseladb_wire::ingress::{BURST_CAPACITY_SECS, ENABLED, MAX_QUEUE_DEPTH, RATE_PER_SEC};
    let cfg = NodeConfig {
        cores: Some(2),
        client: Some(config(DEFAULT_MAX_CONNECTIONS)),
        ..NodeConfig::default()
    };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.wait_ready(Duration::from_secs(10)));
    let open = || {
        let s = TcpStream::connect(node.client_addrs()[0]).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        s
    };
    let mut admin = open();
    let params = [(RATE_PER_SEC, 1), (BURST_CAPACITY_SECS, 1), (MAX_QUEUE_DEPTH, 1), (ENABLED, 1)];
    for (k, (name, v)) in params.into_iter().enumerate() {
        let cmd = Document::new().with("setParameter", 1).with(name, v).with("$db", "admin");
        send(&mut admin, k as i32 + 1, 0, &cmd);
        assert_eq!(recv(&mut admin).unwrap().2.get("ok"), Some(&Value::Double(1.0)));
    }
    let (mut first, mut queued, mut late) = (open(), open(), open());
    send(&mut first, 1, 0, &hello());
    assert!(recv(&mut first).is_some(), "the bucket's token");
    send(&mut queued, 1, 0, &hello());
    // `late` knocks once `queued` holds the only place in the queue, whatever the load.
    let mut id = 8;
    let mut connections = |admin: &mut TcpStream| {
        id += 1;
        let status = Document::new().with("serverStatus", 1).with("$db", "admin");
        send(admin, id, 0, &status);
        let (_, _, d) = recv(admin).unwrap();
        let Some(Value::Document(conns)) = d.get("connections") else { panic!("{d:?}") };
        conns.clone()
    };
    while connections(&mut admin).get("queuedForEstablishment") != Some(&Value::Int32(1)) {
        std::thread::sleep(Duration::from_millis(5));
    }
    send(&mut late, 1, 0, &hello());
    assert!(recv(&mut late).is_none(), "the queue is full");
    drop(queued);
    std::thread::sleep(Duration::from_millis(1300));
    let conns = connections(&mut admin);
    assert_eq!(conns.get("queuedForEstablishment"), Some(&Value::Int32(0)), "{conns:?}");
    let Some(Value::Document(limit)) = conns.get("establishmentRateLimit") else { panic!() };
    assert_eq!(limit.get("rejected"), Some(&Value::Int32(1)), "{limit:?}");
    assert_eq!(limit.get("interruptedDueToClientDisconnect"), Some(&Value::Int32(1)));
    drop(node.stop());
}
