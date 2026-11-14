//! A node that cannot start because its disk fails says what failed and starts nothing (001,
//! 005 FR-003, 007 T019 coverage): the disk of these tests fails every operation, or every read,
//! on the files whose path holds a given text, the way a damaged or full disk fails, with
//! [`MemDisk`] behind it for everything else.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use teseladb_raftlog::{
    Disk, FileId, IoDone, IoErrno, IoOp, MemDisk, open_node, write_atomic, write_node_meta,
};
use teseladb_range::host::RangeError;
use teseladb_server::data::DataError;
use teseladb_server::node::{
    DATA_FORMAT, DATA_FORMAT_FILE, JOIN_VOTERS, Node, NodeConfig, NodeError,
};

/// How long a node of these tests has to be ready.
const READY: Duration = Duration::from_secs(10);

/// What the disk fails.
#[derive(Clone, Copy)]
enum Fails {
    /// Every operation on a matching file.
    Ops,
    /// Reading a matching file whole.
    Reads,
    /// Every operation on a matching file, with an I/O error instead of a full disk.
    Broken,
    /// The thread that uses a matching file dies, the way a crashing driver takes it down.
    Crashes,
    /// Every operation on a matching file from the given one on (counted from 0): the disk fills
    /// up in the middle of the start.
    OpsFrom(usize),
}

/// A [`MemDisk`] that fails what [`Fails`] says on the files whose path contains `text`.
struct FaultyDisk {
    inner: MemDisk,
    text: &'static str,
    fails: Fails,
    /// The path of each open file, to know which file a write or a sync is for.
    files: Vec<(FileId, PathBuf)>,
    /// Whether the faults are on (a disk that fails only from some moment on).
    armed: Arc<AtomicBool>,
    /// Operations on matching files so far.
    seen: usize,
}

impl FaultyDisk {
    fn new(text: &'static str, fails: Fails) -> FaultyDisk {
        FaultyDisk::over(MemDisk::new(), text, fails, Arc::new(AtomicBool::new(true)))
    }

    /// A disk that fills up at the `n`-th operation on the files whose path holds `text`.
    fn new_from(text: &'static str, n: usize) -> FaultyDisk {
        FaultyDisk::over(MemDisk::new(), text, Fails::OpsFrom(n), Arc::new(AtomicBool::new(true)))
    }

    fn over(
        inner: MemDisk,
        text: &'static str,
        fails: Fails,
        armed: Arc<AtomicBool>,
    ) -> FaultyDisk {
        FaultyDisk { inner, text, fails, files: Vec::new(), armed, seen: 0 }
    }

    fn matches(&self, p: &Path) -> bool {
        self.armed.load(Ordering::Acquire) && p.to_string_lossy().contains(self.text)
    }

    fn path_of(&self, op: &IoOp) -> Option<PathBuf> {
        let file = match op {
            IoOp::Create { path, .. }
            | IoOp::Open { path, .. }
            | IoOp::Remove { path }
            | IoOp::RemoveDir { path }
            | IoOp::SyncDir { path } => return Some(path.clone()),
            IoOp::Rename { to, .. } | IoOp::Link { to, .. } => return Some(to.clone()),
            IoOp::WriteAt { file, .. }
            | IoOp::ReadAt { file, .. }
            | IoOp::Sync { file, .. }
            | IoOp::Truncate { file, .. }
            | IoOp::Close { file } => *file,
        };
        self.files.iter().find(|(f, _)| *f == file).map(|(_, p)| p.clone())
    }
}

impl Disk for FaultyDisk {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        if matches!(self.fails, Fails::Crashes) && self.matches(dir) {
            panic!("the disk under {} crashed", dir.display());
        }
        self.inner.list(dir)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        if self.matches(path) {
            return Err(IoErrno::Io);
        }
        self.inner.read_all(path)
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        if let IoOp::Create { file, path, .. } | IoOp::Open { file, path } = &op {
            self.files.push((*file, path.clone()));
        }
        let hit = self.path_of(&op).is_some_and(|p| self.matches(&p));
        let errno = match self.fails {
            Fails::Ops => IoErrno::NoSpace,
            Fails::Broken => IoErrno::Io,
            Fails::OpsFrom(n) if hit => {
                self.seen += 1;
                if self.seen <= n {
                    return self.inner.execute(op);
                }
                IoErrno::NoSpace
            }
            Fails::Reads | Fails::Crashes | Fails::OpsFrom(_) => return self.inner.execute(op),
        };
        if hit {
            // As a real disk answers: the operation without its data, which is how the store
            // knows which of its operations failed.
            return IoDone::Err { op: op.without_data(), errno };
        }
        self.inner.execute(op)
    }
}

