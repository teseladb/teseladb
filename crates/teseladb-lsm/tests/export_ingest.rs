//! Exporting and ingesting (004 story 4, T021, FR-009): an interval exported at a timestamp and
//! ingested into an empty LSM reads the same at every timestamp up to it; ingesting where there
//! is data is refused until a range tombstone and a compaction clear it.

mod common;

use common::{get, open, scan, small, ts};
use teseladb_keys::Hlc;
use teseladb_lsm::{Codec, Commit, FileRef, GcTable, Kind, Lsm, LsmConfig, LsmError};
use teseladb_raftlog::{Disk, FileId, IoOp, MemDisk};
use teseladb_types::{Bytes, GroupId, Nanos};

const G: GroupId = GroupId(1);

fn key(i: u64) -> Vec<u8> {
    format!("k{i:06}").into_bytes()
}

/// Runs the export and returns its files.
fn export(lsm: &mut Lsm, disk: &mut MemDisk, start: &[u8], end: &[u8], at: Hlc) -> Vec<FileRef> {
    let id = lsm.export(start, end, at, "export".into());
    let done = lsm.run_io(disk, Nanos::ZERO);
    let (got, files) = done.exported.into_iter().next().expect("the export finished");
    assert_eq!(got, id);
    files
}

/// Copies files to another disk, as a snapshot travels to another node.
fn send(files: &[FileRef], from: &mut MemDisk, to: &mut MemDisk) -> Vec<FileRef> {
    files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let data = from.read_all(&f.path).unwrap();
            assert_eq!(data.len() as u64, f.size);
            let path = std::path::PathBuf::from("incoming").join(f.path.file_name().unwrap());
            let id = FileId(900 + i as u32);
            to.execute(IoOp::Create { file: id, path: path.clone(), prealloc: 0 });
            to.execute(IoOp::WriteAt {
                file: id,
                offset: 0,
                data: Bytes::from_vec(data),
                batch: 0,
            });
            to.execute(IoOp::Sync { file: id, batch: 0 });
            to.execute(IoOp::Close { file: id });
            FileRef::new(path, f.size)
        })
        .collect()
}

fn ingest(
    lsm: &mut Lsm,
    disk: &mut MemDisk,
    files: Vec<FileRef>,
    start: &[u8],
    end: &[u8],
) -> Result<(), LsmError> {
    let id = lsm.ingest(files, start, end);
    let done = lsm.run_io(disk, Nanos::ZERO);
    let (got, result) = done.ingested.into_iter().next().expect("the ingestion finished");
    assert_eq!(got, id);
    result
}

#[test]
fn an_exported_interval_reads_the_same_after_ingestion() {
    let cfg = LsmConfig {
        memtable_bytes: 8 << 20,
        target_file_bytes: 8 << 20,
        l1_bytes: 32 << 20,
        cache_bytes: 16 << 20,
        // Time stands still here, so the compactions get no throttle and end at once.
        compaction_bytes_per_sec: 1 << 40,
        ..small()
    };
    let (mut a_disk, mut b_disk) = (MemDisk::new(), MemDisk::new());
    let mut a = open(&cfg, &mut a_disk);
    // About 64 MB: 30 000 keys with two versions of 1 KiB, deletions, an intent, a tombstone,
    // and writes after the export timestamp that must not travel.
    let mut index = 0;
    for round in 0..2u64 {
        for i in 0..30_000u64 {
            index += 1;
            let mut v = vec![(i % 251) as u8; 1024];
            v[0] = round as u8;
            a.put(G, index, &key(i), ts(10 + round * 10, 0), &v).unwrap();
        }
    }
    for i in (0..30_000u64).step_by(97) {
        index += 1;
        a.delete(G, index, &key(i), ts(25, 0)).unwrap();
    }
    a.put_intent(G, index + 1, &key(7), ts(26, 0), b"txn").unwrap();
    a.delete_range(G, index + 2, &key(100), &key(200), ts(27, 0)).unwrap();
    a.put(G, index + 3, &key(8), ts(40, 0), b"after").unwrap();
    a.run_io(&mut a_disk, Nanos::ZERO);
    let at = ts(30, 0);
    let files = export(&mut a, &mut a_disk, &key(0), &key(30_000), at);
    let exported: u64 = files.iter().map(|f| f.size).sum();
    assert!(exported >= 60 << 20, "{exported} bytes");
    let sent = send(&files, &mut a_disk, &mut b_disk);
    let mut b = open(&cfg, &mut b_disk);
    ingest(&mut b, &mut b_disk, sent, &key(0), &key(30_000)).unwrap();
    // The same at every timestamp up to the export's, after a restart too.
    drop(b);
    let mut b = open(&cfg, &mut b_disk);
    let (va, vb) = (a.view(), b.view());
    for read in [ts(5, 0), ts(15, 0), ts(25, 0), ts(26, 0), at] {
        let left = scan(&mut a, &mut a_disk, &va, &key(0), &key(30_000), read);
        let right = scan(&mut b, &mut b_disk, &vb, &key(0), &key(30_000), read);
        assert_eq!(left.len(), right.len(), "at {read:?}");
        assert!(left == right, "at {read:?}");
    }
    assert_eq!(
        get(&mut b, &mut b_disk, &vb, &key(8), ts(50, 0)).unwrap().3[0],
        1,
        "the later write stayed"
    );
    let intent = get(&mut b, &mut b_disk, &vb, &key(7), ts(28, 0)).unwrap();
    assert_eq!(intent.2, Kind::Intent);
}

