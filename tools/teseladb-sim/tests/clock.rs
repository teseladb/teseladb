//! Per-node clocks (002 FR-004, research R3): each node draws its drift and wall offset within the
//! limits its workload asks for, the default ones keep NTP's bounds, a workload can widen them to
//! break the clock rule on purpose, a jump moves one node's wall clock only, and every node ticks
//! at its own monotonic rate.

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use common::{Around, drive, drive_seeing};
use teseladb_sim::clock::{ClockLimits, MAX_DRIFT_PPM, MAX_WALL_OFFSET, SimClock};
use teseladb_sim::faults::FaultPlan;
use teseladb_sim::registry::Registry;
use teseladb_sim::world::{CLUSTER_TICK, Event, World};
use teseladb_sim::{SimConfig, Verdict, run_workload};
use teseladb_types::Nanos;

fn within(c: &SimClock, l: ClockLimits) -> bool {
    c.drift_ppm.abs() <= l.max_drift_ppm && c.wall_offset_ns.unsigned_abs() <= l.max_wall_offset.0
}

#[test]
fn every_node_draws_its_clock_within_its_limits() {
    let wide = ClockLimits { max_drift_ppm: 200_000, max_wall_offset: Nanos::from_secs(2) };
    let mut beyond = 0;
    for seed in 1..=100 {
        let w = World::cluster(seed, 3, 2, false);
        assert!(w.nodes.iter().all(|n| within(&n.clock, ClockLimits::default())), "seed {seed}");
        let w = World::with_clocks(seed, 3, 2, false, true, wide);
        assert!(w.nodes.iter().all(|n| within(&n.clock, wide)), "seed {seed}");
        beyond += w.nodes.iter().filter(|n| !within(&n.clock, ClockLimits::default())).count();
    }
    assert!(beyond > 100, "{beyond} nodes past the default limits");
    assert_eq!(ClockLimits::default().max_drift_ppm, MAX_DRIFT_PPM);
    assert_eq!(ClockLimits::default().max_wall_offset, MAX_WALL_OFFSET);
}

#[test]
fn a_workload_chooses_its_clocks_and_the_run_reproduces_them() {
    let wide = ClockLimits { max_drift_ppm: 0, max_wall_offset: Nanos::from_secs(5) };
    let seen: Rc<RefCell<Vec<SimClock>>> = Rc::default();
    let run = |seen: Rc<RefCell<Vec<SimClock>>>| {
        let mut wl = Around::new(Registry::builtin().workload("testkv", 3, 2).unwrap());
        wl.clocks = wide;
        wl.after = Box::new(move |w: &mut World| {
            seen.borrow_mut().extend(w.nodes.iter().map(|n| n.clock));
        });
        let cfg = SimConfig {
            workload: "testkv".into(),
            faults: FaultPlan::None,
            ..SimConfig::default()
        };
        run_workload(&cfg, 6, Box::new(wl))
    };
    let r = run(seen.clone());
    assert_eq!(r.verdict, Verdict::Pass);
    let first: Vec<SimClock> = seen.borrow_mut().drain(..).collect();
    assert!(first.iter().all(|c| c.drift_ppm == 0 && within(c, wide)), "{first:?}");
    assert!(first.iter().any(|c| !within(c, ClockLimits::default())), "{first:?}");
    let again = run(seen.clone());
    assert_eq!((again.trace_hash, seen.borrow().clone()), (r.trace_hash, first));
}

#[test]
fn a_jump_moves_only_that_nodes_wall_clock() {
    let mut w = World::cluster(7, 3, 2, false);
    let second = Nanos::from_secs(1);
    assert!(drive(&mut w, |w| w.now() >= second));
    let before: Vec<(u64, Nanos)> = (0..3).map(|n| (w.wall_ms(n), w.local(n))).collect();
    w.nodes[1].clock.jump(-3_000_000_000);
    let after: Vec<(u64, Nanos)> = (0..3).map(|n| (w.wall_ms(n), w.local(n))).collect();
    assert_eq!(after[1].0, before[1].0 - 3_000);
    assert_eq!(after[1].1, before[1].1, "the monotonic clock does not jump");
    assert_eq!((after[0], after[2]), (before[0], before[2]));
}

#[test]
fn every_node_ticks_at_its_own_rate() {
    let mut w = World::cluster(8, 3, 1, false);
    let mut ticks = [0u64; 3];
    let span = Nanos::from_secs(20);
    let count = |ev: &Event| {
        if let Event::Tick { node, .. } = ev {
            ticks[*node as usize] += 1;
        }
    };
    assert!(drive_seeing(&mut w, count, |w| w.now() >= span));
    for (n, t) in ticks.iter().enumerate() {
        let local = w.local(n as u32).0;
        let expected = local / CLUSTER_TICK.0;
        assert!(t.abs_diff(expected) <= 2, "node {n}: {t} ticks, {expected} expected");
    }
    // The drifts differ, and so do the tick counts.
    let drifts: Vec<i64> = w.nodes.iter().map(|n| n.clock.drift_ppm).collect();
    let (fast, slow) = (
        drifts.iter().position(|d| d == drifts.iter().max().unwrap()).unwrap(),
        drifts.iter().position(|d| d == drifts.iter().min().unwrap()).unwrap(),
    );
    assert!(ticks[fast] > ticks[slow], "{ticks:?} with drifts {drifts:?}");
}
