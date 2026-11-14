//! A core's side of the device log (012 FR-035): a core's store recovers, writes and recovers again
//! through its book straight on the log's state, and the operations the log does not keep get the
//! errors a file system would give.

use std::collections::BTreeSet;
use std::path::PathBuf;

use teseladb_devlog::port::{DirectLink, PortBook};
use teseladb_devlog::state::LogState;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::paths;
use teseladb_raftlog::{
    CoreStore, Disk, FileId, IoDone, IoErrno, IoOp, LogEntry, MemDisk, SimDisk, StorageConfig,
};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

const G: GroupId = GroupId(3);

fn cfg() -> StorageConfig {
    StorageConfig {
        wal_segment_size: 1 << 20,
        max_batch_bytes: 64 << 10,
        ..StorageConfig::default()
    }
}

fn entry(core: u16, index: u64) -> LogEntry {
    let data = Bytes::from_vec(format!("{core}:{index}:{}", "x".repeat(300)).into_bytes());
    LogEntry { index, term: 1, kind: EntryKind::Normal, data }
}

fn books(state: &LogState, cores: u16) -> Vec<PortBook> {
    (0..cores).map(|c| PortBook::new(c, state.segments_of(c).into_iter().collect())).collect()
}

#[test]
fn cores_recover_their_entries_from_the_log() {
    let mut log_disk = SimDisk::default();
    let mut own = [MemDisk::new(), MemDisk::new()];
    let mut state = LogState::recover(&mut log_disk, 64 << 10).unwrap();
    let mut b = books(&state, 2);
    state.allow_tidying();
    for core in 0..2u16 {
        let k = usize::from(core);
        let mut link = DirectLink { state: &mut state, disk: &mut log_disk, core };
        let mut disk = b[k].disk(&mut link, &mut own[k]);
        let mut s = CoreStore::recover(&cfg(), core, &mut disk).unwrap().0;
        s.create_group(G, Bytes::new(), Durability::Strict).unwrap();
        for i in 1..=200 {
            s.append_raft(G, &[entry(core, i)], None, Durability::Strict).unwrap();
            for (_, r) in s.run_io(&mut disk, Nanos::ZERO).tickets {
                r.unwrap();
            }
        }
    }
    // Every WAL segment of the cores is in the log, none on their own disks.
    for (core, disk) in own.iter_mut().enumerate() {
        assert!(disk.list(&paths::wal_dir(core as u16)).unwrap_or_default().is_empty());
    }
    log_disk.crash(&mut teseladb_types::Rng::new(4));
    let mut state = LogState::recover(&mut log_disk, 64 << 10).unwrap();
    let mut b = books(&state, 2);
    for core in 0..2u16 {
        let k = usize::from(core);
        assert!(!state.segments_of(core).is_empty());
        let mut link = DirectLink { state: &mut state, disk: &mut log_disk, core };
        let s = CoreStore::recover(&cfg(), core, &mut b[k].disk(&mut link, &mut own[k])).unwrap().0;
        let got = s.entries(G, 1, 201, u64::MAX).unwrap();
        assert_eq!(got.len(), 200, "core {core}");
        assert_eq!(got[99], entry(core, 100));
    }
}

