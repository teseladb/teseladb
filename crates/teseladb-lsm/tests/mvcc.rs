//! Writing and reading versions (004 story 1, T003): `put`, `delete`, `put_intent`,
//! `resolve_intent` and `delete_range`, read with `get` at several timestamps and with `scan`,
//! from memtables and from SST files, against the model.

mod common;

use common::model::Model;
use common::{get, history, open, scan, small, ts};
use teseladb_lsm::Step;
use teseladb_lsm::{Commit, EntryWrite, Kind, Lsm, LsmConfig, VersionWrite};
use teseladb_raftlog::{Disk, MemDisk};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);

#[test]
fn a_read_sees_the_newest_version_up_to_its_timestamp() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    lsm.put(G, 1, b"a", ts(10, 0), b"v10").unwrap();
    lsm.put(G, 2, b"a", ts(20, 0), b"v20").unwrap();
    lsm.delete(G, 3, b"a", ts(30, 0)).unwrap();
    lsm.put(G, 4, b"a", ts(40, 0), b"v40").unwrap();
    for flushed in [false, true] {
        if flushed {
            lsm.flush();
            lsm.run_io(&mut disk, Nanos::ZERO);
        }
        let v = lsm.view();
        let mut read = |at| get(&mut lsm, &mut disk, &v, b"a", ts(at, 0)).map(|r| r.3);
        assert_eq!(read(5), None);
        assert_eq!(read(10), Some(b"v10".to_vec()));
        assert_eq!(read(15), Some(b"v10".to_vec()));
        assert_eq!(read(25), Some(b"v20".to_vec()));
        assert_eq!(read(30), None, "deleted");
        assert_eq!(read(45), Some(b"v40".to_vec()));
        assert_eq!(get(&mut lsm, &mut disk, &v, b"b", ts(45, 0)), None);
    }
}

#[test]
fn a_scan_returns_one_version_per_key() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    for (i, k) in [b"a", b"b", b"c", b"d"].iter().enumerate() {
        for t in 1..=3u64 {
            lsm.put(G, i as u64 * 3 + t, *k, ts(t * 10, 0), format!("{t}").as_bytes()).unwrap();
        }
    }
    lsm.delete(G, 20, b"c", ts(25, 0)).unwrap();
    let v = lsm.view();
    let rows = scan(&mut lsm, &mut disk, &v, b"a", b"d", ts(25, 0));
    let got: Vec<(Vec<u8>, Vec<u8>)> = rows.into_iter().map(|r| (r.0, r.3)).collect();
    assert_eq!(got, vec![(b"a".to_vec(), b"2".to_vec()), (b"b".to_vec(), b"2".to_vec())]);
    assert!(scan(&mut lsm, &mut disk, &v, b"x", b"z", ts(99, 0)).is_empty());
}

#[test]
fn intents_show_up_and_resolve() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    lsm.put(G, 1, b"k", ts(10, 0), b"old").unwrap();
    lsm.put_intent(G, 2, b"k", ts(20, 0), b"txn-payload").unwrap();
    let v = lsm.view();
    let hit = get(&mut lsm, &mut disk, &v, b"k", ts(30, 0)).unwrap();
    assert_eq!((hit.2, hit.3), (Kind::Intent, b"txn-payload".to_vec()));
    // Aborted after a flush: the old value shows again.
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.resolve_intent(G, 3, b"k", ts(20, 0), None).unwrap();
    let v = lsm.view();
    assert_eq!(get(&mut lsm, &mut disk, &v, b"k", ts(30, 0)).unwrap().3, b"old");
    // Committed at a later timestamp.
    lsm.put_intent(G, 4, b"k", ts(40, 0), b"p").unwrap();
    let commit = Commit { ts: ts(45, 0), kind: Kind::Put, value: b"new" };
    lsm.resolve_intent(G, 5, b"k", ts(40, 0), Some(commit)).unwrap();
    let v = lsm.view();
    assert_eq!(get(&mut lsm, &mut disk, &v, b"k", ts(42, 0)).unwrap().3, b"old");
    assert_eq!(get(&mut lsm, &mut disk, &v, b"k", ts(50, 0)).unwrap().3, b"new");
    // Committed at its own timestamp, as a deletion.
    lsm.put_intent(G, 6, b"k", ts(60, 0), b"p").unwrap();
    let commit = Commit { ts: ts(60, 0), kind: Kind::Delete, value: b"" };
    lsm.resolve_intent(G, 7, b"k", ts(60, 0), Some(commit)).unwrap();
    let v = lsm.view();
    assert_eq!(get(&mut lsm, &mut disk, &v, b"k", ts(61, 0)), None);
}

