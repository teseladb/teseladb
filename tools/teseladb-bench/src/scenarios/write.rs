//! `write` scenario (005 FR-021, SC-003): a confirmed write (`w: "majority"`, `j: true`) of a
//! 100-byte document on a range of three replicas, from the call to the leader's `RangeHost` until
//! its answer: under 300 µs at p50 and 2 ms at p99. `write_cpu` is the same latency without the
//! time spent in the disk.
//!
//! The three replicas run in this process on one thread ([`Cluster`]), each one over its own folder
//! of the data directory through `FileDisk`, with its `MultiRaftCore`, its LSM and its host; their
//! messages go from one core to another in memory. There is no network, and the syncs of the three
//! replicas run one after another, where in a cluster the leader's sync runs while the followers'
//! do: the numbers measure the code and the disk, and the targets (loopback and NVMe) are
//! indicative here. The cluster's clock is logical: it moves one tick of 10 ms every
//! [`OPS_PER_TICK`] operations, outside the timing.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::Path;
use std::rc::Rc;
use std::time::{Duration, Instant};

use teseladb_io::FileDisk;
use teseladb_keys::Hlc;
use teseladb_lsm::{Lsm, LsmConfig};
use teseladb_multiraft::{CoreConfig, GroupDesc, MultiRaftCore, NodeClock, Observation};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::command::Put;
use teseladb_range::host::{
    Mutation, MutationError, RangeError, RangeHost, Reply, ReqId, WriteReq,
};
use teseladb_range::mutate::Outcome;
use teseladb_range::read::ReadConcern;
use teseladb_range::sm::RangeSm;
use teseladb_raft::Role;
use teseladb_raftlog::{CoreStore, Disk, StorageConfig};
use teseladb_types::{GroupId, Histogram, Nanos, NodeId};

use super::{DataDir, at, keep_going, msg, warm_up};
use crate::cli::Args;
use crate::report::Report;

/// The range: it covers the whole data space.
const G: GroupId = GroupId(1);
const START: &[u8] = b"\x01";
const END: &[u8] = b"\x02";
/// One tick of the cluster's logical clock, as the server's and the simulator's.
const TICK: Nanos = Nanos::from_millis(10);
/// Wall clock of the cluster at logical time zero (milliseconds).
const WALL0: u64 = 1_800_000_000_000;
/// Every write of the scenarios: acknowledged once committed and applied on the leader.
const MAJORITY: WriteConcern = WriteConcern { w: W::Majority, j: true };
/// Value type of a document version.
const DOC_TYPE: u8 = 1;
/// Every document: 100 bytes, as the `lsm` scenario's.
const VALUE: [u8; 100] = [7; 100];
/// Documents the writes and the reads go to.
pub(crate) const DOCS: u64 = 1000;
/// Operations between two ticks of the logical clock.
pub(crate) const OPS_PER_TICK: u64 = 10;
/// Ticks an election or an answer may take before the run gives up: 10 s of logical time.
const MAX_TICKS: u32 = 1000;
/// Rounds of message delivery after which the cluster is taken as stuck.
const SETTLE_ROUNDS: u32 = 100_000;
/// Writes timed at least, whatever `--duration` says.
const MIN_WRITES: u64 = 100;

/// Key of document `i`.
pub(crate) fn doc(i: u64) -> Vec<u8> {
    format!("\x01d{i:08}").into_bytes()
}

/// Writes [`VALUE`] at its document, whatever it held.
struct SetDoc(Vec<u8>);

impl Mutation for SetDoc {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let put = Put { key: self.0.clone(), value_type: DOC_TYPE, value: VALUE.to_vec() };
        Ok(Outcome { puts: vec![put], deletes: vec![], change: vec![], result: vec![] })
    }
}

/// A replica: its disk, its core with the range and the core's host.
struct Replica {
    id: NodeId,
    disk: FileDisk,
    core: MultiRaftCore<RangeSm>,
    host: RangeHost,
    replies: Vec<(ReqId, Result<Reply, RangeError>)>,
}

/// One range on three replicas in this process, in logical time.
pub(crate) struct Cluster {
    replicas: Vec<Replica>,
    /// Logical time.
    pub(crate) now: Nanos,
    /// Replicas killed: nothing runs on them and their messages are lost.
    down: BTreeSet<NodeId>,
    /// Time spent running disk operations, the LSM's included.
    pub(crate) disk_time: Duration,
}