#[test]
fn what_the_log_does_not_keep_is_refused() {
    let mut log_disk = MemDisk::new();
    let mut state = LogState::recover(&mut log_disk, 64 << 10).unwrap();
    let mut book = PortBook::new(1, BTreeSet::new());
    let wal = paths::wal_segment(1, 7);
    let mut link = DirectLink { state: &mut state, disk: &mut log_disk, core: 1 };
    let errno = |d: Option<IoDone>| match d {
        Some(IoDone::Err { errno, .. }) => Some(errno),
        _ => None,
    };
    let create = IoOp::Create { file: FileId(1), path: wal.clone(), prealloc: 0 };
    assert!(book.takes(&create));
    assert_eq!(errno(book.submit(&mut link, create.clone())), None);
    assert_eq!(errno(book.submit(&mut link, create)), Some(IoErrno::AlreadyExists));
    let other = PathBuf::from("core-01/snap/1");
    assert!(!book.takes(&IoOp::Remove { path: other.clone() }));
    let rename = IoOp::Rename { from: wal.clone(), to: other };
    assert_eq!(errno(book.submit(&mut link, rename)), Some(IoErrno::Other(18)));
    let rmdir = IoOp::RemoveDir { path: paths::wal_dir(1) };
    assert_eq!(errno(book.submit(&mut link, rmdir.clone())), Some(IoErrno::NotEmpty));
    let open = IoOp::Open { file: FileId(2), path: paths::wal_segment(1, 8) };
    assert_eq!(errno(book.submit(&mut link, open)), Some(IoErrno::NotFound));
    let write = IoOp::WriteAt {
        file: FileId(1),
        offset: 4,
        data: Bytes::from_vec(b"ab".to_vec()),
        batch: 0,
    };
    assert_eq!(errno(book.submit(&mut link, write)), None);
    let read = IoOp::ReadAt { file: FileId(1), offset: 3, len: 10, token: 5 };
    let Some(IoDone::Read { token: 5, data }) = book.submit(&mut link, read) else { panic!() };
    assert_eq!(&data.unwrap()[..], b"\0ab");
    assert_eq!(book.submit(&mut link, IoOp::Sync { file: FileId(1), batch: 0 }), None);
    assert_eq!(errno(book.submit(&mut link, IoOp::Truncate { file: FileId(1), len: 5 })), None);
    assert_eq!(errno(book.submit(&mut link, IoOp::Close { file: FileId(1) })), None);
    assert!(!book.takes(&IoOp::Close { file: FileId(1) }));
    assert_eq!(errno(book.submit(&mut link, IoOp::Remove { path: wal.clone() })), None);
    assert_eq!(errno(book.submit(&mut link, IoOp::Remove { path: wal })), Some(IoErrno::NotFound));
    assert_eq!(errno(book.submit(&mut link, rmdir)), None);
    // The sync asked above is answered by the next flush.
    let answers = state.flush(&mut log_disk);
    assert!(matches!(answers[..], [(1, IoDone::Ok(IoOp::Sync { .. }))]));
}

#[test]
fn a_full_disk_refuses_new_segments() {
    let mut log_disk = SimDisk::default();
    log_disk.set_capacity(0);
    let mut state = LogState::recover(&mut log_disk, 64 << 10).unwrap();
    state.create(0, 1, 0);
    state.sync(0, IoOp::SyncDir { path: paths::wal_dir(0) });
    assert!(state.flush(&mut log_disk).is_empty(), "it waits for room");
    assert!(state.disk_full());
    let mut book = PortBook::new(0, BTreeSet::new());
    let mut link = DirectLink { state: &mut state, disk: &mut log_disk, core: 0 };
    let create = IoOp::Create { file: FileId(9), path: paths::wal_segment(0, 2), prealloc: 0 };
    assert!(matches!(
        book.submit(&mut link, create),
        Some(IoDone::Err { errno: IoErrno::NoSpace, .. })
    ));
    log_disk.set_capacity(u64::MAX);
    assert!(matches!(state.flush(&mut log_disk)[..], [(0, IoDone::Ok(_))]));
    assert!(!state.disk_full());
}

#[test]
fn a_recovery_sync_on_a_full_disk_fails_as_the_disk_does() {
    let mut log_disk = SimDisk::default();
    log_disk.set_capacity(0);
    let mut state = LogState::recover(&mut log_disk, 64 << 10).unwrap();
    let mut book = PortBook::new(0, BTreeSet::new());
    let mut own = MemDisk::new();
    let mut link = DirectLink { state: &mut state, disk: &mut log_disk, core: 0 };
    let mut disk = book.disk(&mut link, &mut own);
    let create = IoOp::Create { file: FileId(1), path: paths::wal_segment(0, 1), prealloc: 0 };
    assert!(matches!(disk.execute(create), IoDone::Ok(_)));
    let sync = IoOp::SyncDir { path: paths::wal_dir(0) };
    assert_eq!(disk.execute(sync.clone()), IoDone::Err { op: sync, errno: IoErrno::NoSpace });
}