#[test]
fn a_range_tombstone_hides_what_it_covers() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    for (i, k) in [b"a", b"b", b"c", b"d"].iter().enumerate() {
        lsm.put(G, i as u64 + 1, *k, ts(10, 0), b"x").unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.delete_range(G, 9, b"b", b"d", ts(20, 0)).unwrap();
    lsm.put(G, 10, b"c", ts(30, 0), b"back").unwrap();
    let v = lsm.view();
    let keys = |rows: Vec<common::model::Row>| rows.into_iter().map(|r| r.0).collect::<Vec<_>>();
    assert_eq!(keys(scan(&mut lsm, &mut disk, &v, b"a", b"z", ts(15, 0))).len(), 4);
    assert_eq!(
        keys(scan(&mut lsm, &mut disk, &v, b"a", b"z", ts(25, 0))),
        vec![b"a".to_vec(), b"d".to_vec()]
    );
    assert_eq!(
        keys(scan(&mut lsm, &mut disk, &v, b"a", b"z", ts(35, 0))),
        vec![b"a".to_vec(), b"c".to_vec(), b"d".to_vec()]
    );
    assert_eq!(get(&mut lsm, &mut disk, &v, b"b", ts(25, 0)), None);
}

#[test]
fn values_larger_than_a_block_get_a_block_of_their_own() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    let big = vec![7u8; 100_000];
    lsm.put(G, 1, b"big", ts(1, 0), &big).unwrap();
    lsm.put(G, 2, b"small", ts(1, 0), b"s").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let v = lsm.view();
    assert_eq!(get(&mut lsm, &mut disk, &v, b"big", ts(2, 0)).unwrap().3, big);
    assert_eq!(get(&mut lsm, &mut disk, &v, b"small", ts(2, 0)).unwrap().3, b"s");
}

#[test]
fn random_histories_match_the_model() {
    for seed in 1..=12 {
        let mut disk = MemDisk::new();
        let mut lsm = open(&small(), &mut disk);
        history(seed, 3000, &mut lsm, &mut disk);
        let m = lsm.metrics();
        assert!(m.compactions_total > 0 && m.level_bytes[1..].iter().any(|b| *b > 0), "{m:?}");
        assert!(m.read_files_per_get > 0.0 && m.write_amp > 1.0, "{m:?}");
    }
}

#[test]
fn random_histories_match_the_model_with_compression_and_a_tiny_cache() {
    let cfg = LsmConfig { codec: teseladb_lsm::Codec::Zlib, cache_bytes: 16 << 10, ..small() };
    for seed in 100..104 {
        let mut disk = MemDisk::new();
        let mut lsm = open(&cfg, &mut disk);
        history(seed, 3000, &mut lsm, &mut disk);
        let m = lsm.metrics();
        assert!(m.compactions_total > 0 && m.level_bytes[1..].iter().any(|b| *b > 0), "{m:?}");
        assert!(m.read_files_per_get > 0.0 && m.write_amp > 1.0, "{m:?}");
    }
}

#[test]
fn the_model_agrees_with_itself_on_resolving() {
    let mut m = Model::default();
    m.write(b"k", ts(1, 0), Kind::Intent, b"p");
    assert_eq!(m.intents(), vec![(b"k".to_vec(), ts(1, 0))]);
    m.resolve(b"k", ts(1, 0), None);
    assert!(m.intents().is_empty());
    assert_eq!(m.get(b"k", ts(5, 0)), None);
}

#[test]
fn an_entry_that_writes_nothing_still_moves_the_flushed_index() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    lsm.flush();
    assert!(
        lsm.run_io(&mut disk, Nanos::ZERO).flushed.is_empty(),
        "an empty memtable is not frozen"
    );
    // An empty range deletes nothing but is an applied entry.
    lsm.delete_range(G, 7, b"m", b"a", ts(1, 0)).unwrap();
    lsm.flush();
    let done = lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(done.flushed, vec![(G, 7)]);
    assert_eq!(lsm.metrics().files, 0);
    assert_eq!(lsm.release_floor(G), None);
}

#[test]
fn range_tombstones_fill_a_memtable_too() {
    let cfg = LsmConfig { memtable_bytes: 4096, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    for i in 0..100u64 {
        let start = format!("k{i:04}");
        let end = format!("k{i:04}z");
        lsm.delete_range(G, i + 1, start.as_bytes(), end.as_bytes(), ts(i + 1, 0)).unwrap();
    }
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert!(lsm.metrics().files > 0, "the tombstones were flushed");
    assert!(lsm.flushed_index(G) > 0);
}

#[test]
fn two_reads_of_one_block_share_its_read() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    lsm.put(G, 1, b"k", ts(1, 0), b"v").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    drop(lsm);
    let mut lsm = open(&small(), &mut disk);
    let (a, b) = (lsm.view(), lsm.view());
    let first = a.get(&mut lsm, b"k", ts(2, 0));
    let second = b.get(&mut lsm, b"k", ts(2, 0));
    assert!(matches!(first, teseladb_lsm::Lookup::Miss(_)));
    assert_eq!(first, second, "the same token");
    assert!(lsm.poll_io(Nanos::ZERO).is_some());
    assert!(lsm.poll_io(Nanos::ZERO).is_none(), "one read only");
}

