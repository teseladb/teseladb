//! The device log's state (012 FR-035, D-154): what each core wrote comes back after a restart,
//! one sync serves every core's writes, old files go once nothing points into them, and a crash at
//! any operation of the disk loses nothing that was answered.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use teseladb_devlog::state::{CopyJob, KEPT_FILES, LogState, SpareJob};
use teseladb_raftlog::{
    Disk, FileId, IoDone, IoErrno, IoOp, MemDisk, SimDisk, SimDiskConfig, StorageError,
};
use teseladb_types::Rng;

fn sync_op(n: u64) -> IoOp {
    IoOp::Sync { file: FileId(1), batch: n }
}

/// `len` bytes without a run of one byte long enough for a `FILL`.
fn varied(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
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

/// Bytes as a core's WAL writes them: random records, often followed by a run of one byte as long
/// as the padding of a batch (or shorter than a `FILL` takes).
fn wal_bytes(rng: &mut Rng, most: u64) -> Vec<u8> {
    let mut data = vec![0u8; rng.range(1, most) as usize];
    rng.fill(&mut data);
    if rng.range(0, 2) == 0 {
        let byte = [0x5a, 0, 7][rng.range(0, 3) as usize];
        let run = rng.range(1, 4096) as usize;
        data.resize(data.len() + run, byte);
    }
    data
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
                    let data = wal_bytes(&mut rng, 6000);
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
    let block = varied(8000);
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
        s.write(0, w, 0, &varied(9000));
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
    s.write(0, 1, 100, &varied(20000));
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
            let data = wal_bytes(rng, 5000);
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
    // A creation cut short while its space was being written with zeros: it holds nothing either.
    let cut = teseladb_devlog::format::segment_path(10);
    let made = disk.execute(IoOp::Create { file: FileId(78), path: cut, prealloc: 8192 });
    assert!(matches!(made, IoDone::Ok(_)));
    let zeros = teseladb_types::Bytes::from_vec(vec![0; 4096]);
    disk.execute(IoOp::WriteAt { file: FileId(78), offset: 0, data: zeros, batch: 0 });
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

/// A disk whose reads of whole files, opens or reads at an offset go wrong as `fault` says.
struct Faulty {
    disk: MemDisk,
    fault: Fault,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    ReadAll,
    Open,
    ShortRead,
    WriteNoSpace,
}

impl Disk for Faulty {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        self.disk.list(dir)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        if self.fault == Fault::ReadAll {
            return Err(IoErrno::Io);
        }
        self.disk.read_all(path)
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        match (self.fault, &op) {
            (Fault::Open, IoOp::Open { .. }) => {
                IoDone::Err { op: op.without_data(), errno: IoErrno::Io }
            }
            (Fault::WriteNoSpace, IoOp::WriteAt { offset, .. }) if *offset > 0 => {
                IoDone::Err { op: op.without_data(), errno: IoErrno::NoSpace }
            }
            (Fault::ShortRead, IoOp::ReadAt { .. }) => match self.disk.execute(op) {
                IoDone::Read { token, data: Ok(d) } => {
                    let short = teseladb_types::Bytes::from_vec(d[..d.len() / 2].to_vec());
                    IoDone::Read { token, data: Ok(short) }
                }
                other => other,
            },
            _ => self.disk.execute(op),
        }
    }
}

/// A log on a memory disk with one record per segment: segment 1 of core 0 holds `one`, and
/// segment 2 of core 0, in a second record, holds `two`.
fn two_records() -> MemDisk {
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
    for (wal, data) in [(1u64, b"one"), (2, b"two")] {
        s.create(0, wal, 0);
        s.write(0, wal, 0, data);
        s.sync(0, sync_op(wal));
        assert!(matches!(s.flush(&mut disk)[..], [(0, IoDone::Ok(_))]));
    }
    disk
}

/// Puts `bytes` at `offset` of the first file of the log on `disk`.
fn overwrite(disk: &mut MemDisk, offset: u64, bytes: Vec<u8>) {
    let path = files(disk)[0].clone();
    assert!(matches!(disk.execute(IoOp::Open { file: FileId(900), path }), IoDone::Ok(_)));
    let data = teseladb_types::Bytes::from_vec(bytes);
    let wrote = disk.execute(IoOp::WriteAt { file: FileId(900), offset, data, batch: 0 });
    assert!(matches!(wrote, IoDone::Ok(_)));
}

/// Where the second record of the first file starts, and its body.
fn second_record(disk: &mut MemDisk) -> (u64, Vec<u8>) {
    use teseladb_devlog::format::{BLOCK, read_record};
    let path = files(disk)[0].clone();
    let data = disk.read_all(&path).unwrap();
    let first = read_record(&data[BLOCK as usize..]).unwrap();
    assert_eq!(first.flush, 1);
    let at = BLOCK as usize + first.len;
    let second = read_record(&data[at..]).unwrap();
    assert_eq!(second.flush, 2);
    (at as u64, second.body.to_vec())
}

#[test]
fn recovery_ends_at_a_record_out_of_order_of_another_version_or_with_broken_frames() {
    use teseladb_devlog::format::record;
    let mut whole = two_records();
    let s = LogState::recover(&mut whole, 1 << 20).unwrap();
    assert_eq!(s.segments_of(0), vec![1, 2]);
    let (at, body) = second_record(&mut whole);
    let mut newer = record(2, &body);
    newer[4] = 3;
    // A record whose flush is not the next one (a stale record of an earlier flush), one of a
    // version this build does not read, and one whose frames do not parse: the log ends before it.
    for damaged in [record(7, &body), newer, record(2, &[0xEE; 40])] {
        let mut disk = two_records();
        overwrite(&mut disk, at, damaged);
        let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
        assert_eq!(s.segments_of(0), vec![1]);
        assert_eq!(s.read(&mut disk, 0, 1).unwrap(), b"one");
        assert_eq!(s.read(&mut disk, 0, 2), Err(IoErrno::NotFound));
        // The next flush goes on from the last record read.
        s.create(0, 3, 0);
        s.sync(0, sync_op(3));
        assert!(matches!(s.flush(&mut disk)[..], [(0, IoDone::Ok(_))]));
        assert_eq!(LogState::recover(&mut disk, 1 << 20).unwrap().segments_of(0), vec![1, 3]);
    }
}

#[test]
fn recovery_fails_with_the_disk_error_of_a_read_or_an_open() {
    for (fault, op) in [(Fault::ReadAll, "read"), (Fault::Open, "open")] {
        let mut disk = Faulty { disk: two_records(), fault };
        match LogState::recover(&mut disk, 1 << 20) {
            Err(StorageError::Io { op: got, errno: IoErrno::Io }) => assert_eq!(got, op),
            other => panic!("{op}: {other:?}"),
        }
    }
}

#[test]
fn a_short_read_of_the_log_is_an_error() {
    let mut disk = Faulty { disk: two_records(), fault: Fault::None };
    let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
    assert_eq!(s.read(&mut disk, 0, 2).unwrap(), b"two");
    disk.fault = Fault::ShortRead;
    assert_eq!(s.read(&mut disk, 0, 2), Err(IoErrno::Io));
}

#[test]
fn no_space_for_a_record_in_a_file_already_made_fails_the_log() {
    let mut disk = Faulty { disk: two_records(), fault: Fault::WriteNoSpace };
    let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
    s.write(0, 1, 3, b"more");
    s.sync(0, sync_op(4));
    // The disk did not refuse a new file: the log stops, as with any other error.
    assert!(matches!(s.flush(&mut disk)[..], [(0, IoDone::Err { errno: IoErrno::NoSpace, .. })]));
    assert!(!s.disk_full());
    assert_eq!(s.failed(), Some(IoErrno::NoSpace));
    s.sync(0, sync_op(5));
    assert!(matches!(s.flush(&mut disk)[..], [(0, IoDone::Err { errno: IoErrno::NoSpace, .. })]));
}

#[test]
fn an_empty_write_lengthens_the_segment_with_zeros() {
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
    s.create(0, 1, 0);
    s.write(0, 1, 0, b"ab");
    s.write(0, 1, 6, &[]);
    s.sync(0, sync_op(1));
    assert_eq!(s.flush(&mut disk).len(), 1);
    assert_eq!(s.read(&mut disk, 0, 1).unwrap(), b"ab\0\0\0\0");
    let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
    assert_eq!(s.read(&mut disk, 0, 1).unwrap(), b"ab\0\0\0\0");
}

#[test]
fn writes_nobody_syncs_wait_until_enough_bytes_wait_or_a_flush() {
    use teseladb_devlog::state::WRITE_AHEAD;
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, 4 << 20).unwrap();
    let mut ops = Vec::new();
    s.create(0, 1, 0);
    s.write(0, 1, 0, b"small");
    assert!(s.buffered() > 0);
    assert!(!s.has_waiting());
    // Below the bytes written ahead, nothing goes to the disk.
    assert!(s.next(&mut disk, &mut ops).is_empty());
    assert!(ops.is_empty());
    s.write(0, 1, 5, &varied(WRITE_AHEAD));
    assert!(s.next(&mut disk, &mut ops).is_empty());
    assert!(matches!(ops[..], [IoOp::WriteAt { .. }]));
    assert_eq!(s.buffered(), 0);
    assert!(!s.syncing());
    for op in ops.drain(..) {
        let done = disk.execute(op);
        assert!(s.on_done(&done).is_empty());
    }
    // A flush writes what waits even when no sync asks for it.
    s.write(0, 1, 5 + WRITE_AHEAD as u64, b"tail");
    assert!(s.flush(&mut disk).is_empty());
    assert_eq!(s.buffered(), 0);
    // A sync asked now is handed out and runs until its result comes back.
    s.sync(0, sync_op(1));
    assert!(s.has_waiting());
    assert!(s.next(&mut disk, &mut ops).is_empty());
    assert!(s.syncing());
    let mut answers = Vec::new();
    for op in ops.drain(..) {
        let done = disk.execute(op);
        answers.extend(s.on_done(&done));
    }
    assert!(matches!(answers[..], [(0, IoDone::Ok(IoOp::Sync { .. }))]));
    assert!(!s.syncing());
    let mut s = LogState::recover(&mut disk, 4 << 20).unwrap();
    let got = s.read(&mut disk, 0, 1).unwrap();
    assert_eq!(got.len(), 9 + WRITE_AHEAD);
    assert_eq!(&got[..5], b"small");
    assert_eq!(&got[got.len() - 4..], b"tail");
}

