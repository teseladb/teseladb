//! Checkers and workloads registered from outside the framework (002 FR-008, FR-010): a checker's
//! `on_step` runs after every event with the nodes' state and its `on_history` at the end with the
//! clients' history; a registered workload runs by name; a checker that fails ends the seed at that
//! step with its name and the trace up to there.

use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_sim::checkers::Violation;
use teseladb_sim::faults::FaultPlan;
use teseladb_sim::history::History;
use teseladb_sim::registry::{Checker, Registry};
use teseladb_sim::workloads::Workload;
use teseladb_sim::world::World;
use teseladb_sim::{Sim, SimConfig, Verdict};

static STEPS: AtomicU64 = AtomicU64::new(0);
static HISTORIES: AtomicU64 = AtomicU64::new(0);

fn count_steps(_: &World) -> Result<(), Violation> {
    STEPS.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

fn count_histories(h: &History) -> Result<(), Violation> {
    assert!(!h.ops.is_empty(), "the history of the run");
    HISTORIES.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// Fails once simulated time passes one second.
fn fails_after_a_second(w: &World) -> Result<(), Violation> {
    if w.now().as_millis() > 1_000 {
        return Err(Violation::safety("late", "simulated time passed one second"));
    }
    Ok(())
}

/// `testkv` under another name, as a later specification would register its own.
fn kv_copy(nodes: u32, cores: u16) -> Box<dyn Workload> {
    teseladb_sim::workloads::kv::KvWorkload::boxed(nodes, cores)
}

fn cfg(workload: &str) -> SimConfig {
    SimConfig { workload: workload.into(), faults: FaultPlan::Default, ..SimConfig::default() }
}

#[test]
fn a_registered_checker_sees_every_step_and_the_history() {
    let mut reg = Registry::builtin();
    reg.register_checker(Checker {
        name: "counter",
        owner: "002",
        on_step: Some(count_steps),
        on_history: Some(count_histories),
    })
    .unwrap();
    reg.register_workload("kv-copy", kv_copy).unwrap();
    let r = Sim::new(reg).run_seed(&cfg("kv-copy"), 3);
    assert_eq!(r.verdict, Verdict::Pass);
    assert_eq!(r.workload, "kv-copy");
    assert_eq!(STEPS.load(Ordering::Relaxed), r.events, "once per event");
    assert_eq!(HISTORIES.load(Ordering::Relaxed), 1);
}

#[test]
fn a_failing_checker_ends_the_seed_at_its_step_with_its_name() {
    let mut reg = Registry::builtin();
    reg.register_checker(Checker {
        name: "late",
        owner: "002",
        on_step: Some(fails_after_a_second),
        on_history: None,
    })
    .unwrap();
    let r = Sim::new(reg).run_seed(&SimConfig { trace: true, ..cfg("testkv") }, 4);
    let Verdict::Fail(v) = &r.verdict else { panic!("{:?}", r.verdict) };
    assert_eq!(v.checker, "late");
    // The trace ends at the failing step.
    assert_eq!(r.trace_lines.as_ref().unwrap().len() as u64, r.events);
    assert!(r.sim_time.as_millis() > 1_000 && r.sim_time.as_millis() < 1_100, "{:?}", r.sim_time);
}

#[test]
fn names_are_unique_and_the_builtin_workloads_are_there() {
    let mut reg = Registry::builtin();
    assert_eq!(
        reg.workload_names(),
        [
            "storage",
            "testkv",
            "lsm",
            "lsm-ingest",
            "range-docs",
            "multirange",
            "splits",
            "follower-reads",
            "relocate",
            "meta2",
            "txn-bank",
            "txn-append",
            "indexes",
            "special-collections",
            "queries"
        ]
    );
    assert!(reg.register_workload("testkv", kv_copy).unwrap_err().contains("already registered"));
    let c = Checker { name: "c", owner: "002", on_step: None, on_history: None };
    reg.register_checker(c).unwrap();
    assert!(reg.register_checker(c).unwrap_err().contains("already registered"));
    assert_eq!(reg.checker_names(), ["c"]);
    assert!(reg.workload("nope", 3, 2).is_none());
    assert_eq!(reg.workload("storage", 3, 2).unwrap().name(), "storage");
}

fn rejects_any_history(_: &History) -> Result<(), Violation> {
    Err(Violation::safety("picky", "no history is good enough"))
}

#[test]
fn a_history_checker_fails_the_seed_and_runs_only_on_workloads_with_history() {
    let mut reg = Registry::builtin();
    let picky = Checker {
        name: "picky",
        owner: "002",
        on_step: None,
        on_history: Some(rejects_any_history),
    };
    reg.register_checker(picky).unwrap();
    let sim = Sim::new(reg);
    let r = sim.run_seed(&cfg("testkv"), 5);
    let Verdict::Fail(v) = r.verdict else { panic!("{:?}", r.verdict) };
    assert_eq!(v.checker, "picky");
    // `storage` has no clients' history: the checker has nothing to look at.
    assert_eq!(sim.run_seed(&cfg("storage"), 5).verdict, Verdict::Pass);
    assert_eq!(sim.registry().checker_names(), ["picky"]);
}
