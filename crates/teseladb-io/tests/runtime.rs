//! The core loop over loopback TCP with a toy logic: ping-pong between two nodes,
//! ticks, disk operations through the disk thread, redial after a refused connection, the peer
//! going away and rejected handshakes.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use teseladb_io::IoClass;
use teseladb_io::handshake::{self, Acceptor, HandshakeError};
use teseladb_io::mailbox::MailboxSender;
use teseladb_io::peer::{Hello, PeerMsg, Versions, encode_frame};
use teseladb_io::runtime::{CoreInput, CoreLogic, CoreRuntime, RuntimeConfig};
use teseladb_raftlog::{Disk, FileId, IoDone, IoOp, MemDisk};
use teseladb_types::{Bytes, Nanos, NodeId};

const CLUSTER: [u8; 16] = [7; 16];

fn hello(node: u64, addr: &str) -> Hello {
    Hello {
        cluster_id: CLUSTER,
        node_id: NodeId(node),
        core: 0,
        cores: 1,
        peer_addr: addr.into(),
        http_addr: String::new(),
        native_addr: String::new(),
        versions: Versions::OWN,
    }
}

enum Cmd {
    /// Start a ping-pong of `n` rounds with a node.
    Ping(NodeId, u64),
    /// Change a node's address.
    Addr(NodeId, String),
    /// Write and read back through the disk.
    Disk,
    /// Send `n` `Goodbye` frames of `size` bytes to a node.
    Bulk(NodeId, usize, usize),
    /// Create a file and write to it, one operation at a time: the write is handed out only
    /// once the creation is done, the way the log's next batch waits for the one in flight.
    OneByOne,
}

#[derive(Default)]
struct Toy {
    addrs: BTreeMap<NodeId, String>,
    limit: u64,
    last_seen: u64,
    ticks: u64,
    io: Vec<IoOp>,
    done: Vec<IoDone>,
    out: Vec<(NodeId, PeerMsg)>,
    unreachable: Vec<NodeId>,
    /// Report a (far) deadline of its own.
    deadline: bool,
    /// `last_seen` and `ticks` as the test sees them while the core runs.
    shared_seen: Arc<AtomicU64>,
    shared_ticks: Arc<AtomicU64>,
    /// How many times a peer was unreachable, as the test sees it.
    shared_unreachable: Arc<AtomicU64>,
    /// Hand out one operation at a time ([`Cmd::OneByOne`]).
    one_by_one: bool,
    /// Operations handed out and not done yet.
    in_flight: usize,
    /// Says it is at rest ([`CoreLogic::resting`]).
    rests: bool,
    /// Wakes from rest ([`CoreLogic::wake`]).
    wakes: u64,
    /// Turns with work that read the wall clock ([`CoreLogic::clock`]).
    clocks: u64,
}

fn ack(req: u64) -> PeerMsg {
    PeerMsg::ForwardResp { req, hlc: None, body: Bytes::new() }
}

impl CoreLogic for Toy {
    type Msg = Cmd;

    fn tick(&mut self, _now: Nanos, wall_ms: u64) {
        assert!(wall_ms > 0);
        self.ticks += 1;
        self.shared_ticks.store(self.ticks, Ordering::Relaxed);
    }

    fn wake(&mut self, _now: Nanos, wall_ms: u64) {
        assert!(wall_ms > 0);
        self.wakes += 1;
    }

    fn clock(&mut self, _now: Nanos, wall_ms: u64) {
        assert!(wall_ms > 0);
        self.clocks += 1;
    }

    fn poll_io(&mut self, _now: Nanos) -> Option<(IoOp, IoClass)> {
        if self.io.is_empty() || (self.one_by_one && self.in_flight > 0) {
            return None;
        }
        self.in_flight += 1;
        Some((self.io.remove(0), IoClass::Data))
    }

    fn on_io_done(&mut self, done: IoDone, class: IoClass, _now: Nanos) {
        assert_eq!(class, IoClass::Data, "each result with the class of its operation");
        self.in_flight -= 1;
        self.done.push(done);
    }

    fn next_deadline(&self) -> Option<Nanos> {
        self.deadline.then_some(Nanos::from_secs(3_600))
    }

    fn on_peer(&mut self, from: NodeId, msg: PeerMsg, _now: Nanos) {
        let PeerMsg::ForwardResp { req, .. } = msg else { panic!("unexpected {msg:?}") };
        self.last_seen = req;
        self.shared_seen.store(req, Ordering::Relaxed);
        if req < self.limit || self.limit == 0 {
            self.out.push((from, ack(req + 1)));
        }
    }

