//! The device log's writer in a pipeline (012 FR-035) on a slow simulated disk: a write runs while
//! a sync runs, a sync starts once the one before and the writes before it ended, and results come
//! back in the order the operations were handed out. A sync asked while another runs goes in the
//! next one, each core learns its syncs durable in the order it asked them, and a crash at any
//! operation keeps every answered write.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use teseladb_devlog::state::LogState;
use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp, SimDisk, SimDiskConfig};
use teseladb_types::Rng;

/// How long a write of the slow disk takes.
const WRITE_NS: u64 = 100_000;
/// How long a sync of the slow disk takes.
const SYNC_NS: u64 = 2_000_000;
const FILE_BYTES: u64 = 16 << 10;

/// A disk whose operations fail after `left` of them: the machine stopped there.
struct Cut {
    disk: SimDisk,
    left: usize,
}

impl Cut {
    fn execute(&mut self, op: IoOp) -> IoDone {
        if self.left == 0 {
            return IoDone::Err { op: op.without_data(), errno: IoErrno::Io };
        }
        self.left -= 1;
        self.disk.execute(op)
    }
}

/// A slow disk in virtual time. Operations handed out run on `disk` when their result comes
/// back; a synchronous operation first waits for every one handed out before it, as an
/// executor that answers in order does.
struct Slow {
    disk: Cut,
    now: u64,
    queue: VecDeque<(IoOp, u64)>,
    /// When the last sync ends.
    sync_end: u64,
    /// When the last syncs handed out together were handed out and when they start: they run
    /// at once, as the executor runs the syncs of several files.
    sync_batch: (u64, u64),
    /// When the last write ends.
    write_end: u64,
    /// When the last result comes back.
    last: u64,
    /// Results back, for the log's `on_done`.
    done: Vec<IoDone>,
}

impl Slow {
    fn new(disk: Cut) -> Slow {
        Slow {
            disk,
            now: 0,
            queue: VecDeque::new(),
            sync_end: 0,
            sync_batch: (u64::MAX, 0),
            write_end: 0,
            last: 0,
            done: Vec::new(),
        }
    }

    fn submit(&mut self, op: IoOp) {
        let end = match op {
            IoOp::Sync { .. } => {
                let start = if self.sync_batch.0 == self.now {
                    self.sync_batch.1
                } else {
                    self.now.max(self.sync_end).max(self.write_end)
                };
                self.sync_batch = (self.now, start);
                self.sync_end = self.sync_end.max(start + SYNC_NS);
                start + SYNC_NS
            }
            _ => {
                self.write_end = self.write_end.max(self.now + WRITE_NS);
                self.now + WRITE_NS
            }
        };
        self.last = self.last.max(end);
        self.queue.push_back((op, self.last));
    }

    fn next_result(&self) -> Option<u64> {
        self.queue.front().map(|(_, at)| *at)
    }

    /// Runs what comes back by `at`.
    fn deliver(&mut self, at: u64) {
        while let Some((op, when)) = self.queue.front().cloned() {
            if when > at {
                break;
            }
            self.queue.pop_front();
            self.now = self.now.max(when);
            let d = self.disk.execute(op);
            self.done.push(d);
        }
    }
}

impl Disk for Slow {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        self.deliver(u64::MAX);
        self.disk.disk.list(dir)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        self.deliver(u64::MAX);
        self.disk.disk.read_all(path)
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        self.deliver(u64::MAX);
        self.disk.execute(op)
    }
}

/// A write of `data` to the end of a core's segment followed by a sync, at `at`. With `rotate`
/// the core first starts a new segment and removes the one before.
struct Arrival {
    at: u64,
    core: u16,
    data: Vec<u8>,
    rotate: bool,
}

/// Where each arrival writes: its core's segment and the offset in it.
fn places(arrivals: &[Arrival]) -> Vec<(u64, u64)> {
    let mut at: BTreeMap<u16, (u64, u64)> = BTreeMap::new();
    arrivals
        .iter()
        .map(|a| {
            let p = at.entry(a.core).or_insert((1, 0));
            if a.rotate && p.1 > 0 {
                *p = (p.0 + 1, 0);
            }
            let place = *p;
            p.1 += a.data.len() as u64;
            place
        })
        .collect()
}

/// An answer: the core, the number of its sync, when, and the log's syncs by then.
type Answer = (u16, u64, u64, u64, bool);

