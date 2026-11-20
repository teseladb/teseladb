//! The cost of a full scan's merge of its sources (the 47 % of the node's CPU the one-node load's
//! profile gave to scans): 200,000 documents over a flushed file of all of them, three files of a
//! third of them each rewritten, and a memtable with a tenth, read at the newest mark, the best
//! of five rounds, in nanoseconds per document.
//!   cargo bench -p teseladb-lsm --bench scan_merge

use std::time::Instant;

use teseladb_keys::Hlc;
use teseladb_lsm::{Lsm, LsmConfig, Step};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const DOCS: u64 = 200_000;
const ROUNDS: usize = 5;

fn key(i: u64) -> Vec<u8> {
    let mut k = vec![1, 0, 0, 0, 1, 0, 0, 0, 7, 0, 0, 0, 0];
    k.extend_from_slice(&i.to_be_bytes());
    k
}

fn main() {
    let mut disk = MemDisk::new();
    let cfg = LsmConfig {
        dir: "core-00/lsm".into(),
        memtable_bytes: 256 << 20,
        cache_bytes: 1 << 30,
        ..LsmConfig::default()
    };
    let mut lsm = Lsm::recover(&cfg, &mut disk).expect("recovery").0;
    let g = GroupId(1);
    let value = vec![7u8; 120];
    let mut index = 0;
    let mut put = |lsm: &mut Lsm, i: u64, t: u64| {
        index += 1;
        lsm.put(g, index, &key(i), Hlc { physical: t, logical: 0 }, &value).expect("put");
    };
    for i in 0..DOCS {
        put(&mut lsm, i, 10);
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    for round in 0..3u64 {
        for i in (round..DOCS).step_by(3) {
            put(&mut lsm, i, 20 + round);
        }
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    for i in (0..DOCS).step_by(10) {
        put(&mut lsm, i, 40);
    }
    let newest = Hlc { physical: u64::MAX >> 1, logical: 0 };
    let (start, end) = (key(0), key(DOCS));
    let mut best = f64::MAX;
    for _ in 0..ROUNDS {
        let view = lsm.view();
        let mut scan = view.scan(&start, &end, newest);
        let mut n = 0u64;
        let began = Instant::now();
        loop {
            match scan.next(&mut lsm) {
                Step::Item(_) => n += 1,
                Step::Done => break,
                Step::Miss(_) => {
                    lsm.run_io(&mut disk, Nanos::ZERO);
                }
                Step::Failed(e) => panic!("{e}"),
            }
        }
        assert_eq!(n, DOCS);
        best = best.min(began.elapsed().as_nanos() as f64 / n as f64);
    }
    println!("scan_merge: {best:.1} ns per document ({DOCS} documents, 5 sources)");
    as_the_load();
}

/// The scan as the one-node load makes it (002 run p72): documents of 300 bytes with their type,
/// a change meta beside each write, a file with every document, two files of level 0 with a
/// third of them rewritten each and a memtable with a tenth and their metas. In nanoseconds for
/// each document and for each entry taken, the best of five rounds.
fn as_the_load() {
    use teseladb_lsm::{DOCUMENT_VALUE, HISTORY_VALUE};
    let mut disk = MemDisk::new();
    let cfg = LsmConfig {
        dir: "core-00/lsm".into(),
        memtable_bytes: 256 << 20,
        cache_bytes: 1 << 30,
        l0_trigger: 100,
        ..LsmConfig::default()
    };
    let mut lsm = Lsm::recover(&cfg, &mut disk).expect("recovery").0;
    let g = GroupId(1);
    let mut doc = vec![7u8; 300];
    doc[0] = DOCUMENT_VALUE;
    let mut meta = vec![3u8; 40];
    meta[0] = HISTORY_VALUE;
    let mut index = 0;
    let mut put = |lsm: &mut Lsm, i: u64, t: u64| {
        let at = Hlc { physical: t, logical: 0 };
        let k = key(i);
        let m = [k.as_slice(), &[0, 0]].concat();
        index += 2;
        lsm.put(g, index - 1, &k, at, &doc).expect("put");
        lsm.put(g, index, &m, at, &meta).expect("put");
    };
    for i in 0..DOCS {
        put(&mut lsm, i, 10);
    }
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    for round in 0..2u64 {
        for i in (round..DOCS).step_by(3) {
            put(&mut lsm, i, 20 + round);
        }
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    for i in (0..DOCS).step_by(10) {
        put(&mut lsm, i, 40);
    }
    let newest = Hlc { physical: u64::MAX >> 1, logical: 0 };
    let (start, end) = (key(0), key(DOCS));
    let (mut best, mut entries) = (f64::MAX, 0);
    // More rounds for a profiler to sample (`TESELADB_BENCH_ROUNDS`).
    let rounds = std::env::var("TESELADB_BENCH_ROUNDS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(ROUNDS);
    for _ in 0..rounds {
        let view = lsm.view();
        let mut scan = view.scan(&start, &end, newest);
        let mut n = 0u64;
        let began = Instant::now();
        loop {
            match scan.next(&mut lsm) {
                Step::Item(_) => n += 1,
                Step::Done => break,
                Step::Miss(_) => {
                    lsm.run_io(&mut disk, Nanos::ZERO);
                }
                Step::Failed(e) => panic!("{e}"),
            }
        }
        assert_eq!(n, DOCS);
        entries = scan.counts().entries;
        best = best.min(began.elapsed().as_nanos() as f64 / n as f64);
    }
    let per_doc = entries as f64 / DOCS as f64;
    println!(
        "scan_merge as the load: {best:.1} ns per document, {per_doc:.2} entries per document, \
         {:.1} ns per entry",
        best / per_doc
    );
}
