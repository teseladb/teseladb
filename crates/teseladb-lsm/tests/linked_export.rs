//! A range that changes core inside the node (004 FR-033, D-167): the export hands over with hard
//! links the files that fall whole in the interval, with their history files, and rewrites only
//! the rest; the ingestion takes both and reads the same as the old core at every mark.

mod common;

use common::{get, open, scan, small, ts};
use teseladb_keys::Hlc;
use teseladb_lsm::{FileRef, Kind, Lsm, LsmConfig};
use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp, MemDisk};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);
/// Bit of the identifiers of the second core's files on the shared disk.
const CORE_BIT: u32 = 1 << 24;

fn key(i: u64) -> Vec<u8> {
    format!("k{i:06}").into_bytes()
}

/// One core's view of the node's disk: both cores number their files from the same base, so the
/// second one's identifiers get [`CORE_BIT`], as each core's executor keeps its own.
struct Mount<'a> {
    disk: &'a mut MemDisk,
    bit: u32,
}

impl Mount<'_> {
    fn map(&self, op: IoOp) -> IoOp {
        let f = |id: FileId| FileId(id.0 ^ self.bit);
        match op {
            IoOp::Create { file, path, prealloc } => IoOp::Create { file: f(file), path, prealloc },
            IoOp::Open { file, path } => IoOp::Open { file: f(file), path },
            IoOp::WriteAt { file, offset, data, batch } => {
                IoOp::WriteAt { file: f(file), offset, data, batch }
            }
            IoOp::ReadAt { file, offset, len, token } => {
                IoOp::ReadAt { file: f(file), offset, len, token }
            }
            IoOp::Sync { file, batch } => IoOp::Sync { file: f(file), batch },
            IoOp::Truncate { file, len } => IoOp::Truncate { file: f(file), len },
            IoOp::Close { file } => IoOp::Close { file: f(file) },
            other => other,
        }
    }
}

impl Disk for Mount<'_> {
    fn list(&mut self, dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>, IoErrno> {
        self.disk.list(dir)
    }

    fn read_all(&mut self, path: &std::path::Path) -> Result<Vec<u8>, IoErrno> {
        self.disk.read_all(path)
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        let op = self.map(op);
        match self.disk.execute(op) {
            IoDone::Ok(op) => IoDone::Ok(self.map(op)),
            IoDone::Err { op, errno } => IoDone::Err { op: self.map(op), errno },
            read => read,
        }
    }
}

fn core(disk: &mut MemDisk, n: u32) -> Mount<'_> {
    Mount { disk, bit: if n == 0 { 0 } else { CORE_BIT } }
}

fn cfg(n: u32) -> LsmConfig {
    LsmConfig {
        dir: format!("core-{n:02}/lsm").into(),
        // Time stands still here, so the compactions get no throttle and end at once.
        compaction_bytes_per_sec: 1 << 40,
        ..small()
    }
}

fn settle(lsm: &mut Lsm, disk: &mut MemDisk, n: u32) {
    lsm.flush();
    lsm.run_io(&mut core(disk, n), Nanos::ZERO);
}

fn compact(lsm: &mut Lsm, disk: &mut MemDisk, n: u32) {
    let id = lsm.compact_now();
    assert_eq!(lsm.run_io(&mut core(disk, n), Nanos::ZERO).compacted, vec![id]);
}

/// Exports `[start, end)` of core 0 at `at` for core 1, linking what it can.
fn export(lsm: &mut Lsm, disk: &mut MemDisk, start: &[u8], end: &[u8], at: Hlc) -> Vec<FileRef> {
    let id = lsm.export_linked(start, end, at, "core-00/out-1".into());
    let done = lsm.run_io(&mut core(disk, 0), Nanos::ZERO);
    let (got, files) = done.exported.into_iter().next().expect("the export finished");
    assert_eq!(got, id);
    files
}

fn ingest(lsm: &mut Lsm, disk: &mut MemDisk, files: Vec<FileRef>, start: &[u8], end: &[u8]) {
    let id = lsm.ingest_range(files, vec![(start.to_vec(), end.to_vec())], (G, 7));
    let done = lsm.run_io(&mut core(disk, 1), Nanos::ZERO);
    assert_eq!(done.ingested, vec![(id, Ok(()))]);
}

/// Every row of `[start, end)` at `at` on core `n`.
fn rows(lsm: &mut Lsm, disk: &mut MemDisk, n: u32, start: &[u8], end: &[u8], at: Hlc) -> Vec<Row> {
    let view = lsm.view();
    scan(lsm, &mut core(disk, n), &view, start, end, at)
}

type Row = common::model::Row;

/// The version of `k` core `n` reads at `at`.
fn read(lsm: &mut Lsm, disk: &mut MemDisk, n: u32, k: &[u8], at: Hlc) -> Option<Row> {
    let view = lsm.view();
    get(lsm, &mut core(disk, n), &view, k, at)
}