    fn on_message(&mut self, msg: Cmd, _now: Nanos) {
        match msg {
            Cmd::Ping(node, n) => {
                self.limit = n;
                self.out.push((node, ack(1)));
            }
            Cmd::Addr(node, addr) => {
                self.addrs.insert(node, addr);
            }
            Cmd::Disk => {
                let file = FileId(1);
                self.io.push(IoOp::Create { file, path: PathBuf::from("x"), prealloc: 0 });
                let data = Bytes::from_vec(b"abc".to_vec());
                self.io.push(IoOp::WriteAt { file, offset: 0, data, batch: 1 });
                self.io.push(IoOp::ReadAt { file, offset: 0, len: 3, token: 9 });
            }
            Cmd::Bulk(node, n, size) => {
                for _ in 0..n {
                    self.out.push((node, PeerMsg::Goodbye { reason: "x".repeat(size) }));
                }
            }
            Cmd::OneByOne => {
                self.one_by_one = true;
                let file = FileId(1);
                self.io.push(IoOp::Create { file, path: PathBuf::from("x"), prealloc: 0 });
                let data = Bytes::from_vec(b"abc".to_vec());
                self.io.push(IoOp::WriteAt { file, offset: 0, data, batch: 1 });
            }
        }
    }

    fn take_outbound(&mut self, out: &mut Vec<(NodeId, PeerMsg)>) {
        out.append(&mut self.out);
    }

    fn peer_addr(&self, node: NodeId) -> Option<String> {
        self.addrs.get(&node).cloned()
    }

    fn peer_unreachable(&mut self, node: NodeId) {
        self.unreachable.push(node);
        self.shared_unreachable.fetch_add(1, Ordering::Relaxed);
    }

    fn io_settled(&self) -> bool {
        self.io.is_empty() && self.in_flight == 0
    }

    fn resting(&self) -> bool {
        self.rests
    }
}

struct Node {
    tx: MailboxSender<CoreInput<Cmd>>,
    thread: JoinHandle<Toy>,
    acceptor: Acceptor,
}

fn start(id: u64, toy: Toy) -> Node {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let h = hello(id, &addr);
    let cfg = RuntimeConfig { tick: Duration::from_millis(5), seed: id, tls: None, origin: None };
    let (rt, tx) = CoreRuntime::new(toy, MemDisk::new(), h.clone(), cfg).unwrap();
    let acceptor = Acceptor::spawn(listener, h, vec![tx.clone()], CoreInput::Inbound).unwrap();
    let thread = thread::spawn(move || rt.run().unwrap().0);
    Node { tx, thread, acceptor }
}

fn stop(n: Node) -> Toy {
    n.tx.send(CoreInput::Stop).ok().unwrap();
    n.acceptor.stop();
    n.thread.join().unwrap()
}

