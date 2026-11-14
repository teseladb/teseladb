//! Recovery: `node.meta`, temporary files, invalid headers, broken batch chains and the
//! distinction between a broken tail and corruption in hand-made files.
mod common;

use std::path::{Path, PathBuf};

use common::*;
use teseladb_raftlog::format::meta::NodeMeta;
use teseladb_raftlog::format::wal::{EntryKind, WalPayload, WalRecord};
use teseladb_raftlog::format::{BLOCK, FileHeader, FileKind, PAD_BYTE};
use teseladb_raftlog::{
    CoreStore, Disk, FileId, IoDone, IoErrno, IoOp, LogEntry, MemDisk, SnapshotMeta, StorageError,
    open_node, paths, read_node_meta, salvage, write_atomic,
};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

fn put_file(disk: &mut MemDisk, path: &Path, data: Vec<u8>) {
    let f = FileId(77_777);
    let _ = disk.execute(IoOp::Remove { path: path.to_path_buf() });
    assert!(matches!(
        disk.execute(IoOp::Create { file: f, path: path.to_path_buf(), prealloc: 0 }),
        IoDone::Ok(_)
    ));
    disk.execute(IoOp::WriteAt { file: f, offset: 0, data: Bytes::from_vec(data), batch: 0 });
    disk.execute(IoOp::Close { file: f });
}

fn header(kind: FileKind, seq: u64) -> Vec<u8> {
    FileHeader::new(kind, 0, seq, 1, 0).encode().to_vec()
}

/// Valid WAL region for batch `seq` (with only the `BATCH` record).
fn wal_batch(seq: u64, prev: u64, flags: u8) -> Vec<u8> {
    let region = vec![PAD_BYTE; BLOCK - 64];
    let rec = WalRecord {
        group: GroupId(0),
        batch_seq: seq,
        flags: 0,
        payload: WalPayload::Batch {
            region_len: u32::try_from(region.len()).expect("a test region"),
            region_crc: teseladb_hash::crc32c(&region),
            prev_seq: prev,
            flags,
        },
    };
    let mut out = Vec::new();
    rec.encode_into(&mut out);
    out.extend_from_slice(&region);
    out
}

fn entry(index: u64, len: usize) -> LogEntry {
    LogEntry {
        index,
        term: 1,
        kind: EntryKind::Normal,
        data: Bytes::from_vec(vec![index.to_le_bytes()[0]; len]),
    }
}

fn recover_err(disk: &mut MemDisk) -> StorageError {
    CoreStore::recover(&small_cfg(), 0, disk).map(|_| ()).expect_err("it should have failed")
}

/// A disk that acknowledged a sync it did not keep (a virtual machine that stops with a full disk) can leave `node.meta`
/// zeroed after the rename of its temporary file, while the temporary file of the next write is whole: the node takes
/// that one, whose checksum proves it was written completely, instead of refusing to start.
#[test]
fn a_zeroed_node_meta_is_taken_from_its_whole_temporary_file() {
    let mut disk = MemDisk::new();
    let m = open_node(&mut disk, 4, [3; 16], 1000).expect("open_node succeeds");
    let newer = NodeMeta { boot_epoch: 7, hlc_upper_bound: 9_000, ..m.clone() };
    put_file(&mut disk, Path::new("node.meta"), vec![0; BLOCK]);
    put_file(&mut disk, Path::new("node.meta.tmp"), newer.encode().to_vec());
    assert_eq!(read_node_meta(&mut disk), Ok(Some(newer.clone())));
    let reopened =
        open_node(&mut disk, 4, [0; 16], 2000).expect("it starts from the temporary file");
    assert_eq!((reopened.boot_epoch, reopened.hlc_upper_bound), (8, 9_000));
    assert_eq!(read_node_meta(&mut disk), Ok(Some(reopened)), "node.meta is whole again");

    // Both damaged: corruption, as before.
    put_file(&mut disk, Path::new("node.meta"), vec![0; BLOCK]);
    put_file(&mut disk, Path::new("node.meta.tmp"), vec![1; BLOCK]);
    assert!(matches!(read_node_meta(&mut disk), Err(StorageError::Corrupt { .. })));
    // A damaged node.meta without a temporary file is corruption too.
    let _ = disk.execute(IoOp::Remove { path: "node.meta.tmp".into() });
    assert!(matches!(read_node_meta(&mut disk), Err(StorageError::Corrupt { .. })));
}