#[test]
fn a_core_change_links_the_whole_files_with_their_history_and_reads_the_same() {
    let mut disk = MemDisk::new();
    let mut a = open(&cfg(0), &mut core(&mut disk, 0));
    // Three rounds over the interval, compacted in between, leave files in several levels with
    // history files; keys of another range sit in the files at both edges.
    let mut index = 0;
    for round in 0..3u64 {
        for i in 0..2_000u64 {
            index += 1;
            let mut v = vec![(i % 251) as u8; 200];
            v[0] = round as u8;
            a.put(G, index, &key(i), ts(10 + round * 10, 0), &v).unwrap();
        }
        for other in [b"a-other".as_slice(), b"z-other"] {
            index += 1;
            a.put(G, index, other, ts(10 + round * 10, 0), b"other").unwrap();
        }
        settle(&mut a, &mut disk, 0);
        if round == 1 {
            compact(&mut a, &mut disk, 0);
        }
    }
    a.delete(G, index + 1, &key(5), ts(35, 0)).unwrap();
    settle(&mut a, &mut disk, 0);
    let at = ts(40, 0);
    let files = export(&mut a, &mut disk, &key(0), &key(2_000), at);
    let linked: Vec<FileRef> = files.iter().filter(|f| f.group > 0).cloned().collect();
    let written: u64 = files.iter().filter(|f| f.group == 0).map(|f| f.size).sum();
    let whole: u64 = linked.iter().map(|f| f.size + f.history.as_ref().map_or(0, |h| h.1)).sum();
    assert!(!linked.is_empty(), "{files:?}");
    assert!(linked.iter().any(|f| f.history.is_some()), "a linked file brings its history");
    assert!(whole > written, "{whole} bytes linked, {written} written");
    let mut b = open(&cfg(1), &mut core(&mut disk, 1));
    ingest(&mut b, &mut disk, files, &key(0), &key(2_000));
    // The old core still reads its files under their own names.
    for restart in [false, true] {
        if restart {
            drop(b);
            b = open(&cfg(1), &mut core(&mut disk, 1));
        }
        for mark in [ts(5, 0), ts(10, 0), ts(15, 0), ts(20, 0), ts(30, 0), ts(35, 0), at] {
            let left = rows(&mut a, &mut disk, 0, &key(0), &key(2_000), mark);
            let right = rows(&mut b, &mut disk, 1, &key(0), &key(2_000), mark);
            assert_eq!(left.len(), right.len(), "at {mark:?}");
            assert!(left == right, "at {mark:?}");
        }
    }
    // The linked history files are there (compactions after the ingestion may write more).
    let histories = linked.iter().filter(|f| f.history.is_some()).count();
    assert!(b.metrics().history_files >= histories);
    // The old core lets the range go and compacts: the new one keeps reading its links.
    let id = a.drop_range(G, vec![(key(0), key(2_000))]);
    assert_eq!(a.run_io(&mut core(&mut disk, 0), Nanos::ZERO).cleared, vec![id]);
    compact(&mut a, &mut disk, 0);
    assert!(rows(&mut a, &mut disk, 0, &key(0), &key(2_000), at).is_empty());
    let got = rows(&mut b, &mut disk, 1, &key(0), &key(2_000), at);
    assert_eq!(got.len(), 1_999);
    compact(&mut b, &mut disk, 1);
    assert_eq!(rows(&mut b, &mut disk, 1, &key(0), &key(2_000), at), got);
}

#[test]
fn files_with_versions_above_the_mark_or_tombstones_are_rewritten() {
    let mut disk = MemDisk::new();
    let mut a = open(&cfg(0), &mut core(&mut disk, 0));
    for i in 0..500u64 {
        a.put(G, i + 1, &key(i), ts(10, 0), b"ten").unwrap();
    }
    settle(&mut a, &mut disk, 0);
    compact(&mut a, &mut disk, 0);
    // A newer version than the mark in one file, a tombstone in another.
    a.put(G, 600, &key(1), ts(50, 0), b"fifty").unwrap();
    settle(&mut a, &mut disk, 0);
    a.delete_range(G, 601, &key(400), &key(410), ts(20, 0)).unwrap();
    settle(&mut a, &mut disk, 0);
    let files = export(&mut a, &mut disk, &key(0), &key(500), ts(30, 0));
    let mut b = open(&cfg(1), &mut core(&mut disk, 1));
    ingest(&mut b, &mut disk, files.clone(), &key(0), &key(500));
    // The deepest file goes whole, under the two files of level 0 that were rewritten.
    assert_eq!(files.iter().filter(|f| f.group > 0).count(), 1, "{files:?}");
    let got = rows(&mut b, &mut disk, 1, &key(0), &key(500), ts(30, 0));
    assert_eq!(got, rows(&mut a, &mut disk, 0, &key(0), &key(500), ts(30, 0)));
    assert_eq!(got.len(), 490);
    assert!(read(&mut b, &mut disk, 1, &key(1), ts(60, 0)).is_some_and(|r| r.3 == b"ten"));
}

