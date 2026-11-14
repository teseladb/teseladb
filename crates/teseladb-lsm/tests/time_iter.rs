//! Iteration by time (004 story 5, T023, FR-018): `scan_time` returns every version written in
//! `(from, to]` and does not open the files whose timestamps fall outside it.

mod common;

use common::model::{Model, Row};
use common::{Gen, Op, apply, open, small, ts};
use teseladb_keys::Hlc;
use teseladb_lsm::{Commit, Kind, Lsm, LsmConfig, Step, View};
use teseladb_raftlog::{Disk, MemDisk};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);

fn by_time(
    lsm: &mut Lsm,
    disk: &mut dyn Disk,
    v: &View,
    start: &[u8],
    end: &[u8],
    from: Hlc,
    to: Hlc,
) -> (Vec<Row>, usize) {
    let mut s = v.scan_time(start, end, from, to);
    let mut out = Vec::new();
    loop {
        match s.next(lsm) {
            Step::Item(i) => out.push((i.key, i.ts, i.kind, i.value.to_vec())),
            Step::Miss(_) => {
                lsm.run_io(disk, Nanos::ZERO);
            }
            Step::Done => return (out, s.sources()),
            Step::Failed(e) => panic!("{e}"),
        }
    }
}

#[test]
fn only_the_files_that_overlap_the_interval_are_opened() {
    let cfg = LsmConfig { l0_trigger: 100, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    // Ten files, each with the writes of 100 timestamps; compaction would mix them, so the test
    // keeps them in level 0.
    let mut index = 0;
    for file in 0..10u64 {
        for i in 0..100u64 {
            index += 1;
            let t = file * 100 + i + 1;
            lsm.put(G, index, format!("k{:03}", t % 37).as_bytes(), ts(t, 0), &t.to_be_bytes())
                .unwrap();
        }
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    assert_eq!(lsm.metrics().files, 10);
    let v = lsm.view();
    let (rows, sources) = by_time(&mut lsm, &mut disk, &v, b"", b"z", ts(950, 0), ts(1000, 0));
    assert_eq!(rows.len(), 50);
    assert!(rows.iter().all(|r| r.1 > ts(950, 0) && r.1 <= ts(1000, 0)));
    // Each flush left the newest version of its 37 keys in its file and the 63 older ones in
    // the file's history file (FR-031).
    assert_eq!(sources, 2, "only the last file and its history file");
    let (rows, sources) = by_time(&mut lsm, &mut disk, &v, b"", b"z", ts(150, 0), ts(250, 0));
    // The second file with its history file, and the history file of the third: the newest
    // versions the third file holds are all past the interval.
    assert_eq!((rows.len(), sources), (100, 3));
    let (rows, _) = by_time(&mut lsm, &mut disk, &v, b"k010", b"k020", ts(0, 0), ts(2000, 0));
    assert!(
        rows.iter()
            .all(|r| r.0.as_slice() >= b"k010".as_slice() && r.0.as_slice() < b"k020".as_slice())
    );
    // Keys below every file: no file is opened.
    let (rows, sources) = by_time(&mut lsm, &mut disk, &v, b"a", b"k000", ts(0, 0), ts(2000, 0));
    assert_eq!((rows.len(), sources), (0, 0));
}

#[test]
fn deletions_and_commits_show_and_resolved_intents_do_not() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    lsm.put(G, 1, b"a", ts(10, 0), b"1").unwrap();
    lsm.delete(G, 2, b"a", ts(20, 0)).unwrap();
    lsm.put_intent(G, 3, b"b", ts(30, 0), b"txn").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let commit = Commit { ts: ts(40, 0), kind: Kind::Put, value: b"2" };
    lsm.resolve_intent(G, 4, b"b", ts(30, 0), Some(commit)).unwrap();
    lsm.delete_range(G, 5, b"c", b"d", ts(45, 0)).unwrap();
    let v = lsm.view();
    let (rows, _) = by_time(&mut lsm, &mut disk, &v, b"", b"z", ts(0, 0), ts(50, 0));
    let got: Vec<(Vec<u8>, Hlc, Kind)> = rows.into_iter().map(|r| (r.0, r.1, r.2)).collect();
    assert_eq!(
        got,
        vec![
            (b"a".to_vec(), ts(20, 0), Kind::Delete),
            (b"a".to_vec(), ts(10, 0), Kind::Put),
            (b"b".to_vec(), ts(40, 0), Kind::Put),
        ]
    );
    let s = v.scan_time(b"", b"z", ts(0, 0), ts(50, 0));
    assert_eq!(s.tombstones().len(), 0, "tombstones are known once the scan opens");
    let mut s = v.scan_time(b"", b"z", ts(40, 0), ts(50, 0));
    while !matches!(s.next(&mut lsm), Step::Done) {
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    assert_eq!(s.tombstones().len(), 1);
}

#[test]
fn random_histories_match_the_model_by_time() {
    for seed in 1..=6 {
        let mut disk = MemDisk::new();
        let mut lsm = open(&small(), &mut disk);
        let mut ops = Gen::new(seed, 40);
        let mut model = Model::default();
        let mut index = 0;
        for n in 0..2000 {
            let op = ops.next(&model);
            if matches!(op, Op::Get { .. } | Op::Scan { .. }) {
                continue;
            }
            apply(&op, &mut lsm, &mut model, &mut disk, &mut index);
            if n % 250 == 0 {
                let v = lsm.view();
                for (from, to) in [(0, 50), (100, 400), (300, 2000), (0, u64::MAX)] {
                    let (f, t) = (ts(from, u32::MAX), ts(to, 0));
                    let (rows, _) = by_time(&mut lsm, &mut disk, &v, b"k005", b"k030", f, t);
                    assert_eq!(
                        rows,
                        model.between(b"k005", b"k030", f, t),
                        "seed {seed}, op {n}, ({from}, {to}]"
                    );
                }
            }
        }
    }
}

#[test]
fn a_time_scan_leaves_files_outside_its_range_and_stays_finished() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&LsmConfig { l0_trigger: 100, ..small() }, &mut disk);
    lsm.put(G, 1, b"a", ts(5, 0), b"1").unwrap();
    lsm.flush();
    lsm.put(G, 2, b"m", ts(6, 0), b"2").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let v = lsm.view();
    let (rows, sources) = by_time(&mut lsm, &mut disk, &v, b"b", b"z", ts(0, 0), ts(10, 0));
    assert_eq!((rows.len(), sources), (1, 1));
    let mut s = v.scan_time(b"", b"z", ts(0, 0), ts(10, 0));
    while !matches!(s.next(&mut lsm), Step::Done) {
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    assert_eq!(s.next(&mut lsm), Step::Done);
}

#[test]
fn a_time_scan_on_a_stopped_core_fails() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    lsm.put(G, 1, b"a", ts(5, 0), b"1").unwrap();
    lsm.flush();
    disk.inject_error("sync", teseladb_raftlog::IoErrno::Io);
    lsm.run_io(&mut disk, Nanos::ZERO);
    let v = lsm.view();
    let mut s = v.scan_time(b"", b"z", ts(0, 0), ts(10, 0));
    assert!(matches!(s.next(&mut lsm), Step::Failed(_)));
}
