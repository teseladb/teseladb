//! Reads from other threads over the published view (004 FR-026, T042). Reader threads take the
//! view the core published and read it without the core, asking the test's main thread for the
//! blocks they miss, as a connection's thread reads them from the file itself:
//! - while the core flushes and compacts, every read matches the model (scans in both directions)
//!   and a file a view still uses is not deleted;
//! - while the core writes new keys, a reader sees every key published before its mark.

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;

use common::model::{Model, Row};
use common::{Gen, Op, apply, open, small, ts};
use teseladb_keys::Hlc;
use teseladb_lsm::{BlockNeed, Lsm, SharedLsm, SharedRead, SharedStep, SharedView, ViewCache};
use teseladb_raftlog::{Disk, MemDisk};
use teseladb_types::{GroupId, Nanos};

/// A block a reader thread asks the main thread for.
type Ask = (BlockNeed, mpsc::Sender<Vec<u8>>);

/// The point read of `key` at `at` over `view`, asking `ask` for every block it misses.
fn get(
    shared: &SharedLsm,
    view: &SharedView,
    ask: &mpsc::Sender<Ask>,
    key: &[u8],
    at: Hlc,
) -> Option<Row> {
    loop {
        match view.get(shared, key, at) {
            SharedRead::Hit(f) => return Some((key.to_vec(), f.ts, f.kind, f.value.to_vec())),
            SharedRead::NotFound => return None,
            SharedRead::Need(need) => supply(shared, ask, need),
            SharedRead::Failed(e) => panic!("read failed: {e}"),
        }
    }
}

/// The scan of `[start, end)` at `at` over `view`.
fn scan(
    shared: &SharedLsm,
    view: &SharedView,
    ask: &mpsc::Sender<Ask>,
    (start, end): (&[u8], &[u8]),
    at: Hlc,
) -> Vec<Row> {
    let mut out = Vec::new();
    let mut s = view.scan(start, end, at);
    loop {
        match s.next(shared) {
            SharedStep::Item(i) => out.push((i.key, i.ts, i.kind, i.value.to_vec())),
            SharedStep::Done => return out,
            SharedStep::Need(need) => supply(shared, ask, need),
            SharedStep::Failed(e) => panic!("scan failed: {e}"),
        }
    }
}

/// The scan of `[start, end)` at `at` over `view` from its last key down.
fn scan_back(
    shared: &SharedLsm,
    view: &SharedView,
    ask: &mpsc::Sender<Ask>,
    (start, end): (&[u8], &[u8]),
    at: Hlc,
) -> Vec<Row> {
    let mut out = Vec::new();
    let mut s = view.scan_back(start, end, at);
    loop {
        match s.next(shared) {
            SharedStep::Item(i) => out.push((i.key, i.ts, i.kind, i.value.to_vec())),
            SharedStep::Done => return out,
            SharedStep::Need(need) => supply(shared, ask, need),
            SharedStep::Failed(e) => panic!("backward scan failed: {e}"),
        }
    }
}

/// Reads the block `need` names through the main thread and gives it to the shared cache.
fn supply(shared: &SharedLsm, ask: &mpsc::Sender<Ask>, need: BlockNeed) {
    let (tx, rx) = mpsc::channel();
    ask.send((need.clone(), tx)).expect("the main thread");
    let bytes = rx.recv().expect("the block");
    shared.supply(&need, &bytes).expect("a good block");
}

/// The bytes `need` names on `disk`; a file deleted while a view used it fails the test.
fn read_block(disk: &mut MemDisk, need: &BlockNeed) -> Vec<u8> {
    let all = disk
        .read_all(std::path::Path::new(&need.path))
        .unwrap_or_else(|e| panic!("{} gone while a view used it: {e:?}", need.path));
    let at = usize::try_from(need.offset).expect("offset");
    let len = usize::try_from(need.len).expect("length");
    all[at..at + len].to_vec()
}

/// Answers the blocks asked for so far; how many.
fn serve(disk: &mut MemDisk, asks: &mpsc::Receiver<Ask>) -> usize {
    let mut served = 0;
    while let Ok((need, reply)) = asks.try_recv() {
        let _ = reply.send(read_block(disk, &need));
        served += 1;
    }
    served
}

/// A round of the readers: the view, the model it holds and the mark to read at.
type Round = Option<(SharedView, Arc<Model>, Hlc)>;

