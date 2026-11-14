//! Fault plans: `none`, `light` (one crash, partition, loss or restart every 2–5 simulated
//! seconds), `default` (one fault every 0.5–2 simulated seconds during the fault window) and
//! `heavy` (more frequent, with clock jumps, a leader's clock at the edge of the margin and full
//! disks). Each plan also sets how often the network reorders and duplicates messages (002
//! research R3b).

use teseladb_types::{Nanos, Rng};

/// Fault plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultPlan {
    /// No faults.
    None,
    /// Few and mild faults.
    Light,
    /// The default one.
    Default,
    /// More and more varied faults.
    Heavy,
}

impl FaultPlan {
    /// By CLI name.
    pub fn parse(s: &str) -> Option<FaultPlan> {
        match s {
            "none" => Some(FaultPlan::None),
            "light" => Some(FaultPlan::Light),
            "default" => Some(FaultPlan::Default),
            "heavy" => Some(FaultPlan::Heavy),
            _ => None,
        }
    }

    /// Name on the command line.
    pub fn name(self) -> &'static str {
        match self {
            FaultPlan::None => "none",
            FaultPlan::Light => "light",
            FaultPlan::Default => "default",
            FaultPlan::Heavy => "heavy",
        }
    }

    /// Probabilities that the network reorders and duplicates a message.
    pub fn net_disorder(self) -> (f64, f64) {
        match self {
            FaultPlan::None => (0.0, 0.0),
            FaultPlan::Light | FaultPlan::Default => (0.001, 0.001),
            FaultPlan::Heavy => (0.01, 0.01),
        }
    }
}

/// Least and most a clock at the edge of the margin is off the median of the others (D-044):
/// inside the 125 ms of the clock rule (D-004) with room for what its peers' samples count as
/// offset besides the clock (a frame's flight and up to a tick on each side), so that the node
/// keeps leading, and past the 100 ms two clocks within ±50 ms can differ.
pub const EDGE_FROM_MEDIAN_MS: (u64, u64) = (90, 110);