/// A log whose files write through (O_DSYNC, 012 FR-035) writes nothing ahead while a sync runs:
/// each record would be a durable write of its own that the next group waits for. What comes
/// meanwhile goes in the next group's one record. A log that syncs its files writes ahead.
#[test]
fn a_log_that_writes_through_writes_no_record_ahead_while_a_sync_runs() {
    for through in [false, true] {
        let mut disk = MemDisk::new();
        let mut s = LogState::recover(&mut disk, 4 << 20).unwrap();
        s.set_write_through(through);
        let mut ops = Vec::new();
        s.create(0, 1, 0);
        s.write(0, 1, 0, b"first");
        s.sync(0, sync_op(1));
        assert!(s.next(&mut disk, &mut ops).is_empty());
        assert!(s.syncing());
        let first: Vec<IoOp> = std::mem::take(&mut ops);
        // While the first group's sync runs, another write and its sync come.
        s.write(0, 1, 5, b"second");
        s.sync(0, sync_op(2));
        assert!(s.next(&mut disk, &mut ops).is_empty());
        let ahead = ops.iter().any(|o| matches!(o, IoOp::WriteAt { .. }));
        assert_eq!(ahead, !through, "written ahead with write-through {through}");
        let mut answers = Vec::new();
        for op in first.into_iter().chain(ops.drain(..)) {
            let done = disk.execute(op);
            answers.extend(s.on_done(&done));
        }
        for _ in 0..4 {
            answers.extend(s.next(&mut disk, &mut ops));
            for op in ops.drain(..) {
                let done = disk.execute(op);
                answers.extend(s.on_done(&done));
            }
        }
        assert_eq!(answers.len(), 2, "both syncs answered: {answers:?}");
        let mut s = LogState::recover(&mut disk, 4 << 20).unwrap();
        assert_eq!(s.read(&mut disk, 0, 1).unwrap(), b"firstsecond");
    }
}