#[test]
fn node_meta_lifecycle() {
    let mut disk = MemDisk::new();
    let m = open_node(&mut disk, 4, [1; 16], 1000).expect("open_node succeeds");
    assert_eq!((m.cores, m.boot_epoch, m.created_unix_ms), (4, 1, 1000));
    let m2 = open_node(&mut disk, 4, [9; 16], 2000).expect("open_node succeeds");
    assert_eq!(m2.boot_epoch, 2);
    assert_eq!(m2.cluster_id, [1; 16], "does not change on reopening");
    assert_eq!(
        open_node(&mut disk, 8, [1; 16], 0),
        Err(StorageError::CoreCountMismatch { meta: 4, requested: 8 })
    );
    // Corrupt and unknown version.
    let mut bad = m2.encode();
    bad[100] ^= 1;
    put_file(&mut disk, Path::new("node.meta"), bad.to_vec());
    assert!(matches!(open_node(&mut disk, 4, [0; 16], 0), Err(StorageError::Corrupt { .. })));
    let mut v = NodeMeta { cores: 4, ..m2.clone() }.encode();
    v[8] = 7;
    let crc = teseladb_hash::crc32c(&v[..BLOCK - 4]);
    v[BLOCK - 4..].copy_from_slice(&crc.to_le_bytes());
    put_file(&mut disk, Path::new("node.meta"), v.to_vec());
    assert_eq!(open_node(&mut disk, 4, [0; 16], 0), Err(StorageError::FormatVersion { found: 7 }));
    // I/O errors.
    let mut d = MemDisk::new();
    d.inject_error("read", IoErrno::Io);
    // read_all does not go through execute: a write failure is simulated.
    d.inject_error("sync", IoErrno::Io);
    assert!(matches!(open_node(&mut d, 1, [0; 16], 0), Err(StorageError::Io { op: "sync", .. })));
    assert!(matches!(
        write_atomic(&mut MemDisk::with_capacity(10), Path::new("x"), vec![0; 100], FileId(1)),
        Err(StorageError::Io { op: "write", .. })
    ));
}

#[test]
fn removes_temporaries_and_ignores_unknown_files() {
    let mut disk = MemDisk::new();
    put_file(&mut disk, &paths::tmp(&paths::wal_segment(0, 3)), vec![1, 2, 3]);
    put_file(&mut disk, &paths::tmp(&paths::group_snapshot(0, GroupId(1), 4)), vec![1, 2, 3]);
    put_file(&mut disk, &paths::wal_dir(0).join("README"), vec![1]);
    put_file(&mut disk, &paths::snap_dir(0).join("README"), vec![1]);
    let (_, rec) = CoreStore::recover(&small_cfg(), 0, &mut disk).expect("recover succeeds");
    assert!(rec.groups.is_empty());
    for dir in [paths::wal_dir(0), paths::snap_dir(0)] {
        let names: Vec<PathBuf> = disk.list(&dir).expect("list succeeds");
        assert!(
            names.iter().all(|p| !p.extension().is_some_and(|e| e.eq_ignore_ascii_case("tmp"))),
            "{names:?}"
        );
        assert!(names.iter().any(|p| p.ends_with("README")), "unknown files stay: {names:?}");
    }
}

#[test]
fn bad_file_headers() {
    let path = paths::wal_segment(0, 1);
    let mut disk = MemDisk::new();
    put_file(&mut disk, &path, header(FileKind::Snapshot, 1));
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }), "{path:?}");
    let mut h = header(FileKind::Wal, 1);
    h[3] ^= 1;
    put_file(&mut disk, &path, h);
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));
    let mut v = header(FileKind::Wal, 1);
    v[8] = 3;
    let crc = teseladb_hash::crc32c(&v[..BLOCK - 4]);
    v[BLOCK - 4..].copy_from_slice(&crc.to_le_bytes());
    put_file(&mut disk, &path, v);
    assert_eq!(recover_err(&mut disk), StorageError::FormatVersion { found: 3 });
}

