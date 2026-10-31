//! Inspection tool: summary without changes, damage it finds with and without `verify`, and a crash
//! at any point leaves a directory it reports as valid.
mod common;

use std::collections::BTreeMap;

use common::*;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_raftlog::inspect::{FileClass, inspect};
use teseladb_raftlog::{
    CoreStore, Disk, FileId, IoOp, LogEntry, MemDisk, SimDisk, SimDiskConfig, SnapshotMeta,
    open_node, paths,
};
use teseladb_types::{Bytes, Durability, GroupId, Nanos, Rng};

fn e(index: u64, len: usize) -> LogEntry {
    LogEntry {
        index,
        term: 1,
        kind: EntryKind::Normal,
        data: Bytes::from_vec(vec![(index % 251) as u8; len]),
    }
}

fn snap(index: u64) -> SnapshotMeta {
    SnapshotMeta { index, term: 1, conf: Bytes::new() }
}

/// Exact copy of the whole disk (to check that inspection touches nothing).
fn image(disk: &mut dyn Disk) -> BTreeMap<std::path::PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut dirs = vec![std::path::PathBuf::new()];
    for c in 0..2u16 {
        dirs.extend([paths::wal_dir(c), paths::snap_dir(c)]);
    }
    for d in dirs {
        for p in disk.list(&d).unwrap_or_default() {
            if let Ok(data) = disk.read_all(&p) {
                out.insert(p, data);
            }
        }
    }
    out
}

/// Directory with two groups, entries and two snapshots of one of them.
fn populated() -> MemDisk {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    open_node(&mut disk, 1, [1; 16], 0).unwrap();
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    for g in [GroupId(4), GroupId(5)] {
        s.create_group(g, Bytes::new(), Durability::Strict).unwrap();
        let entries: Vec<_> = (1..=10).map(|i| e(i, 100)).collect();
        s.append_raft(g, &entries, None, Durability::Strict).unwrap();
    }
    settle(&mut s, &mut disk, &mut now);
    s.save_snapshot(GroupId(4), snap(5), Bytes::from("state")).unwrap();
    settle(&mut s, &mut disk, &mut now);
    s.save_snapshot(GroupId(4), snap(8), Bytes::from("state")).unwrap();
    settle(&mut s, &mut disk, &mut now);
    disk
}

#[test]
fn clean_directory_is_valid_and_summarised_without_changes() {
    let mut disk = populated();
    // A temporary file recovery would delete: inspection does not touch it.
    disk.execute(IoOp::Create {
        file: FileId(1),
        path: paths::snap_dir(0).join("x.tmp"),
        prealloc: 0,
    });
    let before = image(&mut disk);
    for verify in [false, true] {
        let r = inspect(&mut disk, verify);
        assert_eq!(r.exit_code(), 0, "{r}");
        assert_eq!(r.cores.len(), 1);
        let c = &r.cores[0];
        assert_eq!(c.groups, 2);
        assert!(c.wal_records.values().sum::<u64>() > 0);
        for class in [FileClass::Wal, FileClass::GroupSnapshot, FileClass::Tmp] {
            assert!(c.files.iter().any(|f| f.class == class), "{class:?}: {:?}", c.files);
        }
        let text = r.to_string();
        assert!(text.contains("state: valid") && text.contains("core-00"), "{text}");
        assert!(text.contains("WAL records:"), "{text}");
    }
    assert_eq!(image(&mut disk), before, "inspection modifies nothing");
}

fn flip(disk: &mut dyn Disk, path: &std::path::Path, offset: u64) {
    let data = disk.read_all(path).unwrap();
    disk.execute(IoOp::Open { file: FileId(77), path: path.to_path_buf() });
    disk.execute(IoOp::WriteAt {
        file: FileId(77),
        offset,
        data: Bytes::from_vec(vec![data[offset as usize] ^ 0x40]),
        batch: 0,
    });
}

fn first(disk: &mut dyn Disk, class: FileClass) -> std::path::PathBuf {
    let r = inspect(disk, false);
    r.cores[0].files.iter().find(|f| f.class == class).expect("file").path.clone()
}

#[test]
fn damaged_wal_breaks_recovery_and_is_reported() {
    let mut disk = populated();
    let wal = first(&mut disk, FileClass::Wal);
    flip(&mut disk, &wal, 4096 + 40);
    let r = inspect(&mut disk, false);
    assert_eq!(r.exit_code(), 2, "{r}");
    assert!(r.cores[0].problems[0].what.contains("recovery"));
    assert!(r.to_string().contains("CORRUPTION"));
}

#[test]
fn old_snapshots_are_checked_with_verify() {
    let mut disk = populated();
    let snaps: Vec<_> = inspect(&mut disk, false).cores[0]
        .files
        .iter()
        .filter(|f| f.class == FileClass::GroupSnapshot)
        .map(|f| f.path.clone())
        .collect();
    assert_eq!(snaps.len(), 2);
    // The oldest one: recovery does not read it.
    flip(&mut disk, &snaps[0], 4096 + 3);
    assert_eq!(inspect(&mut disk, false).exit_code(), 0);
    let r = inspect(&mut disk, true);
    assert_eq!(r.exit_code(), 2);
    assert_eq!(r.cores[0].problems[0].path, snaps[0]);
    assert_eq!(r.cores[0].problems[0].offset, Some(0));
    // The latest one does prevent recovery.
    flip(&mut disk, &snaps[1], 4096 + 3);
    assert_eq!(inspect(&mut disk, false).exit_code(), 2);
}

