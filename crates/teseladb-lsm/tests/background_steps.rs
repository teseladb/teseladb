//! Background work in steps (004 FR-030, T050): a flush and a compaction take 64 KiB of entries
//! at a time and yield between two steps, and what they write is what they wrote in one go.

mod common;

use std::path::Path;

use common::{open, small, ts};
use teseladb_lsm::{Lsm, LsmConfig, Polled};
use teseladb_raftlog::{Disk, MemDisk};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);
const NOW: Nanos = Nanos::ZERO;

/// Memtables of 512 KiB: a flush has several steps of 64 KiB.
fn cfg() -> LsmConfig {
    LsmConfig { memtable_bytes: 512 << 10, ..small() }
}

/// Runs the LSM's disk operations until none is left, a step of background work at a time:
/// how many times it yielded.
fn run_in_steps(lsm: &mut Lsm, disk: &mut MemDisk) -> usize {
    let mut yields = 0;
    loop {
        match lsm.poll_io_step(NOW) {
            Polled::Op(op) => {
                let done = disk.execute(op);
                lsm.on_io_done(done, NOW);
            }
            Polled::More => yields += 1,
            Polled::Idle => return yields,
        }
    }
}

/// Writes `n` documents of 1 KiB in an order of its own, running the I/O after each with
/// `run`, then flushes and compacts everything.
fn fill(lsm: &mut Lsm, disk: &mut MemDisk, n: u64, mut run: impl FnMut(&mut Lsm, &mut MemDisk)) {
    for i in 1..=n {
        let key = format!("k{:06}", i * 7919 % n);
        lsm.put(G, i, key.as_bytes(), ts(i, 0), &[b'v'; 1024]).unwrap();
        run(lsm, disk);
    }
    lsm.flush();
    run(lsm, disk);
    lsm.compact_all();
    run(lsm, disk);
}

#[test]
fn a_flush_and_a_compaction_yield_between_steps_and_write_the_same_files() {
    let cfg = cfg();
    let (mut whole, mut stepped) = (MemDisk::new(), MemDisk::new());
    let mut a = open(&cfg, &mut whole);
    fill(&mut a, &mut whole, 3_000, |l, d| drop(l.run_io(d, NOW)));
    let mut b = open(&cfg, &mut stepped);
    let mut yields = 0;
    fill(&mut b, &mut stepped, 3_000, |l, d| yields += run_in_steps(l, d));
    // Three megabytes flushed and then compacted, 64 KiB at a time: a step that ends its file
    // or needs a block from the disk hands out an operation and does not count here.
    assert!(yields >= 30, "{yields} yields");
    let dir = Path::new("core-00/lsm");
    let (mut names, mut other) = (whole.list(dir).unwrap(), stepped.list(dir).unwrap());
    names.sort();
    other.sort();
    assert_eq!(names, other);
    for name in names {
        let (one, two) = (whole.read_all(&name).unwrap(), stepped.read_all(&name).unwrap());
        assert!(one == two, "{name:?} differs");
    }
}