/// Rounds of random writes; in each, the readers check the published view against the model
/// while the core only flushes and compacts, which changes the files and not what is visible.
fn rounds(seed: u64, readers: usize) {
    let cfg = small();
    let mut disk = MemDisk::new();
    let mut lsm: Lsm = open(&cfg, &mut disk);
    let shared = lsm.shared();
    let (ask_tx, ask_rx) = mpsc::channel::<Ask>();
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let mut rounds_tx = Vec::new();
    let workers: Vec<_> = (0..readers)
        .map(|_| {
            let (shared, ask, done) = (shared.clone(), ask_tx.clone(), done_tx.clone());
            let (tx, rx) = mpsc::channel::<Round>();
            rounds_tx.push(tx);
            thread::spawn(move || {
                while let Some((view, model, at)) = rx.recv().expect("a round") {
                    for key in model.keys() {
                        let got = get(&shared, &view, &ask, &key, at);
                        assert_eq!(got, model.get(&key, at), "{key:?} at {at:?}");
                    }
                    let all = scan(&shared, &view, &ask, (b"", b"\xff"), at);
                    assert_eq!(all, model.scan(b"", b"\xff", at), "scan at {at:?}");
                    let mut back = scan_back(&shared, &view, &ask, (b"", b"\xff"), at);
                    back.reverse();
                    assert_eq!(back, all, "backward scan at {at:?}");
                    done.send(()).expect("the main thread");
                }
            })
        })
        .collect();
    drop((ask_tx, done_tx));
    let mut ops = Gen::new(seed, 40);
    let mut model = Model::default();
    let mut index = 0;
    let mut blocks = 0;
    for _ in 0..12 {
        for _ in 0..50 {
            let op = ops.next(&model);
            if !matches!(op, Op::Get { .. } | Op::Scan { .. }) {
                apply(&op, &mut lsm, &mut model, &mut disk, &mut index);
            }
        }
        lsm.publish();
        let (view, snapshot) = (shared.view(), Arc::new(model.clone()));
        for at in [ts(ops.clock + 100, u32::MAX), ts(ops.clock.saturating_sub(30), 1)] {
            for tx in &rounds_tx {
                tx.send(Some((view.clone(), snapshot.clone(), at))).expect("a reader");
            }
            let mut finished = 0;
            let mut turn = 0u64;
            while finished < readers {
                blocks += serve(&mut disk, &ask_rx);
                if turn.is_multiple_of(7) {
                    lsm.flush();
                    lsm.publish();
                }
                lsm.run_io(&mut disk, Nanos::ZERO);
                turn += 1;
                while done_rx.try_recv().is_ok() {
                    finished += 1;
                }
                thread::yield_now();
            }
        }
    }
    for tx in &rounds_tx {
        tx.send(None).expect("a reader");
    }
    for w in workers {
        w.join().expect("a reader thread");
    }
    assert!(blocks > 0, "the readers read blocks of the files");
}

#[test]
fn readers_on_other_threads_read_through_flushes_and_compactions() {
    for seed in 1..=4 {
        rounds(seed, 4);
    }
}

#[test]
fn readers_see_every_key_published_before_their_view_while_the_core_writes() {
    let cfg = small();
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let shared = lsm.shared();
    let published = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicU64::new(0));
    let (ask_tx, ask_rx) = mpsc::channel::<Ask>();
    let key = |i: u64| format!("w{i:06}").into_bytes();
    let readers: Vec<_> = (0..4u64)
        .map(|r| {
            let (shared, ask) = (shared.clone(), ask_tx.clone());
            let (published, stop) = (published.clone(), stop.clone());
            thread::spawn(move || {
                let mut reads = 0u64;
                // Each reader goes on until the core is done and it read at least 50 times.
                while stop.load(Ordering::Acquire) == 0 || reads < 50 {
                    let n = published.load(Ordering::Acquire);
                    let view = shared.view();
                    if n == 0 {
                        continue;
                    }
                    // Every key up to `n` is visible at its own mark, and none above `n` at `n`.
                    let i = (reads * 7 + r) % n + 1;
                    let at = ts(i, 0);
                    let row = get(&shared, &view, &ask, &key(i), at).expect("a published key");
                    assert_eq!(row.3, key(i), "key {i}");
                    let below = get(&shared, &view, &ask, &key(i), ts(i - 1, u32::MAX));
                    assert_eq!(below, None, "key {i} below its mark");
                    let seen = scan(&shared, &view, &ask, (b"w", b"x"), ts(n, 0));
                    assert_eq!(seen.len() as u64, n, "keys up to {n}");
                    reads += 1;
                }
                reads
            })
        })
        .collect();
    drop(ask_tx);
    for i in 1..=3000u64 {
        lsm.put(GroupId(1), i, &key(i), ts(i, 0), &key(i)).expect("write");
        lsm.publish();
        published.store(i, Ordering::Release);
        serve(&mut disk, &ask_rx);
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    stop.store(1, Ordering::Release);
    let mut total = 0;
    let mut readers = readers.into_iter().collect::<Vec<_>>();
    while !readers.is_empty() {
        serve(&mut disk, &ask_rx);
        let (done, rest): (Vec<_>, Vec<_>) = readers.into_iter().partition(|r| r.is_finished());
        total += done.into_iter().map(|r| r.join().expect("a reader")).sum::<u64>();
        readers = rest;
        thread::yield_now();
    }
    assert!(total >= 200, "{total} reads");
}