/// Starts a node whose every disk fails as `fails` says on `text`; the error it gives.
fn start_failing(text: &'static str, fails: Fails) -> NodeError {
    let made = Arc::new(Mutex::new(0u32));
    let count = made.clone();
    let open = move || {
        *count.lock().unwrap() += 1;
        Ok(FaultyDisk::new(text, fails))
    };
    match Node::start(NodeConfig::default(), open, None) {
        Ok(n) => {
            n.stop();
            panic!("a node started over a disk that fails on {text}");
        }
        Err(e) => e,
    }
}

#[test]
fn a_data_format_that_cannot_be_written_stops_the_start() {
    let e = start_failing(DATA_FORMAT_FILE, Fails::Ops);
    assert!(e.to_string().starts_with(DATA_FORMAT_FILE), "{e}");
}

#[test]
fn a_node_meta_that_cannot_be_written_or_read_stops_the_start() {
    let e = start_failing("node.meta", Fails::Ops);
    assert!(matches!(e, NodeError::Storage(_)), "{e}");
    let e = start_failing("node.meta", Fails::Reads);
    assert!(matches!(e, NodeError::Storage(_) | NodeError::Io(_)), "{e}");
}

#[test]
fn join_voters_that_cannot_be_read_stop_the_start() {
    let e = start_failing(JOIN_VOTERS, Fails::Reads);
    assert!(e.to_string().contains(JOIN_VOTERS) || matches!(e, NodeError::Storage(_)), "{e}");
}

#[test]
fn a_core_disk_that_cannot_be_opened_stops_the_start() {
    // The node's own disk opens; the first core's does not.
    let opened = Arc::new(Mutex::new(0u32));
    let open = move || {
        let mut n = opened.lock().unwrap();
        *n += 1;
        match *n {
            1 => Ok(MemDisk::new()),
            _ => Err(std::io::Error::other("no disk for the core")),
        }
    };
    let cfg = NodeConfig { cores: Some(1), ..NodeConfig::default() };
    let Err(e) = Node::start(cfg, open, None) else { panic!("started without a core disk") };
    assert!(matches!(e, NodeError::Io(_)), "{e}");
}

#[test]
fn an_lsm_that_cannot_be_created_stops_the_start() {
    let e = start_failing("lsm", Fails::Ops);
    assert!(matches!(e, NodeError::Lsm(_)), "{e}");
}

#[test]
fn a_group0_whose_voters_name_this_node_as_a_learner_too_stops_the_start() {
    // Node 1 recorded itself as Group 0's voter in `join.voters`: it would be voter and learner.
    let mut meta = MemDisk::new();
    let mut m = open_node(&mut meta, 1, [7; 16], 1).unwrap();
    m.node_id = 1;
    write_node_meta(&mut meta, &m).unwrap();
    write_atomic(&mut meta, Path::new(JOIN_VOTERS), b"1".to_vec(), FileId(9)).unwrap();
    let format = DATA_FORMAT.to_string().into_bytes();
    write_atomic(&mut meta, Path::new(DATA_FORMAT_FILE), format, FileId(10)).unwrap();
    let first = Arc::new(Mutex::new(Some(meta)));
    let open = move || Ok(first.lock().unwrap().take().unwrap_or_default());
    let cfg = NodeConfig { cores: Some(1), ..NodeConfig::default() };
    let Err(e) = Node::start(cfg, open, None) else { panic!("started with that Group 0") };
    assert!(e.to_string().starts_with("Group 0"), "{e}");
}

#[test]
fn a_memory_limit_is_split_between_the_cores() {
    let cfg = NodeConfig { cores: Some(2), memory_limit: Some(1 << 30), ..NodeConfig::default() };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    let budget = node.memory().expect("a budget from the limit");
    assert_eq!(budget.per_core(), 1 << 29);
    node.stop();
}