/// The HLC's bound rides in the log's records (005 FR-003, D-172): an offered bound goes in the
/// next record and is durable once its sync ends, and it comes back on recovery.
#[test]
fn an_offered_bound_is_durable_with_the_next_sync_and_comes_back() {
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
    assert_eq!(s.durable_bound(), 0);
    // Offered while nothing is written: no record goes for it alone.
    s.offer_bound(5_000);
    s.flush(&mut disk);
    assert_eq!(s.durable_bound(), 0);
    assert!(files(&mut disk).is_empty());
    // The next record carries it.
    s.create(0, 1, 0);
    s.write(0, 1, 0, b"x");
    s.sync(0, sync_op(1));
    s.flush(&mut disk);
    assert_eq!(s.durable_bound(), 5_000);
    // A lower offer never lowers it.
    s.offer_bound(4_000);
    s.write(0, 1, 1, b"y");
    s.sync(0, sync_op(2));
    s.flush(&mut disk);
    assert_eq!(s.durable_bound(), 5_000);
    s.offer_bound(6_000);
    s.write(0, 1, 2, b"z");
    s.sync(0, sync_op(3));
    s.flush(&mut disk);
    assert_eq!(s.durable_bound(), 6_000);
    let s = LogState::recover(&mut disk, 1 << 20).unwrap();
    assert_eq!(s.durable_bound(), 6_000);
}