/// A fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FaultAction {
    /// The node crashes (losing what was not synced) and comes back after `down`.
    Crash {
        /// Node.
        node: u32,
        /// Time down.
        down: Nanos,
    },
    /// The node that leads the most groups crashes and comes back after `down` (any node if none
    /// leads).
    CrashLeader {
        /// Time down.
        down: Nanos,
    },
    /// The node that leads the most groups is cut off from the rest for `dur`.
    IsolateLeader {
        /// Duration.
        dur: Nanos,
    },
    /// A node that leads no group restarts its process right away (any node if all lead).
    RestartFollower,
    /// The link between the node that leads the most groups and another node goes down in both
    /// directions for `dur`, while both still reach everyone else (a partial partition).
    LeaderLinkDown {
        /// Duration.
        dur: Nanos,
    },
    /// The link between the node that leads the most groups and one of the others goes down for
    /// `dur`; 100 to 400 ms later the third node restarts cut off from the leader too, with its
    /// wall clock stepped past the HLC bound on its disk while it was down (a clock that NTP steps
    /// at boot), so its start does not wait (005 FR-003). A node that has just started must not
    /// vote while the old leader may still serve lease reads (research R3d).
    LeaseSplit {
        /// Duration.
        dur: Nanos,
    },
    /// A byte of the header of a durable file of `node`'s device log, which holds the WAL of
    /// `core` with the other cores', is damaged while the node is down; its restart must detect
    /// it, and then the node stays out (research R3c). In cluster worlds and on one node per run.
    CorruptCore {
        /// Node.
        node: u32,
        /// Core whose WAL the damaged log holds.
        core: u16,
    },
    /// The link between `a` and `b` is down in both directions for `dur`.
    LinkDown {
        /// One end.
        a: u32,
        /// The other end.
        b: u32,
        /// Duration.
        dur: Nanos,
    },
    /// A core stops running for a while.
    Freeze {
        /// Node.
        node: u32,
        /// Core.
        core: u16,
        /// Duration.
        dur: Nanos,
    },
    /// A node's disk becomes `factor` times slower for a while.
    DiskSlow {
        /// Node.
        node: u32,
        /// Factor.
        factor: u64,
        /// Duration.
        dur: Nanos,
    },
    /// A node's disk runs out of space for a while.
    DiskFull {
        /// Node.
        node: u32,
        /// Duration.
        dur: Nanos,
    },
    /// A node's next sync fails with `EIO`.
    SyncError {
        /// Node.
        node: u32,
    },
    /// Network partition into two sides.
    Partition {
        /// Nodes of one side (the rest, on the other).
        side: Vec<u32>,
        /// Duration.
        dur: Nanos,
    },
    /// Message loss for a while.
    Loss {
        /// Probability.
        p_percent: u8,
        /// Duration.
        dur: Nanos,
    },
    /// Cut of the connections between two nodes.
    Cut {
        /// One.
        a: u32,
        /// The other one.
        b: u32,
    },
    /// The wall clock of a node that is down steps to just past the HLC bound on its disk (part of
    /// [`FaultAction::LeaseSplit`]); nothing happens to a node that is up, nor after the heal.
    StepPastBound {
        /// Node.
        node: u32,
    },
    /// The wall clock of the node that leads the most groups moves to `from_median_ns` off the
    /// median of all the clocks, at the edge of the margin, and stays there until the heal, while
    /// the others stay within ±50 ms (D-044); there, each time the node leads a range of data, it
    /// is cut off from the rest as soon as a follower renews its lease, and dies just past the
    /// lease. Its lease reads go out at marks up to that far ahead of the clock of the leader that
    /// follows it, which only the read cache's floor keeps writes above (005 FR-005).
    LeaderClockEdge {
        /// Signed distance from the median (ns).
        from_median_ns: i64,
    },
    /// [`FaultAction::LeaderClockEdge`] on `node`: its wall clock slews toward the edge in steps
    /// the clock rule does not take for a jump, as NTP slews it (D-004).
    ClockEdge {
        /// Node.
        node: u32,
        /// Signed distance from the median (ns).
        from_median_ns: i64,
    },
    /// Jump of a node's wall clock.
    ClockJump {
        /// Node.
        node: u32,
        /// Jump (ns, signed).
        delta_ns: i64,
    },
}

/// A fault of the `light` plan: a crash, a partition, some loss or a restart.
fn light_fault(nodes: u32, node: u32, rng: &mut Rng) -> FaultAction {
    let ms = |rng: &mut Rng, lo: u64, hi: u64| Nanos::from_millis(rng.range(lo, hi + 1));
    match rng.range(0, 4) {
        0 => FaultAction::Crash { node, down: ms(rng, 50, 500) },
        1 => {
            let mut side: Vec<u32> = (0..nodes).filter(|_| rng.chance(0.5)).collect();
            side.sort_unstable();
            FaultAction::Partition { side, dur: ms(rng, 100, 1000) }
        }
        2 => FaultAction::Loss { p_percent: rng.range(1, 6) as u8, dur: ms(rng, 100, 500) },
        _ => FaultAction::RestartFollower,
    }
}