#[test]
fn a_core_whose_log_fails_after_the_start_acknowledges_no_write() {
    let armed = Arc::new(AtomicBool::new(false));
    let on = armed.clone();
    // The cores' logs are in the node's device log (012 FR-035).
    let log = teseladb_io::device_log::format::DIR;
    let open = move || Ok(FaultyDisk::over(MemDisk::new(), log, Fails::Broken, on.clone()));
    let cfg = NodeConfig { cores: Some(1), ..NodeConfig::default() };
    let node = Node::start(cfg, open, None).unwrap();
    assert!(node.wait_ready(READY));
    // The first write waits for the data range to have its leader.
    let deadline = std::time::Instant::now() + READY;
    while node.put(&[1, 1], b"before", Duration::from_millis(200)).is_err() {
        assert!(std::time::Instant::now() < deadline, "no write before the fault");
        std::thread::sleep(Duration::from_millis(10));
    }
    // Fail-stop: once a log write fails, the store acknowledges nothing more. The write whose
    // entry was in the failed batch gets no answer; the range's group stops with its store, and a
    // later one finds it unavailable.
    armed.store(true, Ordering::Release);
    for value in [&b"first"[..], b"second"] {
        let put = node.put(&[1, 1], value, Duration::from_millis(500));
        let refused =
            matches!(put, Err(DataError::NoAnswer | DataError::Range(RangeError::Unavailable)));
        assert!(refused, "{put:?}");
    }
    node.stop();
}

#[test]
fn a_core_thread_that_dies_while_it_starts_stops_the_start() {
    let e = start_failing("core-00", Fails::Crashes);
    assert_eq!(e.to_string(), "core 0 failed");
}

#[test]
fn an_administration_port_already_taken_stops_the_start_and_closes_the_client_port() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let at = taken.local_addr().unwrap();
    let client = teseladb_server::client_listener::ClientConfig {
        addrs: vec![std::net::SocketAddr::from(([127, 0, 0, 1], 0))],
        ..teseladb_server::client_listener::ClientConfig::new(Vec::new())
    };
    for client in [None, Some(client)] {
        let cfg =
            NodeConfig { cores: Some(1), admin_listen: Some(at), client, ..NodeConfig::default() };
        let Err(e) = Node::start(cfg, || Ok(MemDisk::new()), None) else {
            panic!("started on a port already taken");
        };
        assert!(matches!(e, NodeError::Io(_)), "{e}");
    }
}

#[test]
fn a_disk_that_fills_up_at_any_write_of_node_meta_stops_the_start_or_lets_it_end() {
    // Every write of `node.meta` (its creation, its identifier, the clock's bound) in turn.
    let mut failed = 0;
    for n in 0..40 {
        let cfg = NodeConfig { cores: Some(1), ..NodeConfig::default() };
        let open = move || Ok(FaultyDisk::new_from("node.meta", n));
        match Node::start(cfg, open, None) {
            Ok(node) => {
                node.stop();
            }
            Err(e) => {
                assert!(matches!(e, NodeError::Storage(_) | NodeError::Io(_)), "{n}: {e}");
                failed += 1;
            }
        }
    }
    assert!(failed > 1, "more than the first write failed");
}

#[test]
fn a_directory_that_cannot_be_created_cannot_be_locked() {
    let under_a_file = std::env::temp_dir().join(format!("teseladb-lock-{}", std::process::id()));
    std::fs::write(&under_a_file, b"a file").unwrap();
    let e = teseladb_server::node::lock_dir(&under_a_file.join("data")).map(|_| ()).unwrap_err();
    std::fs::remove_file(&under_a_file).unwrap();
    assert!(matches!(e, NodeError::Io(_)), "{e}");
}

#[test]
fn a_node_that_cannot_reach_the_cluster_to_join_does_not_start() {
    let cfg = NodeConfig {
        cores: Some(1),
        join: vec!["127.0.0.1:1".into()],
        join_deadline: Duration::ZERO,
        ..NodeConfig::default()
    };
    let Err(e) = Node::start(cfg, || Ok(MemDisk::new()), None) else { panic!("joined nobody") };
    assert!(matches!(e, NodeError::Join(_)), "{e}");
}

#[test]
fn an_insecure_node_issues_no_token_and_a_status_asked_without_time_is_not_waited_for() {
    let cfg = NodeConfig { cores: Some(1), ..NodeConfig::default() };
    let node = Node::start(cfg, || Ok(MemDisk::new()), None).unwrap();
    assert!(node.create_enroll_token(Duration::from_secs(60)).unwrap_err().contains("PKI"));
    // Without time it does not wait: `None`, or the status when the core answered within the
    // microseconds between the request and the look at its channel (asserting `None` made the
    // test depend on the machine's speed).
    let asked = std::time::Instant::now();
    let _either = node.data_status(Duration::ZERO);
    assert!(asked.elapsed() < Duration::from_millis(200), "{:?}", asked.elapsed());
    assert!(node.data_status(Duration::from_secs(5)).is_some());
    node.stop();
}
