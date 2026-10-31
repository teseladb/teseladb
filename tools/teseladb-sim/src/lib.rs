//! TeselaDB's deterministic simulator (specification 001, FR-009; 002 extends it).
//!
//! A single thread and a single seeded `Rng`: the nodes run the production code (the per-core log,
//! and `MultiRaftCore` with `teseladb-testkv`) over simulated disks, network and clocks
//! ([`world`]), with a fault plan ([`faults`]) and checkers ([`checkers`]). The same seed always
//! gives the same trace ([`trace`]).
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

pub mod checkers;
pub mod clock;
pub mod faults;
pub mod history;
pub mod net;
pub mod node;
pub mod registry;
pub mod trace;
pub mod variants;
pub mod workloads;
pub mod world;

use teseladb_raft::RaftConfig;
use teseladb_types::{Nanos, NodeId};

use crate::checkers::Violation;
use crate::faults::{FaultAction, FaultPlan};
use crate::registry::{Checker, Registry};
use crate::world::{Event, World, answers_heartbeats};

/// Configuration of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimConfig {
    /// Workload.
    pub workload: String,
    /// Nodes.
    pub nodes: u32,
    /// Cores per node.
    pub cores: u16,
    /// Simulated time (the last 5 s without faults).
    pub sim_time: Nanos,
    /// Events of a seed at most; on reaching them it goes to the final check.
    pub steps: u64,
    /// Fault plan.
    pub faults: FaultPlan,
    /// How often the plan's faults come, in percent of its rhythm (100 = as the plan says; FR-005).
    pub fault_rate_pct: u32,
    /// Keep the trace lines.
    pub trace: bool,
    /// Keep the clients' history (JSONL of the contract `history-format.md`).
    pub history: bool,
    /// Stop after this number of events (`replay`).
    pub until_event: Option<u64>,
}

impl Default for SimConfig {
    fn default() -> SimConfig {
        SimConfig {
            workload: "storage".into(),
            nodes: 3,
            cores: 2,
            sim_time: Nanos::from_secs(20),
            steps: DEFAULT_STEPS,
            faults: FaultPlan::Default,
            fault_rate_pct: 100,
            trace: false,
            history: false,
            until_event: None,
        }
    }
}

/// Verdict of a seed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Everything correct.
    Pass,
    /// Property violated.
    Fail(Violation),
}

/// Result of a seed.
#[derive(Clone, Debug)]
pub struct SeedResult {
    /// Seed.
    pub seed: u64,
    /// Workload.
    pub workload: String,
    /// Events processed.
    pub events: u64,
    /// Simulated time reached.
    pub sim_time: Nanos,
    /// SHA-256 of the trace.
    pub trace_hash: [u8; 32],
    /// Verdict.
    pub verdict: Verdict,
    /// Trace lines (with `trace`).
    pub trace_lines: Option<Vec<String>>,
    /// Nodes left out because their recovery found a corrupt core (research R3c).
    pub lost: Vec<u32>,
    /// The clients' history in JSONL (with `history`, for workloads with clients).
    pub history: Option<String>,
    /// Nodes powered on at the end whose Group 0 replica has the cluster created (cluster worlds).
    pub group0_bootstrapped: u32,
}

/// Default of [`SimConfig::steps`] (contract `sim-cli.md`).
pub const DEFAULT_STEPS: u64 = 200_000;

/// Final time without faults to check liveness.
pub const HEAL_PERIOD: Nanos = Nanos::from_secs(5);
/// Restart time of a stopped core (the supervisor restarts the process).
const FAILED_RESTART: Nanos = Nanos::from_millis(100);
/// How far past the HLC bound on its disk the clock of [`FaultAction::StepPastBound`] lands.
const STEP_PAST_BOUND_MS: u64 = 1;
/// Most a clock at the edge of the margin moves in one step: less than a jump for the clock rule
/// (`teseladb_hlc::MAX_CLOCK_OFFSET_MS`), as NTP slews a clock.
const EDGE_SLEW_STEP_NS: i64 = 100_000_000;
/// Time between two steps of a clock going to the edge, many ticks of the clock rule.
const EDGE_SLEW_EVERY: Nanos = Nanos::from_millis(200);
/// How often a clock at the edge checks whether its node leads a range of data again, to cut it
/// off while it leads one.
const EDGE_WAIT_EVERY: Nanos = Nanos::from_millis(50);
/// How long the node at the edge is cut off from the rest (ms).
const EDGE_CUT_MS: (u64, u64) = (300, 1500);
/// How long past the end of its lease the node cut off at the edge dies (ms): its clients' requests
/// end at once, as with a primary that is cut off and then lost, and go to the leader that
/// follows while its lease reads are the newest marks of the range.
const EDGE_DIE_PAST_LEASE_MS: (u64, u64) = (1, 10);
/// Time a node is down before the restart that finds its corrupt core.
const CORRUPT_DOWN: Nanos = Nanos::from_millis(200);

