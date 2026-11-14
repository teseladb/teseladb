//! The device log's state (012 FR-035, D-154): what each core wrote comes back after a restart,
//! one sync serves every core's writes, old files go once nothing points into them, and a crash at
//! any operation of the disk loses nothing that was answered.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use teseladb_devlog::state::{KEPT_FILES, LogState};
use teseladb_raftlog::{
    Disk, FileId, IoDone, IoErrno, IoOp, MemDisk, SimDisk, SimDiskConfig, StorageError,
};
use teseladb_types::Rng;

fn sync_op(n: u64) -> IoOp {
    IoOp::Sync { file: FileId(1), batch: n }
}

/// What a core's WAL segments should hold.
type Model = BTreeMap<(u16, u64), Vec<u8>>;

fn put(model: &mut Model, key: (u16, u64), offset: u64, data: &[u8]) {
    let w = model.entry(key).or_default();
    let end = offset as usize + data.len();
    if w.len() < end {
        w.resize(end, 0);
    }
    w[offset as usize..end].copy_from_slice(data);
}

fn check(state: &mut LogState, disk: &mut dyn Disk, model: &Model) {
    for core in 0..4u16 {
        let wals: Vec<u64> = model.keys().filter(|k| k.0 == core).map(|k| k.1).collect();
        assert_eq!(state.segments_of(core), wals, "core {core}");
        for w in wals {
            assert_eq!(
                state.read(disk, core, w).unwrap(),
                model[&(core, w)],
                "core {core} wal {w}"
            );
        }
    }
}

#[test]
fn one_sync_serves_the_writes_of_every_core() {
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
    for core in 0..8u16 {
        s.create(core, 1, 0);
        s.write(core, 1, 0, &[core as u8; 100]);
        s.sync(core, sync_op(1));
    }
    let answers = s.flush(&mut disk);
    assert_eq!(answers.len(), 8);
    assert!(answers.iter().all(|(_, d)| matches!(d, IoDone::Ok(_))));
    assert_eq!(s.syncs(), 1);
    // Nothing new: a sync asked now is answered without syncing again.
    s.sync(3, sync_op(2));
    assert_eq!(s.flush(&mut disk).len(), 1);
    assert_eq!(s.syncs(), 1);
    let mut again = LogState::recover(&mut disk, 1 << 20).unwrap();
    for core in 0..8u16 {
        assert_eq!(again.read(&mut disk, core, 1).unwrap(), vec![core as u8; 100]);
    }
}

#[test]
fn random_writes_overwrites_truncations_and_removals_survive_restarts() {
    for seed in 0..40 {
        let mut rng = Rng::new(seed);
        let mut disk = MemDisk::new();
        let file_bytes = 64 << 10;
        let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
        s.allow_tidying();
        let mut model = Model::new();
        let mut next_wal = [1u64; 4];
        for step in 0..300 {
            let core = rng.range(0, 4) as u16;
            let wals: Vec<u64> = model.keys().filter(|k| k.0 == core).map(|k| k.1).collect();
            match rng.range(0, 10) {
                0 | 1 if wals.len() < 3 => {
                    let w = next_wal[core as usize];
                    next_wal[core as usize] += 1;
                    let len = rng.range(0, 3) * 4096;
                    s.create(core, w, len);
                    model.insert((core, w), vec![0; len as usize]);
                }
                2 if !wals.is_empty() => {
                    let w = wals[rng.range(0, wals.len() as u64) as usize];
                    s.remove(core, w);
                    model.remove(&(core, w));
                }
                3 if !wals.is_empty() => {
                    let w = wals[rng.range(0, wals.len() as u64) as usize];
                    let len = rng.range(0, model[&(core, w)].len() as u64 + 1);
                    s.truncate(core, w, len);
                    model.get_mut(&(core, w)).unwrap().truncate(len as usize);
                }
                _ if !wals.is_empty() => {
                    let w = wals[rng.range(0, wals.len() as u64) as usize];
                    let cur = model[&(core, w)].len() as u64;
                    // Mostly appends, sometimes an overwrite inside.
                    let offset = if rng.range(0, 4) == 0 { rng.range(0, cur + 1) } else { cur };
                    let mut data = vec![0u8; rng.range(1, 6000) as usize];
                    rng.fill(&mut data);
                    s.write(core, w, offset, &data);
                    put(&mut model, (core, w), offset, &data);
                }
                _ => {}
            }
            if rng.range(0, 5) == 0 {
                s.sync(core, sync_op(step));
                let answers = s.flush(&mut disk);
                assert!(answers.iter().all(|(_, d)| matches!(d, IoDone::Ok(_))), "{answers:?}");
            }
            if rng.range(0, 40) == 0 {
                s.sync(0, sync_op(step));
                s.flush(&mut disk);
                s = LogState::recover(&mut disk, file_bytes).unwrap();
                s.allow_tidying();
                check(&mut s, &mut disk, &model);
            }
        }
        s.sync(0, sync_op(0));
        s.flush(&mut disk);
        check(&mut s, &mut disk, &model);
        let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
        check(&mut s, &mut disk, &model);
    }
}

