//! How a seed's verdict is reached: a violation found while the events run, one found by the
//! final restart of every node, and a run whose events end before the simulated time does.

use teseladb_raftlog::Completions;
use teseladb_sim::checkers::Violation;
use teseladb_sim::faults::FaultPlan;
use teseladb_sim::workloads::Workload;
use teseladb_sim::world::{Event, World};
use teseladb_sim::{SimConfig, Verdict, run_workload};

/// A workload with no work: node 1 is down from the start, a restart of a live node is asked for,
/// and the client (if any) reports a violation.
struct Probe {
    client: bool,
}

impl Workload for Probe {
    fn name(&self) -> &'static str {
        "probe"
    }
    fn cluster(&self) -> bool {
        false
    }
    fn start(&mut self, w: &mut World) {
        w.crash_node(1);
        w.schedule(w.now(), Event::Restart { node: 0 });
        if self.client {
            w.schedule(w.now(), Event::Client { client: 0 });
        }
    }
    fn on_client(&mut self, w: &mut World, _: u32) {
        w.violation = Some(Violation::safety("probe", "seen by the client"));
    }
    fn on_completions(&mut self, _: &mut World, _: u32, _: u16, _: Completions) {}
    fn on_crash(&mut self, _: &mut World, _: u32) {}
    fn on_restart(&mut self, _: &mut World, node: u32) -> Result<(), Violation> {
        if node == 2 { Err(Violation::safety("probe", "lost on node 2")) } else { Ok(()) }
    }
    fn on_heal(&mut self, _: &mut World) {}
    fn finish(&mut self, _: &mut World) -> Result<(), Violation> {
        Ok(())
    }
}

fn cfg() -> SimConfig {
    SimConfig { workload: "probe".into(), faults: FaultPlan::None, ..SimConfig::default() }
}

#[test]
fn the_final_restart_reports_what_a_node_lost() {
    // The live node 0 is not restarted by the event; the run ends when the events do.
    let r = run_workload(&cfg(), 1, Box::new(Probe { client: false }));
    let Verdict::Fail(v) = r.verdict else { panic!("{:?}", r.verdict) };
    assert_eq!((v.checker, v.detail.as_str(), v.liveness), ("probe", "lost on node 2", false));
    assert!(r.sim_time < cfg().sim_time);
}

#[test]
fn a_violation_seen_while_running_ends_the_seed() {
    let r = run_workload(&cfg(), 1, Box::new(Probe { client: true }));
    let Verdict::Fail(v) = r.verdict else { panic!("{:?}", r.verdict) };
    assert_eq!(v.detail, "seen by the client");
}