#[test]
fn wal_chain_rules() {
    // Two chained batches: valid; the next batch number continues.
    let mut disk = MemDisk::new();
    let mut w = header(FileKind::Wal, 1);
    w.extend(wal_batch(1, 0, 0));
    w.extend(wal_batch(2, 1, 0));
    put_file(&mut disk, &paths::wal_segment(0, 1), w.clone());
    let (s, rec) = CoreStore::recover(&small_cfg(), 0, &mut disk).expect("recover succeeds");
    assert_eq!(rec.last_batch, 2);
    assert_eq!(s.next_batch_seq(), 3);
    assert_eq!(s.core(), 0);
    assert_eq!(s.config().wal_segment_size, small_cfg().wal_segment_size);

    // Gap in the chain without `resumed`: corruption even though the checksums match.
    let mut disk = MemDisk::new();
    let mut w = header(FileKind::Wal, 1);
    w.extend(wal_batch(1, 0, 0));
    w.extend(wal_batch(5, 1, 0));
    put_file(&mut disk, &paths::wal_segment(0, 1), w);
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));

    // With `resumed` the jump is valid.
    let mut disk = MemDisk::new();
    let mut w = header(FileKind::Wal, 1);
    w.extend(wal_batch(1, 0, 0));
    w.extend(wal_batch(5, 1, 1));
    put_file(&mut disk, &paths::wal_segment(0, 1), w);
    assert_eq!(
        CoreStore::recover(&small_cfg(), 0, &mut disk).expect("recover succeeds").1.last_batch,
        5
    );

    // Broken tail in a segment that is not the last one and without a resumption afterwards: corruption.
    let mut disk = MemDisk::new();
    let mut a = header(FileKind::Wal, 1);
    a.extend(wal_batch(1, 0, 0));
    let mut torn = wal_batch(2, 1, 0);
    torn[512..1024].fill(0); // a whole unwritten sector
    a.extend(torn);
    let mut b = header(FileKind::Wal, 2);
    b.extend(wal_batch(3, 2, 0));
    put_file(&mut disk, &paths::wal_segment(0, 1), a.clone());
    put_file(&mut disk, &paths::wal_segment(0, 2), b);
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));

    // The same broken tail with the next segment starting with a resumption: valid.
    let mut disk = MemDisk::new();
    let mut b = header(FileKind::Wal, 2);
    b.extend(wal_batch(3, 1, 1));
    put_file(&mut disk, &paths::wal_segment(0, 1), a);
    put_file(&mut disk, &paths::wal_segment(0, 2), b);
    let (_, rec) = CoreStore::recover(&small_cfg(), 0, &mut disk).expect("recover succeeds");
    assert_eq!(rec.last_batch, 3);

    // A damaged batch followed by another valid header: corruption.
    let mut disk = MemDisk::new();
    let mut w = header(FileKind::Wal, 1);
    let mut bad = wal_batch(1, 0, 0);
    bad[700] ^= 1;
    w.extend(bad);
    w.extend(wal_batch(2, 1, 0));
    put_file(&mut disk, &paths::wal_segment(0, 1), w);
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));

    // Last batch with a rotten bit and no zero sectors: corruption, never truncation.
    let mut disk = MemDisk::new();
    let mut w = header(FileKind::Wal, 1);
    let mut rot = wal_batch(1, 0, 0);
    rot[2000] ^= 1;
    w.extend(rot);
    put_file(&mut disk, &paths::wal_segment(0, 1), w);
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));
}

#[test]
fn wal_region_with_garbage_record_is_corrupt() {
    // Region with a correct checksum but an unreadable record inside.
    let mut region = vec![PAD_BYTE; BLOCK - 64];
    region[0..2].copy_from_slice(&0x5752u16.to_le_bytes());
    region[2] = 0x42; // unknown `kind`
    let rec = WalRecord {
        group: GroupId(0),
        batch_seq: 1,
        flags: 0,
        payload: WalPayload::Batch {
            region_len: u32::try_from(region.len()).expect("a test region"),
            region_crc: teseladb_hash::crc32c(&region),
            prev_seq: 0,
            flags: 0,
        },
    };
    let mut w = header(FileKind::Wal, 1);
    rec.encode_into(&mut w);
    w.extend_from_slice(&region);
    let mut disk = MemDisk::new();
    put_file(&mut disk, &paths::wal_segment(0, 1), w);
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));
}

