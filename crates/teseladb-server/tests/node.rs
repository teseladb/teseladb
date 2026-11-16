//! A node over real files (001 FR-018): it recovers its identity and Group 0 after a restart, takes
//! the data directory's lock, refuses another number of cores, and joins a cluster with `--join`,
//! also through a follower and after a restart.

#[path = "common/procs.rs"]
mod procs;

use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_io::engine::disk::DiskPlace;
use teseladb_io::files::BlockingDisk;
use teseladb_io::handshake::JoinRequest;

use teseladb_io::peer::{Hello, PeerMsg, decode_frame, encode_frame};
use teseladb_raft::Role;
use teseladb_server::logic::Group0View;
use teseladb_server::node::{
    FOUNDER, JOIN_DEADLINE, JOIN_VOTERS, Node, NodeConfig, NodeError, join_cluster, lock_dir,
};
use teseladb_server::procs::{NodeArgs, NodeProc};
use teseladb_types::NodeId;

fn main() {
    procs::main(
        &[
            (
                "a_restarted_node_keeps_its_identity_and_group0",
                a_restarted_node_keeps_its_identity_and_group0,
            ),
            ("the_data_directory_has_one_owner", the_data_directory_has_one_owner),
            (
                "nodes_join_with_join_also_through_a_follower_and_after_a_restart",
                nodes_join_with_join_also_through_a_follower_and_after_a_restart,
            ),
            (
                "a_joined_node_without_its_voters_does_not_start",
                a_joined_node_without_its_voters_does_not_start,
            ),
            (
                "a_node_that_stopped_before_its_identifier_joins_again_and_never_founds",
                a_node_that_stopped_before_its_identifier_joins_again_and_never_founds,
            ),
            (
                "a_node_back_on_another_port_keeps_the_cluster_whole",
                a_node_back_on_another_port_keeps_the_cluster_whole,
            ),
            (
                "joining_fails_with_another_core_count_or_nobody_to_ask",
                joining_fails_with_another_core_count_or_nobody_to_ask,
            ),
            ("errors_say_what_failed", errors_say_what_failed),
            (
                "a_join_round_follows_a_bounded_number_of_hints",
                a_join_round_follows_a_bounded_number_of_hints,
            ),
            (
                "a_damaged_node_meta_stops_the_node_without_touching_the_directory",
                a_damaged_node_meta_stops_the_node_without_touching_the_directory,
            ),
            (
                "the_clock_bound_stays_ahead_and_a_restart_waits_past_it",
                the_clock_bound_stays_ahead_and_a_restart_waits_past_it,
            ),
        ],
        &[("files", files_config)],
    );
}

const READY: Duration = Duration::from_secs(10);
const ASK: Duration = Duration::from_secs(1);

/// A data directory removed when the test ends.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("teseladb-node-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Starts a node: a join waits as long as the product does (`JOIN_DEADLINE`) and ends as soon as
/// the cluster accepts it, whatever the host's speed (a slow emulated host took more than 5 s).
fn start(dir: &Dir, cores: Option<u16>, join: Vec<String>) -> Result<Node, NodeError> {
    start_within(dir, cores, join, JOIN_DEADLINE)
}

/// Starts a node whose join gives up after `deadline`: for a join the cluster refuses, which
/// the node asks again until then.
fn start_within(
    dir: &Dir,
    cores: Option<u16>,
    join: Vec<String>,
    deadline: Duration,
) -> Result<Node, NodeError> {
    let cfg = NodeConfig { cores, join, join_deadline: deadline, ..NodeConfig::default() };
    let path = dir.0.clone();
    Node::start(cfg, DiskPlace::Files(std::path::PathBuf::from(&path)), Some(lock_dir(&dir.0)?))
}