fn wait(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn two_nodes_play_ping_pong_and_tick() {
    let (a_ticks, b_seen, b_ticks) = (Arc::default(), Arc::default(), Arc::default());
    let a = start(1, Toy { shared_ticks: Arc::clone(&a_ticks), ..Toy::default() });
    let toy_b = Toy {
        shared_seen: Arc::clone(&b_seen),
        shared_ticks: Arc::clone(&b_ticks),
        ..Toy::default()
    };
    let b = start(2, toy_b);
    let (aa, ba) = (a.acceptor.local_addr().to_string(), b.acceptor.local_addr().to_string());
    a.tx.send(CoreInput::App(Cmd::Addr(NodeId(2), ba))).ok().unwrap();
    b.tx.send(CoreInput::App(Cmd::Addr(NodeId(1), aa))).ok().unwrap();
    b.tx.send(CoreInput::App(Cmd::Ping(NodeId(1), 0))).ok().unwrap();
    a.tx.send(CoreInput::App(Cmd::Ping(NodeId(2), 1_000))).ok().unwrap();
    // Until the exchange ends and both cores ticked, however loaded the machine is.
    let load = |x: &Arc<AtomicU64>| x.load(Ordering::Relaxed);
    wait(|| load(&b_seen) >= 1_000 && load(&a_ticks) > 10 && load(&b_ticks) > 10);
    let ta = stop(a);
    let tb = stop(b);
    // A stops answering at its limit, so the last request B saw is the limit.
    assert!(tb.last_seen >= 1_000, "B saw {}", tb.last_seen);
    assert!(ta.ticks > 10 && tb.ticks > 10, "ticks {} {}", ta.ticks, tb.ticks);
    // Every turn that brought a frame read the wall clock first, many times between two ticks
    // (005 FR-032).
    assert!(ta.clocks > ta.ticks && tb.clocks > tb.ticks, "clocks {} {}", ta.clocks, tb.clocks);
}

#[test]
fn disk_operations_complete_in_order() {
    let a = start(3, Toy::default());
    a.tx.send(CoreInput::App(Cmd::Disk)).ok().unwrap();
    thread::sleep(Duration::from_millis(100));
    let t = stop(a);
    assert_eq!(t.done.len(), 3);
    assert_eq!(t.done[2], IoDone::Read { token: 9, data: Ok(Bytes::from_vec(b"abc".to_vec())) });
}

#[test]
fn a_refused_dial_is_retried_with_the_new_address() {
    let refused: Arc<AtomicU64> = Arc::default();
    let heard: Arc<AtomicU64> = Arc::default();
    let a = start(4, Toy { shared_unreachable: Arc::clone(&refused), ..Toy::default() });
    let b = start(5, Toy { shared_seen: Arc::clone(&heard), ..Toy::default() });
    // A closed port refuses the first dials.
    let dead = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    a.tx.send(CoreInput::App(Cmd::Addr(NodeId(5), dead))).ok().unwrap();
    a.tx.send(CoreInput::App(Cmd::Ping(NodeId(5), 3))).ok().unwrap();
    wait(|| refused.load(Ordering::Relaxed) > 0);
    let real = b.acceptor.local_addr().to_string();
    a.tx.send(CoreInput::App(Cmd::Addr(NodeId(5), real))).ok().unwrap();
    let aa = a.acceptor.local_addr().to_string();
    b.tx.send(CoreInput::App(Cmd::Addr(NodeId(4), aa))).ok().unwrap();
    // The frame was dropped with the failed dial: new pings go out after the backoff.
    let deadline = Instant::now() + Duration::from_secs(20);
    while heard.load(Ordering::Relaxed) == 0 {
        assert!(Instant::now() < deadline, "B never heard from A");
        a.tx.send(CoreInput::App(Cmd::Ping(NodeId(5), 3))).ok().unwrap();
        thread::sleep(Duration::from_millis(200));
    }
    let ta = stop(a);
    let tb = stop(b);
    assert!(ta.unreachable.contains(&NodeId(5)));
    assert!(tb.last_seen >= 1, "B never heard from A");
}

#[test]
fn a_reply_goes_to_the_address_the_peer_announced_last() {
    let a = start(10, Toy::default());
    let b = start(11, Toy::default());
    // A's logic still has B at an address nobody listens on (B came back on another port).
    let old = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().to_string();
    a.tx.send(CoreInput::App(Cmd::Addr(NodeId(11), old))).ok().unwrap();
    let aa = a.acceptor.local_addr().to_string();
    b.tx.send(CoreInput::App(Cmd::Addr(NodeId(10), aa))).ok().unwrap();
    // B dials A; A answers by dialing B at the address of B's `Hello`.
    b.tx.send(CoreInput::App(Cmd::Ping(NodeId(10), 1))).ok().unwrap();
    thread::sleep(Duration::from_millis(500));
    let ta = stop(a);
    let tb = stop(b);
    assert_eq!(tb.last_seen, 2, "A's answer never reached B");
    assert!(ta.unreachable.is_empty(), "A dialed the old address");
}

#[test]
fn a_stale_announced_address_gives_way_to_the_logics_after_a_failed_dial() {
    let a = start(12, Toy::default());
    let aa = a.acceptor.local_addr().to_string();
    // B dials A from a port it then leaves: A learns an address that dies.
    let b = start(13, Toy::default());
    b.tx.send(CoreInput::App(Cmd::Addr(NodeId(12), aa))).ok().unwrap();
    b.tx.send(CoreInput::App(Cmd::Ping(NodeId(12), 1))).ok().unwrap();
    thread::sleep(Duration::from_millis(200));
    drop(stop(b));
    // B comes back elsewhere and does not dial; A's logic has the new address.
    let b = start(13, Toy::default());
    let ba = b.acceptor.local_addr().to_string();
    a.tx.send(CoreInput::App(Cmd::Addr(NodeId(13), ba))).ok().unwrap();
    for _ in 0..30 {
        a.tx.send(CoreInput::App(Cmd::Ping(NodeId(13), 1))).ok().unwrap();
        thread::sleep(Duration::from_millis(100));
    }
    let tb = stop(b);
    drop(stop(a));
    assert!(tb.last_seen >= 1, "A kept dialing the address B left");
}

#[test]
fn a_peer_that_goes_away_is_reported_unreachable() {
    let a = start(6, Toy::default());
    let b = start(7, Toy::default());
    let ba = b.acceptor.local_addr().to_string();
    a.tx.send(CoreInput::App(Cmd::Addr(NodeId(7), ba))).ok().unwrap();
    a.tx.send(CoreInput::App(Cmd::Ping(NodeId(7), 1))).ok().unwrap();
    thread::sleep(Duration::from_millis(200));
    drop(stop(b));
    // The closed connection is noticed when A next writes or reads on it.
    for _ in 0..20 {
        a.tx.send(CoreInput::App(Cmd::Ping(NodeId(7), 1))).ok().unwrap();
        thread::sleep(Duration::from_millis(20));
    }
    let ta = stop(a);
    assert!(ta.unreachable.contains(&NodeId(7)));
}

#[test]
fn an_unknown_address_drops_the_frames() {
    let a = start(8, Toy::default());
    a.tx.send(CoreInput::App(Cmd::Ping(NodeId(99), 1))).ok().unwrap();
    thread::sleep(Duration::from_millis(50));
    let ta = stop(a);
    assert!(ta.unreachable.is_empty());
}

#[test]
fn garbage_after_the_handshake_closes_the_connection() {
    let a = start(9, Toy::default());
    let mut s = TcpStream::connect(a.acceptor.local_addr()).unwrap();
    let got = handshake::dial(&mut s, &hello(10, "x")).unwrap();
    assert_eq!(got.hello.node_id, NodeId(9));
    // A frame with an unknown kind.
    s.write_all(&[4, 0, 0, 0, 0xEE, 0, 0, 0]).unwrap();
    let mut good = Vec::new();
    encode_frame(&ack(5), false, &mut good).unwrap();
    let _ = s.write_all(&good);
    thread::sleep(Duration::from_millis(100));
    let ta = stop(a);
    assert_eq!(ta.last_seen, 0, "a frame after an invalid one was delivered");
}

#[test]
fn handshakes_are_validated() {
    let ours = hello(1, "a");
    let mut other = hello(2, "b");
    assert!(handshake::validate(&ours, &other).is_ok());
    other.cluster_id = [0; 16];
    assert!(matches!(handshake::validate(&ours, &other), Err(HandshakeError::ClusterMismatch)));
    other = Hello { cores: 2, ..hello(2, "b") };
    assert!(matches!(
        handshake::validate(&ours, &other),
        Err(HandshakeError::CoreCountMismatch { local: 1, remote: 2 })
    ));
    other = Hello { core: 1, ..hello(2, "b") };
    assert!(matches!(handshake::validate(&ours, &other), Err(HandshakeError::BadCore(1))));
    other = hello(0, "b");
    let e = handshake::validate(&ours, &other).unwrap_err();
    assert!(e.to_string().contains("NoNodeId"));

    // Over a socket: a server that answers with something other than a Hello, and one that
    // closes, and a dial to a node with another identity.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let mut f = Vec::new();
        encode_frame(&ack(1), false, &mut f).unwrap();
        s.write_all(&f).unwrap();
        let (s2, _) = l.accept().unwrap();
        drop(s2);
        let (mut s3, _) = l.accept().unwrap();
        handshake::answer(&mut s3, &hello(3, "c")).unwrap();
    });
    let mut s = TcpStream::connect(addr).unwrap();
    assert!(matches!(handshake::dial(&mut s, &ours), Err(HandshakeError::NotHello)));
    let mut s = TcpStream::connect(addr).unwrap();
    assert!(matches!(handshake::dial(&mut s, &ours), Err(HandshakeError::Io(_))));
    let (tx, mut rx) = teseladb_io::mailbox::channel(std::sync::Arc::new(
        teseladb_io::poller::Poller::new().unwrap().waker(),
    ));
    handshake::connect(NodeId(4), addr.to_string(), ours.clone(), tx, |o| o);
    let mut got = Vec::new();
    wait(|| !rx.is_empty());
    rx.drain_into(&mut got);
    assert!(matches!(
        got[0].result,
        Err(HandshakeError::UnexpectedNode { expected: NodeId(4), got: NodeId(3) })
    ));
    server.join().unwrap();
}

