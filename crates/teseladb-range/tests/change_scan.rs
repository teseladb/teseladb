//! A change stream's read of a range whose history is larger than the block cache (014 FR-003):
//! the read goes on from where a missing block stopped it, instead of scanning again from the
//! first key. Found in the e2e battery of 04-11-2026: a read of a history of hundreds of MB on
//! disk never answered (each block it loaded pushed an earlier one out of the cache), the leader
//! missed its heartbeats meanwhile and the range went through hundreds of elections.

use std::cell::RefCell;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Lsm, LsmConfig};
use teseladb_range::changes::ChangeScan;
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(7);
/// Versions written: about 1 MiB of history in 4 KiB blocks.
const VERSIONS: u64 = 1_000;
const VALUE_BYTES: usize = 1_000;
/// Most I/O rounds the read may take: a few per block of history.
const MAX_ROUNDS: usize = 5_000;

fn config() -> LsmConfig {
    LsmConfig {
        dir: "core-00/lsm".into(),
        memtable_bytes: 32 << 10,
        block_bytes: 4096,
        target_file_bytes: 64 << 10,
        l1_bytes: 256 << 10,
        // Room for two blocks only: the history does not fit.
        cache_bytes: 8 << 10,
        ..LsmConfig::default()
    }
}

#[test]
fn a_history_larger_than_the_cache_is_read_to_the_end() {
    let mut disk = MemDisk::new();
    let mut lsm = Lsm::recover(&config(), &mut disk).expect("recovery").0;
    let value = |i: u64| {
        let mut v = vec![1u8];
        v.extend(std::iter::repeat_n((i % 251) as u8, VALUE_BYTES));
        v
    };
    for i in 0..VERSIONS {
        let key = [&[1u8][..], &(i % 97).to_be_bytes()].concat();
        let ts = Hlc { physical: 1_000 + i, logical: 0 };
        lsm.put(G, i + 1, &key, ts, &value(i)).expect("written");
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    lsm.flush();
    for _ in 0..100 {
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    let lsm = RefCell::new(lsm);
    let zero = Hlc { physical: 0, logical: 0 };
    let far = Hlc { physical: u64::MAX, logical: 0 };
    let mut scan = ChangeScan::new(&lsm, (&[1u8], &[2u8]), (zero, far));
    let mut rounds = 0;
    let got = loop {
        match scan.step(&lsm) {
            Some(r) => break r.expect("readable"),
            None => {
                rounds += 1;
                assert!(rounds < MAX_ROUNDS, "the read did not end in {MAX_ROUNDS} I/O rounds");
                lsm.borrow_mut().run_io(&mut disk, Nanos::ZERO);
            }
        }
    };
    assert_eq!(got.rows.len() as u64, VERSIONS);
    assert!(rounds > 0, "the history should not fit the cache");
    // Every version once, in key order and, within a key, from the newest.
    let mut keys: Vec<(Vec<u8>, std::cmp::Reverse<Hlc>)> =
        got.rows.iter().map(|r| (r.key.clone(), std::cmp::Reverse(r.ts))).collect();
    let sorted = keys.clone();
    keys.sort();
    assert_eq!(keys, sorted);
}

/// Reads `[1, 2)` in `(from, to]` to the end, with the LSM's I/O between steps.
fn read_all(
    lsm: &RefCell<Lsm>,
    disk: &mut MemDisk,
    from: Hlc,
    to: Hlc,
    budget: usize,
) -> (Vec<Hlc>, Hlc) {
    let mut scan = ChangeScan::new(lsm, (&[1u8], &[2u8]), (from, to)).with_budget(budget);
    let rows = loop {
        match scan.step(lsm) {
            Some(r) => break r.expect("readable").rows,
            None => {
                lsm.borrow_mut().run_io(disk, Nanos::ZERO);
            }
        }
    };
    (rows.iter().map(|r| r.ts).collect(), scan.to())
}

/// A read answers at most its budget of versions: it stops below the first mark that would pass
/// it and says so with its `to`, so the reader asks again from there and gets every version once,
/// whatever the size of the history (e2e battery of 04-11-2026: two versions of 9 MiB passed the
/// 16 MiB of a frame between nodes, and the answer never reached the reader). A mark's versions
/// go together, so the first one goes even when it alone passes the budget.
#[test]
fn a_read_answers_its_budget_and_the_rest_comes_from_its_mark() {
    let mut disk = MemDisk::new();
    let mut lsm =
        Lsm::recover(&LsmConfig { dir: "core-00/lsm".into(), ..LsmConfig::default() }, &mut disk)
            .expect("recovery")
            .0;
    let ts = |p: u64| Hlc { physical: p, logical: 0 };
    let mut value = vec![1u8];
    value.extend(std::iter::repeat_n(7u8, VALUE_BYTES));
    // Marks 10 to 19, two keys each; mark 15 has five keys.
    let mut written = Vec::new();
    for p in 10..20u64 {
        let keys = if p == 15 { 5u8 } else { 2 };
        for k in 0..keys {
            let key = [1u8, k, p as u8];
            lsm.put(G, written.len() as u64 + 1, &key, ts(p), &value).expect("written");
            written.push(ts(p));
        }
    }
    let lsm = RefCell::new(lsm);
    let budget = 3 * VALUE_BYTES;
    let mut from = ts(0);
    let mut got = Vec::new();
    let mut answers = 0;
    while from < ts(19) {
        let (rows, to) = read_all(&lsm, &mut disk, from, ts(1_000), budget);
        assert!(to > from, "every answer moves on");
        assert!(rows.iter().all(|t| *t > from && *t <= to));
        // Never a mark in part: every version of the marks it answers.
        let marks: std::collections::BTreeSet<Hlc> = rows.iter().copied().collect();
        for m in &marks {
            let all = written.iter().filter(|w| *w == m).count();
            assert_eq!(rows.iter().filter(|t| *t == m).count(), all, "mark {m:?}");
        }
        // Over the budget only with a single mark.
        assert!(rows.len() * VALUE_BYTES <= budget + VALUE_BYTES || marks.len() == 1, "{rows:?}");
        got.extend(rows);
        from = to;
        answers += 1;
    }
    got.sort();
    assert_eq!(got, written);
    assert!(answers >= 7, "{answers} answers");
    // Without a budget the whole interval comes at once, up to the mark asked.
    let (rows, to) = read_all(&lsm, &mut disk, ts(0), ts(1_000), usize::MAX);
    assert_eq!((rows.len(), to), (written.len(), ts(1_000)));
}