fn files(disk: &mut dyn Disk) -> Vec<PathBuf> {
    disk.list(Path::new("raftlog")).unwrap_or_default()
}

#[test]
fn old_files_go_and_a_quiet_core_does_not_keep_them() {
    let mut disk = MemDisk::new();
    let file_bytes = 32 << 10;
    let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
    s.allow_tidying();
    // Core 1 writes once and then nothing: its segment stays live in the oldest file.
    s.create(1, 1, 0);
    s.write(1, 1, 0, b"quiet");
    s.sync(1, sync_op(1));
    s.flush(&mut disk);
    // Core 0 writes a lot, a segment at a time, and lets the old segments go.
    let block = vec![7u8; 8000];
    for w in 1..60u64 {
        s.create(0, w, 0);
        for k in 0..3 {
            s.write(0, w, k * 8000, &block);
            s.sync(0, sync_op(w));
            s.flush(&mut disk);
        }
        if w > 1 {
            s.remove(0, w - 1);
        }
    }
    s.sync(0, sync_op(0));
    s.flush(&mut disk);
    let left = files(&mut disk).len();
    assert!(left <= KEPT_FILES + 2, "{left} files left");
    assert_eq!(s.files(), left);
    let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
    assert_eq!(s.read(&mut disk, 1, 1).unwrap(), b"quiet");
    assert_eq!(s.segments_of(0), vec![59]);
    assert_eq!(s.read(&mut disk, 0, 59).unwrap().len(), 24000);
}

#[test]
fn nothing_is_removed_while_the_cores_recover() {
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, 16 << 10).unwrap();
    for w in 1..10u64 {
        s.create(0, w, 0);
        s.write(0, w, 0, &[1; 9000]);
        s.remove(0, w);
        s.sync(0, sync_op(w));
        s.flush(&mut disk);
    }
    let before = files(&mut disk).len();
    assert!(before >= 9);
    s.allow_tidying();
    s.sync(0, sync_op(10));
    s.flush(&mut disk);
    assert!(files(&mut disk).len() < before);
}

#[test]
fn a_full_disk_waits_and_another_error_stops_the_log() {
    let mut disk = MemDisk::with_capacity(4096 * 3);
    let mut s = LogState::recover(&mut disk, 8 << 10).unwrap();
    s.create(0, 1, 0);
    s.write(0, 1, 0, &[1; 100]);
    s.sync(0, sync_op(1));
    assert_eq!(s.flush(&mut disk).len(), 1);
    // The next file does not fit: the sync waits.
    s.write(0, 1, 100, &[2; 20000]);
    s.sync(0, sync_op(2));
    assert!(s.flush(&mut disk).is_empty());
    assert!(s.disk_full());
    let mut bigger = MemDisk::new();
    let mut s2 = LogState::recover(&mut bigger, 8 << 10).unwrap();
    s2.create(0, 1, 0);
    s2.sync(0, sync_op(1));
    bigger.inject_error("sync", IoErrno::Io);
    let a = s2.flush(&mut bigger);
    assert!(matches!(a[0].1, IoDone::Err { errno: IoErrno::Io, .. }));
    assert_eq!(s2.failed(), Some(IoErrno::Io));
    s2.sync(0, sync_op(2));
    assert!(matches!(s2.flush(&mut bigger)[0].1, IoDone::Err { .. }));
}

/// A simulated disk whose operations fail after `left` of them: the machine stopped there.
struct Cut {
    disk: SimDisk,
    left: usize,
}

impl Disk for Cut {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        self.disk.list(dir)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        self.disk.read_all(path)
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        if self.left == 0 {
            return IoDone::Err { op: op.without_data(), errno: IoErrno::Io };
        }
        self.left -= 1;
        self.disk.execute(op)
    }
}

fn sim() -> SimDisk {
    SimDisk::new(SimDiskConfig {
        torn_write_probability: 0.5,
        unsynced_create_probability: 0.5,
        ..SimDiskConfig::default()
    })
}

