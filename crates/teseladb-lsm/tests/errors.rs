//! I/O errors and corruption (004 story 2, T014): a bad checksum marks the core corrupt, a full
//! disk turns it read-only and retries every second, and any other error stops it (FR-014,
//! FR-015).

mod common;

use common::{open, small, ts};
use teseladb_lsm::{Lookup, LsmError, Pressure};
use teseladb_raftlog::{Disk, IoErrno, SimDisk};
use teseladb_types::{GroupId, Nanos};

const G: GroupId = GroupId(1);

fn sst_path(disk: &mut dyn Disk, dir: &std::path::Path) -> std::path::PathBuf {
    disk.list(dir).unwrap().into_iter().find(|p| p.extension().is_some_and(|e| e == "sst")).unwrap()
}

#[test]
fn a_bad_checksum_in_a_block_marks_the_core_corrupt() {
    let cfg = small();
    let mut disk = SimDisk::default();
    let mut lsm = open(&cfg, &mut disk);
    lsm.put(G, 1, b"k", ts(1, 0), &[9; 100]).unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    drop(lsm);
    let sst = sst_path(&mut disk, &cfg.dir);
    assert!(disk.corrupt(&sst, 20));
    let mut lsm = open(&cfg, &mut disk);
    let v = lsm.view();
    assert!(matches!(v.get(&mut lsm, b"k", ts(2, 0)), Lookup::Miss(_)));
    lsm.run_io(&mut disk, Nanos::ZERO);
    let failed = v.get(&mut lsm, b"k", ts(2, 0));
    let number: u64 = sst.file_stem().unwrap().to_str().unwrap().parse().unwrap();
    assert_eq!(failed, Lookup::Failed(LsmError::Corrupt { file: number, offset: 0 }));
    assert_eq!(
        lsm.put(G, 2, b"x", ts(3, 0), b"y"),
        Err(LsmError::Corrupt { file: number, offset: 0 })
    );
    assert!(lsm.health().is_err());
    let mut s = v.scan(b"", b"z", ts(2, 0));
    assert!(matches!(s.next(&mut lsm), teseladb_lsm::Step::Failed(LsmError::Corrupt { .. })));
}

#[test]
fn a_damaged_footer_fails_recovery() {
    let cfg = small();
    let mut disk = SimDisk::default();
    let mut lsm = open(&cfg, &mut disk);
    lsm.put(G, 1, b"k", ts(1, 0), b"v").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    drop(lsm);
    let sst = sst_path(&mut disk, &cfg.dir);
    let len = disk.read_all(&sst).unwrap().len() as u64;
    assert!(disk.corrupt(&sst, len - 1));
    let err = teseladb_lsm::Lsm::recover(&cfg, &mut disk).unwrap_err();
    assert!(matches!(err, LsmError::Corrupt { .. }), "{err:?}");
}

#[test]
fn a_full_disk_turns_read_only_and_retries_every_second() {
    let cfg = small();
    let mut disk = SimDisk::default();
    let mut lsm = open(&cfg, &mut disk);
    lsm.put(G, 1, b"k", ts(1, 0), b"v").unwrap();
    lsm.flush();
    disk.inject_error("write", IoErrno::NoSpace);
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(lsm.health(), Err(LsmError::ReadOnly));
    assert_eq!(lsm.pressure(), Pressure::Reject);
    // Writes the log already confirmed still apply in memory.
    lsm.put(G, 2, b"k2", ts(2, 0), b"v").unwrap();
    // Nothing is retried before a second has passed.
    assert!(lsm.poll_io(Nanos::from_millis(999)).is_none());
    lsm.run_io(&mut disk, Nanos::from_secs(1));
    assert_eq!(lsm.health(), Ok(()));
    assert_eq!(lsm.flushed_index(G), 1);
    drop(lsm);
    let lsm = open(&cfg, &mut disk);
    assert_eq!(lsm.flushed_index(G), 1);
}