#[test]
fn node_meta_problems_and_discovery_without_it() {
    let mut disk = MemDisk::new();
    let r = inspect(&mut disk, true);
    assert!(r.cores.is_empty());
    assert_eq!(r.exit_code(), 0);
    // Without `node.meta`, the cores in the directory.
    let mut s = recover(&small_cfg(), &mut disk);
    let mut now = Nanos::ZERO;
    s.create_group(GroupId(1), Bytes::new(), Durability::Strict).unwrap();
    settle(&mut s, &mut disk, &mut now);
    assert_eq!(inspect(&mut disk, true).cores.len(), 1);
    // Unreadable `node.meta`.
    teseladb_raftlog::write_atomic(
        &mut disk,
        std::path::Path::new("node.meta"),
        b"garbage".to_vec(),
        FileId(5),
    )
    .unwrap();
    let r = inspect(&mut disk, true);
    assert_eq!(r.exit_code(), 2);
    assert_eq!(r.cores.len(), 1);
    assert!(r.to_string().contains("node.meta"), "{r}");
}

#[test]
fn a_crash_at_any_point_leaves_a_valid_directory() {
    // Snapshots release WAL segments while entries keep arriving: a crash at any operation leaves
    // a directory that recovers and that inspection reports as valid.
    let cfg = small_cfg();
    for seed in 0..40u64 {
        let mut rng = Rng::new(seed);
        let mut disk =
            SimDisk::new(SimDiskConfig { torn_write_probability: 0.5, ..Default::default() });
        let mut s = CoreStore::recover(&cfg, 0, &mut disk).unwrap().0;
        let mut now = Nanos::ZERO;
        let g = GroupId(1);
        s.create_group(g, Bytes::new(), Durability::Strict).unwrap();
        settle(&mut s, &mut disk, &mut now);
        let crash_round = rng.range(20, 160);
        for round in 1..=crash_round {
            s.append_raft(g, &[e(round, 40_000)], None, Durability::Strict).unwrap();
            if round % 20 == 0 {
                s.save_snapshot(g, snap(round), Bytes::new()).unwrap();
            }
            let steps = if round == crash_round { rng.range(0, 12) } else { u64::MAX };
            let mut n = 0;
            while n < steps {
                let Some(op) = s.poll_io(now) else {
                    match s.next_deadline() {
                        Some(d) if d > now => {
                            now = d;
                            continue;
                        }
                        _ => break,
                    }
                };
                let d = disk.execute(op);
                s.on_io_done(d, now);
                n += 1;
            }
        }
        disk.crash(&mut rng);
        let (s, _) =
            CoreStore::recover(&cfg, 0, &mut disk).unwrap_or_else(|e| panic!("seed {seed}: {e:?}"));
        drop(s);
        let r = inspect(&mut disk, true);
        assert_eq!(r.exit_code(), 0, "seed {seed}: {r}");
    }
}

/// Disk that fails to read the files whose name contains `bad`.
struct FailingReads<'a> {
    inner: &'a mut MemDisk,
    bad: &'static str,
}

impl Disk for FailingReads<'_> {
    fn list(
        &mut self,
        dir: &std::path::Path,
    ) -> Result<Vec<std::path::PathBuf>, teseladb_raftlog::IoErrno> {
        self.inner.list(dir)
    }
    fn read_all(&mut self, path: &std::path::Path) -> Result<Vec<u8>, teseladb_raftlog::IoErrno> {
        if path.to_str().unwrap().contains(self.bad) {
            return Err(teseladb_raftlog::IoErrno::Io);
        }
        self.inner.read_all(path)
    }
    fn execute(&mut self, op: IoOp) -> teseladb_raftlog::IoDone {
        self.inner.execute(op)
    }
}

/// Disk that serves the first two reads of `path` and fails the next ones.
struct OnceThenFail<'a> {
    inner: &'a mut MemDisk,
    path: std::path::PathBuf,
    reads: u32,
}

impl Disk for OnceThenFail<'_> {
    fn list(
        &mut self,
        d: &std::path::Path,
    ) -> Result<Vec<std::path::PathBuf>, teseladb_raftlog::IoErrno> {
        self.inner.list(d)
    }
    fn read_all(&mut self, p: &std::path::Path) -> Result<Vec<u8>, teseladb_raftlog::IoErrno> {
        if p == self.path {
            self.reads += 1;
            if self.reads > 2 {
                return Err(teseladb_raftlog::IoErrno::Io);
            }
        }
        self.inner.read_all(p)
    }
    fn execute(&mut self, op: IoOp) -> teseladb_raftlog::IoDone {
        self.inner.execute(op)
    }
}

#[test]
fn unknown_and_unreadable_files() {
    let mut disk = populated();
    // A foreign file in the WAL directory.
    disk.execute(IoOp::Create {
        file: FileId(1),
        path: paths::wal_dir(0).join("README"),
        prealloc: 0,
    });
    let r = inspect(&mut disk, true);
    assert_eq!(r.exit_code(), 0, "{r}");
    assert!(r.cores[0].files.iter().any(|f| f.class == FileClass::Other));
    // A snapshot that cannot be read during verification (listing and recovery read it before).
    let snap = r.cores[0]
        .files
        .iter()
        .rfind(|f| f.class == FileClass::GroupSnapshot)
        .unwrap()
        .path
        .clone();
    let r = inspect(&mut OnceThenFail { inner: &mut disk, path: snap.clone(), reads: 0 }, true);
    assert_eq!(r.exit_code(), 2);
    assert!(
        r.cores[0].problems.iter().any(|p| p.path == snap && p.what.contains("cannot be read"))
    );
    // Without being able to read anything of the core: recovery fails and it is reported.
    let r = inspect(&mut FailingReads { inner: &mut disk, bad: "core-00" }, false);
    assert_eq!(r.exit_code(), 2);
}