/// Every record carries the bound in force, so removing the oldest files never loses it.
#[test]
fn the_bound_outlives_the_files_that_go() {
    let mut disk = MemDisk::new();
    let file_bytes = 32 << 10;
    let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
    s.allow_tidying();
    s.offer_bound(9_000);
    let block = varied(8000);
    for w in 1..40u64 {
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
    assert!(files(&mut disk).len() <= KEPT_FILES + 2);
    let s = LogState::recover(&mut disk, file_bytes).unwrap();
    assert_eq!(s.durable_bound(), 9_000);
}

/// A batch of a core's WAL as it writes one: its records and the padding to 4 KiB.
fn batch(records: &[u8]) -> Vec<u8> {
    let mut b = records.to_vec();
    b.resize(4096, 0x5a);
    b
}

/// The padding of a WAL batch goes as a `FILL` frame: the record takes one block instead of two,
/// and the segment reads back byte for byte, also after a restart and after an overwrite inside the
/// padding.
#[test]
fn the_padding_of_a_wal_batch_goes_as_a_fill_and_reads_back() {
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
    let first = batch(&varied(600));
    s.create(0, 1, 8192);
    s.write(0, 1, 0, &first);
    s.sync(0, sync_op(1));
    assert_eq!(s.flush(&mut disk).len(), 1);
    let st = s.stats();
    assert_eq!((st.records, st.record_bytes), (1, 4096));
    assert_eq!(st.filled_bytes, 4096 - 600);
    // Head, `CREATE`, `WRITE` of the 600 bytes and `FILL`; the rest of the block is padding.
    assert_eq!(st.padding_bytes, 4096 - (32 + 24 + (24 + 600) + 24));
    let mut want = first.clone();
    want.resize(8192, 0);
    assert_eq!(s.read(&mut disk, 0, 1).unwrap(), want);
    // A write over part of the padding: the rest of the run stays.
    s.write(0, 1, 1000, b"inside");
    want[1000..1006].copy_from_slice(b"inside");
    // A run too short for a `FILL` goes as data.
    let short = [9u8; 10];
    s.write(0, 1, 4096, &short);
    want[4096..4106].copy_from_slice(&short);
    s.sync(0, sync_op(2));
    s.flush(&mut disk);
    assert_eq!(s.stats().filled_bytes, 4096 - 600);
    assert_eq!(s.read(&mut disk, 0, 1).unwrap(), want);
    let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
    assert_eq!(s.read(&mut disk, 0, 1).unwrap(), want);
    // Truncated inside the run, then removed.
    s.truncate(0, 1, 2000);
    s.sync(0, sync_op(3));
    s.flush(&mut disk);
    assert_eq!(s.read(&mut disk, 0, 1).unwrap(), want[..2000].to_vec());
    s.remove(0, 1);
    s.sync(0, sync_op(4));
    s.flush(&mut disk);
    let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
    assert_eq!(s.read(&mut disk, 0, 1), Err(IoErrno::NotFound));
}

/// A quiet core whose batches are mostly padding keeps them across the copies forward of the
/// oldest file: its `FILL` frames are copied with its bytes, and they keep their file until then.
#[test]
fn a_quiet_core_keeps_its_padding_across_copies_forward() {
    let mut disk = MemDisk::new();
    let file_bytes = 32 << 10;
    let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
    s.allow_tidying();
    let quiet = batch(b"quiet");
    s.create(1, 1, 0);
    s.write(1, 1, 0, &quiet);
    s.sync(1, sync_op(1));
    s.flush(&mut disk);
    let block = varied(8000);
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
    assert!(s.stats().copied_bytes > 0, "{:?}", s.stats());
    assert!(files(&mut disk).len() <= KEPT_FILES + 2);
    assert_eq!(s.read(&mut disk, 1, 1).unwrap(), quiet);
    let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
    assert_eq!(s.read(&mut disk, 1, 1).unwrap(), quiet);
}

/// Rewrites the version of the header and of every record of `path` to 1, as a log written before
/// `FILL` frames has them, with their CRCs.
fn as_version_1(disk: &mut MemDisk, path: &Path) {
    use teseladb_devlog::format::{BLOCK, read_record};
    use teseladb_hash::crc32c;
    let mut data = disk.read_all(path).unwrap();
    data[8] = 1;
    let crc = crc32c(&data[..24]);
    data[24..28].copy_from_slice(&crc.to_le_bytes());
    let mut at = BLOCK as usize;
    while let Some(len) = read_record(&data[at..]).map(|r| r.len) {
        data[at + 4] = 1;
        let crc = crc32c(&data[at..at + 24]);
        data[at + 24..at + 28].copy_from_slice(&crc.to_le_bytes());
        at += len;
    }
    assert!(at > BLOCK as usize);
    assert!(matches!(
        disk.execute(IoOp::Open { file: FileId(901), path: path.into() }),
        IoDone::Ok(_)
    ));
    let data = teseladb_types::Bytes::from_vec(data);
    let wrote = disk.execute(IoOp::WriteAt { file: FileId(901), offset: 0, data, batch: 0 });
    assert!(matches!(wrote, IoDone::Ok(_)));
}

/// A log of version 1 reads as before, and the log goes on after it in a file of version 2.
#[test]
fn a_log_of_version_1_reads_and_goes_on() {
    let mut disk = two_records();
    let path = files(&mut disk)[0].clone();
    as_version_1(&mut disk, &path);
    let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
    assert_eq!(s.read(&mut disk, 0, 1).unwrap(), b"one");
    assert_eq!(s.read(&mut disk, 0, 2).unwrap(), b"two");
    s.write(0, 2, 3, &batch(b"more"));
    s.sync(0, sync_op(3));
    assert!(matches!(s.flush(&mut disk)[..], [(0, IoDone::Ok(_))]));
    let mut s = LogState::recover(&mut disk, 1 << 20).unwrap();
    let mut want = b"two".to_vec();
    want.extend_from_slice(&batch(b"more"));
    assert_eq!(s.read(&mut disk, 0, 2).unwrap(), want);
    assert_eq!(files(&mut disk).len(), 2);
}

/// Runs a file's [`SpareJob`] on `disk`: its outcome.
fn run_job(disk: &mut dyn Disk, job: SpareJob) -> Result<(), IoErrno> {
    job.ops.into_iter().try_for_each(|op| match disk.execute(op) {
        IoDone::Err { errno, .. } => Err(errno),
        _ => Ok(()),
    })
}

/// Writes `n` records of 5000 bytes for segment 1 of core 0 from `from`, each synced.
fn records(s: &mut LogState, disk: &mut dyn Disk, from: u64, n: u64) {
    for i in from..from + n {
        s.write(0, 1, i * 5000, &varied(5000));
        s.sync(0, sync_op(i));
        assert!(s.flush(disk).iter().all(|(_, d)| matches!(d, IoDone::Ok(_))));
    }
}

/// The next file is made ahead once the active one is half full, and the record that does not fit
/// goes into it without making a file; a job not finished in time, or one that failed, leaves the
/// record to make its file as before, and the file whose turn passed holds nothing.
#[test]
fn the_next_file_is_made_ahead_and_a_late_or_failed_one_changes_nothing() {
    let file_bytes = 32 << 10;
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
    s.allow_tidying();
    s.create(0, 1, 0);
    records(&mut s, &mut disk, 0, 1);
    assert!(s.spare_job().is_none(), "the first file is not half full");
    records(&mut s, &mut disk, 1, 2);
    let job = s.spare_job().expect("half full");
    assert!(s.spare_job().is_none(), "one at a time");
    let seq = job.seq;
    s.spare_done(seq, run_job(&mut disk, job));
    let made = s.stats();
    // Each record takes two blocks: three fill a file after its header.
    records(&mut s, &mut disk, 3, 3);
    let st = s.stats();
    assert_eq!(st.files_made, made.files_made + 1);
    assert_eq!(st.files_made_waiting, made.files_made_waiting, "nobody waited for it");
    // A job still running when a record needs its file: the record makes the next one, and the
    // late file, once made, holds nothing.
    let late = s.spare_job().expect("half full again");
    records(&mut s, &mut disk, 6, 1);
    assert_eq!(s.stats().files_made_waiting, st.files_made_waiting + 1);
    let late_seq = late.seq;
    s.spare_done(late_seq, run_job(&mut disk, late));
    // A job that fails: the record makes its file.
    records(&mut s, &mut disk, 7, 2);
    let failed = s.spare_job().expect("half full again");
    s.spare_done(failed.seq, Err(IoErrno::Io));
    assert_eq!(s.failed(), None);
    records(&mut s, &mut disk, 9, 1);
    assert_eq!(s.stats().files_made_waiting, st.files_made_waiting + 2);
    let mut again = LogState::recover(&mut disk, file_bytes).unwrap();
    let got = again.read(&mut disk, 0, 1).unwrap();
    assert_eq!(got.len(), 10 * 5000);
    for i in 0..10 {
        assert_eq!(got[i * 5000..(i + 1) * 5000], varied(5000)[..], "record {i}");
    }
    // The empty file goes with the old ones once the segment does.
    again.allow_tidying();
    again.remove(0, 1);
    again.create(0, 2, 0);
    records(&mut again, &mut disk, 0, 1);
    let left = files(&mut disk);
    assert!(
        !left.iter().any(|p| p == &teseladb_devlog::format::segment_path(late_seq)),
        "{left:?}"
    );
}

/// A disk that notes which blocks of the files it made were written, and counts the writes of
/// anything but zeros that landed on a block nothing had written.
struct Written {
    disk: MemDisk,
    blocks: BTreeMap<FileId, Vec<bool>>,
    on_fresh: usize,
}

impl Disk for Written {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        self.disk.list(dir)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        self.disk.read_all(path)
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        let block = teseladb_devlog::format::BLOCK;
        match &op {
            IoOp::Create { file, prealloc, .. } => {
                self.blocks.insert(*file, vec![false; (*prealloc / block) as usize]);
            }
            IoOp::WriteAt { file, offset, data, .. } => {
                if let Some(blocks) = self.blocks.get_mut(file) {
                    let first = (*offset / block) as usize;
                    let end = (*offset + data.len() as u64).div_ceil(block) as usize;
                    let fresh = (first..end).any(|b| blocks.get(b) != Some(&true));
                    if fresh && data.iter().any(|b| *b != 0) {
                        self.on_fresh += 1;
                    }
                    for b in blocks.iter_mut().take(end).skip(first) {
                        *b = true;
                    }
                }
            }
            _ => {}
        }
        self.disk.execute(op)
    }
}