fn summary(ev: &Event) -> String {
    match ev {
        Event::Disk { node, core, .. } | Event::Tick { node, core, .. } => {
            format!("n{node}c{core}")
        }
        Event::Client { client } => format!("cl{client}"),
        Event::Deliver { from, to, core, .. } => format!("{from}->{to}c{core}"),
        Event::Fault(a) | Event::FaultEnd(a) => format!("{a:?}"),
        Event::Restart { node } => format!("n{node}"),
        Event::Heal | Event::CheckPoint => String::new(),
    }
}

/// Events processed at one simulated instant beyond which a seed fails: simulated time no longer
/// moves (a livelock of the simulated system or of the simulator). A whole run makes about as many
/// over twenty simulated seconds. The limit is also at most half of [`SimConfig::steps`], so that
/// a livelock is caught before the step budget ends the seed.
pub const SAME_INSTANT_LIMIT: u64 = 100_000;

/// The simulator with the workloads and checkers of a [`Registry`].
#[derive(Clone, Debug)]
pub struct Sim {
    registry: Registry,
}

impl Sim {
    /// A simulator with `registry`.
    pub fn new(registry: Registry) -> Sim {
        Sim { registry }
    }

    /// Its registry.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Runs a seed of the configuration's workload with the registered checkers.
    pub fn run_seed(&self, cfg: &SimConfig, seed: u64) -> SeedResult {
        let wl = self
            .registry
            .workload(&cfg.workload, cfg.nodes, cfg.cores)
            .expect("workload validated by the CLI");
        run_with_checkers(cfg, seed, wl, self.registry.checkers())
    }
}

/// Runs a seed with the built-in workloads ([`Registry::builtin`]).
pub fn run_seed(cfg: &SimConfig, seed: u64) -> SeedResult {
    Sim::new(Registry::builtin()).run_seed(cfg, seed)
}

/// Runs a seed of a given workload.
pub fn run_workload(cfg: &SimConfig, seed: u64, wl: Box<dyn workloads::Workload>) -> SeedResult {
    run_with_checkers(cfg, seed, wl, &[])
}

