//! Crashes during flushes, compactions and manifest rotations (004 story 2, T010, SC-001).
//!
//! The Raft log of each range is simulated: every write is kept with its range and index, and
//! entries the LSM declares it no longer needs ([`Lsm::release_floor`]) are dropped from it. Each
//! life recovers the LSM, replays every range's log after its flushed index, checks every read
//! against the model of all writes, writes more while running part of the I/O, and crashes the
//! disk (lost and torn writes, names without their directory sync). Nothing the log held may be
//! lost, and the LSM may never need an entry it released.
//!
//! `TESELADB_LSM_CRASH_SEEDS` sets the number of seeds (CI: 10 000) and `TESELADB_LSM_CRASH_START`
//! the first one.

mod common;

use std::collections::BTreeMap;

use common::model::Model;
use common::{Gen, Op, check_all, small};
use teseladb_lsm::{Commit, Kind, Lsm, LsmConfig};
use teseladb_raftlog::{Disk, SimDisk, SimDiskConfig};
use teseladb_types::{GroupId, Nanos, Rng};

/// A logged write: its range, its index and the operation.
type Entry = (GroupId, u64, Op);

/// The range of a key `kNNN`: three ranges over the 40 keys.
fn range_of(key: &[u8]) -> GroupId {
    let n: u64 = std::str::from_utf8(&key[1..]).unwrap().parse().unwrap();
    GroupId(1 + n / 14)
}

fn write(lsm: &mut Lsm, (range, index, op): &Entry) {
    let (r, i) = (*range, *index);
    match op {
        Op::Write { key, ts, kind: Kind::Put, value } => lsm.put(r, i, key, *ts, value),
        Op::Write { key, ts, kind: Kind::Delete, .. } => lsm.delete(r, i, key, *ts),
        Op::Write { key, ts, kind: Kind::Intent, value } => lsm.put_intent(r, i, key, *ts, value),
        Op::Resolve { key, intent_ts, commit } => {
            let c = commit.as_ref().map(|(ts, kind, value)| Commit { ts: *ts, kind: *kind, value });
            lsm.resolve_intent(r, i, key, *intent_ts, c)
        }
        Op::DeleteRange { start, end, ts } => lsm.delete_range(r, i, start, end, *ts),
        other => unreachable!("not a write: {other:?}"),
    }
    .expect("write");
}

fn apply_to_model(model: &mut Model, op: &Op) {
    match op {
        Op::Write { key, ts, kind, value } => model.write(key, *ts, *kind, value),
        Op::Resolve { key, intent_ts, commit } => model.resolve(key, *intent_ts, commit.clone()),
        Op::DeleteRange { start, end, ts } => model.delete_range(start, end, *ts),
        other => unreachable!("not a write: {other:?}"),
    }
}

/// Runs up to `steps` disk operations.
fn some_io(lsm: &mut Lsm, disk: &mut SimDisk, steps: u64) {
    for _ in 0..steps {
        let Some(op) = lsm.poll_io(Nanos::ZERO) else { return };
        let done = disk.execute(op);
        lsm.on_io_done(done, Nanos::ZERO);
    }
}

struct Life<'a> {
    cfg: &'a LsmConfig,
    rng: Rng,
    ops: Gen,
    model: Model,
    log: Vec<Entry>,
    released: BTreeMap<GroupId, u64>,
    next_index: BTreeMap<GroupId, u64>,
    seen: Seen,
}

/// What the lives went through, so the test can show it exercised what it claims.
#[derive(Debug, Default)]
struct Seen {
    /// Lives that recovered a flushed index from the manifest.
    from_files: u64,
    /// Entries replayed from the log.
    replayed: u64,
    /// Lives whose manifest was not the first one (a rotation happened).
    rotated: u64,
    /// Lives that recovered files below level 0 (a compaction finished).
    compacted: u64,
}