#[test]
fn a_scan_skips_files_too_new_for_it_and_files_with_only_tombstones() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&LsmConfig { l0_trigger: 100, ..small() }, &mut disk);
    lsm.put(G, 1, b"a", ts(5, 0), b"old").unwrap();
    lsm.flush();
    lsm.put(G, 2, b"b", ts(50, 0), b"new").unwrap();
    lsm.flush();
    lsm.delete_range(G, 3, b"x", b"y", ts(60, 0)).unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(lsm.metrics().files, 3);
    let v = lsm.view();
    let rows = scan(&mut lsm, &mut disk, &v, b"", b"z", ts(10, 0));
    assert_eq!(rows.iter().map(|r| r.0.clone()).collect::<Vec<_>>(), vec![b"a".to_vec()]);
}

#[test]
fn an_entry_larger_than_a_memtable_is_flushed_whole() {
    // 100 versions of 1 KiB in one entry: three times the 32 KiB memtable.
    let mut disk = MemDisk::new();
    let cfg = small();
    let mut lsm = open(&cfg, &mut disk);
    let keys: Vec<Vec<u8>> = (0..100).map(|i| format!("k{i:03}").into_bytes()).collect();
    let value = vec![7u8; 1024];
    let writes: Vec<EntryWrite<'_>> =
        keys.iter().map(|k| EntryWrite { key: k, kind: Kind::Put, value: &value }).collect();
    lsm.write_entry(G, 1, ts(10, 0), &writes).unwrap();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let flushed = lsm.flushed_index(G);
    drop(lsm);
    // Whatever the flushed index says is on disk has to be there entirely.
    let mut lsm = open(&cfg, &mut disk);
    let v = lsm.view();
    let seen = keys.iter().filter(|k| get(&mut lsm, &mut disk, &v, k, ts(10, 0)).is_some()).count();
    assert_eq!((flushed, seen), (1, 100), "an entry was split across memtables");
}

#[test]
fn an_entry_writes_intents_and_their_resolutions_at_their_own_timestamps() {
    // 008: one entry retracts an intent, writes its commit and writes a new intent, each at its
    // own timestamp, and all of it lands in one memtable.
    let mut disk = MemDisk::new();
    let cfg = small();
    let mut lsm = open(&cfg, &mut disk);
    lsm.put(G, 1, b"a", ts(5, 0), b"old").unwrap();
    lsm.put_intent(G, 2, b"a", ts(10, 0), b"intent-a").unwrap();
    let big = vec![9u8; 40 << 10];
    let versions = [
        VersionWrite { key: b"a", ts: ts(10, 0), kind: Kind::Retract, value: b"" },
        VersionWrite { key: b"a", ts: ts(15, 0), kind: Kind::Put, value: b"new" },
        VersionWrite { key: b"b", ts: ts(20, 0), kind: Kind::Intent, value: &big },
        VersionWrite { key: b"c", ts: ts(20, 0), kind: Kind::Delete, value: b"" },
    ];
    lsm.write_versions(G, 3, &versions).unwrap();
    let v = lsm.view();
    assert_eq!(get(&mut lsm, &mut disk, &v, b"a", ts(12, 0)).map(|r| r.3), Some(b"old".to_vec()));
    assert_eq!(get(&mut lsm, &mut disk, &v, b"a", ts(15, 0)).map(|r| r.3), Some(b"new".to_vec()));
    let b = get(&mut lsm, &mut disk, &v, b"b", ts(20, 0)).unwrap();
    assert_eq!((b.2, b.3.len()), (Kind::Intent, big.len()));
    lsm.run_io(&mut disk, Nanos::ZERO);
    let flushed = lsm.flushed_index(G);
    drop(lsm);
    let mut lsm = open(&cfg, &mut disk);
    let v = lsm.view();
    let seen = [b"a", b"b"]
        .iter()
        .filter(|k| get(&mut lsm, &mut disk, &v, **k, ts(20, 0)).is_some())
        .count();
    assert_eq!((flushed, seen), (3, 2), "the entry was split across memtables");
}