/// The node of a cluster's test, in a process of its own (D4: one process per node), over the
/// files of `dir`: as [`start_within`] starts one, listening between nodes at `peer` if given.
fn spawn_within(
    dir: &Dir,
    cores: Option<u16>,
    join: Vec<String>,
    deadline: Duration,
    peer: Option<std::net::SocketAddr>,
) -> Result<NodeProc, NodeError> {
    let mut params = vec![format!("deadline_ms={}", deadline.as_millis())];
    params.extend(cores.map(|c| format!("cores={c}")));
    params.extend(peer.map(|p| format!("peer={p}")));
    let params: Vec<&str> = params.iter().map(String::as_str).collect();
    teseladb_server::procs::spawn_self("files", &join, Some(&dir.0), &params)
        .map_err(|e| e.node_error())
}

/// [`spawn_within`] with the product's join deadline.
fn spawn(dir: &Dir, cores: Option<u16>, join: Vec<String>) -> Result<NodeProc, NodeError> {
    spawn_within(dir, cores, join, JOIN_DEADLINE, None)
}

/// The configuration of [`spawn_within`]'s node process.
fn files_config(a: &NodeArgs) -> NodeConfig {
    let number = |k: &str| a.param(k).map(|v| v.parse::<u64>().expect("a number"));
    NodeConfig {
        cores: number("cores").map(|c| u16::try_from(c).expect("cores")),
        join: a.join.clone(),
        join_deadline: number("deadline_ms").map_or(JOIN_DEADLINE, Duration::from_millis),
        listen_peer: a
            .param("peer")
            .map_or_else(|| NodeConfig::default().listen_peer, |p| p.parse().expect("an address")),
        ..NodeConfig::default()
    }
}

/// A node in this process or in its own.
trait Views {
    fn view_within(&self, timeout: Duration) -> Option<Group0View>;
    fn ready_within(&self, timeout: Duration) -> bool;
}

impl Views for Node {
    fn view_within(&self, timeout: Duration) -> Option<Group0View> {
        self.view(timeout)
    }

    fn ready_within(&self, timeout: Duration) -> bool {
        self.wait_ready(timeout)
    }
}

impl Views for NodeProc {
    fn view_within(&self, timeout: Duration) -> Option<Group0View> {
        self.view(timeout)
    }

    fn ready_within(&self, timeout: Duration) -> bool {
        self.wait_ready(timeout)
    }
}

/// Waits for `n` to be ready; if it is not, fails with what its Group 0 looks like.
fn ready(n: &impl Views) {
    assert!(n.ready_within(READY), "not ready after {READY:?}: {:?}", n.view_within(ASK));
}