/// An intent in a file that goes whole, retracted in a file that is rewritten: the `Retract`
/// travels in the files written, above the link, and the intent stays retracted.
#[test]
fn a_retraction_of_an_intent_in_a_linked_file_travels_with_the_files_written() {
    let mut disk = MemDisk::new();
    let mut a = open(&cfg(0), &mut core(&mut disk, 0));
    a.put(G, 1, &key(1), ts(10, 0), b"ten").unwrap();
    a.put_intent(G, 2, &key(1), ts(20, 0), b"txn").unwrap();
    a.put(G, 3, &key(2), ts(10, 0), b"ten").unwrap();
    settle(&mut a, &mut disk, 0);
    compact(&mut a, &mut disk, 0);
    // The retraction shares its file with a key outside the interval.
    a.resolve_intent(G, 4, &key(1), ts(20, 0), None).unwrap();
    a.put(G, 5, b"z-other", ts(21, 0), b"other").unwrap();
    settle(&mut a, &mut disk, 0);
    let files = export(&mut a, &mut disk, &key(0), &key(10), ts(30, 0));
    assert_eq!(files.iter().filter(|f| f.group > 0).count(), 1, "{files:?}");
    let mut b = open(&cfg(1), &mut core(&mut disk, 1));
    ingest(&mut b, &mut disk, files, &key(0), &key(10));
    for n in [0, 1] {
        let lsm = if n == 0 { &mut a } else { &mut b };
        let got = read(lsm, &mut disk, n, &key(1), ts(25, 0)).expect("the value under it");
        assert_eq!((got.1, got.2, got.3), (ts(10, 0), Kind::Put, b"ten".to_vec()), "core {n}");
    }
}

/// A file that would go whole over a rewritten file of a deeper level is rewritten too: the files
/// written go above the links, and its `Retract` would end below the intent it retracts.
#[test]
fn a_file_over_a_rewritten_deeper_one_is_rewritten() {
    let mut disk = MemDisk::new();
    let mut a = open(&cfg(0), &mut core(&mut disk, 0));
    // The intent shares its file with a key outside the interval.
    a.put(G, 1, &key(1), ts(10, 0), b"ten").unwrap();
    a.put_intent(G, 2, &key(1), ts(20, 0), b"txn").unwrap();
    a.put(G, 3, b"z-other", ts(10, 0), b"other").unwrap();
    settle(&mut a, &mut disk, 0);
    compact(&mut a, &mut disk, 0);
    a.resolve_intent(G, 4, &key(1), ts(20, 0), None).unwrap();
    a.put(G, 5, &key(2), ts(21, 0), b"two").unwrap();
    settle(&mut a, &mut disk, 0);
    let files = export(&mut a, &mut disk, &key(0), &key(10), ts(30, 0));
    assert!(files.iter().all(|f| f.group == 0), "{files:?}");
    let mut b = open(&cfg(1), &mut core(&mut disk, 1));
    ingest(&mut b, &mut disk, files, &key(0), &key(10));
    let got = read(&mut b, &mut disk, 1, &key(1), ts(25, 0)).expect("the value under it");
    assert_eq!((got.2, got.3), (Kind::Put, b"ten".to_vec()));
}

/// Linked groups of level 0 keep their order where they are ingested, after a restart too.
#[test]
fn linked_groups_of_level_zero_keep_their_order() {
    let mut disk = MemDisk::new();
    let mut a = open(&cfg(0), &mut core(&mut disk, 0));
    // Three files of level 0, each an intent's file and the next one its retraction.
    for (i, at) in [(1u64, 10u64), (2, 20), (3, 30)] {
        a.put_intent(G, i * 2, &key(1), ts(at, 0), b"txn").unwrap();
        a.put(G, i * 2 + 1, &key(i + 1), ts(at, 0), b"v").unwrap();
        settle(&mut a, &mut disk, 0);
        a.resolve_intent(G, i * 2 + 100, &key(1), ts(at, 0), None).unwrap();
        settle(&mut a, &mut disk, 0);
    }
    let files = export(&mut a, &mut disk, &key(0), &key(10), ts(40, 0));
    assert!(files.iter().filter(|f| f.group > 0).count() >= 3, "{files:?}");
    let mut b = open(&cfg(1), &mut core(&mut disk, 1));
    // A file of another range spans the interval in the last level.
    for (i, other) in [b"a-other".as_slice(), b"z-other"].iter().enumerate() {
        b.put(GroupId(2), i as u64 + 1, other, ts(1, 0), b"o").unwrap();
    }
    settle(&mut b, &mut disk, 1);
    compact(&mut b, &mut disk, 1);
    ingest(&mut b, &mut disk, files, &key(0), &key(10));
    for restart in [false, true] {
        if restart {
            b = open(&cfg(1), &mut core(&mut disk, 1));
        }
        for at in [15, 25, 35] {
            assert_eq!(read(&mut b, &mut disk, 1, &key(1), ts(at, 0)), None, "at {at}");
        }
        assert_eq!(rows(&mut b, &mut disk, 1, &key(0), &key(10), ts(40, 0)).len(), 3);
    }
}
