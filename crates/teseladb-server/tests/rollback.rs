//! A rollback writes its file (005 FR-010, D-034): the range's leader loses its two followers,
//! confirms a `w: 1`, `j: false` write that only its log has, and stops. The other two come back,
//! elect a leader and write over that index. When the old leader comes back, the new leader's
//! entries replace the lost one in its log, and the node writes the lost version to its rollback
//! files under its data directory. The node's handle writes raw keys of no collection, so the
//! version stays in the range's own records, at
//! `rollback/teseladb-internal/range-1/removed.<time>.<term>.<index>.bson`; documents of a
//! collection go to the reference server's `rollback/<UUID>/`
//! (`teseladb_server::rollback::reference_layout`).

use std::fs;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use teseladb_io::FileDisk;
use teseladb_meta::bootstrap::{DATA_RANGE, DATA_START};
use teseladb_raft::Role;
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::host::RangeError;
use teseladb_range::rollback::{ROLLBACK_DIR, RollbackDoc, read_rollback_file};
use teseladb_server::data::{DataError, DataStatus};
use teseladb_server::node::{Node, NodeConfig, lock_dir};
use teseladb_server::rollback::INTERNAL_DIR;
use teseladb_types::NodeId;

const READY: Duration = Duration::from_secs(30);
const ASK: Duration = Duration::from_secs(2);
/// How long after losing its followers the leader gets its `w: 1` write: a client that takes its
/// time, as the 002 battery's Python runner did in `rollback-w1`. A leader cut off keeps the role
/// for 10 s, as a reference server primary does (`step_down_ticks`).
const LATE: Duration = Duration::from_secs(1);

