//! A `SyncError` aimed at a node that is down waits on its disk for the next sync, which its
//! restart does when it creates the range's LSM again (a replica replaced by FR-014 starts
//! without its LSM). The process ends on that error and its supervisor starts it again, as the
//! server does when its recovery fails; the error is the fault's, not a defect of the recovery.
//! Seed 28583 (`range-docs`, heavy plan) had the recovery checker report it.

use teseladb_keys::Hlc;
use teseladb_lsm::{EntryWrite, Kind};
use teseladb_sim::checkers::Violation;
use teseladb_sim::faults::FaultPlan;
use teseladb_sim::registry::Registry;
use teseladb_sim::workloads::Workload;
use teseladb_sim::world::{Event, World};
use teseladb_sim::{SimConfig, Verdict, run_workload};
use teseladb_raftlog::{Completions, Disk, IoErrno, IoOp};
use teseladb_types::{GroupId, Nanos};

/// When node 0 goes down with sync errors waiting on its disks.
const DOWN_AT: Nanos = Nanos::from_secs(3);
/// How long it stays down before its restart.
const DOWN_FOR: Nanos = Nanos::from_millis(200);
/// A group of no workload, for the write whose flush fails.
const UNUSED_GROUP: u64 = 9_999;
/// How long the supervisor takes to restart a process whose core stopped.
const FAILED_RESTART: Nanos = Nanos::from_millis(100);

/// `range-docs` with node 0 crashed at [`DOWN_AT`], its range's LSM removed as a replacement
/// removes it, and sync errors injected while it is down.
struct DownWithErrors {
    inner: Box<dyn Workload>,
    done: bool,
}

impl Workload for DownWithErrors {
    fn name(&self) -> &'static str {
        "down-with-errors"
    }
    fn cluster(&self) -> bool {
        self.inner.cluster()
    }
    fn start(&mut self, w: &mut World) {
        self.inner.start(w);
    }
    fn on_client(&mut self, w: &mut World, client: u32) {
        if !self.done && w.now() >= DOWN_AT && w.nodes[0].alive {
            self.done = true;
            self.inner.on_crash(w, 0);
            w.crash_node(0);
            let dir = w.range_lsm.as_ref().expect("a range world").dir.clone();
            for c in &mut w.nodes[0].cores {
                for path in c.disk.list(&dir).unwrap_or_default() {
                    c.disk.execute(IoOp::Remove { path });
                }
                c.disk.execute(IoOp::SyncDir { path: dir.clone() });
                c.disk.inject_error("sync", IoErrno::Io);
            }
            w.schedule(w.now() + DOWN_FOR, Event::Restart { node: 0 });
        }
        self.inner.on_client(w, client);
    }
    fn on_completions(&mut self, w: &mut World, node: u32, core: u16, c: Completions) {
        self.inner.on_completions(w, node, core, c);
    }
    fn on_crash(&mut self, w: &mut World, node: u32) {
        self.inner.on_crash(w, node);
    }
    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        self.inner.on_restart(w, node)
    }
    fn on_heal(&mut self, w: &mut World) {
        self.inner.on_heal(w);
    }
    fn finish(&mut self, w: &mut World) -> Result<(), Violation> {
        let hit: u64 = w.nodes[0].disks().map(|d| d.stats().injected_errors).sum();
        assert!(hit > 0, "no restart reached a sync while the error waited");
        assert!(w.nodes[0].alive && !w.nodes[0].lost, "node 0 came back");
        self.inner.finish(w)
    }
}

#[test]
fn an_io_error_the_fault_left_on_a_down_node_restarts_it_again() {
    let cfg = SimConfig {
        workload: "range-docs".into(),
        faults: FaultPlan::None,
        ..SimConfig::default()
    };
    for seed in 1..=4 {
        let inner = Registry::builtin().workload("range-docs", 3, 2).expect("registered");
        let r = run_workload(&cfg, seed, Box::new(DownWithErrors { inner, done: false }));
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
        assert!(r.lost.is_empty(), "seed {seed}: {:?}", r.lost);
    }
}

/// `lsm` with node 0 crashed at [`DOWN_AT`], whose core 0 stops on a failed sync while the
/// workload starts the node again, as when the LSM's read-back runs a job.
struct LsmStartError {
    inner: Box<dyn Workload>,
    done: bool,
    /// When the restart that takes the error ran.
    armed: Option<Nanos>,
    /// When the supervisor took node 0 down after it.
    stopped: Option<Nanos>,
}