/// The space of a file of the log is written before its header and before any record lands in
/// it (FR-035): the first file, the one made ahead in its job and the one a record makes for
/// itself when none was made ahead. Space reserved and never written costs every durable write
/// that lands in it a conversion of its extent, which the file system commits in its journal:
/// 280 µs against 54 µs for 4 KiB on the disk of the loads of 002 (19-11-2026).
#[test]
fn the_space_of_a_file_is_written_before_its_header_and_its_records() {
    let file_bytes = 32 << 10;
    let mut disk = Written { disk: MemDisk::new(), blocks: BTreeMap::new(), on_fresh: 0 };
    let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
    s.allow_tidying();
    s.create(0, 1, 0);
    // The first file, which its first record makes.
    records(&mut s, &mut disk, 0, 3);
    // The next one, made ahead.
    let job = s.spare_job().expect("half full");
    let seq = job.seq;
    let made = run_job(&mut disk, job);
    s.spare_done(seq, made);
    records(&mut s, &mut disk, 3, 3);
    // And one that a record makes for itself: the job of the file made ahead did not run.
    let late = s.spare_job().expect("half full again");
    records(&mut s, &mut disk, 6, 3);
    s.spare_done(late.seq, Err(IoErrno::Io));
    assert_eq!(disk.blocks.len(), 3, "files made");
    assert_eq!(disk.on_fresh, 0, "a header or a record landed on space nothing had written");
    for (file, blocks) in &disk.blocks {
        assert!(blocks.iter().all(|w| *w), "{file:?}: {blocks:?}");
    }
    // What the records hold comes back.
    let mut again = LogState::recover(&mut disk, file_bytes).unwrap();
    let got = again.read(&mut disk, 0, 1).unwrap();
    assert_eq!(got.len(), 9 * 5000);
}