#[test]
fn a_flush_takes_its_memtable_a_step_at_a_time() {
    let cfg = cfg();
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    for i in 1..=400u64 {
        lsm.put(G, i, format!("k{i:06}").as_bytes(), ts(i, 0), &[b'v'; 1024]).unwrap();
    }
    lsm.flush();
    // The file is created first. Then each step takes 64 KiB of the 400 KiB and yields, and the
    // end of the file takes two more, one to end its last block and one for the filter's keys
    // (its rows are solved in the step that hands out the bytes): the file's bytes are handed
    // out once, at the end, since they are less than a chunk.
    let Polled::Op(create) = lsm.poll_io_step(NOW) else { panic!("the flush starts") };
    let done = disk.execute(create);
    lsm.on_io_done(done, NOW);
    let mut yields = 0;
    let write = loop {
        match lsm.poll_io_step(NOW) {
            Polled::More => yields += 1,
            Polled::Op(op) => break op,
            Polled::Idle => panic!("a flush in progress"),
        }
    };
    assert!((7..=9).contains(&yields), "{yields} yields for 400 KiB");
    let done = disk.execute(write);
    lsm.on_io_done(done, NOW);
    // Whoever has nothing else to serve asks for the next operation and gets it, however
    // many steps it takes.
    lsm.run_io(&mut disk, NOW);
    for i in 401..=800u64 {
        lsm.put(G, i, format!("k{i:06}").as_bytes(), ts(i, 0), &[b'v'; 1024]).unwrap();
    }
    lsm.flush();
    let op = lsm.poll_io(NOW).expect("the second flush starts");
    let done = disk.execute(op);
    lsm.on_io_done(done, NOW);
    assert!(lsm.poll_io(NOW).is_some(), "its bytes, with every step taken");
    lsm.run_io(&mut disk, NOW);
    let view = lsm.view();
    let found = common::get(&mut lsm, &mut disk, &view, b"k000400", ts(900, 0));
    assert!(found.is_some(), "flushed and read back");
}

/// The end of a file goes in steps too: the filter of a file of many short keys took its core
/// 65 ms at once. A flush of 50 000 keys of 20 bytes takes about 16 steps of 64 KiB of entries,
/// and its filter at least 2 × 50 000 / 4096 more: 4096 keys a step, then as many rows.
#[test]
fn the_end_of_a_file_of_many_keys_goes_in_steps() {
    const N: u64 = 50_000;
    let cfg = LsmConfig { memtable_bytes: 64 << 20, ..small() };
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    for i in 0..N {
        lsm.put(G, i + 1, format!("k{i:06}").as_bytes(), ts(i + 1, 0), b"v").unwrap();
    }
    lsm.flush();
    let yields = run_in_steps(&mut lsm, &mut disk);
    let entries = N * (7 + 12 + 1);
    let fill = entries.div_ceil(64 << 10) + 1;
    let filter = 2 * (N / 4096);
    assert!(yields as u64 >= fill + filter, "{yields} yields");
    let view = lsm.view();
    for i in [0, N / 2, N - 1] {
        let key = format!("k{i:06}");
        let found = common::get(&mut lsm, &mut disk, &view, key.as_bytes(), ts(N + 1, 0));
        assert!(found.is_some(), "{key} flushed and read back");
    }
}

#[test]
fn flushes_and_compactions_are_background_and_a_clients_read_is_not() {
    // 012 FR-028: the executor caps the background's bandwidth while the clients' reads wait,
    // so the LSM tells the two apart.
    let cfg = cfg();
    let mut disk = MemDisk::new();
    let mut lsm = open(&cfg, &mut disk);
    let (mut background, mut reads) = (0, 0);
    let mut run = |lsm: &mut Lsm, disk: &mut MemDisk| loop {
        match lsm.poll_io_step(NOW) {
            Polled::Op(op) => {
                if lsm.is_background(&op) {
                    background += 1;
                } else {
                    reads += 1;
                }
                let done = disk.execute(op);
                lsm.on_io_done(done, NOW);
            }
            Polled::More => {}
            Polled::Idle => break,
        }
    };
    fill(&mut lsm, &mut disk, 600, &mut run);
    assert!(background > 0, "the flushes and the compaction wrote");
    assert_eq!(reads, 0, "nobody read for a client");
    // A cold read of a client asks for a block: that operation is not background work.
    let (mut lsm, _) = Lsm::recover(&cfg, &mut disk).unwrap();
    let view = lsm.view();
    let key = format!("k{:06}", 7919 % 600);
    let teseladb_lsm::Lookup::Miss(_) = view.get(&mut lsm, key.as_bytes(), ts(601, 0)) else {
        panic!("the block of a key just recovered is on the disk");
    };
    let Polled::Op(op) = lsm.poll_io_step(NOW) else { panic!("the read of the missing block") };
    assert!(!lsm.is_background(&op), "{op:?}");
}
