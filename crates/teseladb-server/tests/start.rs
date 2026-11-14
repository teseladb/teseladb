//! The first start and the restart of a node over real files (005 FR-015, FR-016): the first start
//! creates Group 0 and the data range, which takes writes; a restart recovers the LSM, opens the
//! range from its flushed index and replays the log after it, with nothing lost. With two cores
//! the range lives on core 1 and Group 0 on core 0. With a memory limit the memtables flush at
//! their part of it (012 FR-013).

use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use teseladb_io::FileDisk;
use teseladb_lsm::LsmConfig;
use teseladb_meta::bootstrap::{DATA_END, DATA_RANGE, DATA_START, data_range_core};
use teseladb_meta::group0::ReplicaRole;
use teseladb_raft::Role;
use teseladb_range::host::RangeError;
use teseladb_server::data::{DataError, DataStatus};
use teseladb_server::memory::Component;
use teseladb_server::node::{
    DATA_FORMAT, DATA_FORMAT_FILE, FOUNDER, Node, NodeConfig, NodeError, lock_dir,
};
use teseladb_types::NodeId;

const READY: Duration = Duration::from_secs(20);
const ASK: Duration = Duration::from_secs(5);

/// A data directory removed when the test ends.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p = std::env::temp_dir().join(format!("teseladb-start-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// An LSM whose memtable flushes every few kilobytes, so that a restart finds part of the range
/// in its files and part only in the log.
fn small_lsm() -> LsmConfig {
    LsmConfig {
        memtable_bytes: 16 << 10,
        block_bytes: 4096,
        target_file_bytes: 64 << 10,
        l1_bytes: 256 << 10,
        cache_bytes: 1 << 20,
        ..LsmConfig::default()
    }
}

fn start(dir: &Dir, cores: u16) -> Result<Node<FileDisk>, NodeError> {
    let cfg = NodeConfig { cores: Some(cores), lsm: small_lsm(), ..NodeConfig::default() };
    let path = dir.0.clone();
    Node::start(cfg, || FileDisk::open(&path), Some(lock_dir(&dir.0)?))
}

fn key(i: u32) -> Vec<u8> {
    [&DATA_START[..], format!("doc-{i:05}").as_bytes()].concat()
}

fn value(i: u32) -> Vec<u8> {
    format!("{i:05}-{}", "v".repeat(200)).into_bytes()
}

