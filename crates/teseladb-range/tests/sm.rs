//! `RangeSm` (005 T008, FR-001, FR-002): every command of `range-commands.md` applied over the
//! core's LSM, the same entries giving the same state on two replicas, and an unknown `kind` or a
//! key outside the range stopping the group instead of being skipped.

use std::cell::RefCell;
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Lookup, Lsm, LsmConfig, Step};
use teseladb_multiraft::{ApplyCtx, StateMachine};
use teseladb_raftlog::{IoErrno, MemDisk};
use teseladb_range::command::{Command, DedupRecord, Put};
use teseladb_range::sm::{
    RangeOutput, RangeSm, dedup_key, last_mark_key, parse_record_value, record_value, split_value,
};
use teseladb_types::{GroupId, Nanos, Rng};

const G: GroupId = GroupId(7);
const START: &[u8] = b"\x01a";
const END: &[u8] = b"\x01z";
const MAX_ROUNDS: usize = 1000;

fn ts(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

fn cfg() -> LsmConfig {
    LsmConfig { dir: "core-00/lsm".into(), memtable_bytes: 32 << 10, ..LsmConfig::default() }
}

struct Replica {
    disk: MemDisk,
    lsm: Rc<RefCell<Lsm>>,
    sm: RangeSm,
}

impl Replica {
    fn new() -> Replica {
        let mut disk = MemDisk::new();
        let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg(), &mut disk).unwrap().0));
        let sm = RangeSm::open(G, START.to_vec(), END.to_vec(), lsm.clone(), &mut disk).unwrap();
        Replica { disk, lsm, sm }
    }

    /// Flushes everything and starts again from the disk.
    fn restart(self) -> Replica {
        let Replica { mut disk, lsm, .. } = self;
        lsm.borrow_mut().flush();
        lsm.borrow_mut().run_io(&mut disk, Nanos::ZERO);
        drop(lsm);
        let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg(), &mut disk).unwrap().0));
        let sm = RangeSm::open(G, START.to_vec(), END.to_vec(), lsm.clone(), &mut disk).unwrap();
        Replica { disk, lsm, sm }
    }

    fn apply(&mut self, index: u64, at: Hlc, c: &Command) -> Result<RangeOutput, String> {
        let ctx = ApplyCtx { group: G, index, term: 1, ts: at };
        self.sm.apply(&c.encode(), &ctx).map_err(|e| e.0)
    }

    fn get(&mut self, key: &[u8], at: Hlc) -> Option<Vec<u8>> {
        let view = self.lsm.borrow().view();
        for _ in 0..MAX_ROUNDS {
            let found = view.get(&mut self.lsm.borrow_mut(), key, at);
            match found {
                Lookup::Hit(f) => return Some(f.value.to_vec()),
                Lookup::NotFound => return None,
                Lookup::Failed(e) => panic!("{e}"),
                Lookup::Miss(_) => {
                    self.lsm.borrow_mut().run_io(&mut self.disk, Nanos::ZERO);
                }
            }
        }
        panic!("get never finished")
    }

    /// Every visible key of the whole key space at `at`, local keys included.
    fn dump(&mut self, at: Hlc) -> Vec<(Vec<u8>, Vec<u8>)> {
        let view = self.lsm.borrow().view();
        let mut scan = view.scan(b"", b"\xff", at);
        let mut rows = Vec::new();
        for _ in 0..MAX_ROUNDS * 100 {
            let step = scan.next(&mut self.lsm.borrow_mut());
            match step {
                Step::Item(i) => rows.push((i.key, i.value.to_vec())),
                Step::Done => return rows,
                Step::Failed(e) => panic!("{e}"),
                Step::Miss(_) => {
                    self.lsm.borrow_mut().run_io(&mut self.disk, Nanos::ZERO);
                }
            }
        }
        panic!("scan never finished")
    }
}

fn put(key: &[u8], value: &[u8]) -> Put {
    Put { key: key.to_vec(), value_type: 3, value: value.to_vec() }
}