impl Life<'_> {
    /// Recovers, replays and checks; then writes, runs part of the I/O and crashes.
    fn live(&mut self, disk: &mut SimDisk, seed: u64, life: u64) {
        let (mut lsm, flushed) = Lsm::recover(self.cfg, disk)
            .unwrap_or_else(|e| panic!("seed {seed}, life {life}: recovery failed: {e}"));
        for (range, upto) in &self.released {
            let f = flushed.get(range).copied().unwrap_or(0);
            assert!(
                f >= *upto,
                "seed {seed}, life {life}: {range} released {upto} but flushed only {f}"
            );
        }
        self.seen.from_files += u64::from(!flushed.is_empty());
        self.seen.compacted += u64::from(lsm.metrics().level_bytes[1..].iter().any(|b| *b > 0));
        self.seen.rotated +=
            u64::from(disk.list(&self.cfg.dir).unwrap_or_default().iter().any(|p| {
                p.file_name().is_some_and(|n| {
                    n.to_string_lossy().starts_with("MANIFEST-") && n != "MANIFEST-000001"
                })
            }));
        for e in &self.log {
            if e.1 > flushed.get(&e.0).copied().unwrap_or(0) {
                write(&mut lsm, e);
                self.seen.replayed += 1;
            }
        }
        lsm.run_io(disk, Nanos::ZERO);
        check_all(&mut lsm, disk, &self.model, u64::MAX / 2);
        let writes = self.rng.range(50, 600);
        for _ in 0..writes {
            match self.ops.next(&self.model) {
                Op::Flush => lsm.flush(),
                Op::Io | Op::Get { .. } | Op::Scan { .. } => {
                    let steps = self.rng.range(0, 8);
                    some_io(&mut lsm, disk, steps);
                }
                op => {
                    let key = match &op {
                        Op::Write { key, .. } | Op::Resolve { key, .. } => key.clone(),
                        Op::DeleteRange { start, .. } => start.clone(),
                        _ => unreachable!("handled above"),
                    };
                    let range = range_of(&key);
                    let index = self.next_index.entry(range).or_insert(0);
                    *index += 1;
                    let entry = (range, *index, op);
                    write(&mut lsm, &entry);
                    apply_to_model(&mut self.model, &entry.2);
                    self.log.push(entry);
                }
            }
            if self.rng.chance(0.05) {
                self.release(&lsm);
            }
        }
        let steps = self.rng.range(0, 200);
        some_io(&mut lsm, disk, steps);
        self.release(&lsm);
        drop(lsm);
        disk.crash(&mut self.rng);
    }

    /// Drops from the log what the LSM no longer needs.
    fn release(&mut self, lsm: &Lsm) {
        for range in self.next_index.keys().copied().collect::<Vec<_>>() {
            let floor = lsm.release_floor(range).unwrap_or(self.next_index[&range]);
            let r = self.released.entry(range).or_insert(0);
            *r = (*r).max(floor);
            let upto = *r;
            self.log.retain(|(g, i, _)| *g != range || *i > upto);
        }
    }
}

fn run(seed: u64, cfg: &LsmConfig, seen: &mut Seen) {
    let mut disk =
        SimDisk::new(SimDiskConfig { torn_write_probability: 0.5, ..SimDiskConfig::default() });
    let mut life = Life {
        cfg,
        rng: Rng::new(seed),
        ops: Gen::new(seed ^ 0x5eed, 40),
        model: Model::default(),
        log: Vec::new(),
        released: BTreeMap::new(),
        next_index: BTreeMap::new(),
        seen: Seen::default(),
    };
    for n in 0..3 {
        life.live(&mut disk, seed, n);
    }
    let (mut lsm, _) = Lsm::recover(cfg, &mut disk).expect("last recovery");
    for e in &life.log {
        if e.1 > lsm.flushed_index(e.0) {
            write(&mut lsm, e);
        }
    }
    check_all(&mut lsm, &mut disk, &life.model, u64::MAX / 2);
    seen.from_files += life.seen.from_files;
    seen.replayed += life.seen.replayed;
    seen.rotated += life.seen.rotated;
    seen.compacted += life.seen.compacted;
}

#[test]
fn nothing_the_log_held_is_lost_across_crashes() {
    let env = |k: &str, d: u64| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let start = env("TESELADB_LSM_CRASH_START", 0);
    let seeds = env("TESELADB_LSM_CRASH_SEEDS", 60);
    // Small manifests so that rotations happen and crash too.
    let cfg = LsmConfig { manifest_rotate_bytes: 6 * 4096, ..small() };
    let mut seen = Seen::default();
    for seed in start..start + seeds {
        run(seed, &cfg, &mut seen);
    }
    eprintln!("{seeds} seeds: {seen:?}");
    assert!(seen.from_files >= seeds && seen.replayed > seeds, "{seen:?}");
    assert!(seen.rotated > 0 && seen.compacted > 0, "{seen:?}");
}

#[test]
fn a_resolution_is_never_flushed_by_halves() {
    // Every write fills the memtable: the `Retract` and the commit of one entry must still share
    // a memtable, or a flush could hold the entry's index with half of it.
    let cfg = LsmConfig { memtable_bytes: 1, ..small() };
    let mut disk = SimDisk::default();
    let (mut lsm, _) = Lsm::recover(&cfg, &mut disk).unwrap();
    let g = GroupId(1);
    let t = |p| teseladb_keys::Hlc { physical: p, logical: 0 };
    lsm.put_intent(g, 1, b"k", t(10), b"txn").unwrap();
    lsm.resolve_intent(g, 2, b"k", t(10), Some(Commit { ts: t(12), kind: Kind::Put, value: b"v" }))
        .unwrap();
    while lsm.flushed_index(g) < 2 {
        // A flush is installed by the poll after its last operation, which may return nothing.
        let Some(op) = lsm.poll_io(Nanos::ZERO) else { continue };
        let done = disk.execute(op);
        lsm.on_io_done(done, Nanos::ZERO);
    }
    drop(lsm);
    disk.crash(&mut Rng::new(7));
    let (mut lsm, flushed) = Lsm::recover(&cfg, &mut disk).unwrap();
    assert_eq!(flushed[&g], 2, "nothing to replay");
    let v = lsm.view();
    let got = common::get(&mut lsm, &mut disk, &v, b"k", t(13)).map(|r| r.3);
    assert_eq!(got, Some(b"v".to_vec()));
}