impl Cluster {
    /// Three replicas of a new range, each one under `dir/node-<n>`; `seed` draws their election
    /// timeouts, and with `observe` their cores record how each read was confirmed.
    pub(crate) fn open(dir: &Path, seed: u64, observe: bool) -> Result<Cluster, String> {
        let voters: Vec<NodeId> = (1..=3).map(NodeId).collect();
        let lsm_cfg = LsmConfig { dir: "core-00/lsm".into(), ..LsmConfig::default() };
        let mut replicas = Vec::new();
        for &id in &voters {
            let path = dir.join(format!("node-{}", id.0));
            let mut disk = FileDisk::open(&path).map_err(at(&path))?;
            let store = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).map_err(msg)?.0;
            let clock = NodeClock::for_node(id);
            let cfg = CoreConfig {
                clock: clock.clone(),
                instance: seed * 1_000 + id.0,
                observe,
                ..CoreConfig::new(id, 0)
            };
            let mut core = MultiRaftCore::new(cfg, store, seed * 10 + id.0);
            let lsm = Rc::new(RefCell::new(Lsm::recover(&lsm_cfg, &mut disk).map_err(msg)?.0));
            let sm = RangeSm::open(G, START.to_vec(), END.to_vec(), lsm.clone(), &mut disk)
                .map_err(msg)?;
            let desc = GroupDesc { group: G, voters: voters.clone(), learners: vec![] };
            core.create_group(desc, sm).map_err(msg)?;
            let mut host = RangeHost::new(lsm, clock);
            host.add_range(G, START.to_vec(), END.to_vec());
            replicas.push(Replica { id, disk, core, host, replies: vec![] });
        }
        let mut c = Cluster {
            replicas,
            now: Nanos::ZERO,
            down: BTreeSet::new(),
            disk_time: Duration::ZERO,
        };
        c.settle()?;
        Ok(c)
    }

    fn wall(&self) -> u64 {
        WALL0 + self.now.as_millis()
    }

    fn replica(&mut self, n: NodeId) -> &mut Replica {
        self.replicas.iter_mut().find(|r| r.id == n).expect("a replica of the cluster")
    }

    /// Runs disk operations, results and messages until nothing moves.
    fn settle(&mut self) -> Result<(), String> {
        let (now, wall) = (self.now, self.wall());
        for _ in 0..SETTLE_ROUNDS {
            let mut moved = false;
            let mut sent = Vec::new();
            for r in self.replicas.iter_mut().filter(|r| !self.down.contains(&r.id)) {
                let t = Instant::now();
                while let Some(op) = r.core.poll_io(now) {
                    let done = r.disk.execute(op);
                    r.core.on_io_done(done, now);
                    moved = true;
                }
                r.host.lsm().borrow_mut().run_io(&mut r.disk, now);
                self.disk_time += t.elapsed();
                let actions = r.core.drain_actions();
                for (op, result) in actions.done {
                    moved |= r.host.on_done(&mut r.core, op, result);
                }
                moved |= r.host.poll(&mut r.core, now, wall);
                let replies = r.host.take_replies();
                moved |= !replies.is_empty();
                r.replies.extend(replies);
                sent.extend(actions.send.into_iter().map(|(to, f)| (r.id, to, f)));
            }
            moved |= !sent.is_empty();
            for (from, to, frame) in sent {
                if !self.down.contains(&to) {
                    self.replica(to).core.on_peer_message(from, frame, now);
                }
            }
            if !moved {
                return Ok(());
            }
        }
        Err("the cluster never settles".into())
    }

    /// One tick of the logical clock.
    pub(crate) fn tick(&mut self) -> Result<(), String> {
        self.now = self.now + TICK;
        let (now, wall) = (self.now, self.wall());
        for r in self.replicas.iter_mut().filter(|r| !self.down.contains(&r.id)) {
            r.core.on_tick(now, wall);
        }
        self.settle()
    }

    /// A live replica that leads the range (its host holds writes until the term's first entry is
    /// applied).
    fn leader(&self) -> Option<NodeId> {
        self.replicas
            .iter()
            .filter(|r| !self.down.contains(&r.id))
            .filter_map(|r| r.core.status(G).map(|s| (r.id, s)))
            .find(|(_, s)| s.role == Role::Leader)
            .map(|(id, _)| id)
    }

    /// Ticks until a live replica leads the range.
    pub(crate) fn elect(&mut self) -> Result<NodeId, String> {
        for _ in 0..MAX_TICKS {
            self.tick()?;
            if let Some(l) = self.leader() {
                return Ok(l);
            }
        }
        Err("no leader".into())
    }

    /// Kills replica `n`, as a `SIGKILL`: from now on it does nothing.
    pub(crate) fn kill(&mut self, n: NodeId) {
        self.down.insert(n);
    }

    /// A confirmed write of document `key` through replica `n`.
    pub(crate) fn write(&mut self, n: NodeId, key: &[u8]) -> ReqId {
        let (now, wall) = (self.now, self.wall());
        let r = self.replica(n);
        let m = Box::new(SetDoc(key.to_vec()));
        let w = WriteReq { g: G, keys: vec![key.to_vec()], m, dedup: None, wc: MAJORITY };
        r.host.mutate(&mut r.core, w, now, wall)
    }

    /// A `majority` read of document `key` through replica `n` (with the lease when it holds).
    pub(crate) fn get(&mut self, n: NodeId, key: &[u8]) -> ReqId {
        let (now, wall) = (self.now, self.wall());
        let r = self.replica(n);
        r.host.get(&mut r.core, G, key, ReadConcern::Majority, now, wall)
    }

    /// A read of document `key` on replica `n` without the leader, at its closed mark (007
    /// FR-012).
    pub(crate) fn read_follower(&mut self, n: NodeId, key: &[u8]) -> ReqId {
        let (now, wall) = (self.now, self.wall());
        let r = self.replica(n);
        let spec = teseladb_range::host::ReadSpec {
            g: G,
            key: key.to_vec(),
            scan: None,
            backward: false,
            at: None,
            concern: ReadConcern::Local,
            view: teseladb_range::push::TxnView::default(),
            uncertain_to: None,
        };
        r.host.read_follower(&mut r.core, spec, None, now, wall)
    }

    /// The cluster's wall clock now, in milliseconds.
    pub(crate) fn wall_ms(&self) -> u64 {
        self.wall()
    }

    /// The answer to request `id` of replica `n`, ticking while it is not there.
    pub(crate) fn wait(&mut self, n: NodeId, id: ReqId) -> Result<Reply, String> {
        self.settle()?;
        for _ in 0..MAX_TICKS {
            let replies = &mut self.replica(n).replies;
            if let Some(i) = replies.iter().position(|(r, _)| *r == id) {
                return replies.swap_remove(i).1.map_err(|e| format!("request {id}: {e:?}"));
            }
            self.tick()?;
        }
        Err(format!("request {id}: no answer"))
    }

    /// Reads replica `n` registered with its lease since the last call, and reads it confirmed
    /// with ReadIndex (the cluster has to be opened with `observe`).
    pub(crate) fn reads_by_lease(&mut self, n: NodeId) -> (u64, u64) {
        let mut by = [0u64; 2];
        for o in self.replica(n).core.take_observations() {
            if let Observation::ReadRegistered { lease, .. } = o {
                by[usize::from(lease)] += 1;
            }
        }
        (by[1], by[0])
    }
}

