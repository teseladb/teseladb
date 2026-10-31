//! `interval_size` (007 FR-005): the bytes of an interval and the key that halves it, from the
//! memtables and the files' block indexes, with every version counted and no cut between two
//! versions of a key.

use teseladb_keys::Hlc;
use teseladb_lsm::{EntryWrite, IntervalSize, Kind, Lsm, LsmConfig};
use teseladb_raftlog::MemDisk;
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(3);

fn key(i: u32) -> Vec<u8> {
    [&[0x01][..], format!("k{i:05}").as_bytes()].concat()
}

fn fill(lsm: &mut Lsm, from: u32, to: u32, index: &mut u64) {
    let value = vec![7u8; 200];
    for i in from..to {
        *index += 1;
        let k = key(i);
        let w = [EntryWrite { key: &k, kind: Kind::Put, value: &value }];
        lsm.write_entry(G, *index, Hlc { physical: *index, logical: 0 }, &w).unwrap();
    }
}

#[test]
fn an_interval_is_measured_in_memory_and_in_files_and_cut_near_its_middle() {
    let cfg = LsmConfig { block_bytes: 4 << 10, ..LsmConfig::default() };
    let mut disk = MemDisk::new();
    let mut lsm = Lsm::recover(&cfg, &mut disk).unwrap().0;
    let (start, end) = (vec![0x01], vec![0x02]);
    assert_eq!(lsm.interval_size(&start, &end), IntervalSize::default());
    let mut index = 0;
    // One key with many versions: bytes, but no cut inside the interval.
    for _ in 0..3 {
        fill(&mut lsm, 0, 1, &mut index);
    }
    let one = lsm.interval_size(&start, &end);
    assert!(one.bytes > 600);
    assert_eq!(one.cut, None);
    fill(&mut lsm, 1, 2_000, &mut index);
    let mem = lsm.interval_size(&start, &end);
    let cut = mem.cut.clone().expect("a cut");
    assert!(cut > key(800) && cut < key(1_200), "{cut:?}");
    // Flushed to files, the block index gives about the same.
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    let files = lsm.interval_size(&start, &end);
    assert!(files.bytes > 300_000 && files.bytes < 2 * mem.bytes, "{} {}", files.bytes, mem.bytes);
    let cut = files.cut.expect("a cut");
    assert!(cut > key(800) && cut < key(1_200), "{cut:?}");
    // Half of the interval is about half of the bytes, and nothing outside it counts.
    let half = lsm.interval_size(&key(1_000), &end);
    assert!(half.bytes * 3 > files.bytes && half.bytes * 3 < files.bytes * 2);
    assert_eq!(lsm.interval_size(&[0x02], &[0x03]), IntervalSize::default());
}