#[test]
fn ingesting_where_there_is_data_is_refused_until_it_is_cleared() {
    let (mut a_disk, mut b_disk) = (MemDisk::new(), MemDisk::new());
    let mut a = open(&small(), &mut a_disk);
    for i in 0..50 {
        a.put(G, i + 1, &key(i), ts(10, 0), b"from a").unwrap();
    }
    let files = export(&mut a, &mut a_disk, &key(0), &key(50), ts(20, 0));
    let mut b = open(&small(), &mut b_disk);
    b.put(G, 1, &key(25), ts(5, 0), b"local").unwrap();
    let sent = send(&files, &mut a_disk, &mut b_disk);
    // In the memtable.
    assert_eq!(
        ingest(&mut b, &mut b_disk, sent.clone(), &key(0), &key(50)),
        Err(LsmError::IngestOverlap)
    );
    // In a file.
    b.flush();
    b.run_io(&mut b_disk, Nanos::ZERO);
    assert_eq!(
        ingest(&mut b, &mut b_disk, sent.clone(), &key(0), &key(50)),
        Err(LsmError::IngestOverlap)
    );
    // A tombstone alone still counts; once a compaction past the threshold removes it all, the
    // ingestion goes through.
    b.delete_range(G, 2, &key(0), &key(50), ts(6, 0)).unwrap();
    b.flush();
    b.run_io(&mut b_disk, Nanos::ZERO);
    assert_eq!(
        ingest(&mut b, &mut b_disk, sent.clone(), &key(0), &key(50)),
        Err(LsmError::IngestOverlap)
    );
    b.set_gc_thresholds(GcTable::new(vec![(key(0), key(50), ts(7, 0))]));
    b.compact_all();
    b.run_io(&mut b_disk, Nanos::ZERO);
    assert_eq!(b.metrics().files, 0);
    ingest(&mut b, &mut b_disk, sent, &key(0), &key(50)).unwrap();
    let v = b.view();
    assert_eq!(scan(&mut b, &mut b_disk, &v, b"", b"z", ts(30, 0)).len(), 50);
}

#[test]
fn a_file_outside_the_range_or_broken_is_refused() {
    let (mut a_disk, mut b_disk) = (MemDisk::new(), MemDisk::new());
    let mut a = open(&small(), &mut a_disk);
    for i in 0..10 {
        a.put(G, i + 1, &key(i), ts(10, 0), b"v").unwrap();
    }
    let files = export(&mut a, &mut a_disk, &key(0), &key(10), ts(20, 0));
    let sent = send(&files, &mut a_disk, &mut b_disk);
    let mut b = open(&small(), &mut b_disk);
    assert_eq!(
        ingest(&mut b, &mut b_disk, sent.clone(), &key(3), &key(10)),
        Err(LsmError::IngestOverlap)
    );
    let wrong_size = vec![FileRef::new(sent[0].path.clone(), sent[0].size - 1)];
    assert!(matches!(
        ingest(&mut b, &mut b_disk, wrong_size, &key(0), &key(10)),
        Err(LsmError::Corrupt { .. })
    ));
    let short = vec![FileRef::new(sent[0].path.clone(), 4096)];
    assert!(matches!(
        ingest(&mut b, &mut b_disk, short, &key(0), &key(10)),
        Err(LsmError::Corrupt { .. })
    ));
    // The refusals changed nothing: the right ingestion still works.
    ingest(&mut b, &mut b_disk, sent, &key(0), &key(10)).unwrap();
    assert_eq!(b.metrics().files, 1);
}