#[test]
#[expect(
    clippy::many_single_char_names,
    reason = "one letter per store, disk and group of the case"
)]
fn snapshot_read_errors_and_unknown_completions() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let g = group_with_snapshots(&mut disk);
    let now = Nanos::ZERO;
    let mut s = recover(&cfg, &mut disk);
    // I/O error in the read of the latest snapshot.
    let (meta, t) = s.load_snapshot(g).expect("there is a snapshot");
    assert_eq!(meta.index, 300);
    disk.inject_error("read", IoErrno::Io);
    let c = s.run_io(&mut disk, now);
    assert_eq!(c.snapshots.len(), 1);
    assert_eq!(c.snapshots[0].0, t);
    assert_eq!(c.snapshots[0].1, Err(StorageError::Io { op: "read", errno: IoErrno::Io }));
    // Without the error the same read returns the snapshot.
    let (_, t2) = s.load_snapshot(g).expect("load_snapshot succeeds");
    let c = s.run_io(&mut disk, now);
    assert!(matches!(&c.snapshots[..], [(t, Ok((m, _)))] if *t == t2 && m.index == 300));
    // A group without snapshots has nothing to load.
    assert!(s.load_snapshot(GroupId(77)).is_none());
    // An I/O result that matches nothing is ignored.
    let none = s.on_io_done(IoDone::Read { token: 999, data: Ok(Bytes::new()) }, now);
    assert!(none.is_empty());
    let none = s.on_io_done(IoDone::Ok(IoOp::Close { file: FileId(1) }), now);
    assert!(none.is_empty());
}

#[test]
fn batch_seq_crosses_2_pow_32() {
    // A WAL whose last batch is 2^32 − 2: the next ones cross the 32-bit limit.
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    let start = (1u64 << 32) - 2;
    let mut w = header(FileKind::Wal, 1);
    w.extend(wal_batch(start, 0, 1));
    put_file(&mut disk, &paths::wal_segment(0, 1), w);
    let mut s = recover(&cfg, &mut disk);
    assert_eq!(s.next_batch_seq(), start + 1);
    let mut now = Nanos::ZERO;
    let g = GroupId(1);
    s.create_group(g, Bytes::new(), Durability::Strict).expect("create_group succeeds");
    settle(&mut s, &mut disk, &mut now);
    for i in 1..=6 {
        s.append_raft(g, &[entry(i, 100)], None, Durability::Strict).expect("append_raft succeeds");
        let c = settle(&mut s, &mut disk, &mut now);
        assert_eq!(ok_tickets(&c).len(), 1);
    }
    assert!(s.next_batch_seq() > 1 << 32);
    let s2 = recover(&cfg, &mut disk);
    assert_eq!(s2.entries(g, 1, 7, u64::MAX).expect("entries succeeds").len(), 6);
}

#[test]
fn salvage_truncates_at_the_last_complete_batch() {
    let cfg = small_cfg();
    let mut disk = MemDisk::new();
    // Recoverable: touches nothing.
    let mut s = recover(&cfg, &mut disk);
    let mut now = Nanos::ZERO;
    let g = GroupId(1);
    s.create_group(g, Bytes::new(), Durability::Strict).expect("create_group succeeds");
    s.append_raft(g, &[entry(1, 100)], None, Durability::Strict).expect("append_raft succeeds");
    settle(&mut s, &mut disk, &mut now);
    assert!(salvage(&cfg, 0, &mut disk).expect("salvage succeeds").is_empty());
    let last = CoreStore::recover(&cfg, 0, &mut disk).expect("recover succeeds").1.last_batch;

    // WAL with a rotten batch in the middle and another segment after it: truncated and the rest deleted.
    let mut a = header(FileKind::Wal, 10);
    a.extend(wal_batch(100, last, 1));
    let mut rot = wal_batch(101, 100, 0);
    rot[2000] ^= 1;
    a.extend(rot);
    a.extend(wal_batch(102, 101, 0));
    let mut b = header(FileKind::Wal, 11);
    b.extend(wal_batch(103, 102, 0));
    put_file(&mut disk, &paths::wal_segment(0, 10), a);
    put_file(&mut disk, &paths::wal_segment(0, 11), b);
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));
    let report = salvage(&cfg, 0, &mut disk).expect("salvage succeeds");
    assert_eq!(report.removed, vec![paths::wal_segment(0, 11)]);
    assert_eq!(report.truncated.len(), 1, "{report:?}");
    assert!(disk.read_all(&paths::core_dir(0).join("SALVAGED")).is_ok());
    let s2 = recover(&cfg, &mut disk);
    assert_eq!(
        s2.entries(g, 1, 2, u64::MAX).expect("entries succeeds")[0],
        entry(1, 100),
        "what precedes the damage is kept"
    );
}