/// Runs the arrivals through the log's pipeline on `slow`; stops at the first failed answer.
fn drive(state: &mut LogState, slow: &mut Slow, arrivals: &[Arrival]) -> Vec<Answer> {
    let mut answers = Vec::new();
    let places = places(arrivals);
    let mut current: BTreeMap<u16, u64> = BTreeMap::new();
    let mut i = 0;
    let mut ops = Vec::new();
    loop {
        let next_arrival = arrivals.get(i).map(|a| a.at);
        let t = match (next_arrival, slow.next_result()) {
            (None, None) => break,
            (a, r) => a.unwrap_or(u64::MAX).min(r.unwrap_or(u64::MAX)),
        };
        slow.now = slow.now.max(t);
        slow.deliver(t);
        let mut got = Vec::new();
        // What comes back while the log takes its next step (a new file waits for the operations
        // before it), and what the cores asked meanwhile, is taken before the step after it.
        loop {
            while let Some(a) = arrivals.get(i).filter(|a| a.at <= slow.now) {
                let (wal, offset) = places[i];
                match current.insert(a.core, wal) {
                    Some(old) if old == wal => {}
                    old => {
                        state.create(a.core, wal, 0);
                        if let Some(old) = old {
                            state.remove(a.core, old);
                        }
                    }
                }
                state.write(a.core, wal, offset, &a.data);
                state.sync(a.core, IoOp::Sync { file: FileId(1), batch: i as u64 });
                i += 1;
            }
            for d in std::mem::take(&mut slow.done) {
                got.extend(state.on_done(&d).into_iter().map(|a| (slow.now, state.syncs(), a)));
            }
            let ready = state.next(slow, &mut ops);
            got.extend(ready.into_iter().map(|a| (slow.now, state.syncs(), a)));
            for op in ops.drain(..) {
                slow.submit(op);
            }
            if slow.done.is_empty() && arrivals.get(i).is_none_or(|a| a.at > slow.now) {
                break;
            }
        }
        for (at, syncs, (core, d)) in got {
            let (op, ok) = match d {
                IoDone::Ok(op) => (op, true),
                IoDone::Err { op, .. } => (op, false),
                IoDone::Read { .. } => unreachable!("a sync's answer"),
            };
            let IoOp::Sync { batch, .. } = op else { unreachable!("a sync's answer") };
            answers.push((core, batch, at, syncs, ok));
        }
        if answers.iter().any(|a| !a.4) {
            break;
        }
    }
    answers
}

fn sim() -> SimDisk {
    SimDisk::new(SimDiskConfig {
        torn_write_probability: 0.5,
        unsynced_create_probability: 0.5,
        ..SimDiskConfig::default()
    })
}

#[test]
fn a_sync_asked_while_another_runs_goes_in_the_next_one() {
    let mut slow = Slow::new(Cut { disk: sim(), left: usize::MAX });
    let mut state = LogState::recover(&mut slow, 1 << 20).unwrap();
    state.allow_tidying();
    let half = WRITE_NS + SYNC_NS / 2;
    let arrivals = [
        Arrival { at: 0, core: 0, data: vec![1; 300], rotate: false },
        Arrival { at: half, core: 1, data: vec![2; 300], rotate: false },
    ];
    let answers = drive(&mut state, &mut slow, &arrivals);
    assert_eq!(answers.len(), 2, "{answers:?}");
    let (core, _, first_at, first_syncs, _) = answers[0];
    assert_eq!((core, first_at, first_syncs), (0, WRITE_NS + SYNC_NS, 1));
    let (core, _, at, syncs, ok) = answers[1];
    assert!(ok);
    assert_eq!(core, 1);
    // Its record was written while the first sync ran, and the second sync started as soon as
    // the first one ended: it waited for what was left of the first and for its own sync.
    assert_eq!(syncs, 2, "the sync right after the one running");
    let left_of_running = first_at - half;
    assert!(at - half <= left_of_running + WRITE_NS + SYNC_NS, "{answers:?}");
    assert_eq!(at, first_at + SYNC_NS);
}

#[test]
fn a_sync_that_comes_while_its_writes_run_waits_for_them() {
    // The record of the second core is written as the first sync ends: its sync covers it.
    let mut slow = Slow::new(Cut { disk: sim(), left: usize::MAX });
    let mut state = LogState::recover(&mut slow, 1 << 20).unwrap();
    let arrivals = [
        Arrival { at: 0, core: 0, data: vec![1; 300], rotate: false },
        Arrival { at: WRITE_NS + SYNC_NS - 1, core: 1, data: vec![2; 300], rotate: false },
    ];
    let answers = drive(&mut state, &mut slow, &arrivals);
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[1].3, 2);
    let mut disk = slow.disk.disk;
    let mut again = LogState::recover(&mut disk, 1 << 20).unwrap();
    assert_eq!(again.read(&mut disk, 1, 1).unwrap(), vec![2; 300]);
}