/// Faults of a plan within `[0, window)`, in time order; `rate` multiplies how often they come (1 =
/// the plan's rhythm, 2 = twice as often).
pub fn schedule(
    plan: FaultPlan,
    nodes: u32,
    cores: u16,
    window: Nanos,
    rate: f64,
    rng: &mut Rng,
) -> Vec<(Nanos, FaultAction)> {
    let (min_gap, max_gap) = match plan {
        FaultPlan::None => return Vec::new(),
        FaultPlan::Light => (Nanos::from_millis(2000), Nanos::from_millis(5000)),
        FaultPlan::Default => (Nanos::from_millis(500), Nanos::from_millis(2000)),
        FaultPlan::Heavy => (Nanos::from_millis(200), Nanos::from_millis(1000)),
    };
    let scale = |g: Nanos| Nanos(((g.0 as f64) / rate).max(1.0) as u64);
    let (min_gap, max_gap) = (scale(min_gap), scale(max_gap));
    let heavy = plan == FaultPlan::Heavy;
    let mut out = Vec::new();
    let mut t = Nanos(rng.range(min_gap.0, max_gap.0 + 1));
    while t < window {
        let node = rng.range(0, u64::from(nodes)) as u32;
        let ms = |rng: &mut Rng, lo: u64, hi: u64| Nanos::from_millis(rng.range(lo, hi + 1));
        if plan == FaultPlan::Light {
            out.push((t, light_fault(nodes, node, rng)));
            t = t + Nanos(rng.range(min_gap.0, max_gap.0 + 1));
            continue;
        }
        // `default` draws its eleven kinds, the corrupt core and the lease split; `heavy` adds clock
        // jumps, full disks and a leader's clock at the edge of the margin.
        let kind = match rng.range(0, if heavy { 17 } else { 13 }) {
            11 if !heavy => 14,
            12 if !heavy => 15,
            k => k,
        };
        let action = match kind {
            0 | 1 | 13 => FaultAction::Crash { node, down: ms(rng, 50, 800) },
            14 => FaultAction::CorruptCore { node, core: rng.range(0, u64::from(cores)) as u16 },
            15 => FaultAction::LeaseSplit { dur: ms(rng, 300, 1500) },
            2 => FaultAction::Freeze {
                node,
                core: rng.range(0, u64::from(cores)) as u16,
                dur: ms(rng, 50, 500),
            },
            3 => FaultAction::DiskSlow { node, factor: rng.range(5, 100), dur: ms(rng, 100, 1000) },
            4 => FaultAction::SyncError { node },
            5 => {
                let mut side: Vec<u32> = (0..nodes).filter(|_| rng.chance(0.5)).collect();
                side.sort_unstable();
                FaultAction::Partition { side, dur: ms(rng, 100, 2000) }
            }
            6 => {
                if rng.chance(0.5) {
                    FaultAction::Loss { p_percent: rng.range(1, 21) as u8, dur: ms(rng, 100, 1000) }
                } else {
                    let b = (node + 1 + rng.range(0, u64::from(nodes.max(2) - 1)) as u32) % nodes;
                    FaultAction::Cut { a: node, b }
                }
            }
            7 => FaultAction::CrashLeader { down: ms(rng, 50, 800) },
            8 => FaultAction::IsolateLeader { dur: ms(rng, 100, 2000) },
            9 => FaultAction::LeaderLinkDown { dur: ms(rng, 100, 2000) },
            10 => FaultAction::RestartFollower,
            11 => {
                let delta = rng.range(0, 4_000_000_001) as i64 - 2_000_000_000;
                FaultAction::ClockJump { node, delta_ns: delta }
            }
            12 => FaultAction::DiskFull { node, dur: ms(rng, 100, 1000) },
            16 => {
                let (lo, hi) = EDGE_FROM_MEDIAN_MS;
                let ns = Nanos::from_millis(rng.range(lo, hi + 1)).0 as i64;
                FaultAction::LeaderClockEdge {
                    from_median_ns: if rng.chance(0.5) { ns } else { -ns },
                }
            }
            _ => unreachable!("a kind in 0..17"),
        };
        out.push((t, action));
        t = t + Nanos(rng.range(min_gap.0, max_gap.0 + 1));
    }
    out
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn plans_by_name_and_rate() {
        assert_eq!(FaultPlan::parse("none"), Some(FaultPlan::None));
        assert_eq!(FaultPlan::parse("default"), Some(FaultPlan::Default));
        assert_eq!(FaultPlan::parse("heavy"), Some(FaultPlan::Heavy));
        assert_eq!(FaultPlan::parse("light"), Some(FaultPlan::Light));
        assert_eq!(FaultPlan::parse("x"), None);
        let mut rng = Rng::new(3);
        let w = Nanos::from_secs(15);
        assert!(schedule(FaultPlan::None, 3, 2, w, 1.0, &mut rng).is_empty());
        let d = schedule(FaultPlan::Default, 3, 2, w, 1.0, &mut rng);
        assert!((6..=31).contains(&d.len()), "{}", d.len());
        assert!(d.windows(2).all(|p| p[0].0 < p[1].0) && d.iter().all(|(t, _)| *t < w));
        let mut kinds = std::collections::HashSet::new();
        for seed in 0..50 {
            let mut rng = Rng::new(seed);
            let plan = schedule(FaultPlan::Heavy, 5, 2, w, 1.0, &mut rng);
            for (_, a) in plan {
                kinds.insert(std::mem::discriminant(&a));
                if let FaultAction::Cut { a, b } = a {
                    assert_ne!(a, b);
                }
            }
        }
        assert_eq!(kinds.len(), 16, "the heavy plan uses the sixteen kinds");
    }

    #[test]
    fn the_light_plan_is_sparse_and_mild() {
        let w = Nanos::from_secs(15);
        for seed in 0..50 {
            let mut rng = Rng::new(seed);
            let plan = schedule(FaultPlan::Light, 3, 2, w, 1.0, &mut rng);
            assert!((2..=7).contains(&plan.len()), "{}", plan.len());
            for (_, a) in plan {
                assert!(
                    matches!(
                        a,
                        FaultAction::Crash { .. }
                            | FaultAction::Partition { .. }
                            | FaultAction::Loss { .. }
                            | FaultAction::RestartFollower
                    ),
                    "{a:?}"
                );
            }
        }
    }

    #[test]
    fn each_plan_has_its_network_reordering_and_duplicates() {
        assert_eq!(FaultPlan::None.net_disorder(), (0.0, 0.0));
        assert_eq!(FaultPlan::Light.net_disorder(), (0.001, 0.001));
        assert_eq!(FaultPlan::Default.net_disorder(), (0.001, 0.001));
        assert_eq!(FaultPlan::Heavy.net_disorder(), (0.01, 0.01));
    }

    #[test]
    fn the_fault_rate_scales_how_often_faults_come() {
        let w = Nanos::from_secs(15);
        let count = |rate: f64| -> usize {
            (0..50)
                .map(|s| schedule(FaultPlan::Default, 3, 2, w, rate, &mut Rng::new(s)).len())
                .sum()
        };
        let (half, once, twice) = (count(0.5), count(1.0), count(2.0));
        assert!(half * 3 < once * 2 && once * 3 < twice * 2, "{half} {once} {twice}");
        assert!(schedule(FaultPlan::None, 3, 2, w, 4.0, &mut Rng::new(1)).is_empty());
    }

    #[test]
    fn only_heavy_takes_a_leaders_clock_to_the_edge_of_the_margin() {
        let w = Nanos::from_secs(15);
        let edges = |plan| -> Vec<i64> {
            (0..100)
                .flat_map(|s| schedule(plan, 3, 2, w, 1.0, &mut Rng::new(s)))
                .filter_map(|(_, a)| match a {
                    FaultAction::LeaderClockEdge { from_median_ns } => Some(from_median_ns),
                    _ => None,
                })
                .collect()
        };
        assert!(edges(FaultPlan::Default).is_empty() && edges(FaultPlan::Light).is_empty());
        let heavy = edges(FaultPlan::Heavy);
        let (lo, hi) = EDGE_FROM_MEDIAN_MS;
        let ms = |d: i64| d.unsigned_abs() / 1_000_000;
        assert!(heavy.iter().all(|d| (lo..=hi).contains(&ms(*d))), "{heavy:?}");
        assert!(heavy.iter().any(|d| *d > 0) && heavy.iter().any(|d| *d < 0), "both ways");
    }

    #[test]
    fn default_and_heavy_split_a_leaders_lease_on_purpose() {
        let w = Nanos::from_secs(15);
        for plan in [FaultPlan::Default, FaultPlan::Heavy] {
            let found = (0..50).any(|s| {
                schedule(plan, 3, 2, w, 1.0, &mut Rng::new(s))
                    .iter()
                    .any(|(_, a)| matches!(a, FaultAction::LeaseSplit { .. }))
            });
            assert!(found, "{plan:?}");
        }
    }
}