#[test]
fn a_reader_sees_what_its_mark_sees() {
    let cfg = small();
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let shared = lsm.shared();
    lsm.put(GroupId(1), 1, b"k", ts(1, 0), b"one").expect("write");
    lsm.publish();
    let before = shared.view();
    lsm.put(GroupId(1), 2, b"k", ts(2, 0), b"two").expect("write");
    lsm.publish();
    let after = shared.view();
    let (ask, _asks) = mpsc::channel();
    let read = |v: &SharedView, at| get(&shared, v, &ask, b"k", at).map(|r| r.3);
    assert_eq!(read(&before, ts(1, 5)), Some(b"one".to_vec()));
    assert_eq!(read(&after, ts(1, 5)), Some(b"one".to_vec()));
    assert_eq!(read(&after, ts(9, 0)), Some(b"two".to_vec()));
    assert_eq!(read(&after, ts(0, 9)), None);
}

#[test]
fn a_cached_view_belongs_to_its_lsm() {
    // Two LSMs at the same published version: a thread's cache taken from one is not the other's
    // (a core whose LSM opens again starts its versions again).
    let cfg = small();
    let (mut d1, mut d2) = (MemDisk::new(), MemDisk::new());
    let (mut one, mut two) = (open(&cfg, &mut d1), open(&cfg, &mut d2));
    one.put(GroupId(1), 1, b"k", ts(1, 0), b"one").expect("write");
    two.put(GroupId(1), 1, b"k", ts(1, 0), b"two").expect("write");
    one.publish();
    two.publish();
    let (s1, s2) = (one.shared(), two.shared());
    let mut cache = ViewCache::default();
    let (ask, _asks) = mpsc::channel();
    let first = s1.view_cached(&mut cache).clone();
    assert_eq!(get(&s1, &first, &ask, b"k", ts(5, 0)).map(|r| r.3), Some(b"one".to_vec()));
    let second = s2.view_cached(&mut cache).clone();
    assert_eq!(get(&s2, &second, &ask, b"k", ts(5, 0)).map(|r| r.3), Some(b"two".to_vec()));
}

#[test]
fn may_exist_answers_without_reading_a_block() {
    // H-042, as AnotherProduct's `KeyMayExist`: a key with a version in a memtable or in a file whose
    // filter does not rule it out may exist; an absent key is ruled out without any block.
    let cfg = small();
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let shared = lsm.shared();
    lsm.put(GroupId(1), 1, b"k", ts(1, 0), b"one").expect("write");
    lsm.publish();
    let view = shared.view();
    assert!(view.may_exist(b"k"), "in the memtable");
    assert!(!view.may_exist(b"absent"), "nowhere");
    lsm.flush();
    let mut flushed = 0;
    for _ in 0..100 {
        flushed += lsm.run_io(&mut disk, Nanos::ZERO).flushed.len();
    }
    assert!(flushed > 0, "the memtable went to a file");
    lsm.publish();
    let view = shared.view();
    assert!(view.may_exist(b"k"), "in a file, its filter says so");
    assert!(!view.may_exist(b"absent"), "the file's filter rules it out");
}

#[test]
fn the_shared_cache_forgets_the_blocks_of_a_deleted_file() {
    // Found by the sustained load of 002 (11-11-2026): the blocks that reads outside the core
    // gave the shared cache stayed after their files went, holding its memory.
    let mut disk = MemDisk::new();
    let mut lsm = open(&small(), &mut disk);
    let g = GroupId(1);
    for i in 0..400u32 {
        let key = format!("k{i:05}");
        lsm.put(g, u64::from(i) + 1, key.as_bytes(), ts(1, 0), &[9u8; 200]).unwrap();
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    lsm.publish();
    let shared = lsm.shared();
    let view = shared.view();
    let mut s = view.scan(b"k", b"l", ts(2, 0));
    let mut seen = 0;
    loop {
        match s.next(&shared) {
            SharedStep::Item(_) => seen += 1,
            SharedStep::Done => break,
            SharedStep::Need(need) => {
                let bytes = read_block(&mut disk, &need);
                shared.supply(&need, &bytes).expect("a good block");
            }
            SharedStep::Failed(e) => panic!("{e}"),
        }
    }
    assert_eq!(seen, 400);
    drop(s);
    drop(view);
    assert!(shared.cache_bytes() > 0, "the reads filled the shared cache");
    // A compaction of everything rewrites the files; once the core publishes the new version
    // nothing holds the old ones, which go.
    lsm.compact_all();
    lsm.run_io(&mut disk, Nanos::from_secs(10));
    lsm.publish();
    lsm.run_io(&mut disk, Nanos::from_secs(20));
    assert_eq!(shared.cache_bytes(), 0, "the deleted files' blocks are gone");
}