/// p50, p99 and p99.9 of `h`, or not measured (a missed target) when it has no sample.
pub(crate) fn latency_measured(
    report: &mut Report,
    name: &str,
    h: &Histogram,
    p50: Option<f64>,
    p99: Option<f64>,
) {
    if h.count() > 0 {
        report.latency(name, h, p50, p99);
        return;
    }
    report.notes.push(format!("{name} not measured: no sample"));
    let target = crate::report::Target::AtMost;
    report.push(&format!("{name}.p50"), "ns", f64::NAN, p50.map(target));
    report.push(&format!("{name}.p99"), "ns", f64::NAN, p99.map(target));
    report.push(&format!("{name}.p999"), "ns", f64::NAN, None);
}

/// Notes of every range scenario: the machine and what the numbers measure.
pub(crate) fn notes(report: &mut Report) {
    report.notes.push(format!("{} {}", std::env::consts::OS, std::env::consts::ARCH));
    report.notes.push(
        "three replicas in this process on one thread, over files, without network; their syncs \
         run one after another"
            .into(),
    );
    #[cfg(not(target_os = "linux"))]
    report.notes.push("targets set for loopback and NVMe on Linux: indicative here".into());
}

/// Writes the next document through `leader` and returns its latency and that latency without
/// the time spent in the disk, in nanoseconds; `ops` counts the writes, and the logical clock
/// ticks every [`OPS_PER_TICK`] of them, outside the timing.
fn write_next(c: &mut Cluster, leader: NodeId, ops: &mut u64) -> Result<(u64, u64), String> {
    let disk_before = c.disk_time;
    let t = Instant::now();
    let id = c.write(leader, &doc(*ops % DOCS));
    c.wait(leader, id)?;
    let ns = t.elapsed().as_nanos() as u64;
    let disk_ns = (c.disk_time - disk_before).as_nanos() as u64;
    *ops += 1;
    if ops.is_multiple_of(OPS_PER_TICK) {
        c.tick()?;
    }
    Ok((ns.max(1), ns.saturating_sub(disk_ns).max(1)))
}