/// Arrivals of three cores at random instants, a few per sync.
fn arrivals(rng: &mut Rng, n: usize) -> Vec<Arrival> {
    let mut at = 0;
    (0..n)
        .map(|_| {
            at += rng.range(0, SYNC_NS / 3);
            let mut data = vec![0u8; rng.range(1, 3000) as usize];
            rng.fill(&mut data);
            Arrival { at, core: rng.range(0, 3) as u16, data, rotate: rng.range(0, 2) == 0 }
        })
        .collect()
}

#[test]
fn each_core_learns_its_syncs_durable_in_the_order_it_asked_them() {
    for seed in 0..20 {
        let mut rng = Rng::new(seed);
        let arrivals = arrivals(&mut rng, 120);
        let mut slow = Slow::new(Cut { disk: sim(), left: usize::MAX });
        // Files that a run does not fill: a new file waits for the operations before it.
        let mut state = LogState::recover(&mut slow, 4 << 20).unwrap();
        state.allow_tidying();
        let answers = drive(&mut state, &mut slow, &arrivals);
        assert_eq!(answers.len(), arrivals.len(), "seed {seed}");
        for core in 0..3u16 {
            let asked: Vec<u64> =
                (0..arrivals.len() as u64).filter(|i| arrivals[*i as usize].core == core).collect();
            let got: Vec<u64> = answers.iter().filter(|a| a.0 == core).map(|a| a.1).collect();
            assert_eq!(got, asked, "seed {seed} core {core}");
        }
        // No sync waits more than the one running when it came plus its write and its own sync.
        for (core, batch, at, _, _) in &answers {
            let came = arrivals[*batch as usize].at;
            assert!(at - came <= 2 * SYNC_NS + 2 * WRITE_NS, "seed {seed} core {core} {batch}");
        }
    }
}

#[test]
fn a_crash_at_any_operation_of_the_pipeline_keeps_every_answered_write() {
    let arrivals = arrivals(&mut Rng::new(7), 80);
    let places = places(&arrivals);
    let mut whole = Slow::new(Cut { disk: sim(), left: usize::MAX });
    let mut state = LogState::recover(&mut whole, FILE_BYTES).unwrap();
    state.allow_tidying();
    drive(&mut state, &mut whole, &arrivals);
    let total = usize::MAX - whole.disk.left;
    assert!(total > 60, "{total}");
    for cut in 0..total {
        for seed in 0..2u64 {
            let mut slow = Slow::new(Cut { disk: sim(), left: usize::MAX });
            let mut state = LogState::recover(&mut slow, FILE_BYTES).unwrap();
            state.allow_tidying();
            slow.disk.left = cut;
            let answers = drive(&mut state, &mut slow, &arrivals);
            let mut disk = slow.disk.disk;
            disk.crash(&mut Rng::new(seed));
            let mut again = LogState::recover(&mut disk, FILE_BYTES).unwrap();
            for core in 0..3u16 {
                // What the core wrote in its segment up to its last answered sync.
                let Some(last) = answers.iter().filter(|a| a.0 == core && a.4).map(|a| a.1).max()
                else {
                    continue;
                };
                let last = last as usize;
                let wal = places[last].0;
                let want: Vec<u8> = (0..=last)
                    .filter(|i| arrivals[*i].core == core && places[*i].0 == wal)
                    .flat_map(|i| arrivals[i].data.iter().copied())
                    .collect();
                // A segment the log still has reads whole; one it lost was removed later.
                let kept = again.segments_of(core).contains(&wal);
                match again.read(&mut disk, core, wal).map_err(|e| (e, kept)) {
                    Ok(got) => {
                        assert!(got.len() >= want.len(), "cut {cut} seed {seed} core {core}");
                        assert_eq!(&got[..want.len()], &want[..], "cut {cut} {seed} {core}");
                    }
                    // A later arrival of the core removed it, and the removal may have reached
                    // the disk.
                    Err((e, true)) => panic!("cut {cut} seed {seed} core {core}: {e:?}"),
                    Err((_, false)) => assert!(
                        (last + 1..arrivals.len())
                            .any(|i| arrivals[i].core == core && places[i].0 > wal),
                        "cut {cut} seed {seed} core {core}: segment {wal} lost"
                    ),
                }
            }
        }
    }
}

