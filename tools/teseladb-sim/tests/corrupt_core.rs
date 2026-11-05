//! A corrupt core (002 T006, research R3c): the damaged WAL header is detected when the node
//! restarts, the node stays out like one that lost its disk, and the majority keeps what was
//! confirmed and goes on answering.

use teseladb_sim::checkers::Violation;
use teseladb_sim::faults::{FaultAction, FaultPlan};
use teseladb_sim::workloads::Workload;
use teseladb_sim::workloads::kv::KvWorkload;
use teseladb_sim::world::{Event, World};
use teseladb_sim::{SimConfig, Verdict, run_workload};
use teseladb_raftlog::Completions;
use teseladb_types::Nanos;

/// `testkv` with corruptions of the given nodes' core 0 at fixed instants, and a hook that runs
/// once, at the first client turn after [`LATER`], for the cases the fault never produces.
struct Corrupting {
    kv: KvWorkload,
    at: Vec<(Nanos, u32)>,
    later: fn(&mut World),
    later_at: Nanos,
    done: bool,
}

/// When the hook runs by default: well after the corruptions of the tests, well before the end.
const LATER: Nanos = Nanos::from_secs(6);
/// Just before the end of a run: the log has no time left to release the newest segment.
const AT_THE_END: Nanos = Nanos::from_millis(19_500);

fn nothing(_: &mut World) {}

fn corrupting(at: Vec<(Nanos, u32)>) -> Corrupting {
    Corrupting { kv: KvWorkload::new(3, 2), at, later: nothing, later_at: LATER, done: false }
}

/// Damages the header of node 0's newest core 0 WAL segment behind the fault's back (a disk that
/// rots by itself); the newest one is still there at the end, older ones may have been released.
fn rot_node0(w: &mut World) {
    let disk = &mut w.nodes[0].cores[0].disk;
    let wal = teseladb_raftlog::Disk::list(disk, &teseladb_raftlog::paths::wal_dir(0)).unwrap();
    let newest = wal.iter().max().expect("a WAL segment").clone();
    assert!(disk.corrupt(&newest, 3));
}

impl Workload for Corrupting {
    fn name(&self) -> &'static str {
        "corrupting"
    }
    fn cluster(&self) -> bool {
        true
    }
    fn start(&mut self, w: &mut World) {
        self.kv.start(w);
        for &(at, node) in &self.at {
            w.schedule(at, Event::Fault(FaultAction::CorruptCore { node, core: 0 }));
        }
    }
    fn on_client(&mut self, w: &mut World, client: u32) {
        if !self.done && w.now() >= self.later_at {
            self.done = true;
            (self.later)(w);
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
        // A workload is told of a restart only once the node runs: a range host takes its core.
        assert!(w.nodes[node as usize].alive, "n{node} is told it restarted while it is down");
        self.kv.on_restart(w, node)
    }
    fn on_heal(&mut self, w: &mut World) {
        self.kv.on_heal(w);
    }
    fn finish(&mut self, w: &mut World) -> Result<(), Violation> {
        self.kv.finish(w)
    }
}

fn cfg() -> SimConfig {
    SimConfig { workload: "testkv".into(), faults: FaultPlan::None, ..SimConfig::default() }
}

#[test]
fn a_corrupt_core_takes_its_node_out_and_the_majority_goes_on() {
    for seed in 1..=20 {
        let wl = corrupting(vec![(Nanos::from_secs(2), 1)]);
        let r = run_workload(&cfg(), seed, Box::new(wl));
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
        assert_eq!(r.lost, vec![1], "seed {seed}: node 1 is out");
    }
}

#[test]
fn only_one_node_per_run_loses_its_disk() {
    let at = vec![(Nanos::from_secs(2), 1), (Nanos::from_secs(4), 2)];
    let r = run_workload(&cfg(), 7, Box::new(corrupting(at)));
    assert_eq!(r.verdict, Verdict::Pass);
    assert_eq!(r.lost, vec![1], "the second corruption is not applied");
}

#[test]
fn a_restart_asked_for_a_lost_node_does_nothing() {
    let mut wl = corrupting(vec![(Nanos::from_secs(2), 1)]);
    wl.later = |w| w.schedule(w.now(), Event::Restart { node: 1 });
    let r = run_workload(&cfg(), 3, Box::new(wl));
    assert_eq!((r.verdict, r.lost), (Verdict::Pass, vec![1]));
}

#[test]
fn a_core_damaged_at_the_end_keeps_its_node_out_of_the_final_restarts() {
    // Range-docs heavy at 200 %: the fault damaged a core just before the end, the final restarts
    // found it and took the node out, and the workload was still told the node restarted.
    let at = AT_THE_END + Nanos::from_millis(300);
    for seed in 1..=5 {
        let r = run_workload(&cfg(), seed, Box::new(corrupting(vec![(at, 1)])));
        assert_eq!((r.verdict, r.lost), (Verdict::Pass, vec![1]), "seed {seed}");
    }
}

