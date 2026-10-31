//! Support shared by the LSM's tests: a small configuration that flushes and compacts often,
//! reads that run the I/O they wait for, and the random operations every story uses against the
//! model (004 T002).

#![allow(dead_code)]

pub mod model;

use teseladb_keys::Hlc;
use teseladb_lsm::{Commit, Kind, Lookup, Lsm, LsmConfig, Step, View};
use teseladb_raftlog::Disk;
use teseladb_types::{GroupId, Nanos, Rng};

use model::{Model, Row};

/// A timestamp.
pub fn ts(physical: u64, logical: u32) -> Hlc {
    Hlc { physical, logical }
}

/// Small sizes: 4 KiB blocks, 32 KiB memtables, 64 KiB files.
pub fn small() -> LsmConfig {
    LsmConfig {
        dir: "core-00/lsm".into(),
        memtable_bytes: 32 << 10,
        block_bytes: 4096,
        target_file_bytes: 64 << 10,
        l1_bytes: 256 << 10,
        cache_bytes: 256 << 10,
        ..LsmConfig::default()
    }
}

/// Opens (or reopens) an LSM on `disk`.
pub fn open(cfg: &LsmConfig, disk: &mut dyn Disk) -> Lsm {
    Lsm::recover(cfg, disk).expect("recovery").0
}

/// Most I/O rounds a read may wait for before the test calls it stuck.
const MAX_ROUNDS: usize = 1000;

/// `get` at `ts`, running the I/O it waits for.
pub fn get(lsm: &mut Lsm, disk: &mut dyn Disk, view: &View, key: &[u8], at: Hlc) -> Option<Row> {
    for _ in 0..MAX_ROUNDS {
        match view.get(lsm, key, at) {
            Lookup::Hit(f) => return Some((key.to_vec(), f.ts, f.kind, f.value.to_vec())),
            Lookup::NotFound => return None,
            Lookup::Failed(e) => panic!("get failed: {e}"),
            Lookup::Miss(_) => {
                lsm.run_io(disk, Nanos::ZERO);
            }
        }
    }
    panic!("get never finished");
}

/// A whole scan of `[start, end)` at `ts`, running the I/O it waits for.
pub fn scan(
    lsm: &mut Lsm,
    disk: &mut dyn Disk,
    view: &View,
    start: &[u8],
    end: &[u8],
    at: Hlc,
) -> Vec<Row> {
    let mut out = Vec::new();
    let mut s = view.scan(start, end, at);
    let mut stuck = 0;
    loop {
        match s.next(lsm) {
            Step::Item(i) => {
                stuck = 0;
                out.push((i.key, i.ts, i.kind, i.value.to_vec()));
            }
            Step::Miss(_) => {
                stuck += 1;
                assert!(stuck < MAX_ROUNDS, "scan never finished");
                lsm.run_io(disk, Nanos::ZERO);
            }
            Step::Done => return out,
            Step::Failed(e) => panic!("scan failed: {e}"),
        }
    }
}

/// An operation of the random histories.
#[derive(Clone, Debug)]
pub enum Op {
    /// A version.
    Write { key: Vec<u8>, ts: Hlc, kind: Kind, value: Vec<u8> },
    /// Resolution of a pending intent.
    Resolve { key: Vec<u8>, intent_ts: Hlc, commit: Option<(Hlc, Kind, Vec<u8>)> },
    /// Range deletion.
    DeleteRange { start: Vec<u8>, end: Vec<u8>, ts: Hlc },
    /// Point read.
    Get { key: Vec<u8>, ts: Hlc },
    /// Range read.
    Scan { start: Vec<u8>, end: Vec<u8>, ts: Hlc },
    /// Freezes the memtable.
    Flush,
    /// Runs the pending I/O.
    Io,
}

/// Random operations over `keys` logical keys, with timestamps that mostly grow.
pub struct Gen {
    rng: Rng,
    clock: u64,
    keys: u64,
}

impl Gen {
    /// A generator.
    pub fn new(seed: u64, keys: u64) -> Gen {
        Gen { rng: Rng::new(seed), clock: 100, keys }
    }

    fn key(&mut self) -> Vec<u8> {
        format!("k{:03}", self.rng.range(0, self.keys)).into_bytes()
    }

    fn stamp(&mut self) -> Hlc {
        self.clock += self.rng.range(0, 3);
        let physical =
            if self.rng.chance(0.2) { self.clock - self.rng.range(0, 60) } else { self.clock };
        ts(physical, self.rng.range(0, 3) as u32)
    }

    fn value(&mut self) -> Vec<u8> {
        let len = if self.rng.chance(0.02) {
            self.rng.range(5000, 12_000)
        } else {
            self.rng.range(0, 64)
        };
        let mut v = vec![0; len as usize];
        self.rng.fill(&mut v);
        v
    }