/// A log without an active file, as every log that just started, makes its first file ahead:
/// the first record does not wait for the space of its file to be written. A job that failed is
/// not tried again until a record made its own file, so that a disk that fails is not asked over
/// and over while nothing writes.
#[test]
fn the_first_file_is_made_ahead_and_a_failed_job_waits_for_a_record() {
    let file_bytes = 32 << 10;
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
    let job = s.spare_job().expect("no active file");
    assert!(s.spare_job().is_none(), "one at a time");
    let seq = job.seq;
    let made = run_job(&mut disk, job);
    s.spare_done(seq, made);
    s.create(0, 1, 0);
    records(&mut s, &mut disk, 0, 1);
    let st = s.stats();
    assert_eq!((st.files_made, st.files_made_waiting), (1, 0), "nobody waited for the file");
    // After a restart there is no active file again; its job fails, and no other is handed out
    // until a record has made its file.
    let mut again = LogState::recover(&mut disk, file_bytes).unwrap();
    let failed = again.spare_job().expect("no active file");
    again.spare_done(failed.seq, Err(IoErrno::Io));
    assert!(again.spare_job().is_none(), "a failed job is not tried again at once");
    records(&mut again, &mut disk, 1, 3);
    assert_eq!(again.stats().files_made_waiting, 1);
    assert!(again.spare_job().is_some(), "half full, after a record made its file");
}

/// A crash at any operation, the file made ahead included, keeps every answered write.
#[test]
fn a_crash_while_a_file_is_made_ahead_keeps_every_answered_write() {
    let file_bytes = 16 << 10;
    let run = |disk: &mut dyn Disk, acked: &mut Vec<u8>| {
        let Ok(mut s) = LogState::recover(disk, file_bytes) else { return };
        s.allow_tidying();
        s.create(0, 1, 0);
        for i in 0..12u64 {
            let data = varied(3000);
            s.write(0, 1, i * 3000, &data);
            s.sync(0, sync_op(i));
            let answers = s.flush(disk);
            if answers.is_empty() || answers.iter().any(|(_, d)| !matches!(d, IoDone::Ok(_))) {
                return;
            }
            acked.extend_from_slice(&data);
            if let Some(job) = s.spare_job() {
                let seq = job.seq;
                let made = run_job(disk, job);
                s.spare_done(seq, made);
            }
        }
    };
    let mut counter = Cut { disk: sim(), left: usize::MAX };
    run(&mut counter, &mut Vec::new());
    let total = usize::MAX - counter.left;
    for cut in 0..total {
        for seed in 0..3u64 {
            let mut acked = Vec::new();
            let mut disk = Cut { disk: sim(), left: cut };
            run(&mut disk, &mut acked);
            disk.disk.crash(&mut Rng::new(seed));
            let mut disk = disk.disk;
            let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
            if acked.is_empty() {
                continue;
            }
            let got = s.read(&mut disk, 0, 1).unwrap();
            assert_eq!(&got[..acked.len()], &acked[..], "cut {cut} seed {seed}");
            s.allow_tidying();
            s.write(0, 1, got.len() as u64, b"after");
            s.sync(0, sync_op(99));
            assert!(matches!(s.flush(&mut disk)[0].1, IoDone::Ok(_)), "cut {cut}");
        }
    }
}

