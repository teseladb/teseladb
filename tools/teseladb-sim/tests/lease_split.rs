//! `LeaseSplit` (research R3d): the third node restarts with its wall clock stepped past the HLC
//! bound on its disk, so its start does not wait for the bound (005 FR-003) and only its refusal
//! to vote right after starting keeps a second leader out while the old lease may run. Without
//! the step, the start waits at least 500 ms, longer than any lease, and the faulty variant
//! `vote_after_restart` went undetected. After the heal no clock steps.

use teseladb_sim::checkers::Violation;
use teseladb_sim::faults::{FaultAction, FaultPlan};
use teseladb_sim::workloads::Workload;
use teseladb_sim::workloads::kv::KvWorkload;
use teseladb_sim::world::{Event, World};
use teseladb_sim::{SimConfig, Verdict, run_workload};
use teseladb_raftlog::Completions;
use teseladb_types::Nanos;

/// When node 1 goes down.
const DOWN_AT: Nanos = Nanos::from_secs(3);

/// `testkv` with node 1 crashed at [`DOWN_AT`] and restarted at once, its clock stepped past the
/// bound or not; it notes when the restart ran.
struct Stepped {
    kv: KvWorkload,
    step: bool,
    healed: bool,
    crashed_at: Option<Nanos>,
    restarted_at: Option<Nanos>,
}

impl Workload for Stepped {
    fn name(&self) -> &'static str {
        "stepped"
    }
    fn cluster(&self) -> bool {
        true
    }
    fn start(&mut self, w: &mut World) {
        self.kv.start(w);
    }
    fn on_client(&mut self, w: &mut World, client: u32) {
        if self.crashed_at.is_none() && w.now() >= DOWN_AT && w.nodes[1].alive {
            self.crashed_at = Some(w.now());
            w.healed = self.healed;
            let now = w.now();
            w.schedule(now, Event::Fault(FaultAction::Crash { node: 1, down: Nanos::ZERO }));
            if self.step {
                w.schedule(now, Event::Fault(FaultAction::StepPastBound { node: 1 }));
            }
        }
        self.kv.on_client(w, client);
    }
    fn on_completions(&mut self, w: &mut World, node: u32, core: u16, c: Completions) {
        self.kv.on_completions(w, node, core, c);
    }
    fn on_crash(&mut self, w: &mut World, node: u32) {
        self.kv.on_crash(w, node);
    }
    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        if node == 1 && self.restarted_at.is_none() {
            self.restarted_at = Some(w.now());
        }
        self.kv.on_restart(w, node)
    }
    fn on_heal(&mut self, w: &mut World) {
        self.kv.on_heal(w);
    }
    fn finish(&mut self, w: &mut World) -> Result<(), Violation> {
        let (crashed, restarted) = (self.crashed_at.unwrap(), self.restarted_at.unwrap());
        let waited = restarted.saturating_since(crashed);
        if self.step && !self.healed {
            assert_eq!(waited, Nanos::ZERO, "a clock past the bound starts at once");
        } else {
            assert!(waited >= Nanos::from_millis(500), "it waited {waited:?} for the bound");
        }
        self.kv.finish(w)
    }
}

fn run(step: bool, healed: bool) {
    let cfg =
        SimConfig { workload: "testkv".into(), faults: FaultPlan::None, ..SimConfig::default() };
    for seed in 1..=3 {
        let wl = Stepped {
            kv: KvWorkload::new(3, 2),
            step,
            healed,
            crashed_at: None,
            restarted_at: None,
        };
        let r = run_workload(&cfg, seed, Box::new(wl));
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
    }
}

#[test]
fn a_clock_stepped_past_the_bound_restarts_without_waiting() {
    run(true, false);
}

#[test]
fn without_the_step_the_restart_waits_for_the_bound() {
    run(false, false);
}

#[test]
fn after_the_heal_the_clock_is_not_stepped() {
    run(true, true);
}