/// A data directory removed when the test ends.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Dir {
        let p =
            std::env::temp_dir().join(format!("teseladb-rollback-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A port nobody listens on now, so that a node keeps its address across restarts.
fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").and_then(|l| l.local_addr()).expect("a free port")
}

fn start(dir: &Dir, peer: SocketAddr, join: Vec<String>) -> Node<FileDisk> {
    let cfg = NodeConfig { cores: Some(1), listen_peer: peer, join, ..NodeConfig::default() };
    let path = dir.0.clone();
    let lock = lock_dir(&dir.0).expect("the data directory");
    Node::start(cfg, || FileDisk::open(&path), Some(lock)).expect("the node starts")
}

fn key(name: &str) -> Vec<u8> {
    [&DATA_START[..], name.as_bytes()].concat()
}

fn until<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let end = Instant::now() + READY;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < end, "never: {what}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// The range's status, or `None` if its core did not answer in time (a busy machine): the
/// callers ask again.
fn status(n: &Node<FileDisk>) -> Option<DataStatus> {
    n.data_status(ASK)
}

fn leads(n: &Node<FileDisk>) -> bool {
    status(n).is_some_and(|s| s.role == Some(Role::Leader))
}

/// The rollback files under `dir`.
fn rollback_files(dir: &Path) -> Vec<PathBuf> {
    let range = dir.join(ROLLBACK_DIR).join(INTERNAL_DIR).join(format!("range-{}", DATA_RANGE.0));
    fs::read_dir(range)
        .map(|d| d.map_while(Result::ok).map(|e| e.path()).collect())
        .unwrap_or_default()
}

/// Attempts at cutting a leader off before it takes the `w: 1` write.
const CUT_OFF_ATTEMPTS: usize = 5;

/// The range's leader writes with the majority, loses its two followers and a second later
/// confirms a `w: 1`, `j: false` write of `lost` that only its log has. Returns the leader and
/// the followers, which are stopped. If an election moved the leadership before the followers
/// went (heartbeats late on a busy machine), the followers come back and it starts again.
fn leader_takes_a_lost_write(
    nodes: &mut [Option<Node<FileDisk>>],
    dirs: &[Dir],
    ports: &[SocketAddr],
    lost: &[u8],
) -> (usize, Vec<usize>) {
    let w1 = WriteConcern { w: W::Nodes(1), j: false };
    for _ in 0..CUT_OFF_ATTEMPTS {
        // Through the node that leads when it is asked: an election may move the leadership.
        let l = until("a majority write", || {
            let l = nodes.iter().position(|n| n.as_ref().is_some_and(leads))?;
            nodes[l].as_ref()?.put(&key("kept-before"), b"1", ASK).ok().map(|_| l)
        });
        let others: Vec<usize> = (0..3).filter(|i| *i != l).collect();
        let leader = nodes[l].take().expect("started");
        for &i in &others {
            nodes[i].take().expect("started").stop();
        }
        thread::sleep(LATE);
        // A mark the node's clock cannot give yet (its bound on disk is late on a busy machine)
        // is refused with `Overloaded` before anything is proposed: the client tries again, as a
        // driver retries a retryable write (005 FR-003).
        let put = until("an answer to the w: 1 write", || {
            match leader.put_with(lost, b"lost-value", w1, ASK) {
                Err(DataError::Range(RangeError::Overloaded)) => None,
                r => Some(r),
            }
        });
        nodes[l] = Some(leader);
        match put {
            Ok(_) => return (l, others),
            Err(DataError::Range(RangeError::NotLeader { .. })) => {
                for &i in &others {
                    nodes[i] = Some(start(&dirs[i], ports[i], Vec::new()));
                }
            }
            Err(e) => panic!("a leader cut off for a second still takes a w: 1 write: {e:?}"),
        }
    }
    panic!("the leader lost the leadership before its followers went, {CUT_OFF_ATTEMPTS} times");
}

#[test]
fn a_write_acknowledged_with_w1_and_lost_in_a_rollback_goes_to_a_rollback_file() {
    let dirs = [Dir::new("1"), Dir::new("2"), Dir::new("3")];
    let ports = [free_port(), free_port(), free_port()];
    let mut nodes: Vec<Option<Node<FileDisk>>> = Vec::new();
    nodes.push(Some(start(&dirs[0], ports[0], Vec::new())));
    let seed = vec![ports[0].to_string()];
    for i in 1..3 {
        nodes.push(Some(start(&dirs[i], ports[i], seed.clone())));
    }
    let three = [NodeId(1), NodeId(2), NodeId(3)];
    for n in nodes.iter().flatten() {
        assert!(n.wait_ready(READY));
        until("three voters", || status(n).filter(|s| s.voters == three).map(|_| ()));
    }
    let lost_key = key("lost");
    let (l, others) = leader_takes_a_lost_write(&mut nodes, &dirs, &ports, &lost_key);
    // The leader goes too, with the write in its log only.
    nodes[l].take().expect("started").stop();

    // The other two elect a leader of a later term and write over that index.
    for &i in &others {
        nodes[i] = Some(start(&dirs[i], ports[i], Vec::new()));
    }
    let alive: Vec<&Node<FileDisk>> = others.iter().filter_map(|&i| nodes[i].as_ref()).collect();
    until("a write after the rollback", || {
        alive.iter().find_map(|n| n.put(&key("kept-after"), b"2", ASK).ok())
    });

    // The old leader comes back: the new leader's entries replace the lost one.
    nodes[l] = Some(start(&dirs[l], ports[l], Vec::new()));
    // The file with the lost write.
    let lost_in = |f: &PathBuf| {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
        assert!(name.starts_with("removed.") && name.ends_with(".bson"), "{name}");
        // A file still being written does not read whole yet: it is looked at again.
        let docs = fs::read(f).ok().and_then(|b| read_rollback_file(&b).ok()).unwrap_or_default();
        docs.iter().any(|d| {
            matches!(d, RollbackDoc::Version { range, key: k, value, .. }
                if *range == DATA_RANGE && *k == lost_key && value == b"lost-value")
        })
    };
    let files = until("the rollback file", || {
        let files = rollback_files(&dirs[l].0);
        files.iter().any(lost_in).then_some(files)
    });
    assert_eq!(files.len(), 1, "{files:?}");
    // The others never wrote one, and the lost write is not in the range.
    for &i in &others {
        assert!(rollback_files(&dirs[i].0).is_empty());
    }
    let alive: Vec<&Node<FileDisk>> = nodes.iter().flatten().collect();
    // The leader may change while the old one comes back (an election on a busy machine): each
    // read goes to the node that leads at that moment.
    let get = |k: &[u8]| {
        until("a read", || alive.iter().filter(|n| leads(n)).find_map(|n| n.get(k, ASK).ok()))
    };
    assert_eq!(get(&lost_key), None);
    assert_eq!(get(&key("kept-before")), Some(b"1".to_vec()));
    assert_eq!(get(&key("kept-after")), Some(b"2".to_vec()));
    for n in nodes.into_iter().flatten() {
        n.stop();
    }
}