/// A fake node: accepts one connection on a listener of its own and answers the handshake as
/// node `id`, followed by `extra` bytes.
fn fake_peer(id: u64, extra: &'static [u8]) -> (String, JoinHandle<TcpStream>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let h = hello(id, &addr);
    let t = thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        handshake::answer(&mut s, &h).unwrap();
        s.write_all(extra).unwrap();
        s
    });
    (addr, t)
}

fn count_goodbyes(s: &mut TcpStream, want: usize) -> usize {
    use std::io::Read;
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 1 << 16];
    let mut seen = 0;
    while seen < want {
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "closed after {seen} frames");
        buf.extend_from_slice(&chunk[..n]);
        while let Some((msg, used)) = teseladb_io::peer::decode_frame(&buf).unwrap() {
            assert!(matches!(msg, PeerMsg::Goodbye { .. }));
            buf.drain(..used);
            seen += 1;
        }
    }
    seen
}

#[test]
fn a_slow_reader_gets_everything_once_it_reads() {
    let a = start(20, Toy { deadline: true, ..Toy::default() });
    let (addr, peer) = fake_peer(21, b"");
    a.tx.send(CoreInput::App(Cmd::Addr(NodeId(21), addr))).ok().unwrap();
    // 24 MiB with nobody reading: the socket buffers fill and the rest waits for writability.
    a.tx.send(CoreInput::App(Cmd::Bulk(NodeId(21), 24, 1 << 20))).ok().unwrap();
    let mut s = peer.join().unwrap();
    thread::sleep(Duration::from_millis(200));
    // What went out before the connection was up was queued (up to 8 MiB); the rest of the burst
    // was sent on the connection.
    let more = 6;
    a.tx.send(CoreInput::App(Cmd::Bulk(NodeId(21), more, 1 << 20))).ok().unwrap();
    let got = count_goodbyes(&mut s, 8 + more - 1);
    assert!(got >= 8 + more - 1);
    let ta = stop(a);
    assert!(ta.unreachable.is_empty());
}