/// Runs a seed of a given workload; each checker's `on_step` runs after every event and its
/// `on_history` at the end.
pub fn run_with_checkers(
    cfg: &SimConfig,
    seed: u64,
    mut wl: Box<dyn workloads::Workload>,
    checkers: &[Checker],
) -> SeedResult {
    let mut w =
        World::with_clocks(seed, cfg.nodes, cfg.cores, cfg.trace, wl.cluster(), wl.clocks());
    (w.net.params.reorder, w.net.params.duplicate) = cfg.faults.net_disorder();
    let window = cfg.sim_time.saturating_since(HEAL_PERIOD);
    let rate = f64::from(cfg.fault_rate_pct) / 100.0;
    let plan = faults::schedule(cfg.faults, cfg.nodes, cfg.cores, window, rate, &mut w.rng);
    let faulty = !plan.is_empty();
    for (at, a) in plan {
        w.schedule(at, Event::Fault(a));
    }
    if faulty {
        w.schedule(window, Event::Heal);
    }
    wl.start(&mut w);
    let mut events = 0u64;
    let mut restarting = vec![false; cfg.nodes as usize];
    let (mut instant, mut same) = (Nanos::ZERO, 0u64);
    let same_instant_limit = SAME_INSTANT_LIMIT.min(cfg.steps / 2);
    let verdict = loop {
        let Some((at, seq, ev)) = w.pop() else { break Verdict::Pass };
        if at > cfg.sim_time || events >= cfg.steps || cfg.until_event.is_some_and(|n| events >= n)
        {
            break Verdict::Pass;
        }
        events += 1;
        if at == instant {
            same += 1;
            if same > same_instant_limit {
                let detail = format!("{same_instant_limit} events at {at:?} without time moving");
                break Verdict::Fail(Violation::liveness("progress", detail));
            }
        } else {
            (instant, same) = (at, 0);
        }
        let (kind, sum) = (ev.kind(), summary(&ev));
        let node = match &ev {
            Event::Disk { node, .. } | Event::Tick { node, .. } | Event::Restart { node } => *node,
            _ => 0,
        };
        w.trace.record(at, seq, kind, node, &sum);
        if let Err(v) = dispatch(&mut w, wl.as_mut(), ev, &mut restarting) {
            break Verdict::Fail(v);
        }
        if let Some(v) = checkers.iter().filter_map(|c| c.on_step).find_map(|f| f(&w).err()) {
            break Verdict::Fail(v);
        }
    };
    let group0_count = w.group0_bootstrapped();
    // At the end: every node crashes and recovers, and what was acknowledged and liveness are checked.
    let verdict = match verdict {
        Verdict::Pass if cfg.until_event.is_none() => {
            final_check(&mut w, wl.as_mut(), group0_count)
        }
        v => v,
    };
    let verdict = match (verdict, wl.history()) {
        (Verdict::Pass, Some(h)) => {
            match checkers.iter().filter_map(|c| c.on_history).find_map(|f| f(h).err()) {
                Some(v) => Verdict::Fail(v),
                None => Verdict::Pass,
            }
        }
        (v, _) => v,
    };
    let sim_time = w.now();
    let lost = w.nodes.iter().filter(|n| n.lost).map(|n| n.id).collect();
    let group0_bootstrapped = group0_count;
    let history = if cfg.history { wl.history().map(|h| h.to_jsonl()) } else { None };
    let (trace_hash, trace_lines) =
        std::mem::replace(&mut w.trace, trace::Trace::new(false)).finish();
    SeedResult {
        seed,
        workload: cfg.workload.clone(),
        events,
        sim_time,
        trace_hash,
        verdict,
        trace_lines,
        lost,
        history,
        group0_bootstrapped,
    }
}

fn final_check(w: &mut World, wl: &mut dyn workloads::Workload, group0: u32) -> Verdict {
    for node in 0..w.nodes.len() as u32 {
        if w.nodes[node as usize].alive {
            wl.on_crash(w, node);
            w.crash_node(node);
        }
        // A node that lost its disk stays out (research R3c).
        if w.nodes[node as usize].lost {
            continue;
        }
        // A core the fault damaged just before the end takes its node out here.
        match restart(w, node) {
            Err(v) => return Verdict::Fail(v),
            Ok(false) => continue,
            Ok(true) => {}
        }
        if let Err(v) = wl.on_restart(w, node) {
            return Verdict::Fail(v);
        }
    }
    if group0 > 0
        && let Err(v) = group0_survives(w, wl)
    {
        return Verdict::Fail(v);
    }
    match wl.finish(w) {
        Ok(()) => Verdict::Pass,
        Err(v) => Verdict::Fail(v),
    }
}

/// Time the cluster gets after the final restarts to rebuild Group 0 from its logs.
pub const GROUP0_SETTLE: Nanos = Nanos::from_secs(5);

/// Durability of Group 0 (002 T012): once created it survives every node's crash and restart.
/// The cluster runs, without new `Bootstrap`s, until every node that is not out has it again from
/// its log or the others', or [`GROUP0_SETTLE`] goes by.
fn group0_survives(w: &mut World, wl: &mut dyn workloads::Workload) -> Result<(), Violation> {
    w.close_bootstrap();
    let end = w.now() + GROUP0_SETTLE;
    let mut restarting = vec![false; w.nodes.len()];
    let expected = w.nodes.iter().filter(|n| !n.lost).count() as u32;
    while w.group0_bootstrapped() < expected {
        match w.pop() {
            Some((at, _, ev)) if at <= end => dispatch(w, wl, ev, &mut restarting)?,
            _ => {
                let detail = format!(
                    "Group 0 on {} of {expected} nodes {} ms after the final restart",
                    w.group0_bootstrapped(),
                    GROUP0_SETTLE.as_millis()
                );
                return Err(Violation::safety("durability", detail));
            }
        }
    }
    Ok(())
}

