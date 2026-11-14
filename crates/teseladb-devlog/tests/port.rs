//! A core's side of the device log (012 FR-035): a core's store recovers, writes and recovers again
//! through its book straight on the log's state, and the operations the log does not keep get the
//! errors a file system would give.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use teseladb_devlog::port::{DirectLink, LogLink, LogReq, PortBook};
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

/// A link to a log that is gone: nothing it is sent arrives.
struct Gone;

impl LogLink for Gone {
    fn send(&mut self, _req: LogReq) -> bool {
        false
    }

    fn read(&mut self, _wal: u64) -> Result<Vec<u8>, IoErrno> {
        Err(IoErrno::Io)
    }

    fn disk_full(&self) -> bool {
        false
    }

    fn wait_sync(&mut self, op: IoOp) -> IoDone {
        IoDone::Err { op, errno: IoErrno::Io }
    }
}

fn errno_of(d: Option<IoDone>) -> Option<IoErrno> {
    match d {
        Some(IoDone::Err { errno, .. }) => Some(errno),
        Some(IoDone::Read { data: Err(errno), .. }) => Some(errno),
        _ => None,
    }
}

#[test]
fn a_log_that_is_gone_fails_what_it_would_have_kept() {
    let mut book = PortBook::new(2, [5].into_iter().collect());
    assert_eq!(book.core(), 2);
    let mut link = Gone;
    let fresh = IoOp::Create { file: FileId(1), path: paths::wal_segment(2, 6), prealloc: 0 };
    assert_eq!(errno_of(book.submit(&mut link, fresh)), Some(IoErrno::Io));
    // The creation did not count: the segment is not the core's.
    let open_new = IoOp::Open { file: FileId(1), path: paths::wal_segment(2, 6) };
    assert_eq!(errno_of(book.submit(&mut link, open_new)), Some(IoErrno::NotFound));
    let open = IoOp::Open { file: FileId(2), path: paths::wal_segment(2, 5) };
    assert_eq!(errno_of(book.submit(&mut link, open)), None);
    let data = Bytes::from_vec(b"x".to_vec());
    let write = IoOp::WriteAt { file: FileId(2), offset: 0, data, batch: 0 };
    let refused = [
        write,
        IoOp::Truncate { file: FileId(2), len: 0 },
        IoOp::Sync { file: FileId(2), batch: 0 },
        IoOp::ReadAt { file: FileId(2), offset: 0, len: 1, token: 1 },
        IoOp::Remove { path: paths::wal_segment(2, 5) },
    ];
    for op in refused {
        assert_eq!(errno_of(book.submit(&mut link, op.clone())), Some(IoErrno::Io), "{op:?}");
    }
}

#[test]
fn a_removed_segment_takes_the_files_open_on_it_with_it() {
    let mut log_disk = MemDisk::new();
    let mut state = LogState::recover(&mut log_disk, 64 << 10).unwrap();
    let mut book = PortBook::new(0, BTreeSet::new());
    let mut link = DirectLink { state: &mut state, disk: &mut log_disk, core: 0 };
    let (one, two) = (paths::wal_segment(0, 1), paths::wal_segment(0, 2));
    let create = |file, path| IoOp::Create { file: FileId(file), path, prealloc: 0 };
    assert_eq!(errno_of(book.submit(&mut link, create(1, one.clone()))), None);
    assert_eq!(errno_of(book.submit(&mut link, create(2, two))), None);
    let reopen = IoOp::Open { file: FileId(3), path: one.clone() };
    assert_eq!(errno_of(book.submit(&mut link, reopen)), None);
    assert_eq!(errno_of(book.submit(&mut link, IoOp::Remove { path: one })), None);
    for (file, kept) in [(1, false), (3, false), (2, true)] {
        let sync = IoOp::Sync { file: FileId(file), batch: 0 };
        assert_eq!(book.takes(&sync), kept, "file {file}");
    }
    // A rename into or out of a WAL segment is the log's, which refuses it; one elsewhere is not.
    let other = PathBuf::from("core-00/snap/1");
    let into = IoOp::Rename { from: other.clone(), to: paths::wal_segment(0, 9) };
    assert!(book.takes(&into));
    assert!(!book.takes(&IoOp::Link { from: other.clone(), to: other.join("b") }));
}

#[test]
fn the_disk_view_leaves_what_is_not_the_wal_on_the_core_disk() {
    let mut log_disk = MemDisk::new();
    let mut state = LogState::recover(&mut log_disk, 64 << 10).unwrap();
    let mut book = PortBook::new(0, BTreeSet::new());
    let mut own = MemDisk::new();
    // A WAL segment of an earlier format, on the core's own disk.
    let old = paths::wal_segment(0, 4);
    assert!(matches!(
        own.execute(IoOp::Create { file: FileId(1), path: old.clone(), prealloc: 0 }),
        IoDone::Ok(_)
    ));
    let data = Bytes::from_vec(b"old".to_vec());
    own.execute(IoOp::WriteAt { file: FileId(1), offset: 0, data, batch: 0 });
    let mut link = DirectLink { state: &mut state, disk: &mut log_disk, core: 0 };
    let mut disk = book.disk(&mut link, &mut own);
    assert_eq!(disk.read_all(&old), Err(IoErrno::NotFound));
    assert_eq!(disk.inner_read_all(&old).unwrap(), b"old");
    let snap = PathBuf::from("core-00/snap/1");
    let made = disk.execute(IoOp::Create { file: FileId(2), path: snap.clone(), prealloc: 0 });
    assert!(matches!(made, IoDone::Ok(_)));
    let data = Bytes::from_vec(b"snap".to_vec());
    let wrote = disk.execute(IoOp::WriteAt { file: FileId(2), offset: 0, data, batch: 0 });
    assert!(matches!(wrote, IoDone::Ok(_)));
    assert_eq!(disk.read_all(&snap).unwrap(), b"snap");
    assert_eq!(disk.list(Path::new("core-00/snap")).unwrap(), vec![snap.clone()]);
    // What the view wrote outside the WAL is on the core's own disk.
    assert_eq!(own.read_all(&snap).unwrap(), b"snap");
}