#[test]
fn an_entry_writes_and_deletes_at_one_timestamp() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    lsm.put(G, 1, b"old", ts(5, 0), b"x").unwrap();
    let writes = [
        EntryWrite { key: b"new", kind: Kind::Put, value: b"y" },
        EntryWrite { key: b"old", kind: Kind::Delete, value: b"" },
    ];
    lsm.write_entry(G, 2, ts(10, 0), &writes).unwrap();
    let v = lsm.view();
    assert_eq!(get(&mut lsm, &mut disk, &v, b"old", ts(9, 0)).map(|r| r.3), Some(b"x".to_vec()));
    assert_eq!(get(&mut lsm, &mut disk, &v, b"old", ts(10, 0)), None);
    assert_eq!(get(&mut lsm, &mut disk, &v, b"new", ts(10, 0)).map(|r| r.3), Some(b"y".to_vec()));
}

#[test]
fn entries_applied_again_below_the_flushed_index_do_not_hold_the_log_back() {
    // After a restart the range applies its log again from its last snapshot, also the entries
    // its LSM had flushed (FR-015): they are in a file, so only what is above the flushed index
    // holds the log.
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    for i in 1..=3u64 {
        lsm.put(G, i, b"k", ts(i, 0), b"v").unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(lsm.flushed_index(G), 3);
    drop(lsm);
    let mut lsm = open(&small(), &mut disk);
    for i in 1..=3u64 {
        lsm.put(G, i, b"k", ts(i, 0), b"v").unwrap();
    }
    assert_eq!(lsm.release_floor(G), None, "everything it applied is in a file");
    lsm.put(G, 4, b"k", ts(4, 0), b"v").unwrap();
    assert_eq!(lsm.release_floor(G), Some(3), "only entry 4 is in memory alone");
}

#[test]
fn a_backward_scan_reads_its_last_keys_without_reading_the_rest() {
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    for i in 0..2_000u32 {
        let key = format!("k{i:05}");
        lsm.put(G, u64::from(i) + 1, key.as_bytes(), ts(1, 0), &[7; 64]).unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let blocks: u64 = lsm.metrics().blocks_written.iter().sum();
    drop(lsm);
    // Cold: every block it needs is read from disk.
    let mut lsm = open(&small(), &mut disk);
    let v = lsm.view();
    let mut s = v.scan_back(b"k", b"l", ts(2, 0));
    let (mut reads, mut keys) = (0, Vec::new());
    while keys.len() < 3 {
        match s.next(&mut lsm) {
            Step::Item(i) => keys.push(i.key),
            Step::Miss(_) => {
                while let Some(op) = lsm.poll_io(Nanos::ZERO) {
                    reads += 1;
                    let done = disk.execute(op);
                    lsm.on_io_done(done, Nanos::ZERO);
                }
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(keys, [b"k01999".to_vec(), b"k01998".to_vec(), b"k01997".to_vec()]);
    assert!(blocks >= 20, "the data spans {blocks} blocks");
    assert!(reads <= 2, "{reads} block reads for the last three keys");
}

/// A key with many versions, among the files of other keys in a level of several files, reads
/// each version at its mark: a point read finds a key's file of each level by a binary search
/// (H-051 of the native tests: looking at every file of a large LSM, point reads spent their time
/// comparing keys).
#[test]
fn a_key_with_many_versions_among_many_files_reads_each_version() {
    const VERSIONS: u64 = 120;
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    let mut index = 0;
    let mut put = |lsm: &mut Lsm, key: &[u8], at: u64, value: &[u8]| {
        index += 1;
        lsm.put(G, index, key, ts(at, 0), value).unwrap();
    };
    for i in 0..400u32 {
        put(&mut lsm, format!("a{i:04}").as_bytes(), 1, &[1; 300]);
    }
    // Each version of `m` 1 KiB: 120 KiB, more than one file of 64 KiB.
    for v in 1..=VERSIONS {
        put(&mut lsm, b"m", 10 + v, &[u8::try_from(v).unwrap(); 1024]);
    }
    for i in 0..400u32 {
        put(&mut lsm, format!("z{i:04}").as_bytes(), 2, &[2; 300]);
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.compact_all();
    // Rounds of I/O until the compaction of everything ran.
    for _ in 0..1000 {
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    let m = lsm.metrics();
    assert_eq!(m.level_bytes[0], 0, "everything below level 0: {m:?}");
    assert!(m.level_files.iter().skip(1).any(|n| *n > 4), "several files in a level: {m:?}");
    let view = lsm.view();
    for v in 1..=VERSIONS {
        let row = get(&mut lsm, &mut disk, &view, b"m", ts(10 + v, 0)).expect("a version");
        assert_eq!(row.3, vec![u8::try_from(v).unwrap(); 1024], "version {v}");
    }
    assert_eq!(get(&mut lsm, &mut disk, &view, b"m", ts(5, 0)), None, "none before the first");
    assert!(get(&mut lsm, &mut disk, &view, b"a0399", ts(3, 0)).is_some());
    assert!(get(&mut lsm, &mut disk, &view, b"z0000", ts(3, 0)).is_some());
}