#[test]
fn a_crash_at_any_operation_keeps_every_answered_write() {
    // How many operations a whole run takes.
    let run = |disk: &mut dyn Disk, rng: &mut Rng, acked: &mut Model, last: &mut Model| {
        let mut s = match LogState::recover(disk, 16 << 10) {
            Ok(s) => s,
            Err(_) => return,
        };
        s.allow_tidying();
        let mut model = acked.clone();
        let mut next = model.keys().map(|k| k.1).max().unwrap_or(0) + 1;
        for step in 0..40u64 {
            let core = rng.range(0, 3) as u16;
            let mine: Vec<u64> = model.keys().filter(|k| k.0 == core).map(|k| k.1).collect();
            if mine.is_empty() || rng.range(0, 6) == 0 {
                s.create(core, next, 0);
                model.insert((core, next), Vec::new());
                if let Some(old) = mine.first().filter(|_| mine.len() > 1) {
                    s.remove(core, *old);
                    model.remove(&(core, *old));
                }
                next += 1;
                *last = model.clone();
                continue;
            }
            let w = *mine.last().unwrap();
            let mut data = vec![0u8; rng.range(1, 5000) as usize];
            rng.fill(&mut data);
            let at = model[&(core, w)].len() as u64;
            s.write(core, w, at, &data);
            put(&mut model, (core, w), at, &data);
            *last = model.clone();
            s.sync(core, sync_op(step));
            let answers = s.flush(disk);
            if answers.iter().any(|(_, d)| !matches!(d, IoDone::Ok(_))) || answers.is_empty() {
                return;
            }
            *acked = model.clone();
        }
    };
    let mut counter = Cut { disk: sim(), left: usize::MAX };
    run(&mut counter, &mut Rng::new(1), &mut Model::new(), &mut Model::new());
    let total = usize::MAX - counter.left;
    assert!(total > 50, "{total}");
    for cut in 0..total {
        for seed in 0..3u64 {
            let mut rng = Rng::new(1);
            let mut acked = Model::new();
            let mut last = Model::new();
            let mut disk = Cut { disk: sim(), left: cut };
            run(&mut disk, &mut rng, &mut acked, &mut last);
            let mut crash = Rng::new(seed);
            disk.disk.crash(&mut crash);
            let mut disk = disk.disk;
            let mut s = LogState::recover(&mut disk, 16 << 10).unwrap();
            // Every answered write is there, but in a segment removed after the answer, whose
            // removal may have reached the disk.
            for ((core, w), data) in acked.iter().filter(|(k, _)| last.contains_key(k)) {
                let got = s.read(&mut disk, *core, *w).unwrap_or_else(|e| {
                    panic!("cut {cut} seed {seed} {core}/{w}: {e:?} {:?}", s.segments_of(*core))
                });
                assert!(got.len() >= data.len(), "cut {cut} seed {seed} {core}/{w}");
                assert_eq!(&got[..data.len()], &data[..], "cut {cut} seed {seed} {core}/{w}");
            }
            // And the log goes on after the crash.
            s.allow_tidying();
            s.create(2, 1000, 0);
            s.write(2, 1000, 0, b"after");
            s.sync(2, sync_op(1));
            assert!(matches!(s.flush(&mut disk)[0].1, IoDone::Ok(_)), "cut {cut}");
            let mut s = LogState::recover(&mut disk, 16 << 10).unwrap();
            assert_eq!(s.read(&mut disk, 2, 1000).unwrap(), b"after");
        }
    }
}

/// A log with one file whose record holds `kept` for segment 1 of core 0, and that file.
fn kept_log() -> (SimDisk, PathBuf) {
    let mut disk = sim();
    let mut s = LogState::recover(&mut disk, 16 << 10).unwrap();
    s.create(0, 1, 0);
    s.write(0, 1, 0, b"kept");
    s.sync(0, sync_op(1));
    assert!(matches!(s.flush(&mut disk)[0].1, IoDone::Ok(_)));
    let first = files(&mut disk)[0].clone();
    (disk, first)
}

#[test]
fn a_damaged_header_stops_the_log_and_a_creation_cut_short_does_not() {
    let (mut disk, _) = kept_log();
    // A creation cut short before its header was durable: a file of zeros, which holds nothing.
    let cut = teseladb_devlog::format::segment_path(9);
    let made = disk.execute(IoOp::Create { file: FileId(77), path: cut, prealloc: 8192 });
    assert!(matches!(made, IoDone::Ok(_)));
    disk.execute(IoOp::SyncDir { path: teseladb_devlog::format::DIR.into() });
    let mut again = LogState::recover(&mut disk, 16 << 10).unwrap();
    assert_eq!(again.read(&mut disk, 0, 1).unwrap(), b"kept");
    // Any byte of the header block of a file with records: the log does not start.
    for at in [0u64, 17, 25, 300, 4095] {
        let (mut d, first) = kept_log();
        assert!(d.corrupt(&first, at));
        match LogState::recover(&mut d, 16 << 10) {
            Err(StorageError::Corrupt { file, offset: 0 }) => assert_eq!(file, first),
            other => panic!("byte {at}: {other:?}"),
        }
    }
}