/// One group: `next` and its operations on `disk`, until nothing is left; the answers.
fn settle(state: &mut LogState, disk: &mut SimDisk) -> Vec<(u16, IoDone)> {
    let mut answers = Vec::new();
    let mut ops = Vec::new();
    loop {
        answers.extend(state.next(disk, &mut ops));
        if ops.is_empty() {
            return answers;
        }
        for op in ops.drain(..) {
            let done = disk.execute(op);
            answers.extend(state.on_done(&done));
        }
    }
}

fn synced(state: &mut LogState, disk: &mut SimDisk, core: u16) {
    state.sync(core, IoOp::Sync { file: FileId(1), batch: 0 });
    let answers = settle(state, disk);
    assert!(answers.iter().all(|(c, d)| *c == core && matches!(d, IoDone::Ok(_))), "{answers:?}");
}

#[test]
fn a_file_freed_by_a_rewrite_written_behind_a_sync_stays_until_that_rewrite_is_durable() {
    let file_bytes = 8 << 10;
    // A crash loses every unsynced write whole, and every name not synced.
    let cfg = SimDiskConfig {
        torn_write_probability: 0.0,
        unsynced_create_probability: 0.0,
        ..SimDiskConfig::default()
    };
    let mut disk = SimDisk::new(cfg);
    let mut state = LogState::recover(&mut disk, file_bytes).unwrap();
    state.allow_tidying();
    // File 1: core 0's segment and its first bytes. File 2: the rest of them, more than half a
    // file, so that the log never copies it forward. Then files of core 1.
    state.create(0, 1, 0);
    state.write(0, 1, 0, &[1; 10]);
    synced(&mut state, &mut disk, 0);
    state.write(0, 1, 10, &[2; 5000]);
    synced(&mut state, &mut disk, 0);
    state.create(1, 1, 0);
    for i in 0..5u64 {
        state.write(1, 1, i * 3000, &[3; 3000]);
        synced(&mut state, &mut disk, 1);
    }
    // The log copied what file 1 still held forward and removed it: file 2, which only holds
    // bytes of core 0's segment, is the oldest one.
    let files = disk.list(Path::new("raftlog")).unwrap();
    assert!(!files.iter().any(|p| p.ends_with("0000000000000001.log")), "{files:?}");
    let oldest = files.iter().min().unwrap().clone();
    assert!(oldest.ends_with("0000000000000002.log"), "{files:?}");
    // A sync of core 1 runs; meanwhile core 0 writes the same bytes of its segment again, in a
    // record written behind that sync, which lets go of the last bytes of file 2.
    state.write(1, 1, 15_000, &[4; 100]);
    state.sync(1, IoOp::Sync { file: FileId(1), batch: 1 });
    let mut ops = Vec::new();
    assert!(state.next(&mut disk, &mut ops).is_empty());
    let (syncs, writes): (Vec<IoOp>, Vec<IoOp>) =
        ops.drain(..).partition(|op| matches!(op, IoOp::Sync { .. }));
    assert!(!syncs.is_empty());
    for op in writes {
        assert!(state.on_done(&disk.execute(op)).is_empty());
    }
    state.write(0, 1, 10, &[5; 5000]);
    state.sync(0, IoOp::Sync { file: FileId(1), batch: 2 });
    assert!(state.next(&mut disk, &mut ops).is_empty());
    assert!(ops.iter().all(|op| !matches!(op, IoOp::Sync { .. })), "written behind the sync");
    for op in ops.drain(..) {
        assert!(state.on_done(&disk.execute(op)).is_empty());
    }
    let mut answers = Vec::new();
    for op in syncs {
        answers.extend(state.on_done(&disk.execute(op)));
    }
    assert_eq!(answers.len(), 1, "core 1's sync ended");
    // The next step starts the rewrite's sync; file 2 stays: the record that let go of it is not
    // durable yet.
    assert!(state.next(&mut disk, &mut ops).is_empty());
    assert!(disk.list(Path::new("raftlog")).unwrap().contains(&oldest), "removed too soon");
    // The machine stops before that sync ends: core 0's segment holds what its answered sync
    // made durable.
    disk.crash(&mut Rng::new(1));
    let mut again = LogState::recover(&mut disk, file_bytes).unwrap();
    let want: Vec<u8> = [vec![1; 10], vec![2; 5000]].concat();
    assert_eq!(again.read(&mut disk, 0, 1).unwrap(), want);
}