#[test]
fn an_oversized_frame_is_dropped_and_the_link_keeps_working() {
    let a = start(22, Toy::default());
    let (addr, peer) = fake_peer(23, b"");
    a.tx.send(CoreInput::App(Cmd::Addr(NodeId(23), addr))).ok().unwrap();
    a.tx.send(CoreInput::App(Cmd::Bulk(NodeId(23), 1, 17 << 20))).ok().unwrap();
    a.tx.send(CoreInput::App(Cmd::Bulk(NodeId(23), 2, 10))).ok().unwrap();
    let mut s = peer.join().unwrap();
    assert_eq!(count_goodbyes(&mut s, 2), 2);
    stop(a);
}

#[test]
fn garbage_after_the_peers_hello_closes_the_new_link() {
    let a = start(24, Toy::default());
    let (addr, peer) = fake_peer(25, &[4, 0, 0, 0, 0xEE, 0, 0, 0]);
    a.tx.send(CoreInput::App(Cmd::Addr(NodeId(25), addr))).ok().unwrap();
    a.tx.send(CoreInput::App(Cmd::Bulk(NodeId(25), 1, 10))).ok().unwrap();
    let _s = peer.join().unwrap();
    thread::sleep(Duration::from_millis(200));
    let ta = stop(a);
    assert!(ta.unreachable.contains(&NodeId(25)));
}

struct PanicDisk;

impl teseladb_raftlog::Disk for PanicDisk {
    fn list(&mut self, _dir: &std::path::Path) -> Result<Vec<PathBuf>, teseladb_raftlog::IoErrno> {
        Ok(Vec::new())
    }
    fn read_all(&mut self, _path: &std::path::Path) -> Result<Vec<u8>, teseladb_raftlog::IoErrno> {
        Ok(Vec::new())
    }
    fn execute(&mut self, _op: IoOp) -> IoDone {
        panic!("disk failure in the test");
    }
}