#[test]
fn any_other_io_error_stops_the_core() {
    for op in ["sync", "create", "sync_dir"] {
        let cfg = small();
        let mut disk = SimDisk::default();
        let mut lsm = open(&cfg, &mut disk);
        lsm.put(G, 1, b"k", ts(1, 0), b"v").unwrap();
        lsm.flush();
        disk.inject_error(op, IoErrno::Io);
        lsm.run_io(&mut disk, Nanos::ZERO);
        assert_eq!(lsm.health(), Err(LsmError::Stopped(IoErrno::Io)), "{op}");
        assert_eq!(lsm.put(G, 2, b"x", ts(2, 0), b"y"), Err(LsmError::Stopped(IoErrno::Io)));
        assert_eq!(
            lsm.delete_range(G, 3, b"a", b"b", ts(2, 0)),
            Err(LsmError::Stopped(IoErrno::Io))
        );
        assert!(lsm.poll_io(Nanos::ZERO).is_none(), "nothing more is written");
        let v = lsm.view();
        assert_eq!(v.get(&mut lsm, b"k", ts(1, 0)), Lookup::Failed(LsmError::Stopped(IoErrno::Io)));
    }
}

#[test]
fn a_failed_read_stops_the_core() {
    let cfg = small();
    let mut disk = SimDisk::default();
    let mut lsm = open(&cfg, &mut disk);
    lsm.put(G, 1, b"k", ts(1, 0), b"v").unwrap();
    lsm.flush();
    lsm.run_io(&mut disk, Nanos::ZERO);
    drop(lsm);
    let mut lsm = open(&cfg, &mut disk);
    let v = lsm.view();
    assert!(matches!(v.get(&mut lsm, b"k", ts(2, 0)), Lookup::Miss(_)));
    disk.inject_error("read", IoErrno::Io);
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(v.get(&mut lsm, b"k", ts(2, 0)), Lookup::Failed(LsmError::Stopped(IoErrno::Io)));
}

#[test]
fn nothing_more_goes_out_while_an_operation_is_in_flight() {
    let cfg = small();
    let mut disk = SimDisk::default();
    let mut lsm = open(&cfg, &mut disk);
    lsm.put(G, 1, b"k", ts(1, 0), b"v").unwrap();
    lsm.flush();
    let op = lsm.poll_io(Nanos::ZERO).expect("the flush starts");
    assert!(lsm.poll_io(Nanos::ZERO).is_none(), "one operation at a time");
    let done = disk.execute(op);
    lsm.on_io_done(done, Nanos::ZERO);
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(lsm.flushed_index(G), 1);
}

#[test]
fn a_bad_block_met_by_a_compaction_marks_the_core_corrupt() {
    let cfg = small();
    let mut disk = SimDisk::default();
    let mut lsm = open(&cfg, &mut disk);
    for i in 0..4u64 {
        lsm.put(G, i + 1, format!("k{i}").as_bytes(), ts(1, 0), &[5; 200]).unwrap();
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
        if i == 0 {
            let sst = sst_path(&mut disk, &cfg.dir);
            assert!(disk.corrupt(&sst, 10));
        }
    }
    assert!(matches!(lsm.health(), Err(LsmError::Corrupt { .. })), "{:?}", lsm.health());
}

#[test]
fn a_failed_read_of_a_compaction_stops_the_core() {
    let cfg = small();
    let mut disk = SimDisk::default();
    let mut lsm = open(&cfg, &mut disk);
    for i in 0..3u64 {
        lsm.put(G, i + 1, format!("k{i}").as_bytes(), ts(1, 0), b"v").unwrap();
        lsm.flush();
        lsm.run_io(&mut disk, Nanos::ZERO);
    }
    lsm.put(G, 4, b"k3", ts(1, 0), b"v").unwrap();
    lsm.flush();
    disk.inject_error("read", IoErrno::Io);
    lsm.run_io(&mut disk, Nanos::ZERO);
    assert_eq!(lsm.health(), Err(LsmError::Stopped(IoErrno::Io)));
}