#[test]
fn damage_the_fault_did_not_cause_is_a_violation() {
    // While the node runs, just before the end: the final restart finds it.
    let mut wl = corrupting(Vec::new());
    wl.later = rot_node0;
    wl.later_at = AT_THE_END;
    let r = run_workload(&cfg(), 4, Box::new(wl));
    match r.verdict {
        Verdict::Fail(v) => assert_eq!(v.checker, "recovery", "{v:?}"),
        other => panic!("{other:?}"),
    }
    // While it is down after a crash: the restart finds it.
    let mut wl = corrupting(Vec::new());
    wl.later = |w| {
        w.crash_node(0);
        rot_node0(w);
        w.schedule(w.now() + Nanos::from_millis(100), Event::Restart { node: 0 });
    };
    let r = run_workload(&cfg(), 4, Box::new(wl));
    assert!(matches!(&r.verdict, Verdict::Fail(v) if v.checker == "recovery"), "{:?}", r.verdict);
}

#[test]
fn a_damaged_header_that_recovery_accepted_would_be_a_violation() {
    // The mark without the damage stands for a recovery that missed it.
    let mut wl = corrupting(Vec::new());
    wl.later = |w| {
        w.crash_node(2);
        w.nodes[2].corrupted = true;
        w.schedule(w.now() + Nanos::from_millis(100), Event::Restart { node: 2 });
    };
    let r = run_workload(&cfg(), 5, Box::new(wl));
    match r.verdict {
        Verdict::Fail(v) => {
            assert!(v.detail.contains("a damaged WAL header was accepted"), "{v:?}")
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_core_with_nothing_on_disk_cannot_be_damaged() {
    // At the very start nothing is durable yet: the fault changes nothing and the node comes back.
    let r = run_workload(&cfg(), 6, Box::new(corrupting(vec![(Nanos::ZERO, 1)])));
    assert_eq!((r.verdict, r.lost), (Verdict::Pass, Vec::new()));
}

#[test]
fn storage_worlds_never_lose_a_disk() {
    // Without replicas a lost disk is lost data: the fault only applies to cluster worlds.
    let cfg =
        SimConfig { workload: "storage".into(), faults: FaultPlan::None, ..SimConfig::default() };
    let storage = teseladb_sim::registry::Registry::builtin().workload("storage", 3, 2).unwrap();
    let mut r = run_workload(&cfg, 8, storage);
    assert_eq!(r.verdict, Verdict::Pass);
    struct OnStorage(Box<dyn Workload>);
    impl Workload for OnStorage {
        fn name(&self) -> &'static str {
            "on-storage"
        }
        fn cluster(&self) -> bool {
            false
        }
        fn start(&mut self, w: &mut World) {
            self.0.start(w);
            w.schedule(
                Nanos::from_secs(2),
                Event::Fault(FaultAction::CorruptCore { node: 1, core: 0 }),
            );
        }
        fn on_client(&mut self, w: &mut World, c: u32) {
            self.0.on_client(w, c);
        }
        fn on_completions(&mut self, w: &mut World, n: u32, core: u16, c: Completions) {
            self.0.on_completions(w, n, core, c);
        }
        fn on_crash(&mut self, w: &mut World, n: u32) {
            self.0.on_crash(w, n);
        }
        fn on_restart(&mut self, w: &mut World, n: u32) -> Result<(), Violation> {
            self.0.on_restart(w, n)
        }
        fn on_heal(&mut self, w: &mut World) {
            self.0.on_heal(w);
        }
        fn finish(&mut self, w: &mut World) -> Result<(), Violation> {
            self.0.finish(w)
        }
    }
    let wl =
        OnStorage(teseladb_sim::registry::Registry::builtin().workload("storage", 3, 2).unwrap());
    r = run_workload(&cfg, 8, Box::new(wl));
    assert_eq!((r.verdict, r.lost), (Verdict::Pass, Vec::new()));
}

#[test]
fn a_node_already_down_is_damaged_and_found_out_when_it_restarts() {
    let mut wl = corrupting(Vec::new());
    wl.later = |w| {
        w.crash_node(1);
        let now = w.now();
        w.schedule(now, Event::Fault(FaultAction::CorruptCore { node: 1, core: 0 }));
        w.schedule(now + Nanos::from_millis(150), Event::Restart { node: 1 });
    };
    let r = run_workload(&cfg(), 11, Box::new(wl));
    assert_eq!((r.verdict, r.lost), (Verdict::Pass, vec![1]));
}
