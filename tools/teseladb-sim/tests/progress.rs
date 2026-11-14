//! A seed whose simulated time stops moving fails instead of running forever.

use teseladb_raftlog::Completions;
use teseladb_sim::checkers::Violation;
use teseladb_sim::faults::FaultPlan;
use teseladb_sim::workloads::Workload;
use teseladb_sim::world::{Event, World};
use teseladb_sim::{SAME_INSTANT_LIMIT, SimConfig, Verdict, run_workload};

/// A client that takes its next turn at the same instant, for ever.
struct Spin;

impl Workload for Spin {
    fn name(&self) -> &'static str {
        "spin"
    }
    fn cluster(&self) -> bool {
        false
    }
    fn start(&mut self, w: &mut World) {
        w.schedule(w.now(), Event::Client { client: 0 });
    }
    fn on_client(&mut self, w: &mut World, client: u32) {
        w.schedule(w.now(), Event::Client { client });
    }
    fn on_completions(&mut self, _: &mut World, _: u32, _: u16, _: Completions) {}
    fn on_crash(&mut self, _: &mut World, _: u32) {}
    fn on_restart(&mut self, _: &mut World, _: u32) -> Result<(), Violation> {
        Ok(())
    }
    fn on_heal(&mut self, _: &mut World) {}
    fn finish(&mut self, _: &mut World) -> Result<(), Violation> {
        Ok(())
    }
}

#[test]
fn a_seed_whose_time_stops_moving_fails() {
    let cfg = SimConfig { faults: FaultPlan::None, ..SimConfig::default() };
    let r = run_workload(&cfg, 1, Box::new(Spin));
    let Verdict::Fail(v) = r.verdict else { panic!("{:?}", r.verdict) };
    assert_eq!((v.checker, v.liveness), ("progress", true));
    assert!(r.events > SAME_INSTANT_LIMIT);
}

#[test]
fn a_small_step_budget_still_catches_a_livelock() {
    // Half of the budget at one instant is already a livelock: it fails before the budget ends.
    let cfg = SimConfig { faults: FaultPlan::None, steps: 1_000, ..SimConfig::default() };
    let r = run_workload(&cfg, 1, Box::new(Spin));
    let Verdict::Fail(v) = r.verdict else { panic!("{:?}", r.verdict) };
    assert_eq!(v.checker, "progress");
    assert!(v.detail.starts_with("500 events at"), "{v:?}");
}