fn dispatch(
    w: &mut World,
    wl: &mut dyn workloads::Workload,
    ev: Event,
    restarting: &mut [bool],
) -> Result<(), Violation> {
    match ev {
        Event::Disk { node, core, life } => {
            let frozen = w.nodes[node as usize].cores[core as usize].frozen_until;
            if frozen > w.now() && w.nodes[node as usize].life == life {
                // The core is frozen: it handles the result when it thaws.
                w.schedule(frozen, Event::Disk { node, core, life });
                return Ok(());
            }
            if let Some(c) = w.on_disk(node, core, life) {
                wl.on_completions(w, node, core, c);
                restart_if_failed(w, node, core, restarting);
            }
        }
        Event::Tick { node, core, life } => {
            if let Some(c) = w.on_tick(node, core, life) {
                wl.on_completions(w, node, core, c);
                restart_if_failed(w, node, core, restarting);
            }
        }
        Event::Client { client } => wl.on_client(w, client),
        Event::Deliver { from, to, core, epoch, frame } => {
            let frozen = w.nodes[to as usize].cores[core as usize].frozen_until;
            if frozen > w.now() {
                // A frozen core reads its socket when it thaws.
                w.schedule(frozen, Event::Deliver { from, to, core, epoch, frame });
                return Ok(());
            }
            w.on_deliver(from, to, core, epoch, &frame);
            if w.edge_cut.is_some_and(|(n, ..)| n == to) && answers_heartbeats(&frame) {
                edge_cut(w, wl);
            }
            if w.nodes[to as usize].alive {
                wl.on_completions(w, to, core, teseladb_raftlog::Completions::default());
                // A frame can stop a group (an entry or a snapshot it cannot take).
                restart_if_failed(w, to, core, restarting);
            }
        }
        Event::Fault(a) => apply_fault(w, wl, a),
        Event::FaultEnd(a) => end_fault(w, a),
        Event::Restart { node } => {
            // The process waits until the wall clock passes the HLC bound on disk (005 FR-003).
            if let Some(at) = w.start_wait(node) {
                w.schedule(at, Event::Restart { node });
                return Ok(());
            }
            restarting[node as usize] = false;
            let n = &w.nodes[node as usize];
            if !n.alive && !n.lost && restart(w, node)? {
                wl.on_restart(w, node)?;
                // The workload's start of the node (an LSM's recovery) may have taken a disk
                // error the plan left there and stopped a core that has nothing more for its disk:
                // its supervisor sees it now, as after any other event of the core.
                for core in 0..w.nodes[node as usize].cores.len() as u16 {
                    restart_if_failed(w, node, core, restarting);
                }
            }
        }
        Event::Heal => {
            w.healed = true;
            w.net.heal();
            for n in &mut w.nodes {
                n.clock.resync();
                n.disk_slow = 1;
                for c in &mut n.cores {
                    c.disk.set_capacity(u64::MAX);
                }
            }
            wl.on_heal(w);
        }
        Event::CheckPoint => {}
    }
    match w.violation.take() {
        Some(v) => Err(v),
        None => Ok(()),
    }
}

/// Starts `node`: `Ok(false)` if its recovery found the core damaged by `CorruptCore`, which
/// takes it out for good, or failed on an I/O error the plan injected (a `SyncError` aimed at the
/// node while it was down) or on a full disk (a `DiskFull` that holds it): the process exits and
/// its supervisor starts it again, as the server does when its recovery fails. A damage it
/// accepts, or any other error, is a violation.
fn restart(w: &mut World, node: u32) -> Result<bool, Violation> {
    let corrupted = std::mem::take(&mut w.nodes[node as usize].corrupted);
    let injected = injected_errors(w, node);
    match w.start_node(node) {
        Err(teseladb_raftlog::StorageError::Corrupt { .. }) if corrupted => {
            w.nodes[node as usize].lost = true;
            Ok(false)
        }
        Err(e) if start_fails_again(&e, injected_errors(w, node) > injected) => {
            w.crash_node(node);
            w.nodes[node as usize].corrupted = corrupted;
            let at = w.now() + FAILED_RESTART;
            w.schedule(at, Event::Restart { node });
            Ok(false)
        }
        Err(e) => Err(Violation::safety("recovery", format!("n{node}: {e}"))),
        Ok(()) if corrupted => Err(Violation::safety(
            "recovery",
            format!("n{node}: a damaged WAL header was accepted"),
        )),
        Ok(()) => Ok(true),
    }
}