/// Waits until the status of the range satisfies `ok`.
fn wait_status(n: &Node<FileDisk>, ok: impl Fn(&DataStatus) -> bool) -> DataStatus {
    let end = Instant::now() + READY;
    loop {
        let s = n.data_status(ASK).expect("the range's core answers");
        if ok(&s) {
            return s;
        }
        assert!(Instant::now() < end, "the range never got there: {s:?}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// Writes until the range takes it (it may not lead yet right after a start).
fn put(n: &Node<FileDisk>, i: u32) {
    let end = Instant::now() + READY;
    loop {
        match n.put(&key(i), &value(i), ASK) {
            Ok(_) => return,
            Err(e) => assert!(Instant::now() < end, "write {i}: {e:?}"),
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Reads document `i`, asking again while the range answers that it is busy (`Overloaded` when
/// the machine is loaded), as `put` writes.
fn get(n: &Node<FileDisk>, i: u32) -> Option<Vec<u8>> {
    let end = Instant::now() + READY;
    loop {
        match n.get(&key(i), ASK) {
            Ok(v) => return v,
            Err(e) => assert!(Instant::now() < end, "read {i}: {e:?}"),
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn leads(s: &DataStatus) -> bool {
    s.role == Some(Role::Leader)
}

#[test]
fn the_first_start_creates_group0_and_the_data_range() {
    let dir = Dir::new("first");
    let n = start(&dir, 1).unwrap();
    assert!(n.wait_ready(READY));
    // Group 0 records the range over the whole data space, with the founder as its replica.
    let end = Instant::now() + READY;
    let desc = loop {
        if let Some(d) = n.view(ASK).and_then(|v| v.data_range) {
            break d;
        }
        assert!(Instant::now() < end, "Group 0 never created the data range");
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(desc.group, DATA_RANGE);
    assert_eq!((desc.start.as_slice(), desc.end.as_slice()), (&DATA_START[..], &DATA_END[..]));
    assert_eq!(desc.replicas, [(FOUNDER, ReplicaRole::Voter)]);
    assert_eq!(desc.core, data_range_core(1));

    let s = wait_status(&n, leads);
    assert_eq!((s.voters.as_slice(), s.learners.len()), (&[FOUNDER][..], 0));
    put(&n, 1);
    assert_eq!(get(&n, 1), Some(value(1)));
    assert_eq!(get(&n, 2), None);
    // Outside the data space nothing is written.
    let outside = n.put(b"\x05x", b"v", ASK);
    assert_eq!(outside, Err(DataError::Range(RangeError::OutOfRange)));
    n.stop();
}

#[test]
fn a_restart_recovers_the_lsm_and_replays_the_log_from_the_flushed_index() {
    let dir = Dir::new("restart");
    let n = start(&dir, 2).unwrap();
    assert!(n.wait_ready(READY));
    wait_status(&n, leads);
    // Enough to flush the memtable several times, and a tail that stays in the log.
    for i in 0..300 {
        put(&n, i);
    }
    let before = wait_status(&n, |s| s.flushed > 0);
    assert!(before.applied > before.flushed, "a tail only in the log: {before:?}");
    n.stop();

    let n = start(&dir, 2).unwrap();
    assert!(n.wait_ready(READY));
    assert_eq!(n.node_id(), FOUNDER);
    let after = wait_status(&n, |s| leads(s) && s.applied >= before.applied);
    assert!(after.flushed >= before.flushed, "{before:?} then {after:?}");
    for i in 0..300 {
        assert_eq!(get(&n, i), Some(value(i)), "document {i}");
    }
    put(&n, 300);
    assert_eq!(get(&n, 300), Some(value(300)));
    // Group 0 has one range: the restart did not create another.
    let v = n.view(ASK).unwrap();
    assert_eq!(v.data_range.map(|d| d.group), Some(DATA_RANGE));
    assert_eq!(v.nodes, [NodeId(1)]);
    n.stop();
}

/// With `--memory-limit` (012 FR-013) the node splits it between its cores and each core's
/// memtables flush at their part, well before the size the LSM gives a memtable on its own; the
/// node keeps working with that little memory.
#[test]
fn the_memtables_flush_at_their_part_of_the_memory_limit() {
    let dir = Dir::new("memory");
    let cfg = NodeConfig { cores: Some(2), memory_limit: Some(1 << 20), ..NodeConfig::default() };
    let path = dir.0.clone();
    let n = Node::start(cfg, || FileDisk::open(&path), Some(lock_dir(&dir.0).unwrap())).unwrap();
    let memory = n.memory().unwrap();
    assert_eq!((memory.total, memory.cores), (1 << 20, 2));
    let lsm = n.core_lsm();
    let tables = lsm.max_frozen as u64 + 1;
    assert_eq!(lsm.memtable_bytes, memory.part(Component::Memtables) / tables);
    assert!(lsm.memtable_bytes < LsmConfig::default().memtable_bytes);
    assert_eq!(lsm.cache_bytes, memory.part(Component::BlockCache));
    assert!(n.wait_ready(READY));
    wait_status(&n, leads);
    for i in 0..300 {
        put(&n, i);
    }
    let s = wait_status(&n, |s| s.flushed > 0);
    assert!(s.flushed <= s.applied, "{s:?}");
    for i in (0..300).step_by(37) {
        assert_eq!(get(&n, i), Some(value(i)), "document {i}");
    }
    n.stop();
    // Without a limit, three quarters of the memory the process has.
    let dir = Dir::new("memory-default");
    let cfg = NodeConfig { cores: Some(1), ..NodeConfig::default() };
    let path = dir.0.clone();
    let n = Node::start(cfg, || FileDisk::open(&path), Some(lock_dir(&dir.0).unwrap())).unwrap();
    let host = teseladb_sys::memory_limit::host_default();
    assert_eq!(n.memory().map(|m| m.total), host);
    n.stop();
}

#[test]
fn a_directory_of_another_data_format_refuses_to_start_and_says_so() {
    let dir = Dir::new("format");
    let n = start(&dir, 1).unwrap();
    assert!(n.wait_ready(READY));
    n.stop();
    let file = dir.0.join(DATA_FORMAT_FILE);
    assert_eq!(fs::read_to_string(&file).unwrap(), DATA_FORMAT.to_string());
    // Written by an earlier version, which kept no format file: format 1.
    fs::remove_file(&file).unwrap();
    let Err(err) = start(&dir, 1) else { panic!("a directory of format 1 started") };
    assert!(matches!(err, NodeError::DataFormat { found: 1 }), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("data format 1") && text.contains(&format!("format {DATA_FORMAT}")));
    // A later format, and one that is no number, are refused too.
    fs::write(&file, (DATA_FORMAT + 1).to_string()).unwrap();
    assert!(
        matches!(start(&dir, 1), Err(NodeError::DataFormat { found }) if found == DATA_FORMAT + 1)
    );
    fs::write(&file, "two").unwrap();
    assert!(matches!(start(&dir, 1), Err(NodeError::Config(_))));
    // Its own format starts again.
    fs::write(&file, DATA_FORMAT.to_string()).unwrap();
    let n = start(&dir, 1).unwrap();
    assert!(n.wait_ready(READY));
    n.stop();
}

/// Puts the cores' WAL back where format 7 kept it, one file per segment under `core-NN/wal`, and
/// removes the device log.
fn back_to_per_core_logs(dir: &Dir, cores: u16) {
    use teseladb_io::device_log::state::LogState;
    let mut disk = FileDisk::open(&dir.0).unwrap();
    let mut log = LogState::recover(&mut disk, 64 << 20).unwrap();
    for core in 0..cores {
        for seq in log.segments_of(core) {
            let bytes = log.read(&mut disk, core, seq).unwrap();
            let path = dir.0.join(teseladb_raftlog::paths::wal_segment(core, seq));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }
    }
    fs::remove_dir_all(dir.0.join(teseladb_io::device_log::format::DIR)).unwrap();
    fs::write(dir.0.join(DATA_FORMAT_FILE), "7").unwrap();
}

#[test]
fn a_directory_of_format_7_moves_its_logs_to_the_device_log() {
    // 012 FR-035: the WAL segments of each core go into the device log on the first start.
    let dir = Dir::new("format-7");
    let n = start(&dir, 2).unwrap();
    assert!(n.wait_ready(READY));
    wait_status(&n, leads);
    for i in 0..50 {
        put(&n, i);
    }
    n.stop();
    back_to_per_core_logs(&dir, 2);
    let n = start(&dir, 2).unwrap();
    assert!(n.wait_ready(READY));
    for i in 0..50 {
        assert_eq!(get(&n, i), Some(value(i)), "document {i}");
    }
    n.stop();
    assert_eq!(fs::read_to_string(dir.0.join(DATA_FORMAT_FILE)).unwrap(), DATA_FORMAT.to_string());
    for core in 0..2 {
        let wal = dir.0.join(teseladb_raftlog::paths::wal_dir(core));
        assert_eq!(fs::read_dir(&wal).map_or(0, Iterator::count), 0, "core {core}");
    }
    // And it starts again from the device log alone.
    let n = start(&dir, 2).unwrap();
    assert!(n.wait_ready(READY));
    assert_eq!(get(&n, 49), Some(value(49)));
    n.stop();
}