#[test]
fn the_core_stops_when_its_disk_thread_dies() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let h = hello(26, &l.local_addr().unwrap().to_string());
    let (rt, tx) =
        CoreRuntime::new(Toy::default(), PanicDisk, h, RuntimeConfig::default()).unwrap();
    let t = thread::spawn(move || rt.run().unwrap());
    // The first operation kills the disk thread; a later one finds it gone and stops the core.
    for _ in 0..50 {
        if tx.send(CoreInput::App(Cmd::Disk)).is_err() || t.is_finished() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    let (toy, disk) = t.join().unwrap();
    assert!(disk.is_none());
    assert!(toy.done.is_empty());
}

#[test]
fn the_acceptor_closes_failed_handshakes() {
    let a = start(27, Toy::default());
    // Garbage instead of a Hello, and a peer that closes without saying anything.
    let mut s = TcpStream::connect(a.acceptor.local_addr()).unwrap();
    s.write_all(&[4, 0, 0, 0, 0xEE, 0, 0, 0]).unwrap();
    let s2 = TcpStream::connect(a.acceptor.local_addr()).unwrap();
    s2.shutdown(std::net::Shutdown::Write).unwrap();
    // A peer of another cluster.
    let mut s3 = TcpStream::connect(a.acceptor.local_addr()).unwrap();
    let other = Hello { cluster_id: [1; 16], ..hello(28, "x") };
    assert!(handshake::dial(&mut s3, &other).is_err());
    thread::sleep(Duration::from_millis(100));
    let ta = stop(a);
    assert_eq!(ta.last_seen, 0);
}

#[test]
fn the_logic_is_reachable_before_the_loop_runs() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let cfg = RuntimeConfig { tick: Duration::from_millis(5), seed: 40, tls: None, origin: None };
    let (mut rt, tx) =
        CoreRuntime::new(Toy::default(), MemDisk::new(), hello(40, &addr), cfg).unwrap();
    rt.logic().addrs.insert(NodeId(41), "set before running".into());
    let thread = thread::spawn(move || rt.run().unwrap().0);
    tx.send(CoreInput::Stop).ok().unwrap();
    let toy = thread.join().unwrap();
    assert_eq!(toy.addrs[&NodeId(41)], "set before running");
}

#[test]
fn a_stop_waits_for_the_writes_the_logic_still_holds() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let (rt, tx) = CoreRuntime::new(
        Toy::default(),
        MemDisk::new(),
        hello(42, &addr),
        RuntimeConfig::default(),
    )
    .unwrap();
    // Both reach the first turn: the creation goes to the disk, the write waits for it, and the
    // stop comes before its completion.
    tx.send(CoreInput::App(Cmd::OneByOne)).ok().unwrap();
    tx.send(CoreInput::Stop).ok().unwrap();
    let (toy, disk) = rt.run().unwrap();
    assert_eq!(toy.done.len(), 2, "{:?}", toy.done);
    assert!(toy.io.is_empty() && toy.in_flight == 0);
    let mut disk = disk.expect("the disk thread lives");
    assert_eq!(disk.read_all(std::path::Path::new("x")).unwrap(), b"abc");
}

#[test]
fn a_stop_does_not_wait_for_a_disk_thread_that_died() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let h = hello(43, &l.local_addr().unwrap().to_string());
    let (rt, tx) =
        CoreRuntime::new(Toy::default(), PanicDisk, h, RuntimeConfig::default()).unwrap();
    // The creation kills the disk thread: its completion never comes, and the stop ends anyway.
    tx.send(CoreInput::App(Cmd::OneByOne)).ok().unwrap();
    tx.send(CoreInput::Stop).ok().unwrap();
    let start = Instant::now();
    let (toy, disk) = rt.run().unwrap();
    assert!(disk.is_none());
    assert!(toy.done.is_empty());
    assert!(start.elapsed() < Duration::from_secs(5), "{:?}", start.elapsed());
}

/// A node of node `node` that speaks protocol versions `min` to `max`.
fn speaking(node: u64, min: u16, max: u16) -> Hello {
    Hello { versions: Versions { min, max }, ..hello(node, "x") }
}