fn wait_view(n: &impl Views, ok: impl Fn(&Group0View) -> bool) -> Group0View {
    let end = Instant::now() + READY;
    loop {
        let v = n.view_within(ASK).expect("core 0 answers");
        if ok(&v) || Instant::now() >= end {
            return v;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn a_restarted_node_keeps_its_identity_and_group0() {
    let dir = Dir::new("restart");
    let n = start(&dir, Some(2), Vec::new()).unwrap();
    ready(&n);
    let (id, cluster) = (n.node_id(), n.cluster_id());
    assert_eq!((id, n.cores()), (FOUNDER, 2));
    assert!(format!("{n:?}").contains("node_id"));
    assert_eq!(n.stop().len(), 2);

    // Without --cores the directory's cores are taken.
    let n = start(&dir, None, Vec::new()).unwrap();
    ready(&n);
    assert_eq!((n.node_id(), n.cluster_id(), n.cores()), (id, cluster, 2));
    let v = wait_view(&n, |v| v.role == Some(Role::Leader));
    assert_eq!((v.voters.as_slice(), v.nodes.as_slice()), (&[FOUNDER][..], &[FOUNDER][..]));
    n.stop();

    // Another number of cores is refused, with the one to use.
    let e = start(&dir, Some(3), Vec::new()).unwrap_err();
    assert!(e.to_string().contains("created with 2 cores and 3 were requested"), "{e}");
}

fn the_data_directory_has_one_owner() {
    let dir = Dir::new("lock");
    let n = start(&dir, Some(1), Vec::new()).unwrap();
    let e = lock_dir(&dir.0).unwrap_err();
    assert!(matches!(e, NodeError::Locked));
    assert_eq!(e.to_string(), "another process is using the data directory");
    n.stop();
    drop(lock_dir(&dir.0).expect("free once the node stopped"));
}

fn nodes_join_with_join_also_through_a_follower_and_after_a_restart() {
    let (d1, d2, d3) = (Dir::new("join-1"), Dir::new("join-2"), Dir::new("join-3"));
    let n1 = spawn(&d1, Some(1), Vec::new()).unwrap();
    ready(&n1);
    let n2 = spawn(&d2, None, vec![n1.peer_addr().to_string()]).unwrap();
    assert_eq!((n2.node_id(), n2.cluster_id(), n2.cores()), (NodeId(2), n1.cluster_id(), 1));
    ready(&n2);
    assert!(fs::read_to_string(d2.0.join(JOIN_VOTERS)).unwrap().contains('1'));
    // The third asks the follower first: the `not_leader` hint takes it to the leader.
    let v = wait_view(&n2, |v| v.voters.len() == 2);
    assert_eq!(v.voters, [NodeId(1), NodeId(2)]);
    let seeds = vec![n2.peer_addr().to_string()];
    let n3 = spawn(&d3, None, seeds).unwrap();
    assert_eq!(n3.node_id(), NodeId(3));
    let ids = [NodeId(1), NodeId(2), NodeId(3)];
    assert_eq!(wait_view(&n1, |v| v.voters == ids).voters, ids);

    // A node that joined restarts from its log, without --join.
    n3.stop();
    let n3 = spawn(&d3, None, Vec::new()).unwrap();
    assert_eq!((n3.node_id(), n3.cluster_id()), (NodeId(3), n1.cluster_id()));
    ready(&n3);
    assert_eq!(wait_view(&n3, |v| v.voters == ids).voters, ids);
    for n in [n1, n2, n3] {
        n.stop();
    }
}

fn a_joined_node_without_its_voters_does_not_start() {
    let (d1, d2) = (Dir::new("voters-1"), Dir::new("voters-2"));
    let n1 = spawn(&d1, Some(1), Vec::new()).unwrap();
    ready(&n1);
    let n2 = spawn(&d2, None, vec![n1.peer_addr().to_string()]).unwrap();
    n2.stop();
    fs::remove_file(d2.0.join(JOIN_VOTERS)).unwrap();
    let e = spawn(&d2, None, Vec::new()).unwrap_err();
    assert!(matches!(&e, NodeError::Config(m) if m.contains(JOIN_VOTERS)), "{e}");
    fs::write(d2.0.join(JOIN_VOTERS), "1\nx\n").unwrap();
    let e = spawn(&d2, None, Vec::new()).unwrap_err();
    assert!(e.to_string().contains("not a list of node ids"), "{e}");
    n1.stop();
}

/// `node.meta` as it is between `open_node` and the write of the node's identifier.
fn unassign(dir: &Dir) {
    let mut disk = BlockingDisk::open(&dir.0).unwrap();
    let mut meta = teseladb_raftlog::read_node_meta(&mut disk).unwrap().expect("node.meta");
    meta.node_id = 0;
    meta.cluster_id = [9; 16];
    teseladb_raftlog::write_node_meta(&mut disk, &meta).unwrap();
}

fn a_node_that_stopped_before_its_identifier_joins_again_and_never_founds() {
    let (d1, d2) = (Dir::new("unassigned-1"), Dir::new("unassigned-2"));
    let n1 = spawn(&d1, Some(1), Vec::new()).unwrap();
    ready(&n1);
    let seeds = vec![n1.peer_addr().to_string()];
    let n2 = spawn(&d2, None, seeds.clone()).unwrap();
    let addr2 = n2.peer_addr();
    n2.stop();
    unassign(&d2);
    // Without --join it cannot learn its identifier: it does not start (and does not found).
    let e = spawn(&d2, None, Vec::new()).unwrap_err();
    assert!(matches!(&e, NodeError::Config(m) if m.contains("--join")), "{e}");
    // With --join, from the same address, it is node 2 of the same cluster again.
    let n2 = spawn_within(&d2, None, seeds, JOIN_DEADLINE, Some(addr2)).unwrap();
    assert_eq!((n2.node_id(), n2.cluster_id()), (NodeId(2), n1.cluster_id()));
    ready(&n2);
    n2.stop();
    n1.stop();
}

fn a_node_back_on_another_port_keeps_the_cluster_whole() {
    let (d1, d2, d3, d4) =
        (Dir::new("port-1"), Dir::new("port-2"), Dir::new("port-3"), Dir::new("port-4"));
    let n1 = spawn(&d1, Some(1), Vec::new()).unwrap();
    ready(&n1);
    let seeds = vec![n1.peer_addr().to_string()];
    let n2 = spawn(&d2, None, seeds.clone()).unwrap();
    let n3 = spawn(&d3, None, seeds).unwrap();
    let ids = [NodeId(1), NodeId(2), NodeId(3)];
    assert_eq!(wait_view(&n1, |v| v.voters == ids).voters, ids);
    // Node 3 comes back on another port: it is ready only once Group 0 has the new one.
    let old = n3.peer_addr();
    n3.stop();
    let n3 = spawn(&d3, None, Vec::new()).unwrap();
    assert_ne!(n3.peer_addr(), old);
    ready(&n3);
    assert!(n3.view(ASK).unwrap().addrs_current);
    // Without node 1, nodes 2 and 3 still elect a leader and take a join.
    n1.stop();
    let n4 = spawn(&d4, None, vec![n2.peer_addr().to_string()]).unwrap();
    assert_eq!(n4.node_id(), NodeId(4));
    ready(&n4);
    for n in [n2, n3, n4] {
        n.stop();
    }
}

fn joining_fails_with_another_core_count_or_nobody_to_ask() {
    let (d1, d2) = (Dir::new("cores-1"), Dir::new("cores-2"));
    let n1 = spawn(&d1, Some(1), Vec::new()).unwrap();
    ready(&n1);
    // Refused for good: a short deadline only bounds how long the node asks again.
    let refused = Duration::from_secs(2);
    let e =
        spawn_within(&d2, Some(2), vec![n1.peer_addr().to_string()], refused, None).unwrap_err();
    assert!(matches!(&e, NodeError::Join(m) if m.contains("CoreCountMismatch")), "{e}");
    n1.stop();

    let req = JoinRequest {
        name: "x".into(),
        peer_addr: "127.0.0.1:1".into(),
        http_addr: String::new(),
        native_addr: String::new(),
        cores: 0,
    };
    let e = join_cluster(&["127.0.0.1:1".into()], &req, Duration::ZERO).unwrap_err();
    assert!(e.to_string().starts_with("could not join the cluster: 127.0.0.1:1"), "{e}");
}

fn errors_say_what_failed() {
    let io = NodeError::from(std::io::Error::other("disk gone"));
    assert_eq!(io.to_string(), "disk gone");
    let st = NodeError::from(teseladb_raftlog::StorageError::FormatVersion { found: 9 });
    assert!(st.to_string().starts_with("storage: FormatVersion"), "{st}");
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let cfg = NodeConfig { listen_peer: busy.local_addr().unwrap(), ..NodeConfig::default() };
    let e = Node::start(cfg, DiskPlace::Memory, None).unwrap_err();
    assert!(matches!(e, NodeError::Io(_)), "{e}");
    // No disk to open, and a disk whose first write fails (the data format file).
    let open_failed: teseladb_io::engine::disk::DiskMaker =
        Arc::new(|| Err(std::io::Error::other("no disk")));
    let e = Node::start(NodeConfig::default(), DiskPlace::Made(open_failed), None);
    assert_eq!(e.map(|_| ()).unwrap_err().to_string(), "no disk");
    let first = Arc::new(std::sync::Mutex::new(true));
    let open: teseladb_io::engine::disk::DiskMaker = Arc::new(move || {
        let mut disk = teseladb_raftlog::MemDisk::new();
        if std::mem::take(&mut *first.lock().unwrap()) {
            disk.inject_error("write", teseladb_raftlog::IoErrno::NoSpace);
        }
        Ok(Box::new(disk) as Box<dyn teseladb_raftlog::Disk + Send>)
    });
    let e =
        Node::start(NodeConfig::default(), DiskPlace::Made(open), None).map(|_| ()).unwrap_err();
    assert!(matches!(e, NodeError::Storage(_)), "{e}");
    let lsm = NodeError::Lsm(teseladb_lsm::LsmError::IngestOverlap);
    assert!(lsm.to_string().starts_with("LSM: "), "{lsm}");
}

/// A node that only answers joins, on a thread of the test: each joiner's `Hello` gets its own,
/// and its `JoinRequest` the refusal `answer(own address)`.
struct FakeNode {
    stop: Arc<std::sync::atomic::AtomicBool>,
    thread: thread::JoinHandle<()>,
}

impl FakeNode {
    fn stop(self) {
        self.stop.store(true, Ordering::Release);
        self.thread.join().unwrap();
    }
}

/// Reads one frame from `s` (`buf` holds what came after the last one).
fn read_frame(s: &mut std::net::TcpStream, buf: &mut Vec<u8>) -> Option<PeerMsg> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Ok(Some((m, used))) = decode_frame(buf) {
            buf.drain(..used);
            return Some(m);
        }
        let n = std::io::Read::read(s, &mut chunk).ok().filter(|n| *n > 0)?;
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn write_frame(s: &mut std::net::TcpStream, m: &PeerMsg) {
    let mut out = Vec::new();
    encode_frame(m, false, &mut out).unwrap();
    std::io::Write::write_all(s, &out).unwrap();
}

fn fake_node(answer: fn(&str) -> String, asked: Arc<AtomicUsize>) -> (FakeNode, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let own = addr.clone();
    let hello = Hello {
        cluster_id: [7; 16],
        node_id: NodeId(1),
        core: 0,
        cores: 1,
        peer_addr: addr.clone(),
        http_addr: String::new(),
        native_addr: String::new(),
        versions: teseladb_io::peer::Versions::OWN,
    };
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopping = stop.clone();
    let thread = thread::spawn(move || {
        while !stopping.load(Ordering::Acquire) {
            let Ok((mut s, _)) = listener.accept() else {
                thread::sleep(Duration::from_millis(5));
                continue;
            };
            s.set_nonblocking(false).unwrap();
            let mut buf = Vec::new();
            let Some(PeerMsg::Hello(_)) = read_frame(&mut s, &mut buf) else { continue };
            write_frame(&mut s, &PeerMsg::Hello(hello.clone()));
            let Some(PeerMsg::JoinRequest { .. }) = read_frame(&mut s, &mut buf) else { continue };
            asked.fetch_add(1, Ordering::Relaxed);
            write_frame(&mut s, &PeerMsg::JoinResponse(Err(answer(&own))));
        }
    });
    (FakeNode { stop, thread }, addr)
}

fn a_join_round_follows_a_bounded_number_of_hints() {
    let req = JoinRequest {
        name: "x".into(),
        peer_addr: "127.0.0.1:1".into(),
        http_addr: String::new(),
        native_addr: String::new(),
        cores: 0,
    };
    // A node that always names itself as the leader: the seed and three hints, then give up.
    let asked = Arc::new(AtomicUsize::new(0));
    let (a, addr) = fake_node(|own| format!("not_leader {own}"), asked.clone());
    let e = join_cluster(std::slice::from_ref(&addr), &req, Duration::ZERO).unwrap_err();
    assert_eq!(e.to_string(), format!("could not join the cluster: {addr}: not_leader {addr}"));
    assert_eq!(asked.load(Ordering::Relaxed), 4);
    a.stop();
    // Any other reason is the last error, without hints.
    let asked = Arc::new(AtomicUsize::new(0));
    let (a, addr) = fake_node(|_| "busy".to_owned(), asked.clone());
    let e = join_cluster(std::slice::from_ref(&addr), &req, Duration::ZERO).unwrap_err();
    assert_eq!(e.to_string(), format!("could not join the cluster: {addr}: busy"));
    assert_eq!(asked.load(Ordering::Relaxed), 1);
    a.stop();
}

fn a_damaged_node_meta_stops_the_node_without_touching_the_directory() {
    let dir = Dir::new("corrupt-meta");
    let n = start(&dir, Some(1), Vec::new()).unwrap();
    ready(&n);
    n.stop();
    let meta = dir.0.join("node.meta");
    let mut bytes = fs::read(&meta).unwrap();
    bytes[10] ^= 0xff;
    fs::write(&meta, &bytes).unwrap();
    let e = start(&dir, Some(1), Vec::new()).unwrap_err();
    assert!(matches!(e, NodeError::Storage(_)), "{e}");
    assert!(e.to_string().starts_with("storage: "), "{e}");
    // Nothing was rewritten: the damaged file is still there for `inspect` or a person.
    assert_eq!(fs::read(&meta).unwrap(), bytes);
}

fn wall_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

fn bound_on_disk(dir: &Dir) -> u64 {
    let mut disk = BlockingDisk::open(&dir.0).unwrap();
    let (_, bound) = teseladb_server::hlc_bound::read_bound(&mut disk).unwrap().expect("hlc.bound");
    bound
}

fn meta_bound(dir: &Dir) -> u64 {
    let mut disk = BlockingDisk::open(&dir.0).unwrap();
    teseladb_raftlog::read_node_meta(&mut disk).unwrap().expect("node.meta").hlc_upper_bound
}

fn the_clock_bound_stays_ahead_and_a_restart_waits_past_it() {
    // 005 FR-003, D-158: no mark goes out above the bound in `hlc.bound`, which stays at most
    // `AHEAD_MS` ahead of the marks while the node runs; a node that stops cleanly writes the
    // exact bound, and one whose clock is behind the bound waits before starting.
    let dir = Dir::new("hlc-bound");
    let n = start(&dir, Some(1), Vec::new()).unwrap();
    ready(&n);
    // Never more than `AHEAD_MS` ahead. A thread that waited too long under load may leave it
    // behind the clock for a moment (no mark goes out then); it gets ahead again.
    let (bound, now) = (bound_on_disk(&dir), wall_ms());
    assert!(bound <= now + teseladb_hlc::AHEAD_MS, "bound {bound}, clock {now}");
    let renewed = READY + Duration::from_millis(teseladb_hlc::AHEAD_MS);
    let end = Instant::now() + renewed;
    while bound_on_disk(&dir) <= wall_ms() && Instant::now() < end {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(bound_on_disk(&dir) > wall_ms(), "the bound gets ahead of the clock");
    let bound = bound_on_disk(&dir);
    let end = Instant::now() + renewed;
    while bound_on_disk(&dir) == bound && Instant::now() < end {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(bound_on_disk(&dir) > bound, "the bound moves while the node runs");
    assert!(n.clock_error().is_none());
    let running = bound_on_disk(&dir);
    n.stop();
    // A clean stop leaves the exact bound, one millisecond past the last mark, in both files: no
    // higher than the bound it ran under (a leader with a read cache takes its clock ahead of
    // the wall clock by the cache's margin, so the exact bound may be ahead of it too).
    let exact = bound_on_disk(&dir);
    assert!(exact <= running, "exact bound {exact} above the running one {running}");
    assert_eq!(meta_bound(&dir), exact);
    let n = start(&dir, None, Vec::new()).unwrap();
    assert!(bound_on_disk(&dir) >= exact, "the bound never goes back");
    ready(&n);
    n.stop();

    // The bound is ahead of the clock, as after the clock went back.
    let ahead = wall_ms() + 700;
    let mut disk = BlockingDisk::open(&dir.0).unwrap();
    teseladb_server::hlc_bound::BoundFile::open(&mut disk, ahead).unwrap();
    drop(disk);
    let n = start(&dir, None, Vec::new()).unwrap();
    assert!(wall_ms() > ahead, "it started before its clock passed the bound");
    assert!(bound_on_disk(&dir) >= ahead, "the bound never goes back");
    ready(&n);
    // The bound follows the marks (Group 0's own entries ask for them), not the bare clock: the
    // first mark past it writes a new one.
    let end = Instant::now() + READY;
    while bound_on_disk(&dir) <= ahead && Instant::now() < end {
        thread::sleep(Duration::from_millis(5));
    }
    assert!(bound_on_disk(&dir) > ahead, "a new bound once marks go out again");
    n.stop();
}