/// Runs the reads of a [`CopyJob`] on `disk`: what they read at each offset.
fn run_copy(
    disk: &mut dyn Disk,
    job: CopyJob,
) -> Result<Vec<(u64, teseladb_types::Bytes)>, IoErrno> {
    let mut out = Vec::new();
    for op in job.reads {
        let IoOp::ReadAt { offset, .. } = op else { panic!("a copy only reads") };
        match disk.execute(op) {
            IoDone::Read { data: Ok(d), .. } => out.push((offset, d)),
            IoDone::Read { data: Err(e), .. } | IoDone::Err { errno: e, .. } => return Err(e),
            IoDone::Ok(_) => panic!("a read gives its data"),
        }
    }
    Ok(out)
}

/// A sync of `core` driven as the node's writer drives it, with `next` and the results of its
/// operations: no copy forward runs inside it.
fn pipeline_sync(s: &mut LogState, disk: &mut dyn Disk, core: u16, n: u64) {
    s.sync(core, sync_op(n));
    let mut answers = Vec::new();
    let mut ops = Vec::new();
    loop {
        answers.extend(s.next(disk, &mut ops));
        if ops.is_empty() {
            break;
        }
        for op in ops.drain(..) {
            answers.extend(s.on_done(&disk.execute(op)));
        }
    }
    assert!(answers.iter().all(|(_, d)| matches!(d, IoDone::Ok(_))), "{answers:?}");
}

/// A copy forward reads the pieces the oldest file still holds in few long reads, apart from the
/// writer; what the cores write, cut or remove while those reads run wins over the copy.
#[test]
fn a_copy_forward_reads_apart_and_what_comes_meanwhile_wins() {
    let file_bytes = 32 << 10;
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
    s.allow_tidying();
    let mut model = Model::new();
    // Core 1: twenty small pieces of segment 1 and a padded batch of segment 2, in one record of
    // the first file.
    s.create(1, 1, 0);
    s.create(1, 2, 0);
    model.insert((1, 1), Vec::new());
    for i in 0..20u64 {
        let piece: Vec<u8> = (0..100).map(|b| (b + i) as u8).collect();
        s.write(1, 1, i * 100, &piece);
        put(&mut model, (1, 1), i * 100, &piece);
    }
    s.write(1, 2, 0, &batch(b"padded"));
    model.insert((1, 2), batch(b"padded"));
    pipeline_sync(&mut s, &mut disk, 1, 0);
    // Core 0 writes until the oldest file is due for a copy.
    let block = varied(8000);
    let mut job = None;
    let mut last = 0;
    for w in 1..40u64 {
        s.create(0, w, 0);
        s.write(0, w, 0, &block);
        if w > 1 {
            s.remove(0, w - 1);
        }
        pipeline_sync(&mut s, &mut disk, 0, w);
        job = s.copy_job();
        if job.is_some() {
            model.insert((0, w), block.clone());
            last = w;
            break;
        }
    }
    let job = job.expect("the oldest file is due for a copy");
    assert_eq!(job.reads.len(), 1, "{:?}", job.reads);
    assert!(s.copy_job().is_none(), "one copy at a time");
    // Meanwhile core 1 overwrites a piece, cuts the segment in the middle of another and removes
    // its second segment.
    s.write(1, 1, 300, b"newer");
    put(&mut model, (1, 1), 300, b"newer");
    s.truncate(1, 1, 1250);
    model.get_mut(&(1, 1)).unwrap().truncate(1250);
    s.remove(1, 2);
    model.remove(&(1, 2));
    pipeline_sync(&mut s, &mut disk, 1, 50);
    let file = job.file;
    let read = run_copy(&mut disk, job);
    assert!(s.copy_done(file, read).is_empty());
    pipeline_sync(&mut s, &mut disk, 1, 51);
    assert!(s.stats().copied_bytes > 0 && s.stats().copied_bytes < 2000, "{:?}", s.stats());
    // The copied file goes, and everything reads as the cores left it.
    for k in 1..5u64 {
        s.write(0, last, 8000 * k, &block);
        put(&mut model, (0, last), 8000 * k, &block);
        pipeline_sync(&mut s, &mut disk, 0, 100 + k);
    }
    let mut again = LogState::recover(&mut disk, file_bytes).unwrap();
    let w0: Vec<u64> = model.keys().filter(|k| k.0 == 0).map(|k| k.1).collect();
    assert_eq!(again.segments_of(0), w0);
    for (key, want) in &model {
        assert_eq!(&again.read(&mut disk, key.0, key.1).unwrap(), want, "{key:?}");
    }
    assert!(!files(&mut disk).iter().any(|p| p == &teseladb_devlog::format::segment_path(file)));
}