#[test]
fn an_export_of_part_of_the_keys_takes_only_that_part() {
    let mut disk = MemDisk::new();
    let mut a = open(&small(), &mut disk);
    for i in 0..300 {
        a.put(G, i + 1, &key(i), ts(10, 0), &[3; 200]).unwrap();
    }
    a.put_intent(G, 400, &key(150), ts(12, 0), b"txn").unwrap();
    a.flush();
    a.run_io(&mut disk, Nanos::ZERO);
    a.resolve_intent(
        G,
        401,
        &key(150),
        ts(12, 0),
        Some(Commit { ts: ts(13, 0), kind: Kind::Delete, value: b"" }),
    )
    .unwrap();
    let files = export(&mut a, &mut disk, &key(100), &key(200), ts(20, 0));
    let mut other = MemDisk::new();
    let sent = send(&files, &mut disk, &mut other);
    let mut b = open(&small(), &mut other);
    ingest(&mut b, &mut other, sent, &key(100), &key(200)).unwrap();
    let v = b.view();
    let rows = scan(&mut b, &mut other, &v, b"", b"z", ts(20, 0));
    assert_eq!(rows.len(), 99, "100 keys, one deleted by the commit");
    assert!(rows.iter().all(|r| r.0 >= key(100) && r.0 < key(200)));
    let empty = export(&mut a, &mut disk, b"x", b"y", ts(20, 0));
    assert!(empty.is_empty());
}