#[test]
fn a_write_puts_and_deletes_at_the_entry_mark() {
    let mut r = Replica::new();
    let w1 = Command::Write {
        puts: vec![put(b"\x01doc1", b"v1"), put(b"\x01doc2", b"v2")],
        deletes: vec![],
        dedup: None,
        live_delta: 0,
    };
    assert_eq!(r.apply(1, ts(10), &w1), Ok(RangeOutput::Applied { ts: ts(10) }));
    let w2 = Command::Write {
        puts: vec![put(b"\x01doc1", b"v1b")],
        deletes: vec![b"\x01doc2".to_vec()],
        dedup: None,
        live_delta: 0,
    };
    r.apply(2, ts(20), &w2).unwrap();
    assert_eq!(r.sm.applied_index(), 2);
    let read =
        |r: &mut Replica, k: &[u8], t| r.get(k, ts(t)).map(|v| split_value(&v).unwrap().1.to_vec());
    assert_eq!(read(&mut r, b"\x01doc1", 15), Some(b"v1".to_vec()));
    assert_eq!(read(&mut r, b"\x01doc1", 20), Some(b"v1b".to_vec()));
    assert_eq!(read(&mut r, b"\x01doc2", 15), Some(b"v2".to_vec()));
    assert_eq!(read(&mut r, b"\x01doc2", 20), None);
    assert_eq!(split_value(&r.get(b"\x01doc1", ts(20)).unwrap()).unwrap().0, 3, "value type kept");
    assert_eq!(r.sm.last_mark(), ts(20));
    assert!(r.get(&last_mark_key(G), ts(20)).is_some(), "the last mark is a local key");
}

#[test]
fn the_dedup_record_is_written_with_the_entry() {
    let mut r = Replica::new();
    let record = DedupRecord { lsid: 42, txn_number: 3, stmt_id: 1, result: b"n=1".to_vec() };
    let w = Command::Write {
        puts: vec![put(b"\x01d", b"x")],
        deletes: vec![],
        dedup: Some(record.clone()),
        live_delta: 0,
    };
    r.apply(1, ts(10), &w).unwrap();
    let key = dedup_key(G, record.lsid, record.txn_number, record.stmt_id);
    // The record keeps the write's mark before its result (007 FR-022: a split's copy keeps it).
    assert_eq!(r.get(&key, ts(10)), Some(record_value(ts(10), b"n=1")));
    assert_eq!(parse_record_value(&record_value(ts(10), b"n=1")), Some((ts(10), &b"n=1"[..])));
    assert_eq!(parse_record_value(b"short"), None);
    assert!(key < START.to_vec(), "local keys sort before the range's data");
}

#[test]
fn two_replicas_applying_the_same_entries_hold_the_same_state() {
    let mut a = Replica::new();
    let mut b = Replica::new();
    let mut rng = Rng::new(5);
    let mut entries = Vec::new();
    for i in 1..=300u64 {
        let key = |rng: &mut Rng| format!("\x01k{:02}", rng.range(0, 40)).into_bytes();
        let c = match rng.range(0, 10) {
            0 => Command::Noop,
            1 => Command::SetGcThreshold { ts: ts(i * 5) },
            2 => Command::ComputeChecksum { at: ts(i * 10) },
            _ => {
                let mut value = vec![0u8; rng.range(0, 2000) as usize];
                rng.fill(&mut value);
                let k = key(&mut rng);
                Command::Write {
                    puts: vec![put(&k, &value)],
                    deletes: vec![key(&mut rng)],
                    dedup: None,
                    live_delta: 0,
                }
            }
        };
        entries.push((i, ts(i * 10), c));
    }
    // `b` restarts halfway: it opens at the last entry its LSM flushed and the log replays the
    // rest (FR-015), as the core does.
    for (i, at, c) in &entries[..150] {
        assert_eq!(a.apply(*i, *at, c), b.apply(*i, *at, c), "entry {i}");
    }
    let mut b = b.restart();
    let reopened = b.sm.applied_index();
    assert!(reopened <= 150 && reopened > 100, "{reopened}");
    for (i, at, c) in &entries[reopened as usize..150] {
        b.apply(*i, *at, c).unwrap();
    }
    for (i, at, c) in &entries[150..] {
        assert_eq!(a.apply(*i, *at, c), b.apply(*i, *at, c), "entry {i}");
    }
    for at in [ts(1_000), ts(3_000), ts(10_000)] {
        assert_eq!(a.dump(at), b.dump(at), "at {at:?}");
    }
    assert_eq!((a.sm.gc_threshold(), a.sm.last_mark()), (b.sm.gc_threshold(), b.sm.last_mark()));
    assert_eq!((a.sm.applied_index(), b.sm.applied_index()), (300, 300));
}