#[test]
fn broken_chains_are_corruption() {
    // WAL: the second batch does not chain to the first (and it is not a broken tail: more follows).
    let mut disk = MemDisk::new();
    let mut w = header(FileKind::Wal, 1);
    w.extend(wal_batch(1, 0, 0));
    w.extend(wal_batch(2, 7, 0));
    w.extend(wal_batch(3, 2, 0));
    put_file(&mut disk, &paths::wal_segment(0, 1), w);
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));
}

fn group_with_snapshots(disk: &mut MemDisk) -> GroupId {
    let cfg = small_cfg();
    let mut s = recover(&cfg, disk);
    let mut now = Nanos::ZERO;
    let g = GroupId(3);
    s.create_group(g, Bytes::new(), Durability::Strict).expect("create_group succeeds");
    for i in 1..=300u64 {
        let e = LogEntry {
            index: i,
            term: 1,
            kind: EntryKind::Normal,
            data: Bytes::from_vec(vec![1; 20_000]),
        };
        s.append_raft(g, &[e], None, Durability::Strict).expect("append_raft succeeds");
        if i % 50 == 0 {
            s.save_snapshot(
                g,
                SnapshotMeta { index: i, term: 1, conf: Bytes::new() },
                Bytes::new(),
            )
            .expect("save_snapshot succeeds");
        }
        settle(&mut s, disk, &mut now);
    }
    g
}

#[test]
fn damaged_lost_or_foreign_latest_snapshot_is_corruption() {
    let snaps = |disk: &mut MemDisk| -> Vec<PathBuf> {
        let mut v: Vec<_> = disk
            .list(&paths::snap_dir(0))
            .expect("list succeeds")
            .into_iter()
            .filter(|p| p.to_str().expect("to_str succeeds").contains("/g"))
            .collect();
        v.sort();
        v
    };
    // Damaged.
    let mut disk = MemDisk::new();
    group_with_snapshots(&mut disk);
    let latest = snaps(&mut disk).pop().expect("pop succeeds");
    let mut data = disk.read_all(&latest).expect("read_all succeeds");
    let n = data.len();
    data[n - 1] ^= 1;
    put_file(&mut disk, &latest, data);
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));

    // From another group with this one's name.
    let mut disk = MemDisk::new();
    let g = group_with_snapshots(&mut disk);
    let latest = snaps(&mut disk).pop().expect("pop succeeds");
    let data = disk.read_all(&latest).expect("read_all succeeds");
    let name = latest
        .file_name()
        .expect("file_name succeeds")
        .to_str()
        .expect("to_str succeeds")
        .replace(&format!("g{:016x}", g.0), "g0000000000000009");
    put_file(&mut disk, &paths::snap_dir(0).join(&name), data.clone());
    let foreign = paths::snap_dir(0).join(name);
    let s = recover(&small_cfg(), &mut disk);
    assert!(s.group(GroupId(9)).is_none());
    drop(s);
    assert!(disk.read_all(&foreign).is_err(), "snapshot of a group that does not exist: deleted");
    put_file(&mut disk, &latest, {
        let mut other = data;
        // Same contents but the index in the name does not match.
        other.truncate(other.len());
        other
    });
    let renamed =
        latest.to_str().expect("to_str succeeds").replace("-i00000000000001", "-i00000000000002");
    let bytes = disk.read_all(&latest).expect("read_all succeeds");
    disk.execute(IoOp::Remove { path: latest.clone() });
    put_file(&mut disk, Path::new(&renamed), bytes);
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));

    // Another existing group's snapshot under this group's name.
    let mut disk = MemDisk::new();
    let g = group_with_snapshots(&mut disk);
    let mut s = recover(&small_cfg(), &mut disk);
    s.create_group(GroupId(9), Bytes::new(), Durability::Strict).expect("create_group succeeds");
    let mut now = Nanos::ZERO;
    settle(&mut s, &mut disk, &mut now);
    drop(s);
    let latest = snaps(&mut disk).pop().expect("pop succeeds");
    let data = disk.read_all(&latest).expect("read_all succeeds");
    let name = latest
        .file_name()
        .expect("file_name succeeds")
        .to_str()
        .expect("to_str succeeds")
        .replace(&format!("g{:016x}", g.0), "g0000000000000009");
    let foreign = paths::snap_dir(0).join(name);
    put_file(&mut disk, &foreign, data);
    match recover_err(&mut disk) {
        StorageError::Corrupt { file, offset: 0 } => assert_eq!(file, foreign),
        e => panic!("{e:?}"),
    }

    // Lost: the entries released from the WAL are no longer covered by anything.
    let mut disk = MemDisk::new();
    group_with_snapshots(&mut disk);
    for p in snaps(&mut disk) {
        disk.execute(IoOp::Remove { path: p });
    }
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));
}