/// A failed start the supervisor retries: on a full disk, or on an I/O error the plan injected
/// (`injected`).
fn start_fails_again(e: &teseladb_raftlog::StorageError, injected: bool) -> bool {
    use teseladb_raftlog::{IoErrno, StorageError};
    match e {
        StorageError::DiskFull | StorageError::Io { errno: IoErrno::NoSpace, .. } => true,
        StorageError::Io { .. } => injected,
        _ => false,
    }
}

/// Operations of `node`'s disks that failed with an injected error so far.
fn injected_errors(w: &World, node: u32) -> u64 {
    w.nodes[node as usize].cores.iter().map(|c| c.disk.stats().injected_errors).sum()
}

/// Damages a WAL segment header of `core` of `node` while it is down (research R3c): only in
/// cluster worlds, where the other replicas keep the data, and on one node per run.
fn corrupt_core(w: &mut World, wl: &mut dyn workloads::Workload, node: u32, core: u16) {
    if !w.cluster_mode || w.nodes.iter().any(|n| n.lost || n.corrupted) {
        return;
    }
    if w.nodes[node as usize].alive {
        wl.on_crash(w, node);
        w.crash_node(node);
        let at = w.now() + CORRUPT_DOWN;
        w.schedule(at, Event::Restart { node });
    }
    let offset = w.rng.range(0, teseladb_raftlog::format::BLOCK as u64);
    let disk = &mut w.nodes[node as usize].cores[core as usize].disk;
    let wal = teseladb_raftlog::Disk::list(disk, &teseladb_raftlog::paths::wal_dir(core));
    if let Some(segment) = wal.unwrap_or_default().into_iter().find(|p| disk.is_durable(p))
        && disk.corrupt(&segment, offset)
    {
        w.nodes[node as usize].corrupted = true;
    }
}

/// A stopped core (fail-stop) does not come back by itself: the supervisor restarts the node's
/// process. It looks after every event of the core (a disk result, a tick, a frame), since a group
/// that stops while handling a frame may leave its core nothing more for the disk.
fn restart_if_failed(w: &mut World, node: u32, core: u16, restarting: &mut [bool]) {
    // The supervisor also restarts a node whose health reports a stopped group.
    let failed = match w.paused_core(node, core) {
        Some(c) => c.store().is_failed() || c.has_stopped_group(),
        None => w.store(node, core).is_some_and(|s| s.is_failed()),
    };
    // A core whose LSM stopped (an I/O error) or found corruption stops too (004 FR-014, FR-015).
    let lsm = &w.nodes[node as usize].cores[core as usize].lsm;
    let failed = failed
        || lsm.as_ref().is_some_and(
            |l| matches!(l.borrow().health(), Err(e) if e != teseladb_lsm::LsmError::ReadOnly),
        );
    if failed && !restarting[node as usize] {
        restarting[node as usize] = true;
        let at = w.now() + FAILED_RESTART;
        w.schedule(at, Event::Fault(FaultAction::Crash { node, down: Nanos::ZERO }));
    }
}

