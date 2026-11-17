//! A toy core logic and toy nodes on the engine, for the tests of the core's task and of the
//! connections between nodes.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use teseladb_io::IoClass;
use teseladb_io::engine::{
    CoreConfig, CoreDisk, CoreHandle, CoreInput, CoreLogic, CoreSender, DiskWaker, NodeEngine,
};
use teseladb_io::handshake::{self, AcceptorOptions};
use teseladb_io::peer::{Hello, PeerMsg, Versions};
use teseladb_io::secure::PeerTls;
use teseladb_io::service::ServiceClass;
use teseladb_raftlog::{Disk, FileId, IoDone, IoOp, MemDisk};
use teseladb_types::{Bytes, Nanos, NodeId};

pub const CLUSTER: [u8; 16] = [7; 16];

pub fn hello(node: u64, addr: &str) -> Hello {
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

pub enum Cmd {
    /// Start a ping-pong of `n` rounds with a node.
    Ping(NodeId, u64),
    /// Change a node's address.
    Addr(NodeId, String),
    /// Write and read back through the disk.
    Disk,
    /// Send `n` `Goodbye` frames of `size` bytes to a node.
    Bulk(NodeId, usize, usize),
    /// Create a file and write to it, one operation at a time.
    OneByOne,
    /// Keeps the core busy for this long (a request that costs it CPU).
    Stall(Duration),
    /// A new request, refused when the core is overloaded.
    Request(u64),
    /// A long scan, in the background class (012 FR-030).
    Scan(u64),
    /// Hand this message to another core through its sender (from the core's own shard).
    Relay(CoreSender<Cmd>, Box<Cmd>),
}

#[derive(Default)]
pub struct Toy {
    pub addrs: BTreeMap<NodeId, String>,
    pub limit: u64,
    pub last_seen: u64,
    pub ticks: u64,
    pub io: Vec<IoOp>,
    pub done: Vec<IoDone>,
    pub out: Vec<(NodeId, PeerMsg)>,
    pub unreachable: Vec<NodeId>,
    /// Report a (far) deadline of its own.
    pub deadline: bool,
    pub shared_seen: Arc<AtomicU64>,
    pub shared_ticks: Arc<AtomicU64>,
    pub shared_unreachable: Arc<AtomicU64>,
    pub shared_wakes: Arc<AtomicU64>,
    pub one_by_one: bool,
    pub in_flight: usize,
    /// Says it is at rest ([`CoreLogic::resting`]).
    pub rests: bool,
    pub wakes: u64,
    pub clocks: u64,
    pub served: Vec<u64>,
    pub refused: Vec<u64>,
    /// What [`CoreLogic::file_sends`] was told.
    pub file_sends: Option<bool>,
    /// Every frame received, with its sender.
    pub frames: Vec<(NodeId, PeerMsg)>,
}

pub fn ack(req: u64) -> PeerMsg {
    PeerMsg::ForwardResp { req, hlc: None, body: Bytes::new() }
}

/// The class the toy gives an operation: its writes are background work, as a compaction's
/// (012 FR-028), the rest the data's.
pub fn class_of(op: &IoOp) -> IoClass {
    match op {
        IoOp::WriteAt { .. } => IoClass::Background,
        _ => IoClass::Data,
    }
}

/// The class of the operation a result answers.
pub fn done_class(done: &IoDone) -> IoClass {
    match done {
        IoDone::Ok(op) | IoDone::Err { op, .. } => class_of(op),
        IoDone::Read { .. } => IoClass::Data,
    }
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
        self.shared_wakes.store(self.wakes, Ordering::Relaxed);
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
        let op = self.io.remove(0);
        let class = class_of(&op);
        Some((op, class))
    }

    fn on_io_done(&mut self, done: IoDone, class: IoClass, _now: Nanos) {
        assert_eq!(class, done_class(&done), "each result with the class of its operation");
        self.in_flight -= 1;
        self.done.push(done);
    }

    fn next_deadline(&self) -> Option<Nanos> {
        self.deadline.then_some(Nanos::from_secs(3_600))
    }

    fn on_peer(&mut self, from: NodeId, msg: PeerMsg, _now: Nanos) {
        self.frames.push((from, msg.clone()));
        let PeerMsg::ForwardResp { req, .. } = msg else { return };
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
            Cmd::Stall(d) => thread::sleep(d),
            Cmd::Request(id) | Cmd::Scan(id) => self.served.push(id),
            Cmd::Relay(to, msg) => {
                let _ = to.send(CoreInput::App(*msg));
            }
        }
    }

    fn service_class(&self, msg: &Cmd) -> ServiceClass {
        match msg {
            Cmd::Scan(_) => ServiceClass::Background,
            _ => ServiceClass::Foreground,
        }
    }

    fn refuse_overloaded(&mut self, msg: Cmd) -> Option<Cmd> {
        match msg {
            Cmd::Request(id) => {
                self.refused.push(id);
                None
            }
            other => Some(other),
        }
    }

    fn take_outbound(&mut self, out: &mut Vec<(NodeId, PeerMsg)>) {
        out.append(&mut self.out);
    }

    fn file_sends(&mut self, on: bool) {
        self.file_sends = Some(on);
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

/// What a toy core gives back when it stops: its logic, its disk (`None` if the disk thread
/// died) and whether the disk was gone.
pub type Stopped<D> = (Toy, Option<D>, bool);

/// A toy node: one core on a shard of the test's engine, with its peer listener.
pub struct Node<D: Disk + Send + 'static = MemDisk> {
    pub tx: CoreSender<Cmd>,
    pub addr: SocketAddr,
    pub core: CoreHandle<Cmd, Stopped<D>>,
}

