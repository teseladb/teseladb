//! The quiesce period of a stop (007 FR-027, T040): before its connections close, a stopping node
//! answers every `hello` with `ShutdownInProgress` and a new `topologyVersion`, the waiting ones at
//! once, and still runs the other commands, so that a driver's monitor marks it unknown and the
//! driver starts no command on a connection that is about to close (a driver that does can wait
//! for that command forever: measured on 14-11-2026 with the shell of the chaos tests against a
//! port that closes its connections, with `socketTimeoutMS` set). The quiesce ends once no client
//! has started a command other than the handshake for a while, and never past its longest.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};
use teseladb_io::engine::disk::DiskPlace;

use teseladb_bson::{Document, RawDoc, Value};
use teseladb_server::client_listener::{
    ClientConfig, ClientListener, NodePresence, NodeState, QUIESCE_MESSAGE,
};
use teseladb_server::log::{Format, Level, Logger};
use teseladb_wire::dispatch::{ClusterTime, NodeInfo, Topology};

const PID: [u8; 12] = [4; 12];
const SHUTDOWN_IN_PROGRESS: i32 = 91;

/// A node's state with its presence, which a test sends into its quiesce period.
struct Stopping(Arc<NodePresence>);

impl NodeState for Stopping {
    fn info(&self) -> NodeInfo {
        NodeInfo {
            topology: Topology { process_id: PID, counter: self.0.counter() },
            cluster_time: ClusterTime { time: 5, increment: 1 },
            now_ms: 1,
            keys: None,
            replica_set: None,
        }
    }

    fn quiescing(&self) -> bool {
        self.0.quiescing()
    }

    fn note_command(&self) {
        if self.0.quiescing() {
            self.0.note_command();
        }
    }

    fn watch_topology(&self, waker: &Arc<teseladb_io::poller::Waker>) {
        self.0.watch(waker);
    }
}

fn listener(presence: &Arc<NodePresence>) -> ClientListener {
    let cfg = ClientConfig {
        addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
        ..ClientConfig::new(Vec::new())
    };
    ClientListener::spawn(&cfg, Arc::new(Stopping(presence.clone()))).unwrap()
}

fn run(s: &mut TcpStream, id: i32, cmd: Document) -> Document {
    let cmd = cmd.with("$db", "admin");
    let msg = teseladb_wire::msg::OpMsg::encode(id, 0, 0, &cmd.to_bytes().unwrap());
    s.write_all(&msg).unwrap();
    read(s)
}

fn read(s: &mut TcpStream) -> Document {
    let mut h = [0u8; teseladb_wire::msg::HEADER_LEN];
    s.read_exact(&mut h).unwrap();
    let len = i32::from_le_bytes(h[..4].try_into().unwrap()) as usize;
    let mut rest = vec![0u8; len - h.len()];
    s.read_exact(&mut rest).unwrap();
    Document::from(RawDoc::from_bytes(&rest[5..]).unwrap())
}