#[test]
fn two_nodes_speak_the_highest_version_both_have() {
    // An upgraded node (4 and 5) with one that is not (4), both upgraded, and the other way round.
    let agreed = |a: (u16, u16), b: (u16, u16)| {
        handshake::validate(&speaking(1, a.0, a.1), &speaking(2, b.0, b.1)).ok()
    };
    assert_eq!(agreed((4, 5), (4, 4)), Some(4));
    assert_eq!(agreed((4, 4), (4, 5)), Some(4));
    assert_eq!(agreed((4, 5), (4, 5)), Some(5));
    assert_eq!(agreed((4, 5), (5, 6)), Some(5));
    assert_eq!(agreed((5, 6), (4, 5)), Some(5));
    // No version in common: the handshake fails and names both ranges.
    let e = handshake::validate(&speaking(1, 4, 4), &speaking(2, 5, 6)).unwrap_err();
    assert!(
        matches!(e, HandshakeError::VersionMismatch { local, remote }
            if local == Versions { min: 4, max: 4 } && remote == Versions { min: 5, max: 6 }),
        "{e}"
    );
    // Over a socket, mid-upgrade: the upgraded node dials one that only speaks 4, and both
    // speak 4.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        handshake::answer(&mut s, &speaking(3, 4, 4)).unwrap().version
    });
    let mut s = TcpStream::connect(addr).unwrap();
    let got = handshake::dial(&mut s, &speaking(1, 4, 5)).unwrap();
    assert_eq!((got.version, got.hello.versions), (4, Versions { min: 4, max: 4 }));
    assert_eq!(server.join().unwrap(), 4);
}

#[test]
fn an_idle_core_sleeps_without_spinning_and_spins_only_after_work() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let cfg = RuntimeConfig { tick: Duration::from_millis(1), seed: 50, tls: None, origin: None };
    let (mut rt, tx) =
        CoreRuntime::new(Toy::default(), MemDisk::new(), hello(50, &addr), cfg).unwrap();
    // Ticks alone: every turn wakes up for its tick and sleeps again without spinning.
    for _ in 0..20 {
        rt.turn().unwrap();
    }
    assert_eq!(rt.spins(), 0, "an idle core does not spin");
    // A message is work: the turn after the one that took it spins before sleeping.
    tx.send(CoreInput::App(Cmd::Addr(NodeId(51), "x".into()))).ok().unwrap();
    rt.turn().unwrap();
    rt.turn().unwrap();
    assert_eq!(rt.spins(), 1, "the turn after some work spins");
    // And with nothing more, the core goes back to sleeping without spinning.
    for _ in 0..5 {
        rt.turn().unwrap();
    }
    assert_eq!(rt.spins(), 1);
}

#[test]
fn a_core_at_rest_ticks_ten_times_less_and_ticks_at_once_when_work_arrives() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let cfg = RuntimeConfig { tick: Duration::from_millis(10), seed: 60, tls: None, origin: None };
    let toy = Toy { rests: true, ..Toy::default() };
    let (mut rt, tx) = CoreRuntime::new(toy, MemDisk::new(), hello(60, &addr), cfg).unwrap();
    let run_for = |rt: &mut CoreRuntime<Toy, MemDisk>, d: Duration| {
        let start = Instant::now();
        while start.elapsed() < d {
            rt.turn().unwrap();
        }
    };
    // A second at 10 ms before it rests, then 100 ms: about 100 + 10 in 2 s, against 200.
    run_for(&mut rt, Duration::from_secs(2));
    let rested = rt.logic().ticks;
    assert!(rested <= 140, "{rested} ticks in 2 s");
    // Work ends the rest: the turn that takes it wakes the logic first, with a fresh wall clock,
    // and leaves the whole tick for its time.
    tx.send(CoreInput::App(Cmd::Addr(NodeId(61), "x".into()))).ok().unwrap();
    rt.turn().unwrap();
    assert_eq!(rt.logic().wakes, 1, "the clock part of a tick before the work");
    assert_eq!(rt.logic().ticks, rested, "and no whole tick");
    // And the ticks go back to every 10 ms: about 20 in 200 ms, against 2 at rest.
    run_for(&mut rt, Duration::from_millis(200));
    assert!(rt.logic().ticks >= rested + 8, "{} ticks", rt.logic().ticks - rested);
}

#[test]
fn a_core_whose_logic_is_busy_never_rests() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let cfg = RuntimeConfig { tick: Duration::from_millis(10), seed: 62, tls: None, origin: None };
    let (mut rt, _tx) =
        CoreRuntime::new(Toy::default(), MemDisk::new(), hello(62, &addr), cfg).unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1_500) {
        rt.turn().unwrap();
    }
    // Every 10 ms all along: about 150.
    assert!(rt.logic().ticks >= 120, "{} ticks", rt.logic().ticks);
}