/// A port free now on the loopback interface.
pub fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap()
}

/// A toy core of node `h.node_id` (its core `h.core`) on `shard`, with its disk and `tick`.
pub fn start_core<D: Disk + Send + 'static>(
    engine: &NodeEngine,
    shard: u16,
    h: &Hello,
    toy: Toy,
    disk: D,
    tick: Duration,
    tls: Option<PeerTls>,
) -> CoreHandle<Cmd, Stopped<D>> {
    start_core_built(engine, shard, h, tick, tls, move || (toy, disk))
}

/// As [`start_core`], with the toy and its disk made by `build` where the engine builds the core.
pub fn start_core_built<D: Disk + Send + 'static>(
    engine: &NodeEngine,
    shard: u16,
    h: &Hello,
    tick: Duration,
    tls: Option<PeerTls>,
    build: impl FnOnce() -> (Toy, D) + Send + 'static,
) -> CoreHandle<Cmd, Stopped<D>> {
    let seed = h.node_id.0 * 16 + u64::from(h.core);
    let cfg = CoreConfig { hello: h.clone(), tick, seed, tls, origin: Instant::now() };
    engine
        .start_core(
            shard,
            cfg,
            move || {
                let (toy, disk) = build();
                Ok::<_, std::io::Error>((toy, InlineDisk::new(disk), None))
            },
            |toy, disk: InlineDisk<D>, gone| (toy, disk.disk, gone),
        )
        .unwrap()
}

/// The disk executor of the toy cores: runs each operation of `D` on the shard when the task
/// hands it out, and gives its result at the next reap. A disk that panics is gone: it completes
/// nothing more and is not given back.
pub struct InlineDisk<D> {
    disk: Option<D>,
    done: Vec<(IoDone, IoClass)>,
    waker: Option<DiskWaker>,
}

impl<D> InlineDisk<D> {
    pub fn new(disk: D) -> InlineDisk<D> {
        InlineDisk { disk: Some(disk), done: Vec::new(), waker: None }
    }
}

impl<D: Disk + Send + 'static> CoreDisk for InlineDisk<D> {
    fn attach(&mut self, waker: DiskWaker) {
        self.waker = Some(waker);
    }

    fn submit(&mut self, op: IoOp, class: IoClass) -> bool {
        let Some(d) = self.disk.as_mut() else { return false };
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.execute(op))) {
            Ok(done) => {
                self.done.push((done, class));
                if let Some(w) = &self.waker {
                    w.wake();
                }
                true
            }
            Err(_) => {
                self.disk = None;
                false
            }
        }
    }

    fn flush(&mut self) -> bool {
        self.disk.is_some()
    }

    fn reap(&mut self, out: &mut Vec<(IoDone, IoClass)>) {
        out.append(&mut self.done);
    }

    fn ready(&self) -> bool {
        !self.done.is_empty()
    }

    fn alive(&self) -> bool {
        self.disk.is_some()
    }
}

/// Starts node `id` on `shard` with `toy`, its disk and `tick`.
pub fn start_with<D: Disk + Send + 'static>(
    engine: &NodeEngine,
    shard: u16,
    id: u64,
    toy: Toy,
    disk: D,
    tick: Duration,
    tls: Option<PeerTls>,
) -> Node<D> {
    let addr = free_port();
    let h = hello(id, &addr.to_string());
    let core = start_core(engine, shard, &h, toy, disk, tick, tls.clone());
    let opts = AcceptorOptions { tls, ..AcceptorOptions::default() };
    let bound = engine.listen_peers(addr, &h, &opts, shard, 1).unwrap();
    assert_eq!(bound, addr);
    Node { tx: core.sender(), addr, core }
}

pub fn start(engine: &NodeEngine, shard: u16, id: u64, toy: Toy) -> Node {
    start_with(engine, shard, id, toy, MemDisk::new(), Duration::from_millis(5), None)
}

pub fn send(n: &Node<impl Disk + Send + 'static>, c: Cmd) {
    assert!(n.tx.send(CoreInput::App(c)).is_ok());
}

pub fn stop<D: Disk + Send + 'static>(engine: &NodeEngine, n: Node<D>) -> Stopped<D> {
    engine.stop_peers(Some(n.addr));
    n.tx.send(CoreInput::Stop).ok().unwrap();
    n.core.join().unwrap()
}

pub fn wait(cond: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(5));
    }
}

pub fn load(x: &Arc<AtomicU64>) -> u64 {
    x.load(Ordering::Relaxed)
}

/// A fake node: accepts one connection on a listener of its own and answers the handshake as
/// node `id`, followed by `extra` bytes.
pub fn fake_peer(id: u64, extra: &'static [u8]) -> (String, JoinHandle<TcpStream>) {
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

pub fn count_goodbyes(s: &mut TcpStream, want: usize) -> usize {
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