#[test]
fn salvage_edge_cases() {
    let cfg = small_cfg();
    // A WAL segment cut in the middle of a batch, and another that ends right at its last batch.
    let mut disk = MemDisk::new();
    let mut a = header(FileKind::Wal, 1);
    a.extend(wal_batch(1, 0, 0));
    a.extend(wal_batch(2, 1, 0));
    let mut cut = header(FileKind::Wal, 2);
    cut.extend(wal_batch(3, 2, 0));
    cut.extend(&wal_batch(4, 3, 0)[..1000]);
    cut.extend([7u8; 100]);
    // Behind it, another segment without resumption: the cut cannot be a broken tail.
    let mut after = header(FileKind::Wal, 3);
    after.extend(wal_batch(5, 4, 0));
    put_file(&mut disk, &paths::wal_segment(0, 1), a);
    put_file(&mut disk, &paths::wal_segment(0, 2), cut);
    put_file(&mut disk, &paths::wal_segment(0, 3), after);
    let r = salvage(&cfg, 0, &mut disk).expect("salvage succeeds");
    assert_eq!(r.truncated.len(), 1, "{r:?}");
    assert_eq!(r.removed, vec![paths::wal_segment(0, 3)]);
    CoreStore::recover(&cfg, 0, &mut disk).expect("recover succeeds");

    // Damage salvage cannot fix (the latest snapshot of a group): it touches nothing and fails.
    let mut disk = MemDisk::new();
    group_with_snapshots(&mut disk);
    let mut snaps = disk.list(&paths::snap_dir(0)).expect("list succeeds");
    snaps.sort();
    let latest = snaps.pop().expect("pop succeeds");
    let mut data = disk.read_all(&latest).expect("read_all succeeds");
    let n = data.len();
    data[n - 1] ^= 1;
    put_file(&mut disk, &latest, data);
    assert!(salvage(&cfg, 0, &mut disk).is_err());
    assert!(disk.read_all(&paths::core_dir(0).join("SALVAGED")).is_err());
}

/// A header whose write did not reach the disk whole: one of its two sectors with content is
/// still zero.
fn torn_header(kind: FileKind, seq: u64, keep_first: bool) -> Vec<u8> {
    let mut h = header(kind, seq);
    let zeroed = if keep_first { BLOCK - 512..BLOCK } else { 0..512 };
    h[zeroed].fill(0);
    h
}

#[test]
fn a_segment_created_by_a_crashed_run_is_discarded() {
    for keep_first in [true, false] {
        // Segment 1 ends in a broken batch; segment 2 was created but its header never got
        // written whole: recovery removes 2 and accepts the broken end of 1.
        let mut disk = MemDisk::new();
        let mut w1 = header(FileKind::Wal, 1);
        w1.extend(wal_batch(1, 0, 0));
        let mut torn = wal_batch(2, 1, 0);
        torn[512..1024].fill(0);
        w1.extend(torn);
        put_file(&mut disk, &paths::wal_segment(0, 1), w1);
        let mut w2 = torn_header(FileKind::Wal, 2, keep_first);
        w2.resize(4 * BLOCK, 0);
        put_file(&mut disk, &paths::wal_segment(0, 2), w2);
        CoreStore::recover(&small_cfg(), 0, &mut disk).expect("recovers");
        assert!(disk.read_all(&paths::wal_segment(0, 2)).is_err(), "removed");
    }
}

