//! `LeaderClockEdge` (D-044, precision of 30-10-2026): the `heavy` plan takes the clock of the node
//! that leads the most groups to the edge of the 125 ms margin (D-004) while the others stay within
//! ±50 ms, so that the read cache's 250 ms floor (005 FR-005) is put to the test. The clock slews
//! there in steps the clock rule does not take for a jump, ends inside the margin as the node sees
//! it, and stays until the heal; there, while its node leads, it is cut off right after a lease
//! renewal and dies just past that lease (research R3e of 002); after the heal no clock moves.

use teseladb_hlc::MAX_CLOCK_OFFSET_MS;
use teseladb_raftlog::Completions;
use teseladb_sim::checkers::Violation;
use teseladb_sim::edge_offset;
use teseladb_sim::faults::{FaultAction, FaultPlan};
use teseladb_sim::registry::Registry;
use teseladb_sim::workloads::Workload;
use teseladb_sim::world::{Event, World};
use teseladb_sim::{SimConfig, Verdict, run_workload};
use teseladb_types::Nanos;

/// When the fault comes.
const EDGE_AT: Nanos = Nanos::from_secs(3);
/// When the clock is looked at: the slew is over and the peers heard it for a second.
const LOOK_AT: Nanos = Nanos::from_secs(6);

/// `range-docs` with a [`FaultAction::LeaderClockEdge`] at [`EDGE_AT`].
struct Edge {
    inner: Box<dyn Workload>,
    from_median_ns: i64,
    healed: bool,
    sent: bool,
    /// Offsets of every clock before the fault.
    before: Vec<i64>,
    /// The node whose clock moved, its offset from the median it should have, and its own view of
    /// that offset, at [`LOOK_AT`].
    seen: Option<(u32, i64, Option<i64>)>,
    /// Nodes that died after the fault, in order.
    died: Vec<u32>,
}

impl Workload for Edge {
    fn name(&self) -> &'static str {
        "edge"
    }
    fn cluster(&self) -> bool {
        self.inner.cluster()
    }
    fn start(&mut self, w: &mut World) {
        self.inner.start(w);
    }
    fn on_client(&mut self, w: &mut World, client: u32) {
        let now = w.now();
        if !self.sent && now >= EDGE_AT {
            self.sent = true;
            w.healed = self.healed;
            self.before = w.nodes.iter().map(|n| n.clock.offset_ns()).collect();
            let a = FaultAction::LeaderClockEdge { from_median_ns: self.from_median_ns };
            w.schedule(now, Event::Fault(a));
        }
        if self.seen.is_none() && now >= LOOK_AT {
            let moved = (0..w.nodes.len() as u32)
                .filter(|&n| w.nodes[n as usize].clock.offset_ns() != self.before[n as usize])
                .collect::<Vec<_>>();
            if let [node] = moved[..] {
                let off = w.nodes[node as usize].clock.offset_ns();
                let target = edge_offset(w, node, self.from_median_ns).expect("three nodes");
                // The cores read the node's monotonic clock.
                let mono = w.nodes[node as usize].clock.mono(now);
                let cores = w.nodes[node as usize].cores.len() as u16;
                let view = (0..cores)
                    .find_map(|c| w.cluster_core(node, c).and_then(|c| c.clock_offset(mono)));
                self.seen = Some((node, off - target, view));
            } else {
                assert!(moved.is_empty(), "only one clock moves: {moved:?}");
                self.seen = Some((u32::MAX, 0, None));
            }
        }
        self.inner.on_client(w, client);
    }
    fn on_completions(&mut self, w: &mut World, node: u32, core: u16, c: Completions) {
        self.inner.on_completions(w, node, core, c);
    }
    fn on_crash(&mut self, w: &mut World, node: u32) {
        if self.sent {
            self.died.push(node);
        }
        self.inner.on_crash(w, node);
    }
    fn on_restart(&mut self, w: &mut World, node: u32) -> Result<(), Violation> {
        self.inner.on_restart(w, node)
    }
    fn on_heal(&mut self, w: &mut World) {
        self.inner.on_heal(w);
    }
    fn finish(&mut self, w: &mut World) -> Result<(), Violation> {
        let (node, off, view) = self.seen.expect("looked at the clocks");
        if self.healed {
            assert_eq!(node, u32::MAX, "after the heal no clock moves");
        } else {
            assert_ne!(node, u32::MAX, "the leader's clock moved");
            assert_eq!(off, 0, "it ends at the edge");
            // Its own view counts the frames' flight and, hearing one peer, takes the lower of
            // two clocks as the median; either way it stays inside the margin.
            let view = view.expect("it hears its peers");
            assert!(view.abs() <= MAX_CLOCK_OFFSET_MS, "inside the margin: {view} ms");
            // Leading at the edge, it was cut off and died past its lease.
            assert!(self.died.contains(&node), "the node at the edge died: {:?}", self.died);
        }
        self.inner.finish(w)
    }
}

fn run(from_median_ms: i64, healed: bool) {
    let cfg = SimConfig {
        workload: "range-docs".into(),
        faults: FaultPlan::None,
        ..SimConfig::default()
    };
    for seed in 1..=3 {
        let inner = Registry::builtin().workload("range-docs", 3, 2).expect("registered");
        let wl = Edge {
            inner,
            from_median_ns: from_median_ms * 1_000_000,
            healed,
            sent: false,
            before: Vec::new(),
            seen: None,
            died: Vec::new(),
        };
        let r = run_workload(&cfg, seed, Box::new(wl));
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
    }
}

#[test]
fn a_leaders_clock_ahead_slews_to_the_edge_and_stays_in_margin() {
    run(120, false);
}

#[test]
fn a_leaders_clock_behind_slews_to_the_edge_and_stays_in_margin() {
    run(-110, false);
}

#[test]
fn after_the_heal_the_clock_does_not_go_to_the_edge() {
    run(120, true);
}