#[test]
fn the_gc_threshold_only_rises_and_survives_a_restart() {
    let mut r = Replica::new();
    r.apply(1, ts(100), &Command::SetGcThreshold { ts: ts(50) }).unwrap();
    r.apply(2, ts(110), &Command::SetGcThreshold { ts: ts(40) }).unwrap();
    assert_eq!(r.sm.gc_threshold(), ts(50));
    let r = r.restart();
    assert_eq!(r.sm.gc_threshold(), ts(50));
    // Entry 2 wrote nothing: the range reopens at 1 and the log replays 2.
    assert_eq!(r.sm.applied_index(), 1);
}

#[test]
fn a_checksum_request_answers_with_its_mark() {
    let mut r = Replica::new();
    let out = r.apply(1, ts(100), &Command::ComputeChecksum { at: ts(90) });
    assert_eq!(out, Ok(RangeOutput::Checksum { at: ts(90) }));
    assert_eq!(r.apply(2, ts(110), &Command::Noop), Ok(RangeOutput::Applied { ts: ts(110) }));
    r.sm.skip(3);
    assert_eq!(r.sm.applied_index(), 3);
}

#[test]
fn an_unknown_kind_or_a_key_outside_the_range_stops_the_group() {
    let mut r = Replica::new();
    let ctx = ApplyCtx { group: G, index: 1, term: 1, ts: ts(10) };
    assert!(r.sm.apply(&9u16.to_le_bytes(), &ctx).is_err());
    for key in [&b"\x01"[..], b"\x01z", b"\x02a", b"\x00local"] {
        let w = Command::Write {
            puts: vec![put(key, b"x")],
            deletes: vec![],
            dedup: None,
            live_delta: 0,
        };
        assert!(r.apply(1, ts(10), &w).is_err(), "{key:?}");
        let d = Command::Write {
            puts: vec![],
            deletes: vec![key.to_vec()],
            dedup: None,
            live_delta: 0,
        };
        assert!(r.apply(1, ts(10), &d).is_err(), "{key:?}");
    }
    assert_eq!(r.sm.applied_index(), 0, "nothing was applied");
    assert!(r.dump(ts(99)).is_empty(), "nothing was written");
}

#[test]
fn a_snapshot_restores_only_over_data_the_lsm_already_holds() {
    let mut r = Replica::new();
    r.apply(
        1,
        ts(10),
        &Command::Write {
            puts: vec![put(b"\x01a", b"1")],
            deletes: vec![],
            dedup: None,
            live_delta: 0,
        },
    )
    .unwrap();
    r.apply(2, ts(20), &Command::SetGcThreshold { ts: ts(5) }).unwrap();
    let snap = r.sm.snapshot();
    // The same node after a restart: its LSM holds the data up to the snapshot.
    let mut r = r.restart();
    r.sm.restore(&snap).unwrap();
    assert_eq!((r.sm.applied_index(), r.sm.gc_threshold(), r.sm.last_mark()), (2, ts(5), ts(10)));
    assert!(r.sm.has_data_for(&snap));
    // A replica without the data refuses: it has to arrive by ingest (FR-012).
    let mut empty = Replica::new();
    assert!(!empty.sm.has_data_for(&snap), "the core waits for the data");
    assert!(empty.sm.has_data_for(&snap[..3]), "restore reports a cut snapshot");
    assert!(empty.sm.restore(&snap).is_err());
    assert!(empty.sm.restore(&snap[..3]).is_err(), "a cut snapshot");
    let mut other = snap.clone();
    other[0] ^= 1;
    assert!(r.sm.restore(&other).is_err(), "another range's snapshot");
}