#[test]
fn a_last_segment_without_batches_does_not_hide_the_broken_end_of_the_previous_one() {
    // Recovery resumed in segment 2, wrote its header and the run crashed before the first batch.
    let mut disk = MemDisk::new();
    let mut w1 = header(FileKind::Wal, 1);
    w1.extend(wal_batch(1, 0, 0));
    let mut torn = wal_batch(2, 1, 0);
    torn[512..1024].fill(0);
    w1.extend(torn);
    put_file(&mut disk, &paths::wal_segment(0, 1), w1.clone());
    let mut w2 = header(FileKind::Wal, 2);
    w2.resize(3 * BLOCK, 0);
    put_file(&mut disk, &paths::wal_segment(0, 2), w2.clone());
    CoreStore::recover(&small_cfg(), 0, &mut disk).expect("recovers");
    assert!(disk.read_all(&paths::wal_segment(0, 2)).is_err());

    // The same with the first batch of segment 2 torn too.
    put_file(&mut disk, &paths::wal_segment(0, 1), w1.clone());
    let mut w2b = header(FileKind::Wal, 2);
    let mut first = wal_batch(3, 1, 1);
    first[1024..1536].fill(0);
    w2b.extend(first);
    put_file(&mut disk, &paths::wal_segment(0, 2), w2b);
    CoreStore::recover(&small_cfg(), 0, &mut disk).expect("recovers");

    // But a header of another kind, or a damaged batch followed by a good one, is corruption.
    put_file(&mut disk, &paths::wal_segment(0, 1), w1.clone());
    put_file(&mut disk, &paths::wal_segment(0, 2), header(FileKind::Snapshot, 2));
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));
    put_file(&mut disk, &paths::wal_segment(0, 1), w1);
    let mut w2c = header(FileKind::Wal, 2);
    let mut bad = wal_batch(3, 1, 1);
    bad[40] ^= 0xff;
    w2c.extend(bad);
    w2c.extend(wal_batch(4, 3, 0));
    put_file(&mut disk, &paths::wal_segment(0, 2), w2c);
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));
}

#[test]
fn a_torn_header_in_the_middle_is_corruption() {
    let mut disk = MemDisk::new();
    put_file(&mut disk, &paths::wal_segment(0, 1), torn_header(FileKind::Wal, 1, true));
    let mut w2 = header(FileKind::Wal, 2);
    w2.extend(wal_batch(1, 0, 0));
    put_file(&mut disk, &paths::wal_segment(0, 2), w2);
    assert!(matches!(recover_err(&mut disk), StorageError::Corrupt { .. }));
    // A file shorter than a header at the end is a creation that went no further.
    let mut disk = MemDisk::new();
    let mut w1 = header(FileKind::Wal, 1);
    w1.extend(wal_batch(1, 0, 0));
    put_file(&mut disk, &paths::wal_segment(0, 1), w1);
    put_file(&mut disk, &paths::wal_segment(0, 2), vec![0; 100]);
    CoreStore::recover(&small_cfg(), 0, &mut disk).expect("recovers");
}

#[test]
fn a_core_set_aside_starts_empty_and_keeps_its_files_for_salvage() {
    let mut disk = MemDisk::new();
    let g = group_with_snapshots(&mut disk);
    assert!(!teseladb_raftlog::rebuilding(&mut disk, 0));
    let moved = teseladb_raftlog::set_aside(&mut disk, 0).expect("set_aside succeeds");
    assert!(moved > 2, "{moved}");
    assert!(teseladb_raftlog::rebuilding(&mut disk, 0));
    // Recovery ignores what was set aside: an empty core.
    let s = recover(&small_cfg(), &mut disk);
    assert!(s.group(g).is_none());
    let wal = disk.list(&paths::wal_dir(0)).expect("list succeeds");
    assert!(
        !wal.is_empty()
            && wal.iter().all(|p| p.to_str().expect("to_str succeeds").ends_with(".corrupt"))
    );
    // Again: nothing new to set aside.
    drop(s);
    assert_eq!(teseladb_raftlog::set_aside(&mut disk, 0).expect("set_aside succeeds"), 0);
    let mut s = recover(&small_cfg(), &mut disk);
    // Done rebuilding: the marker goes with the next batch.
    s.clear_rebuild_marker();
    let mut now = Nanos::ZERO;
    s.create_group(GroupId(1), Bytes::new(), Durability::Strict).expect("create_group succeeds");
    settle(&mut s, &mut disk, &mut now);
    assert!(!teseladb_raftlog::rebuilding(&mut disk, 0));
}