/// Runs the scenario.
pub fn run(args: &Args) -> Result<Report, String> {
    run_with(args, MIN_WRITES)
}

fn run_with(args: &Args, min_writes: u64) -> Result<Report, String> {
    let dir = DataDir::new(args.data.as_deref(), "write")?;
    let mut c = Cluster::open(&dir.path, 0, false)?;
    let leader = c.elect()?;
    let mut ops = 0;
    warm_up(args.warmup, || write_next(&mut c, leader, &mut ops).map(drop))?;
    let (mut total, mut cpu) = (Histogram::new(), Histogram::new());
    let clock = Instant::now();
    let mut n = 0;
    while keep_going(n, min_writes, clock.elapsed() >= args.duration) {
        let (ns, cpu_ns) = write_next(&mut c, leader, &mut ops)?;
        total.record(ns);
        cpu.record(cpu_ns);
        n += 1;
    }
    let mut report = Report::new("write");
    report.latency("write", &total, Some(300_000.0), Some(2_000_000.0));
    report.latency("write_cpu", &cpu, None, None);
    report.push("write.count", "n", n as f64, None);
    notes(&mut report);
    Ok(report)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
pub(crate) mod tests {
    use super::*;

    /// An empty folder of its own for a test, given as `--data`: the test removes it.
    pub(crate) fn scratch(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("teseladb-bench-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn a_short_run_reports_every_metric() {
        let data = scratch("write");
        let args = Args {
            scenario: "write".into(),
            data: Some(data.clone()),
            duration: Duration::ZERO,
            warmup: Duration::from_millis(1),
            ..Args::default()
        };
        // The warm-up's writes are not timed: the count is the minimum whatever it wrote.
        let r = run_with(&args, 25).unwrap();
        std::fs::remove_dir_all(&data).unwrap();
        for name in [
            "write.p50",
            "write.p99",
            "write.p999",
            "write_cpu.p50",
            "write_cpu.p99",
            "write_cpu.p999",
        ] {
            let m = r.metrics.iter().find(|m| m.name == name).unwrap_or_else(|| panic!("{name}"));
            assert!(m.value > 0.0, "{r}");
        }
        let value = |name: &str| r.metrics.iter().find(|m| m.name == name).unwrap().value;
        assert_eq!(value("write.count"), 25.0);
        assert!(value("write_cpu.p50") <= value("write.p50"), "{r}");
        let targets = r.metrics.iter().filter(|m| m.target.is_some()).count();
        assert_eq!(targets, 2, "{r}");
    }

    #[test]
    fn a_latency_without_samples_is_not_measured() {
        let mut r = Report::new("read");
        latency_measured(&mut r, "read_lease", &Histogram::new(), Some(50_000.0), None);
        assert_eq!(r.metrics.len(), 3);
        assert!(r.metrics.iter().all(|m| m.value.is_nan()), "{r}");
        assert_eq!(r.missed().len(), 1, "{r}");
        assert_eq!(r.notes, ["read_lease not measured: no sample"]);
    }

    #[test]
    fn a_killed_leader_is_replaced_and_answers_nothing() {
        let tmp = scratch("kill");
        let mut c = Cluster::open(&tmp, 1, false).unwrap();
        let old = c.elect().unwrap();
        let follower = (1..=3).map(NodeId).find(|&n| n != old).unwrap();
        let id = c.write(follower, &doc(0));
        assert!(c.wait(follower, id).unwrap_err().contains("NotLeader"));
        c.kill(old);
        let id = c.write(old, &doc(0));
        assert!(c.wait(old, id).unwrap_err().contains("no answer"));
        let new = c.elect().unwrap();
        assert_ne!(new, old);
        std::fs::remove_dir_all(&tmp).unwrap();
    }
}
