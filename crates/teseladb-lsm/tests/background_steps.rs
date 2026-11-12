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
    // The file is created first. Then each step takes 64 KiB of the 400 KiB and yields: the
    // file's bytes are handed out once, at the end, since they are less than a chunk.
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
    assert!((5..=7).contains(&yields), "{yields} yields for 400 KiB");
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
