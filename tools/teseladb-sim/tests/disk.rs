//! The simulated disk inside the simulated node (002 FR-003): a crash tears the unsynced sectors
//! of the last write, a full disk refuses writes until space comes back, an injected I/O error
//! stops the core until the supervisor restarts the node, and a slow disk stretches its latency.
//! The workload's checkers must pass through all of it.

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::Around;
use teseladb_sim::faults::{FaultAction, FaultPlan};
use teseladb_sim::registry::Registry;
use teseladb_sim::world::{Event, World};
use teseladb_sim::{SimConfig, Verdict, run_workload};
use teseladb_raftlog::SimDiskStats;
use teseladb_types::Nanos;

/// Every disk's counters at the end, by node.
type Seen = Rc<RefCell<Vec<SimDiskStats>>>;

fn sum(stats: &[SimDiskStats], f: impl Fn(&SimDiskStats) -> u64) -> u64 {
    stats.iter().map(f).sum()
}

/// `storage` with `fault` at one second, and its disks' counters per node at the end.
fn storage(
    seed: u64,
    faults: FaultPlan,
    fault: Option<FaultAction>,
) -> (Verdict, Vec<SimDiskStats>, Vec<u64>) {
    let seen: Seen = Rc::default();
    let lives: Rc<RefCell<Vec<u64>>> = Rc::default();
    let mut wl = Around::new(Registry::builtin().workload("storage", 3, 2).unwrap());
    wl.before = Box::new(move |w: &mut World| {
        if let Some(a) = fault.clone() {
            w.schedule(Nanos::from_secs(1), Event::Fault(a));
        }
    });
    let (s, l) = (seen.clone(), lives.clone());
    wl.after = Box::new(move |w: &mut World| {
        let per_node = w.nodes.iter().map(|n| {
            let mut t = SimDiskStats::default();
            for d in n.disks() {
                let d = d.stats();
                t.ops += d.ops;
                t.crashes += d.crashes;
                t.torn_sectors += d.torn_sectors;
                t.no_space += d.no_space;
                t.injected_errors += d.injected_errors;
            }
            t
        });
        *s.borrow_mut() = per_node.collect();
        *l.borrow_mut() = w.nodes.iter().map(|n| n.life).collect();
    });
    let cfg = SimConfig { workload: "storage".into(), faults, ..SimConfig::default() };
    let r = run_workload(&cfg, seed, Box::new(wl));
    let (stats, lives) = (seen.borrow().clone(), lives.borrow().clone());
    (r.verdict, stats, lives)
}

#[test]
fn crashes_tear_unsynced_sectors_and_recovery_gets_past_them() {
    let mut torn = 0;
    for seed in 1..=10 {
        let (v, stats, _) = storage(seed, FaultPlan::Default, None);
        assert_eq!(v, Verdict::Pass, "seed {seed}");
        assert!(sum(&stats, |s| s.crashes) > 0, "seed {seed}");
        torn += sum(&stats, |s| s.torn_sectors);
    }
    assert!(torn > 0, "some crash tore a write");
}

#[test]
fn a_full_disk_refuses_writes_until_space_comes_back() {
    let full = FaultAction::DiskFull { node: 0, dur: Nanos::from_millis(500) };
    let (v, stats, _) = storage(3, FaultPlan::None, Some(full));
    assert_eq!(v, Verdict::Pass);
    assert!(stats[0].no_space > 0, "{:?}", stats[0]);
    assert!(stats[1].no_space == 0 && stats[2].no_space == 0);
}

#[test]
fn an_io_error_stops_the_core_until_the_node_restarts() {
    let (v, stats, lives) = storage(4, FaultPlan::None, Some(FaultAction::SyncError { node: 0 }));
    assert_eq!(v, Verdict::Pass);
    assert_eq!(stats[0].injected_errors, 1, "{:?}", stats[0]);
    // The supervisor restarted node 0 once more than the others.
    assert_eq!(lives[0], lives[1] + 1, "{lives:?}");
}

#[test]
fn a_slow_disk_gets_through_fewer_operations() {
    let slow = FaultAction::DiskSlow { node: 0, factor: 50, dur: Nanos::from_secs(30) };
    let (v, stats, _) = storage(5, FaultPlan::None, Some(slow));
    let (_, normal, _) = storage(5, FaultPlan::None, None);
    assert_eq!(v, Verdict::Pass);
    // Not 50 times fewer: the engine puts more writes in each sync while its clients wait.
    assert!(stats[0].ops * 2 < normal[0].ops, "{} against {}", stats[0].ops, normal[0].ops);
    // The others keep their pace.
    assert!(stats[1].ops * 10 > normal[1].ops * 9, "{} against {}", stats[1].ops, normal[1].ops);
}