fn counter(reply: &Document) -> i64 {
    let Some(Value::Document(tv)) = reply.get("topologyVersion") else { panic!("{reply:?}") };
    match tv.get("counter") {
        Some(Value::Int64(c)) => *c,
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_quiescing_port_answers_every_hello_with_shutdown_in_progress_and_runs_the_rest() {
    let presence = Arc::new(NodePresence::default());
    let l = listener(&presence);
    let mut s = TcpStream::connect(l.addrs()[0]).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let before = run(&mut s, 1, Document::new().with("hello", 1));
    assert_eq!(before.get("ok"), Some(&Value::Double(1.0)));
    presence.quiesce();
    let during = run(&mut s, 2, Document::new().with("hello", 1));
    assert_eq!(during.get("ok"), Some(&Value::Double(0.0)), "{during:?}");
    assert_eq!(during.get("code"), Some(&Value::Int32(SHUTDOWN_IN_PROGRESS)));
    assert_eq!(during.get("codeName"), Some(&Value::from("ShutdownInProgress")));
    assert_eq!(during.get("errmsg"), Some(&Value::from(QUIESCE_MESSAGE)));
    assert!(counter(&during) > counter(&before), "a new topologyVersion");
    // The legacy handshake too; the connection stays open and its other commands run.
    let legacy = run(&mut s, 3, Document::new().with("isMaster", 1));
    assert_eq!(legacy.get("code"), Some(&Value::Int32(SHUTDOWN_IN_PROGRESS)));
    let ping = run(&mut s, 4, Document::new().with("ping", 1));
    assert_eq!(ping.get("ok"), Some(&Value::Double(1.0)));
    l.stop();
}

#[test]
fn a_waiting_hello_answers_at_once_when_the_quiesce_starts() {
    let presence = Arc::new(NodePresence::default());
    let l = listener(&presence);
    let mut s = TcpStream::connect(l.addrs()[0]).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let tv = Document::new().with("processId", Value::ObjectId(PID)).with("counter", 0i64);
    let wait = Document::new()
        .with("hello", 1)
        .with("topologyVersion", tv)
        .with("maxAwaitTimeMS", 60_000)
        .with("$db", "admin");
    let msg = teseladb_wire::msg::OpMsg::encode(
        1,
        0,
        teseladb_wire::msg::flags::EXHAUST_ALLOWED,
        &wait.to_bytes().unwrap(),
    );
    s.write_all(&msg).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    let asked = Instant::now();
    presence.quiesce();
    let reply = read(&mut s);
    assert!(asked.elapsed() < Duration::from_secs(5), "answered in {:?}", asked.elapsed());
    assert_eq!(reply.get("code"), Some(&Value::Int32(SHUTDOWN_IN_PROGRESS)), "{reply:?}");
    // The stream ended with the error: the next request is answered as usual.
    let ping = run(&mut s, 2, Document::new().with("ping", 1));
    assert_eq!(ping.get("ok"), Some(&Value::Double(1.0)));
    l.stop();
}

#[test]
fn the_quiesce_of_a_node_ends_once_its_clients_go_quiet_and_never_past_its_longest() {
    let node =
        teseladb_server::node::Node::start(common_config(), DiskPlace::Memory, None).unwrap();
    assert!(node.wait_ready(Duration::from_secs(30)));
    let log = Logger::new(Level::Info, Format::Text, Box::new(std::io::sink()));
    // Nobody sends: it ends after the quiet time.
    let t = Instant::now();
    teseladb_server::quiesce(&node, &log, Duration::from_secs(10), Duration::from_millis(300));
    assert!(t.elapsed() < Duration::from_secs(3), "{:?}", t.elapsed());
    // A client that keeps running commands holds it until the longest.
    let to = node.client_addrs()[0];
    let busy = std::thread::spawn(move || {
        let mut s = TcpStream::connect(to).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let end = Instant::now() + Duration::from_millis(1500);
        let mut id = 1;
        while Instant::now() < end {
            assert_eq!(
                run(&mut s, id, Document::new().with("ping", 1)).get("ok"),
                Some(&Value::Double(1.0))
            );
            id += 1;
            std::thread::sleep(Duration::from_millis(20));
        }
    });
    std::thread::sleep(Duration::from_millis(100));
    let t = Instant::now();
    teseladb_server::quiesce(&node, &log, Duration::from_millis(1000), Duration::from_millis(300));
    let took = t.elapsed();
    assert!(took >= Duration::from_millis(950) && took < Duration::from_secs(3), "{took:?}");
    busy.join().unwrap();
    node.stop();
}

fn common_config() -> teseladb_server::node::NodeConfig {
    teseladb_server::node::NodeConfig {
        cores: Some(1),
        client: Some(ClientConfig {
            addrs: vec![SocketAddr::from(([127, 0, 0, 1], 0))],
            ..ClientConfig::new(Vec::new())
        }),
        ..teseladb_server::node::NodeConfig::default()
    }
}