#[test]
fn an_export_waits_for_the_flush_a_rotation_held_back() {
    let cfg = LsmConfig { manifest_rotate_bytes: 2 * 4096, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    // The first page of the manifest is its snapshot; two flushes take it past two pages, and
    // the rotation starts right after the second one.
    for i in 0..2 {
        lsm.put(G, i + 1, &key(i), ts(10, 0), b"v").unwrap();
        lsm.flush();
    }
    loop {
        let op = lsm.poll_io(Nanos::ZERO).expect("work until the rotation");
        let rotation = matches!(&op, IoOp::Create { path, .. }
            if path.file_name().is_some_and(|n| n.to_string_lossy().starts_with("MANIFEST-")));
        let done = disk.execute(op);
        lsm.on_io_done(done, Nanos::ZERO);
        if rotation {
            break;
        }
    }
    // While the rotation runs, a memtable is frozen and an export asked for.
    lsm.put(G, 10, &key(10), ts(11, 0), b"last").unwrap();
    let id = lsm.export(&key(0), &key(20), ts(20, 0), "export".into());
    let done = lsm.run_io(&mut disk, Nanos::ZERO);
    let (got, files) = done.exported.into_iter().next().expect("the export finished");
    assert_eq!(got, id);
    let mut other = MemDisk::new();
    let sent = send(&files, &mut disk, &mut other);
    let mut b = open(&small(), &mut other);
    ingest(&mut b, &mut other, sent, &key(0), &key(20)).unwrap();
    let v = b.view();
    assert_eq!(
        get(&mut b, &mut other, &v, &key(10), ts(20, 0)).unwrap().3,
        b"last",
        "the frozen memtable went in"
    );
}

#[test]
fn an_export_does_not_wait_for_the_compaction_budget() {
    let cfg = LsmConfig { compaction_bytes_per_sec: 4096, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    for i in 0..200 {
        lsm.put(G, i + 1, &key(i), ts(10, 0), &[1; 100]).unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.compact_all();
    // The compaction spends its second of budget and stops; once it ends a second later, the
    // budget is below zero and the export still runs.
    let mut now = Nanos::ZERO;
    while lsm.metrics().compactions_total == 0 {
        lsm.run_io(&mut disk, now);
        now = now + Nanos::from_secs(1);
    }
    let id = lsm.export(&key(0), &key(200), ts(20, 0), "export".into());
    let done = lsm.run_io(&mut disk, now);
    assert_eq!(done.exported.first().map(|e| e.0), Some(id));
}

#[test]
fn a_range_snapshot_ingests_two_intervals_and_its_flushed_index_at_once() {
    // 005 FR-012: a range's local keys and its data are two intervals; they arrive in one
    // ingestion with the snapshot's index as the range's flushed index.
    let cfg = LsmConfig { compaction_bytes_per_sec: 1 << 40, ..small() };
    let (mut a_disk, mut b_disk) = (MemDisk::new(), MemDisk::new());
    let mut a = open(&cfg, &mut a_disk);
    a.put(G, 1, b"\x00g1-local", ts(10, 0), b"mark").unwrap();
    for i in 0..200u64 {
        a.put(G, 2 + i, &key(i), ts(20, 0), b"doc").unwrap();
    }
    let local = (b"\x00g1".to_vec(), b"\x00g2".to_vec());
    let data = (b"k".to_vec(), b"l".to_vec());
    let mut files = export(&mut a, &mut a_disk, &local.0, &local.1, ts(99, 0));
    files.extend(export(&mut a, &mut a_disk, &data.0, &data.1, ts(99, 0)));
    let files = send(&files, &mut a_disk, &mut b_disk);
    let mut b = open(&cfg, &mut b_disk);
    let id = b.ingest_range(files.clone(), vec![local.clone(), data.clone()], (G, 201));
    let done = b.run_io(&mut b_disk, Nanos::ZERO);
    assert_eq!(done.ingested, vec![(id, Ok(()))]);
    assert_eq!(b.flushed_index(G), 201);
    let v = b.view();
    assert_eq!(
        get(&mut b, &mut b_disk, &v, b"\x00g1-local", ts(99, 0)).map(|r| r.3),
        Some(b"mark".to_vec())
    );
    assert_eq!(scan(&mut b, &mut b_disk, &v, &data.0, &data.1, ts(99, 0)).len(), 200);
    // After a restart the files and the flushed index are there.
    drop(b);
    let mut b = open(&cfg, &mut b_disk);
    assert_eq!(b.flushed_index(G), 201);
    // Again over the same intervals: refused, nothing changes.
    let again =
        send(&export(&mut a, &mut a_disk, &data.0, &data.1, ts(99, 0)), &mut a_disk, &mut b_disk);
    let id = b.ingest_range(again, vec![local.clone(), data.clone()], (G, 300));
    let done = b.run_io(&mut b_disk, Nanos::ZERO);
    assert_eq!(done.ingested, vec![(id, Err(LsmError::IngestOverlap))]);
    assert_eq!(b.flushed_index(G), 201);
}

#[test]
fn a_file_outside_every_interval_is_refused() {
    let cfg = LsmConfig { compaction_bytes_per_sec: 1 << 40, ..small() };
    let (mut a_disk, mut b_disk) = (MemDisk::new(), MemDisk::new());
    let mut a = open(&cfg, &mut a_disk);
    a.put(G, 1, b"m-outside", ts(10, 0), b"x").unwrap();
    let files = send(&export(&mut a, &mut a_disk, b"m", b"n", ts(99, 0)), &mut a_disk, &mut b_disk);
    let mut b = open(&cfg, &mut b_disk);
    let id = b.ingest_range(
        files,
        vec![(b"a".to_vec(), b"b".to_vec()), (b"k".to_vec(), b"l".to_vec())],
        (G, 5),
    );
    let done = b.run_io(&mut b_disk, Nanos::ZERO);
    assert_eq!(done.ingested, vec![(id, Err(LsmError::IngestOverlap))]);
    assert_eq!(b.flushed_index(G), 0);
}

#[test]
fn an_embedder_that_runs_the_io_gets_what_finished_inside_poll_io() {
    // A job may end inside `poll_io` (an export whose last step needs no I/O): its completion
    // waits there until `take_completions`, so an idle LSM does not keep it forever.
    let cfg = LsmConfig { compaction_bytes_per_sec: 1 << 40, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    lsm.put(G, 1, &key(1), ts(10, 0), b"v").unwrap();
    let id = lsm.export(b"a", b"z", ts(99, 0), "export".into());
    let mut seen = Vec::new();
    while let Some(op) = lsm.poll_io(Nanos::ZERO) {
        let done = disk.execute(op);
        seen.extend(lsm.on_io_done(done, Nanos::ZERO).exported);
    }
    seen.extend(lsm.take_completions().exported);
    assert_eq!(seen.iter().map(|(i, _)| *i).collect::<Vec<_>>(), vec![id]);
    assert_eq!(lsm.take_completions(), teseladb_lsm::Completions::default(), "taken once");
}

#[test]
fn clearing_a_range_lowers_its_flushed_index_first_and_lets_its_snapshot_in() {
    // 005 FR-012: a replica that held old data of a range empties it before ingesting the
    // range's snapshot. Its flushed index goes back to 0 before anything is deleted, so a crash in
    // between never leaves an index that promises data no longer there.
    let cfg = LsmConfig { compaction_bytes_per_sec: 1 << 40, ..small() };
    let other = GroupId(2);
    let (mut a_disk, mut b_disk) = (MemDisk::new(), MemDisk::new());
    let mut a = open(&cfg, &mut a_disk);
    let mut b = open(&cfg, &mut b_disk);
    for i in 0..300u64 {
        a.put(G, i + 1, &key(i), ts(10, 0), b"new").unwrap();
        b.put(G, i + 1, &key(i), ts(5, 0), b"old").unwrap();
    }
    b.put(G, 400, b"\x00g1-local", ts(5, 0), b"mark").unwrap();
    b.put(other, 7, b"m-other", ts(5, 0), b"kept").unwrap();
    b.flush();
    b.run_io(&mut b_disk, Nanos::ZERO);
    assert_eq!(b.flushed_index(G), 400);
    let intervals = vec![(b"\x00g1".to_vec(), b"\x00g2".to_vec()), (b"k".to_vec(), b"l".to_vec())];
    let id = b.clear_range(G, intervals.clone());
    let done = b.run_io(&mut b_disk, Nanos::ZERO);
    assert_eq!(done.cleared, vec![id]);
    assert_eq!(b.flushed_index(G), 0);
    // A restart keeps the lowered index and the other range.
    drop(b);
    let mut b = open(&cfg, &mut b_disk);
    assert_eq!((b.flushed_index(G), b.flushed_index(other)), (0, 7));
    let v = b.view();
    assert_eq!(get(&mut b, &mut b_disk, &v, &key(3), ts(99, 0)), None, "the old data is gone");
    assert_eq!(
        get(&mut b, &mut b_disk, &v, b"m-other", ts(99, 0)).map(|r| r.3),
        Some(b"kept".to_vec())
    );
    // The snapshot goes in.
    let files = export(&mut a, &mut a_disk, b"k", b"l", ts(99, 0));
    let files = send(&files, &mut a_disk, &mut b_disk);
    let id = b.ingest_range(files, intervals, (G, 300));
    assert_eq!(b.run_io(&mut b_disk, Nanos::ZERO).ingested, vec![(id, Ok(()))]);
    assert_eq!(b.flushed_index(G), 300);
    let v = b.view();
    assert_eq!(
        get(&mut b, &mut b_disk, &v, &key(3), ts(99, 0)).map(|r| r.3),
        Some(b"new".to_vec())
    );
}

/// Runs the I/O of `lsm` until the reset of a clearing of `G` is installed: its flushed index is
/// back at 0, its tombstones wait for their flush and their compaction has not started.
fn until_reset(lsm: &mut Lsm, disk: &mut MemDisk) {
    while lsm.flushed_index(G) != 0 {
        let op = lsm.poll_io(Nanos::ZERO).expect("the clearing goes on");
        let done = disk.execute(op);
        lsm.on_io_done(done, Nanos::ZERO);
    }
}

#[test]
fn a_threshold_handed_while_a_range_clears_waits_for_its_ingest() {
    // 005 FR-012, FR-013: the host hands the thresholds of the core's ranges whenever one goes up,
    // also while another clearing runs. The clearing still removes everything, and the threshold
    // handed meanwhile holds over the ingested snapshot, the clearing's own no longer.
    let cfg = LsmConfig { compaction_bytes_per_sec: 1 << 40, ..small() };
    let intervals = vec![(b"k".to_vec(), b"l".to_vec())];
    for whole_table in [false, true] {
        let (mut a_disk, mut b_disk) = (MemDisk::new(), MemDisk::new());
        let mut a = open(&cfg, &mut a_disk);
        let mut b = open(&cfg, &mut b_disk);
        let mut index = 0;
        for t in [10u64, 20, 30] {
            for i in 0..50u64 {
                index += 1;
                a.put(G, index, &key(i), ts(t, 0), format!("v{t}").as_bytes()).unwrap();
            }
        }
        for i in 0..50u64 {
            b.put(G, i + 1, &key(i), ts(5, 0), b"old").unwrap();
        }
        b.flush();
        b.run_io(&mut b_disk, Nanos::ZERO);
        b.clear_range(G, intervals.clone());
        until_reset(&mut b, &mut b_disk);
        if whole_table {
            b.set_gc_thresholds(GcTable::new(vec![(key(0), key(50), ts(25, 0))]));
        } else {
            b.set_range_gc_threshold(&intervals, ts(25, 0));
        }
        b.run_io(&mut b_disk, Nanos::ZERO);
        assert_eq!(b.metrics().files, 0, "the clearing removed everything");
        let files = export(&mut a, &mut a_disk, b"k", b"l", ts(99, 0));
        let files = send(&files, &mut a_disk, &mut b_disk);
        let id = b.ingest_range(files, intervals.clone(), (G, index));
        assert_eq!(b.run_io(&mut b_disk, Nanos::ZERO).ingested, vec![(id, Ok(()))]);
        // The threshold of 25 drops v10 only: v20 is the newest version at or below it.
        b.compact_all();
        b.run_io(&mut b_disk, Nanos::ZERO);
        let v = b.view();
        let at =
            |b: &mut Lsm, disk: &mut MemDisk, t| get(b, disk, &v, &key(3), ts(t, 0)).map(|r| r.3);
        assert_eq!(at(&mut b, &mut b_disk, 15), None, "{whole_table}");
        assert_eq!(at(&mut b, &mut b_disk, 22), Some(b"v20".to_vec()), "{whole_table}");
        assert_eq!(at(&mut b, &mut b_disk, 35), Some(b"v30".to_vec()), "{whole_table}");
    }
}

/// Sizes for the ingestions under write pressure (004 T039): two levels, every flush compacted.
fn pressure_cfg() -> LsmConfig {
    LsmConfig {
        levels: 2,
        l0_trigger: 1,
        target_file_bytes: 16 << 10,
        compaction_bytes_per_sec: 1 << 40,
        ingest_compaction_bytes_per_sec: 1 << 40,
        ..small()
    }
}

/// An LSM with keys `a…` and `z…` in its last level and, if `upper`, more of them in level 1:
/// with `upper`, every level below level 0 has a file that spans `[m, n)`.
fn around_m(cfg: &LsmConfig, disk: &mut MemDisk, upper: bool) -> Lsm {
    let mut lsm = open(cfg, disk);
    let mut index = 0;
    let mut write = |lsm: &mut Lsm, disk: &mut MemDisk, n: u64| {
        for i in 0..n {
            for side in ["a", "z"] {
                index += 1;
                let k = format!("{side}{i:05}");
                lsm.put(G, index, k.as_bytes(), ts(index, 0), &[side.as_bytes()[0]; 200]).unwrap();
            }
        }
        lsm.flush();
        lsm.run_io(disk, Nanos::ZERO);
    };
    write(&mut lsm, disk, 500);
    lsm.compact_all();
    lsm.run_io(disk, Nanos::ZERO);
    if upper {
        write(&mut lsm, disk, 20);
    }
    lsm
}

/// The interval `[m, n)` exported from an LSM of its own, in more files than the level 0 limit.
fn m_files(cfg: &LsmConfig, to: &mut MemDisk) -> Vec<FileRef> {
    let mut disk = MemDisk::new();
    let mut src = open(&LsmConfig { memtable_bytes: 8 << 20, ..cfg.clone() }, &mut disk);
    for i in 0..2000u64 {
        src.put(G, i + 1, format!("m{i:05}").as_bytes(), ts(i + 1, 0), &[b'm'; 200]).unwrap();
    }
    let files = export(&mut src, &mut disk, b"m", b"n", ts(5000, 0));
    assert!(files.len() > cfg.l0_slow, "{} files", files.len());
    send(&files, &mut disk, to)
}

/// Every key the test wrote reads back: `m…` from the ingestion, `a…` and `z…` from before.
fn reads_everything(lsm: &mut Lsm, disk: &mut MemDisk) {
    let v = lsm.view();
    let rows = scan(lsm, disk, &v, b"a", b"zz", ts(u64::MAX >> 1, 0));
    let count = |p: u8| rows.iter().filter(|r| r.0[0] == p).count();
    assert_eq!((count(b'a'), count(b'm'), count(b'z')), (500, 2000, 500));
}

#[test]
fn an_ingestion_where_every_level_spans_its_interval_makes_room_in_the_last_level() {
    let cfg = pressure_cfg();
    let mut disk = MemDisk::new();
    let mut lsm = around_m(&cfg, &mut disk, true);
    let before = lsm.metrics();
    assert!(before.level_files[1] >= 1 && before.level_files[2] >= 1, "{before:?}");
    let files = m_files(&cfg, &mut disk);
    let n = files.len();
    ingest(&mut lsm, &mut disk, files, b"m", b"n").unwrap();
    let after = lsm.metrics();
    // The files went to the last level, beside the pieces of the file that spanned them.
    assert_eq!(after.level_files[0], 0, "{after:?}");
    assert!(after.level_files[2] >= before.level_files[2] + n, "{after:?}");
    assert_eq!(after.ingest_compactions_total, 1);
    assert!(after.ingest_compaction_bytes_total > 0);
    reads_everything(&mut lsm, &mut disk);
    drop(lsm);
    let mut lsm = open(&cfg, &mut disk);
    reads_everything(&mut lsm, &mut disk);
}

#[test]
fn an_ingestion_with_a_free_level_needs_no_compaction() {
    let cfg = pressure_cfg();
    let mut disk = MemDisk::new();
    let mut lsm = around_m(&cfg, &mut disk, false);
    let files = m_files(&cfg, &mut disk);
    ingest(&mut lsm, &mut disk, files, b"m", b"n").unwrap();
    // The files went to level 1, the deepest free one, and from there down as it filled up.
    let after = lsm.metrics();
    assert_eq!(after.level_files[0], 0, "{after:?}");
    assert_eq!((after.ingest_compactions_total, after.ingest_compaction_bytes_total), (0, 0));
    reads_everything(&mut lsm, &mut disk);
}

#[test]
fn the_compaction_for_an_ingestion_spends_its_own_budget() {
    let rate = 4096;
    let cfg = LsmConfig { ingest_compaction_bytes_per_sec: rate, ..pressure_cfg() };
    let mut disk = MemDisk::new();
    let mut lsm = around_m(&cfg, &mut disk, true);
    let files = m_files(&cfg, &mut disk);
    let id = lsm.ingest(files, b"m", b"n");
    // The other compactions have budget to spare; this one moves `rate` bytes per second.
    let mut seconds = 0u64;
    let result = loop {
        let done = lsm.run_io(&mut disk, Nanos::from_secs(seconds));
        if let Some((got, r)) = done.ingested.into_iter().next() {
            assert_eq!(got, id);
            break r;
        }
        seconds += 1;
        assert!(seconds < 10_000, "the ingestion never ended");
    };
    result.unwrap();
    let moved = lsm.metrics().ingest_compaction_bytes_total;
    // Each second adds `rate`, on top of the first second's; the last operation may overdraw it.
    assert!(
        moved.div_ceil(rate) >= seconds && seconds + 1 >= moved / rate - 1,
        "{moved} {seconds}"
    );
    reads_everything(&mut lsm, &mut disk);
}

#[test]
fn an_export_goes_ahead_of_a_compaction_in_progress() {
    // FR-028: a compaction of 60 KiB with 4 KiB a second of budget takes seconds; the export of
    // a range that changes core does not wait for it.
    let cfg = LsmConfig { compaction_bytes_per_sec: 4096, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    for round in 0..3u64 {
        for i in 0..200 {
            lsm.put(G, round * 200 + i + 1, &key(i), ts(10 + round, 0), &[round as u8; 100])
                .unwrap();
        }
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    lsm.compact_all();
    // It spends its second of budget and stops halfway: time stands still.
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(lsm.metrics().compactions_total, 0, "the compaction is in progress");
    let id = lsm.export(&key(0), &key(200), ts(20, 0), "export".into());
    let done = lsm.run_io(&mut disk, Nanos::ZERO);
    let (got, files) = done.exported.into_iter().next().expect("the export did not wait");
    assert_eq!(got, id);
    assert_eq!(lsm.metrics().compactions_total, 0, "the compaction still waits for its budget");
    // The compaction goes on where it stopped, and the data reads the same after both.
    let mut now = Nanos::ZERO;
    while lsm.metrics().compactions_total == 0 {
        now = now + Nanos::from_secs(1);
        lsm.run_io(&mut disk, now);
    }
    let v = lsm.view();
    for i in [0, 77, 199] {
        assert_eq!(get(&mut lsm, &mut disk, &v, &key(i), ts(20, 0)).unwrap().3, [2; 100]);
        assert_eq!(get(&mut lsm, &mut disk, &v, &key(i), ts(10, 0)).unwrap().3, [0; 100]);
    }
    // The exported files hold every version, as the files had them when it ran.
    let mut other = MemDisk::new();
    let mut b = open(&cfg, &mut other);
    let sent = send(&files, &mut disk, &mut other);
    ingest(&mut b, &mut other, sent, &key(0), &key(200)).unwrap();
    let v = b.view();
    for i in [0, 77, 199] {
        assert_eq!(get(&mut b, &mut other, &v, &key(i), ts(20, 0)).unwrap().3, [2; 100]);
        assert_eq!(get(&mut b, &mut other, &v, &key(i), ts(11, 0)).unwrap().3, [1; 100]);
    }
}

/// Two ranges side by side in the files of one core: `G` in `[k, l)` with versions and
/// history, `other` around it in `a…` and `m…`, all compacted below level 0.
fn two_ranges(cfg: &LsmConfig, disk: &mut MemDisk) -> Lsm {
    let other = GroupId(2);
    let mut lsm = open(cfg, disk);
    let mut index = 0;
    for t in [10u64, 20, 30] {
        for i in 0..2000u64 {
            index += 1;
            lsm.put(G, index, &key(i), ts(t, 0), &[b'v'; 200]).unwrap();
        }
    }
    for i in 0..200u64 {
        lsm.put(other, i + 1, format!("a{i:04}").as_bytes(), ts(5, 0), &[b'a'; 200]).unwrap();
        lsm.put(other, i + 1, format!("m{i:04}").as_bytes(), ts(5, 0), &[b'm'; 200]).unwrap();
    }
    lsm.compact_all();
    lsm.run_io(disk, Nanos::ZERO);
    lsm
}

#[test]
fn dropping_a_range_that_left_removes_its_whole_files_without_a_compaction() {
    // 004 FR-032 (D-166): the files whose keys all fall inside the range leave the version in
    // the manifest record that lowers its flushed index, with their history files, and the
    // core's other files are not rewritten.
    let cfg = LsmConfig { compaction_bytes_per_sec: 1 << 40, codec: Codec::None, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = two_ranges(&cfg, &mut disk);
    let before = lsm.metrics();
    assert!(before.files >= 4, "{} files", before.files);
    let intervals = vec![(b"k".to_vec(), b"l".to_vec())];
    let id = lsm.drop_range(G, intervals.clone());
    let done = lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(done.cleared, vec![id]);
    let after = lsm.metrics();
    assert_eq!(after.compactions_total, before.compactions_total, "nothing compacted");
    assert!(
        after.files + 3 < before.files,
        "{} files of {}: {before:?}",
        after.files,
        before.files
    );
    assert!(after.history_files < before.history_files || before.history_files == 0);
    assert_eq!(lsm.flushed_index(G), 0);
    // A restart reads the same version from the manifest.
    let files = lsm.metrics().files;
    drop(lsm);
    let mut lsm = open(&cfg, &mut disk);
    assert_eq!((lsm.metrics().files, lsm.flushed_index(G)), (files, 0));
    // What the files left of the range, an ordinary compaction collects.
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let v = lsm.view();
    assert_eq!(scan(&mut lsm, &mut disk, &v, b"k", b"l", ts(99, 0)).len(), 0);
    assert_eq!(scan(&mut lsm, &mut disk, &v, b"a", b"b", ts(99, 0)).len(), 200);
    assert_eq!(scan(&mut lsm, &mut disk, &v, b"m", b"n", ts(99, 0)).len(), 200);
}

#[test]
fn clearing_a_range_also_removes_its_whole_files_before_compacting() {
    // 004 FR-032: a clearing for an ingest drops the whole files first and still leaves the
    // intervals empty when it ends.
    let cfg = LsmConfig { compaction_bytes_per_sec: 1 << 40, codec: Codec::None, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = two_ranges(&cfg, &mut disk);
    let id = lsm.clear_range(G, vec![(b"k".to_vec(), b"l".to_vec())]);
    assert_eq!(lsm.run_io(&mut disk, Nanos::ZERO).cleared, vec![id]);
    let v = lsm.view();
    assert_eq!(scan(&mut lsm, &mut disk, &v, b"k", b"l", ts(99, 0)).len(), 0);
    assert_eq!(scan(&mut lsm, &mut disk, &v, b"a", b"b", ts(99, 0)).len(), 200);
}