fn apply_fault(w: &mut World, wl: &mut dyn workloads::Workload, a: FaultAction) {
    let now = w.now();
    // Faults aimed at the leader find it now.
    let a = match a {
        FaultAction::CrashLeader { down } => FaultAction::Crash { node: w.busiest_leader(), down },
        FaultAction::IsolateLeader { dur } => {
            FaultAction::Partition { side: vec![w.busiest_leader()], dur }
        }
        FaultAction::RestartFollower => {
            FaultAction::Crash { node: w.follower(), down: Nanos::ZERO }
        }
        FaultAction::LeaseSplit { dur } => {
            // The leader, one follower cut off from it, and the third one restarted now.
            let a = w.busiest_leader();
            let n = w.nodes.len() as u32;
            let b = (a + 1 + w.rng.range(0, u64::from(n) - 1) as u32) % n;
            let third = (0..n).find(|&x| x != a && x != b).expect("three nodes at least");
            // Once the cut-off follower is ready to campaign, the third one restarts without hearing
            // from the leader again, while the lease it gave the leader may still be running: only
            // the refusal to vote right after starting keeps a second leader out.
            let later = now + Nanos::from_millis(w.rng.range(100, 401));
            let rest = dur.saturating_since(later.saturating_since(now));
            // The step comes after the crash and before the restart it schedules.
            w.schedule(later, Event::Fault(FaultAction::Crash { node: third, down: Nanos::ZERO }));
            w.schedule(later, Event::Fault(FaultAction::StepPastBound { node: third }));
            w.schedule(later, Event::Fault(FaultAction::LinkDown { a, b: third, dur: rest }));
            FaultAction::LinkDown { a, b, dur }
        }
        FaultAction::LeaderClockEdge { from_median_ns } => {
            FaultAction::ClockEdge { node: w.busiest_leader(), from_median_ns }
        }
        FaultAction::LeaderLinkDown { dur } => {
            let a = w.busiest_leader();
            let n = w.nodes.len() as u64;
            let b = (u64::from(a) + 1 + w.rng.range(0, n - 1)) % n;
            FaultAction::LinkDown { a, b: b as u32, dur }
        }
        a => a,
    };
    match &a {
        FaultAction::CorruptCore { node, core } => corrupt_core(w, wl, *node, *core),
        FaultAction::Crash { node, down } => {
            if w.nodes[*node as usize].alive {
                wl.on_crash(w, *node);
                w.crash_node(*node);
                w.schedule(now + *down, Event::Restart { node: *node });
            }
        }
        FaultAction::Freeze { node, core, dur } => {
            let c = &mut w.nodes[*node as usize].cores[*core as usize];
            c.frozen_until = c.frozen_until.max(now + *dur);
        }
        FaultAction::DiskSlow { node, factor, dur } => {
            w.nodes[*node as usize].disk_slow = *factor;
            w.schedule(now + *dur, Event::FaultEnd(a.clone()));
        }
        FaultAction::DiskFull { node, dur } => {
            for c in &mut w.nodes[*node as usize].cores {
                let used = c.disk.used();
                c.disk.set_capacity(used);
            }
            w.schedule(now + *dur, Event::FaultEnd(a.clone()));
        }
        FaultAction::SyncError { node } => {
            let core = w.rng.range(0, w.nodes[*node as usize].cores.len() as u64) as usize;
            w.nodes[*node as usize].cores[core]
                .disk
                .inject_error("sync", teseladb_raftlog::IoErrno::Io);
        }
        FaultAction::Partition { side, dur } => {
            let rest: Vec<u32> = (0..w.nodes.len() as u32).filter(|n| !side.contains(n)).collect();
            w.net.partition(&[side.clone(), rest]);
            w.schedule(now + *dur, Event::FaultEnd(a.clone()));
        }
        FaultAction::Loss { p_percent, dur } => {
            w.net.params.loss = f64::from(*p_percent) / 100.0;
            w.schedule(now + *dur, Event::FaultEnd(a.clone()));
        }
        FaultAction::Cut { a: x, b } => w.net.cut(*x, *b),
        FaultAction::LinkDown { a: x, b, dur } => {
            w.net.set_reach(*x, *b, false);
            w.net.set_reach(*b, *x, false);
            w.schedule(now + *dur, Event::FaultEnd(a.clone()));
        }
        FaultAction::ClockJump { node, delta_ns } => w.nodes[*node as usize].clock.jump(*delta_ns),
        FaultAction::ClockEdge { node, from_median_ns } => {
            // After the heal NTP holds every clock and the slew stops.
            if let Some(target) = edge_offset(w, *node, *from_median_ns)
                && !w.healed
            {
                let clock = &mut w.nodes[*node as usize].clock;
                let left = target - clock.offset_ns();
                let step = left.clamp(-EDGE_SLEW_STEP_NS, EDGE_SLEW_STEP_NS);
                clock.jump(step);
                if step != left {
                    w.schedule(now + EDGE_SLEW_EVERY, Event::Fault(a.clone()));
                } else if !w.leads_data(*node) {
                    w.schedule(now + EDGE_WAIT_EVERY, Event::Fault(a.clone()));
                } else {
                    // At the edge and leading, the node is cut off at its next lease renewal.
                    let dur = Nanos::from_millis(w.rng.range(EDGE_CUT_MS.0, EDGE_CUT_MS.1));
                    w.edge_cut = Some((*node, *from_median_ns, dur));
                }
            }
        }
        FaultAction::StepPastBound { node } => {
            let n = &w.nodes[*node as usize];
            let wall = w.wall_ms(*node);
            // After the heal no clock steps: the liveness check counts on the clocks NTP set.
            if let Some(until) = teseladb_hlc::wait_on_start(n.hlc_bound, wall)
                && !n.alive
                && !w.healed
            {
                let ahead_ms = until - wall + STEP_PAST_BOUND_MS;
                w.nodes[*node as usize].clock.jump((ahead_ms * 1_000_000) as i64);
            }
        }
        FaultAction::CrashLeader { .. }
        | FaultAction::IsolateLeader { .. }
        | FaultAction::LeaderClockEdge { .. }
        | FaultAction::LeaderLinkDown { .. }
        | FaultAction::LeaseSplit { .. }
        | FaultAction::RestartFollower => unreachable!("resolved above"),
    }
}