    /// The next operation; resolutions pick one of the model's pending intents.
    pub fn next(&mut self, model: &Model) -> Op {
        let roll = self.rng.range(0, 100);
        match roll {
            0..40 => {
                let (key, ts, value) = (self.key(), self.stamp(), self.value());
                Op::Write { key, ts, kind: Kind::Put, value }
            }
            40..48 => Op::Write {
                key: self.key(),
                ts: self.stamp(),
                kind: Kind::Delete,
                value: Vec::new(),
            },
            48..54 => {
                let (key, ts, value) = (self.key(), self.stamp(), self.value());
                Op::Write { key, ts, kind: Kind::Intent, value }
            }
            54..60 => {
                let pending = model.intents();
                if pending.is_empty() {
                    return Op::Io;
                }
                let (key, intent_ts) =
                    pending[self.rng.range(0, pending.len() as u64) as usize].clone();
                let commit = match self.rng.range(0, 4) {
                    0 => None,
                    1 => Some((intent_ts, Kind::Put, self.value())),
                    2 => Some((
                        ts(intent_ts.physical + self.rng.range(1, 5), 0),
                        Kind::Delete,
                        Vec::new(),
                    )),
                    _ => Some((
                        ts(intent_ts.physical + self.rng.range(0, 5), 1),
                        Kind::Put,
                        self.value(),
                    )),
                };
                Op::Resolve { key, intent_ts, commit }
            }
            60..62 => {
                let (a, b) = (self.key(), self.key());
                let (start, end) = if a <= b { (a, b) } else { (b, a) };
                Op::DeleteRange { start, end, ts: self.stamp() }
            }
            62..82 => {
                let at = ts(
                    self.clock - self.rng.range(0, 80).min(self.clock),
                    self.rng.range(0, 3) as u32,
                );
                Op::Get { key: self.key(), ts: at }
            }
            82..90 => {
                let (a, b) = (self.key(), self.key());
                let (start, end) = if a <= b { (a, b) } else { (b, a) };
                Op::Scan {
                    start,
                    end,
                    ts: ts(self.clock - self.rng.range(0, 40).min(self.clock), 2),
                }
            }
            90..93 => Op::Flush,
            _ => Op::Io,
        }
    }
}

/// Applies `op` to the LSM and the model, and checks every read against the model.
/// `index` is the Raft index of the next write.
pub fn apply(op: &Op, lsm: &mut Lsm, model: &mut Model, disk: &mut dyn Disk, index: &mut u64) {
    let range = GroupId(1);
    match op {
        Op::Write { key, ts, kind, value } => {
            *index += 1;
            match kind {
                Kind::Put => lsm.put(range, *index, key, *ts, value),
                Kind::Delete => lsm.delete(range, *index, key, *ts),
                Kind::Intent => lsm.put_intent(range, *index, key, *ts, value),
                Kind::Retract => unreachable!("not written by callers"),
            }
            .expect("write");
            model.write(key, *ts, *kind, value);
        }
        Op::Resolve { key, intent_ts, commit } => {
            *index += 1;
            let c = commit.as_ref().map(|(ts, kind, value)| Commit { ts: *ts, kind: *kind, value });
            lsm.resolve_intent(range, *index, key, *intent_ts, c).expect("resolve");
            model.resolve(key, *intent_ts, commit.clone());
        }
        Op::DeleteRange { start, end, ts } => {
            *index += 1;
            lsm.delete_range(range, *index, start, end, *ts).expect("delete_range");
            model.delete_range(start, end, *ts);
        }
        Op::Get { key, ts } => {
            let view = lsm.view();
            assert_eq!(
                get(lsm, disk, &view, key, *ts),
                model.get(key, *ts),
                "get {key:?} at {ts:?}"
            );
        }
        Op::Scan { start, end, ts } => {
            let view = lsm.view();
            assert_eq!(
                scan(lsm, disk, &view, start, end, *ts),
                model.scan(start, end, *ts),
                "scan at {ts:?}"
            );
        }
        Op::Flush => lsm.flush(),
        Op::Io => {
            lsm.run_io(disk, Nanos::ZERO);
        }
    }
}

/// Every key read at several timestamps, and whole scans, against the model.
pub fn check_all(lsm: &mut Lsm, disk: &mut dyn Disk, model: &Model, until: u64) {
    let view = lsm.view();
    for at in [0, until / 4, until / 2, until.saturating_sub(10), until, u64::MAX] {
        let read = ts(at, u32::MAX);
        for key in model.keys() {
            assert_eq!(get(lsm, disk, &view, &key, read), model.get(&key, read), "{key:?} at {at}");
        }
        assert_eq!(
            scan(lsm, disk, &view, b"", b"\xff", read),
            model.scan(b"", b"\xff", read),
            "at {at}"
        );
    }
}

/// Runs `n` random operations of seed `seed` and checks the end state.
pub fn history(seed: u64, n: usize, lsm: &mut Lsm, disk: &mut dyn Disk) -> Model {
    let mut ops = Gen::new(seed, 40);
    let mut model = Model::default();
    let mut index = 0;
    for _ in 0..n {
        let op = ops.next(&model);
        apply(&op, lsm, &mut model, disk, &mut index);
    }
    lsm.run_io(disk, Nanos::ZERO);
    check_all(lsm, disk, &model, ops.clock + 100);
    model
}
