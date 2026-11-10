//! The core loop with an io_uring disk (Linux, 012 FR-002 and acceptance scenario 1 of story 1):
//! the logic's operations go to the core's ring, their completions come back through the same
//! poller in submission order, and no disk thread exists.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_io::NodeDisk;
use teseladb_io::node_disk::{DirectIo, IoExecutor, choose};
use teseladb_io::peer::{Hello, PeerMsg, Versions};
use teseladb_io::runtime::{CoreInput, CoreLogic, CoreRuntime, RuntimeConfig};
use teseladb_raftlog::{FileId, IoDone, IoErrno, IoOp};
use teseladb_types::{Bytes, Nanos, NodeId};

/// Writes, a sync and a read back, queued when the loop starts.
#[derive(Default)]
struct Writer {
    io: Vec<IoOp>,
    done: Vec<IoDone>,
    seen: Arc<AtomicUsize>,
}

impl CoreLogic for Writer {
    type Msg = IoOp;
    fn tick(&mut self, _now: Nanos, _wall_ms: u64) {}
    fn poll_io(&mut self, _now: Nanos) -> Option<IoOp> {
        if self.io.is_empty() { None } else { Some(self.io.remove(0)) }
    }
    fn on_io_done(&mut self, done: IoDone, _now: Nanos) {
        self.done.push(done);
        self.seen.fetch_add(1, Ordering::Relaxed);
    }
    fn next_deadline(&self) -> Option<Nanos> {
        None
    }
    fn on_peer(&mut self, _from: NodeId, _msg: PeerMsg, _now: Nanos) {}
    fn on_message(&mut self, op: IoOp, _now: Nanos) {
        self.io.push(op);
    }
    fn take_outbound(&mut self, _out: &mut Vec<(NodeId, PeerMsg)>) {}
    fn peer_addr(&self, _node: NodeId) -> Option<String> {
        None
    }
    fn peer_unreachable(&mut self, _node: NodeId) {}
    fn io_settled(&self) -> bool {
        self.io.is_empty()
    }

    fn resting(&self) -> bool {
        false
    }
}

fn hello() -> Hello {
    Hello {
        cluster_id: [1; 16],
        node_id: NodeId(1),
        core: 7,
        cores: 8,
        peer_addr: "127.0.0.1:1".into(),
        http_addr: String::new(),
        native_addr: String::new(),
        versions: Versions::OWN,
    }
}

fn threads_named(name: &str) -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter(|t| {
            let comm = t.as_ref().unwrap().path().join("comm");
            std::fs::read_to_string(comm).is_ok_and(|c| c.trim() == name)
        })
        .count()
}

#[test]
fn the_core_drives_its_disk_through_its_own_ring() {
    let root = std::env::temp_dir().join(format!("teseladb-rt-uring-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let choice = choose(IoExecutor::Uring, DirectIo::Auto, &std::env::temp_dir()).unwrap();
    assert!(choice.uring && choice.direct, "{choice}");
    let disk = NodeDisk::open(&root, &choice).unwrap();
    let file = FileId(1);
    let mut io = vec![IoOp::Create { file, path: PathBuf::from("wal/1"), prealloc: 1 << 20 }];
    for i in 0..64u64 {
        let data = Bytes::from_vec(vec![i as u8; 4096]);
        io.push(IoOp::WriteAt { file, offset: i * 4096, data, batch: i });
    }
    io.push(IoOp::Sync { file, batch: 64 });
    io.push(IoOp::ReadAt { file, offset: 63 * 4096, len: 4096, token: 5 });
    io.push(IoOp::Sync { file: FileId(9), batch: 0 });
    let n = io.len();
    let seen = Arc::new(AtomicUsize::new(0));
    let logic = Writer { io, done: Vec::new(), seen: Arc::clone(&seen) };
    let cfg = RuntimeConfig { tick: Duration::from_millis(5), seed: 1, tls: None, origin: None };
    let (rt, tx) = CoreRuntime::new(logic, disk, hello(), cfg).unwrap();
    assert_eq!(threads_named("disk-7"), 0, "no disk thread");
    let core = thread::spawn(move || rt.run().unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    while seen.load(Ordering::Relaxed) < n {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(2));
    }
    // An operation that fails before reaching the kernel, alone: no completion signals it, and
    // the loop still hands it over.
    tx.send(CoreInput::App(IoOp::Close { file: FileId(8) })).ok().unwrap();
    while seen.load(Ordering::Relaxed) < n + 1 {
        assert!(Instant::now() < deadline, "timed out");
        thread::sleep(Duration::from_millis(2));
    }
    tx.send(CoreInput::Stop).ok().unwrap();
    let (logic, disk) = core.join().unwrap();
    assert!(matches!(disk, Some(NodeDisk::Uring(_))));
    let d = &logic.done;
    assert_eq!(d.len(), n + 1);
    for (i, done) in d[1..65].iter().enumerate() {
        let IoDone::Ok(IoOp::WriteAt { batch, .. }) = done else { panic!("{done:?}") };
        assert_eq!(*batch, i as u64, "in submission order");
    }
    assert!(matches!(d[65], IoDone::Ok(IoOp::Sync { batch: 64, .. })));
    assert_eq!(d[66], IoDone::Read { token: 5, data: Ok(Bytes::from_vec(vec![63; 4096])) });
    assert!(matches!(d[67], IoDone::Err { errno: IoErrno::BadFile, .. }));
    assert!(matches!(d[68], IoDone::Err { op: IoOp::Close { .. }, errno: IoErrno::BadFile }));
    std::fs::remove_dir_all(&root).unwrap();
}