/// Cuts off the node of [`World::edge_cut`] right after a follower renewed its lease, if it still
/// leads: its lease reads go on to the end of the lease at marks ahead of the clock of the leader
/// that follows it, the longest a correct leader can, and past the lease it dies (research R3e).
/// Its clock stays at the edge: once back, it is cut off again the next time it leads.
fn edge_cut(w: &mut World, wl: &mut dyn workloads::Workload) {
    let Some((node, from_median_ns, dur)) = w.edge_cut.take() else { return };
    if w.healed {
        return;
    }
    let now = w.now();
    let again = FaultAction::ClockEdge { node, from_median_ns };
    if !w.leads_data(node) {
        w.schedule(now + EDGE_WAIT_EVERY, Event::Fault(again));
        return;
    }
    w.schedule(now + dur, Event::Fault(again));
    apply_fault(w, wl, FaultAction::Partition { side: vec![node], dur });
    let (lo, hi) = EDGE_DIE_PAST_LEASE_MS;
    let after = RaftConfig::new(NodeId(1)).lease_dur + Nanos::from_millis(w.rng.range(lo, hi));
    if after < dur {
        let down = dur.saturating_since(after);
        w.schedule(now + after, Event::Fault(FaultAction::Crash { node, down }));
    }
}

/// The wall clock offset (ns) that puts `node` `from_median_ns` off the median of every node's
/// clock as NTP keeps it (the lower one with an even count, as the clock rule takes it; jumps
/// left out, since the others stay within ±50 ms, D-044), counting `node` at the far end on that
/// side, which is where it lands; `None` in a world of one node.
pub fn edge_offset(w: &World, node: u32, from_median_ns: i64) -> Option<i64> {
    let mut all: Vec<i64> =
        w.nodes.iter().filter(|n| n.id != node).map(|n| n.clock.wall_offset_ns).collect();
    if all.is_empty() {
        return None;
    }
    all.push(if from_median_ns > 0 { i64::MAX } else { i64::MIN });
    all.sort_unstable();
    Some(all[(all.len() - 1) / 2] + from_median_ns)
}

fn end_fault(w: &mut World, a: FaultAction) {
    match a {
        FaultAction::DiskSlow { node, .. } => w.nodes[node as usize].disk_slow = 1,
        FaultAction::DiskFull { node, .. } => {
            for c in &mut w.nodes[node as usize].cores {
                c.disk.set_capacity(u64::MAX);
            }
        }
        FaultAction::Partition { .. } | FaultAction::Loss { .. } => w.net.heal(),
        FaultAction::LinkDown { a, b, .. } => {
            w.net.set_reach(a, b, true);
            w.net.set_reach(b, a, true);
        }
        _ => {}
    }
}