#[test]
fn a_stopped_core_stops_the_group_without_applying() {
    let mut r = Replica::new();
    let w = Command::Write {
        puts: vec![put(b"\x01a", b"1")],
        deletes: vec![],
        dedup: None,
        live_delta: 0,
    };
    r.apply(1, ts(10), &w).unwrap();
    r.lsm.borrow_mut().flush();
    r.disk.inject_error("sync", IoErrno::Io);
    r.lsm.borrow_mut().run_io(&mut r.disk, Nanos::ZERO);
    assert!(r.apply(2, ts(20), &w).is_err());
    assert!(r.apply(2, ts(20), &Command::SetGcThreshold { ts: ts(9) }).is_err());
    assert_eq!(r.sm.applied_index(), 1);
}

#[test]
#[should_panic(expected = "bounds of a data range")]
fn a_range_cannot_cover_local_keys() {
    let mut disk = MemDisk::new();
    let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg(), &mut disk).unwrap().0));
    let _ = RangeSm::open(G, b"\x00".to_vec(), END.to_vec(), lsm, &mut disk);
}

#[test]
#[should_panic(expected = "bounds of a data range")]
fn a_range_needs_its_start_below_its_end() {
    let mut disk = MemDisk::new();
    let lsm = Rc::new(RefCell::new(Lsm::recover(&cfg(), &mut disk).unwrap().0));
    let _ = RangeSm::open(G, END.to_vec(), START.to_vec(), lsm, &mut disk);
}

#[test]
fn a_purge_of_keys_that_are_not_the_ranges_idempotence_records_stops_the_group() {
    let mut r = Replica::new();
    let other = dedup_key(GroupId(G.0 + 1), 1, 1, 0);
    for key in [b"\x01doc".to_vec(), other] {
        let purge = Command::PurgeDedup { keys: vec![dedup_key(G, 1, 1, 0), key.clone()] };
        assert!(r.apply(1, ts(10), &purge).is_err(), "{key:?}");
    }
    assert_eq!(r.sm.applied_index(), 0, "nothing was applied");
}

/// The leader's search for expired idempotence records (FR-011) stops at a full batch and skips
/// the records already deleted and those newer than its cutoff.
#[test]
fn a_purge_search_takes_a_batch_of_expired_records_and_skips_the_rest() {
    use teseladb_range::dedup::{PURGE_BATCH, PurgeScan};
    let mut r = Replica::new();
    let write = |stmt_id: u32| Command::Write {
        puts: vec![],
        deletes: vec![],
        dedup: Some(DedupRecord { lsid: 7, txn_number: 1, stmt_id, result: vec![] }),
        live_delta: 0,
    };
    r.apply(1, ts(10), &write(0)).unwrap();
    r.apply(2, ts(11), &Command::PurgeDedup { keys: vec![dedup_key(G, 7, 1, 0)] }).unwrap();
    let mut index = 2;
    for stmt in 1..=PURGE_BATCH as u32 + 1 {
        index += 1;
        r.apply(index, ts(20), &write(stmt)).unwrap();
        if index % 2_000 == 0 {
            r.lsm.borrow_mut().flush();
            r.lsm.borrow_mut().run_io(&mut r.disk, Nanos::ZERO);
        }
    }
    r.apply(index + 1, ts(1_000), &write(u32::MAX)).unwrap();
    let mut search = PurgeScan::new(&r.lsm, G, ts(500));
    let mut batch = None;
    for _ in 0..MAX_ROUNDS * 100 {
        match search.step(&r.lsm) {
            Some(found) => {
                batch = Some(found.expect("the LSM answers"));
                break;
            }
            None => {
                r.lsm.borrow_mut().run_io(&mut r.disk, Nanos::ZERO);
            }
        }
    }
    let batch = batch.expect("the search ends");
    assert_eq!(batch.len(), PURGE_BATCH, "one full batch; the rest go in the next round");
    assert!(!batch.contains(&dedup_key(G, 7, 1, 0)), "the deleted record is not purged again");
    assert!(!batch.contains(&dedup_key(G, 7, 1, u32::MAX)), "the newer record stays");
}