impl Workload for LsmStartError {
    fn name(&self) -> &'static str {
        "lsm-start-error"
    }
    fn cluster(&self) -> bool {
        self.inner.cluster()
    }
    fn start(&mut self, w: &mut World) {
        self.inner.start(w);
    }
    fn on_client(&mut self, w: &mut World, client: u32) {
        if !self.done && w.now() >= DOWN_AT && w.nodes[0].alive {
            self.done = true;
            self.inner.on_crash(w, 0);
            w.crash_node(0);
            w.schedule(w.now() + DOWN_FOR, Event::Restart { node: 0 });
        }
        self.inner.on_client(w, client);
    }
    fn on_completions(&mut self, w: &mut World, node: u32, core: u16, c: Completions) {
        self.inner.on_completions(w, node, core, c);
    }
    fn on_crash(&mut self, w: &mut World, node: u32) {
        if node == 0 && self.armed.is_some() && self.stopped.is_none() {
            self.stopped = Some(w.now());
        }
        self.inner.on_crash(w, node);
    }
    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        self.inner.on_restart(w, node)?;
        if node == 0 && self.done && self.armed.is_none() {
            // As the LSM's read-back does when a job runs meanwhile: a flush whose sync fails.
            let now = w.now();
            self.armed = Some(now);
            let c = &mut w.nodes[0].cores[0];
            c.disk.inject_error("sync", IoErrno::Io);
            let lsm = c.lsm.clone().expect("the workload recovered it");
            let mut lsm = lsm.borrow_mut();
            let put = [EntryWrite { key: b"x", kind: Kind::Put, value: b"y" }];
            lsm.write_entry(GroupId(UNUSED_GROUP), 1, Hlc { physical: 1, logical: 0 }, &put)
                .expect("written to the memtable");
            lsm.flush();
            lsm.run_io(&mut c.disk, now);
            assert!(lsm.health().is_err(), "the LSM stopped on the failed sync");
        }
        Ok(())
    }
    fn on_heal(&mut self, w: &mut World) {
        self.inner.on_heal(w);
    }
    fn finish(&mut self, w: &mut World) -> Result<(), Violation> {
        // The supervisor restarts the process as soon as it sees the stopped core, not when
        // something else happens to reach that core.
        let (armed, stopped) = (self.armed.unwrap(), self.stopped.expect("restarted again"));
        assert_eq!(stopped.saturating_since(armed), FAILED_RESTART);
        self.inner.finish(w)
    }
}

/// Seed 979 of `lsm` (heavy plan): a core that stopped on an error during the workload's start of
/// its node had nothing more for its disk, so no event showed it to the supervisor and it applied
/// nothing after the heal.
#[test]
fn a_core_stopped_while_the_workload_starts_its_node_is_restarted() {
    let cfg = SimConfig { workload: "lsm".into(), faults: FaultPlan::None, ..SimConfig::default() };
    for seed in 1..=4 {
        let inner = Registry::builtin().workload("lsm", 3, 2).expect("registered");
        let wl = LsmStartError { inner, done: false, armed: None, stopped: None };
        let r = run_workload(&cfg, seed, Box::new(wl));
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
    }
}

/// How long node 0's disks stay full after its restart: the supervisor tries again meanwhile.
const FULL_FOR: Nanos = Nanos::from_millis(700);

/// `range-docs` with node 0 crashed at [`DOWN_AT`], its range's LSM removed as a replacement
/// removes it, and its disks full when it starts again, until [`FULL_FOR`] later.
struct DownOnFullDisk {
    inner: Box<dyn Workload>,
    done: bool,
    /// When the disks get their space back.
    freed_at: Option<Nanos>,
}

impl Workload for DownOnFullDisk {
    fn name(&self) -> &'static str {
        "down-on-full-disk"
    }
    fn cluster(&self) -> bool {
        self.inner.cluster()
    }
    fn start(&mut self, w: &mut World) {
        self.inner.start(w);
    }
    fn on_client(&mut self, w: &mut World, client: u32) {
        if !self.done && w.now() >= DOWN_AT && w.nodes[0].alive {
            self.done = true;
            self.inner.on_crash(w, 0);
            w.crash_node(0);
            let dir = w.range_lsm.as_ref().expect("a range world").dir.clone();
            for c in &mut w.nodes[0].cores {
                for path in c.disk.list(&dir).unwrap_or_default() {
                    c.disk.execute(IoOp::Remove { path });
                }
                c.disk.execute(IoOp::SyncDir { path: dir.clone() });
                let used = c.disk.used();
                c.disk.set_capacity(used);
            }
            w.schedule(w.now() + DOWN_FOR, Event::Restart { node: 0 });
            self.freed_at = Some(w.now() + DOWN_FOR + FULL_FOR);
        }
        if self.freed_at.is_some_and(|at| w.now() >= at) {
            self.freed_at = None;
            for c in &mut w.nodes[0].cores {
                c.disk.set_capacity(u64::MAX);
            }
        }
        self.inner.on_client(w, client);
    }
    fn on_completions(&mut self, w: &mut World, node: u32, core: u16, c: Completions) {
        self.inner.on_completions(w, node, core, c);
    }
    fn on_crash(&mut self, w: &mut World, node: u32) {
        self.inner.on_crash(w, node);
    }
    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        self.inner.on_restart(w, node)
    }
    fn on_heal(&mut self, w: &mut World) {
        self.inner.on_heal(w);
    }
    fn finish(&mut self, w: &mut World) -> Result<(), Violation> {
        assert!(self.done, "node 0 went down");
        assert!(w.nodes[0].alive && !w.nodes[0].lost, "node 0 came back");
        self.inner.finish(w)
    }
}

/// Seed 3662 of `range-docs` (heavy plan): a node started while a `DiskFull` fault held its disk
/// failed to create its range's LSM, and the recovery checker took the full disk for a damaged
/// one. The process exits and its supervisor starts it again until there is space, as the server
/// does.
#[test]
fn a_node_started_on_a_full_disk_comes_back_once_there_is_space() {
    let cfg = SimConfig {
        workload: "range-docs".into(),
        faults: FaultPlan::None,
        ..SimConfig::default()
    };
    for seed in 1..=4 {
        let inner = Registry::builtin().workload("range-docs", 3, 2).expect("registered");
        let wl = DownOnFullDisk { inner, done: false, freed_at: None };
        let r = run_workload(&cfg, seed, Box::new(wl));
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
        assert!(r.lost.is_empty(), "seed {seed}: {:?}", r.lost);
    }
}
