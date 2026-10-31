//! Fixed views (004 story 1, T004, FR-017): a long scan returns the same after a compaction
//! replaces the files under it, and those files stay on disk until no view holds them.

mod common;

use common::{open, scan, small, ts};
use teseladb_lsm::{Step, View};
use teseladb_raftlog::{Disk, MemDisk};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);

fn ssts(disk: &mut MemDisk, view_dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = disk
        .list(view_dir)
        .unwrap()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".sst"))
        .collect();
    v.sort();
    v
}

#[test]
fn a_long_scan_does_not_change_when_a_compaction_ends_under_it() {
    let cfg = small();
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let mut index = 0;
    for round in 0..3u64 {
        for k in 0..300u64 {
            index += 1;
            let value = format!("r{round}-k{k}-{}", "x".repeat(40));
            lsm.put(G, index, format!("k{k:04}").as_bytes(), ts(10 + round, 0), value.as_bytes())
                .unwrap();
        }
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    let view: View = lsm.view();
    let expected = scan(&mut lsm, &mut disk, &view, b"", b"z", ts(11, 0));
    let before = ssts(&mut disk, &cfg.dir);
    // Half of a scan, then a compaction of everything, then the rest of the scan.
    let mut s = view.scan(b"", b"z", ts(11, 0));
    let mut got = Vec::new();
    while got.len() < expected.len() / 2 {
        match s.next(&mut lsm) {
            Step::Item(i) => got.push((i.key, i.ts, i.kind, i.value.to_vec())),
            Step::Miss(_) => {
                lsm.run_io(&mut disk, Nanos::ZERO);
            }
            other => panic!("{other:?}"),
        }
    }
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert!(lsm.metrics().compactions_total > 0);
    let during = ssts(&mut disk, &cfg.dir);
    assert!(before.iter().all(|f| during.contains(f)), "files in use stay: {before:?} {during:?}");
    loop {
        match s.next(&mut lsm) {
            Step::Item(i) => got.push((i.key, i.ts, i.kind, i.value.to_vec())),
            Step::Miss(_) => {
                lsm.run_io(&mut disk, Nanos::ZERO);
            }
            Step::Done => break,
            Step::Failed(e) => panic!("{e}"),
        }
    }
    assert_eq!(got, expected);
    // A new view reads the compacted files and sees the same.
    let fresh = lsm.view();
    assert_eq!(scan(&mut lsm, &mut disk, &fresh, b"", b"z", ts(11, 0)), expected);
    // Once nothing holds the old files, they go.
    drop((s, view));
    lsm.run_io(&mut disk, Nanos::ZERO);
    let after = ssts(&mut disk, &cfg.dir);
    assert!(before.iter().all(|f| !after.contains(f)), "{before:?} {after:?}");
}

#[test]
fn a_view_taken_before_a_flush_still_reads_the_memtable_it_had() {
    let cfg = small();
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    lsm.put(G, 1, b"a", ts(1, 0), b"1").unwrap();
    let view = lsm.view();
    let mut s = view.scan(b"", b"z", ts(5, 0));
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.put(G, 2, b"b", ts(2, 0), b"2").unwrap();
    let mut keys = Vec::new();
    loop {
        match s.next(&mut lsm) {
            Step::Item(i) => keys.push(i.key),
            Step::Miss(_) => {
                lsm.run_io(&mut disk, Nanos::ZERO);
            }
            Step::Done => break,
            Step::Failed(e) => panic!("{e}"),
        }
    }
    assert_eq!(keys, vec![b"a".to_vec()], "the new memtable is not in the old view");
    assert_eq!(s.next(&mut lsm), Step::Done, "a finished scan stays finished");
}