/// Under a write load the log takes at most about twice what its cores still hold, plus the files
/// it always keeps: a core that held its WAL for a while left the oldest files more than half
/// live, and those, never copied forward, kept every newer file behind them, however empty (the
/// one-node load kept 6.6 GiB of log for 2 GiB held).
#[test]
fn the_log_stays_within_twice_what_is_held_under_a_write_load() {
    let file_bytes = 32 << 10;
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
    s.allow_tidying();
    let mut model = Model::new();
    let mut rng = Rng::new(7);
    // Core 1 keeps its one segment: what it wrote stays live. First it writes most of what the
    // log takes, so that its oldest files stay more than half live.
    s.create(1, 1, 0);
    model.insert((1, 1), Vec::new());
    let mut held = 0u64;
    let block = varied(3000);
    let mut most = 0usize;
    for w in 1..400u64 {
        s.create(0, w, 0);
        s.write(0, w, 0, &block);
        if w > 1 {
            s.remove(0, w - 1);
        }
        // Core 1 writes three times what core 0 does for a while, then a twentieth of it.
        let len = if w < 40 { 9000 } else { 150 };
        let data: Vec<u8> = (0..len).map(|_| rng.range(1, 250) as u8).collect();
        s.write(1, 1, held, &data);
        put(&mut model, (1, 1), held, &data);
        held += len as u64;
        s.sync(0, sync_op(w));
        assert!(s.flush(&mut disk).iter().all(|(_, d)| matches!(d, IoDone::Ok(_))));
        let bound = 2 * held + (KEPT_FILES as u64 + 3) * file_bytes;
        let taken = files(&mut disk).len() as u64 * file_bytes;
        most = most.max(files(&mut disk).len());
        assert!(taken <= bound, "step {w}: {taken} bytes of log for {held} held");
    }
    model.insert((0, 399), block.clone());
    assert!(most > KEPT_FILES, "the log grew past the files it keeps: {most}");
    let mut again = LogState::recover(&mut disk, file_bytes).unwrap();
    for (key, want) in &model {
        assert_eq!(&again.read(&mut disk, key.0, key.1).unwrap(), want, "{key:?}");
    }
}

/// A read of a copy forward that fails stops the log, as the read did when it ran in the writer.
#[test]
fn a_failed_read_of_a_copy_forward_stops_the_log() {
    let file_bytes = 32 << 10;
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, file_bytes).unwrap();
    s.allow_tidying();
    s.create(1, 1, 0);
    s.write(1, 1, 0, b"held");
    pipeline_sync(&mut s, &mut disk, 1, 0);
    let block = varied(8000);
    let job = (1..40u64)
        .find_map(|w| {
            s.create(0, w, 0);
            s.write(0, w, 0, &block);
            if w > 1 {
                s.remove(0, w - 1);
            }
            pipeline_sync(&mut s, &mut disk, 0, w);
            s.copy_job()
        })
        .expect("due");
    s.sync(0, sync_op(77));
    let answers = s.copy_done(job.file, Err(IoErrno::Io));
    assert!(matches!(answers[..], [(0, IoDone::Err { errno: IoErrno::Io, .. })]), "{answers:?}");
    assert_eq!(s.failed(), Some(IoErrno::Io));
}

/// Placing a write looks at the extents it overlaps and at no other: a segment that thousands of
/// writes filled takes the next one as an empty segment does. Before, each write walked every
/// extent of its segment from the first one, so a segment cost the square of its writes to fill
/// (002 run `c0n3p`: 4 % of a node's CPU, on the shard that writes the log, half-way through a
/// segment), and the log got slower write after write until the next segment.
#[test]
fn a_write_looks_at_the_extents_it_overlaps_and_at_no_other() {
    const WRITES: u64 = 2_000;
    let mut disk = MemDisk::new();
    let mut s = LogState::recover(&mut disk, 16 << 20).unwrap();
    let mut model = Model::new();
    s.create(0, 1, 0);
    let data = varied(100);
    for i in 0..WRITES {
        s.write(0, 1, i * 100, &data);
        put(&mut model, (0, 1), i * 100, &data);
    }
    s.sync(0, sync_op(1));
    s.flush(&mut disk);
    let appended = s.extents_examined();
    assert!(appended <= WRITES, "{appended} extents examined by {WRITES} writes at the end");
    // A write over the middle of others cuts those it covers, wherever they are.
    let over = vec![9u8; 250];
    s.write(0, 1, 100_050, &over);
    put(&mut model, (0, 1), 100_050, &over);
    s.truncate(0, 1, 150_020);
    model.get_mut(&(0, 1)).unwrap().truncate(150_020);
    s.sync(0, sync_op(2));
    s.flush(&mut disk);
    // The three extents the write covers and the one before its start, and those the truncation
    // takes: the one it cuts and the 499 past it.
    let examined = s.extents_examined() - appended;
    assert!(examined <= 4 + 501, "{examined} extents examined");
    check(&mut s, &mut disk, &model);
    let mut again = LogState::recover(&mut disk, 16 << 20).unwrap();
    check(&mut again, &mut disk, &model);
}
