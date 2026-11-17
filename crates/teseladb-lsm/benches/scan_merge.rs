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
}
