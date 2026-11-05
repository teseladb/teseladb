//! The supervisor restarts a node whose health reports a stopped group, whatever stopped it: a
//! group can stop while the core handles a frame from the leader (an entry it cannot apply, a
//! snapshot it cannot install), and then its core may have nothing more for its disk.

use teseladb_multiraft::{ApplyCtx, ApplyError, GroupDesc, RestoreError};
use teseladb_sim::checkers::Violation;
use teseladb_sim::faults::FaultPlan;
use teseladb_sim::node::{RegisteredSm, SmKind};
use teseladb_sim::workloads::Workload;
use teseladb_sim::world::{Event, World};
use teseladb_sim::{SimConfig, Verdict, run_workload};
use teseladb_raftlog::Completions;
use teseladb_types::{Bytes, GroupId, Nanos, NodeId};

/// The group, alone on core 1 of each node.
const G: GroupId = GroupId(1);
const KIND: &str = "poisoned";
/// The entry no replica can apply.
const POISON: &[u8] = b"poison";
/// When the workload proposes it.
const POISON_AT: Nanos = Nanos::from_secs(2);
/// How often the workload looks at the nodes.
const LOOK_EVERY: Nanos = Nanos::from_millis(20);
/// Longest a live node may keep a stopped group: the supervisor restarts it 100 ms after it sees
/// it, and sees it within a tick.
const STOPPED_AT_MOST: Nanos = Nanos::from_millis(500);

/// Counts entries; refuses [`POISON`].
#[derive(Default)]
struct Poisoned {
    applied: u64,
}

impl RegisteredSm for Poisoned {
    fn apply(&mut self, payload: &[u8], ctx: &ApplyCtx) -> Result<Bytes, ApplyError> {
        if payload == POISON {
            return Err(ApplyError("poison".into()));
        }
        self.applied = ctx.index;
        Ok(Bytes::from_vec(Vec::new()))
    }

    fn skip(&mut self, index: u64) {
        self.applied = index;
    }

    fn snapshot(&self) -> Vec<u8> {
        self.applied.to_le_bytes().to_vec()
    }

    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError> {
        let word = snap.try_into().map_err(|_| RestoreError("short snapshot".into()))?;
        self.applied = u64::from_le_bytes(word);
        Ok(())
    }

    fn applied_index(&self) -> u64 {
        self.applied
    }
}

fn poisoned() -> Box<dyn RegisteredSm> {
    Box::new(Poisoned::default())
}

/// Proposes [`POISON`] once on the leader and watches how long each live node keeps the group
/// stopped.
struct Watch {
    nodes: u32,
    proposed: bool,
    /// Per node: the life in which its group stopped and since when.
    stopped_since: Vec<Option<(u64, Nanos)>>,
    stops: u32,
    violation: Option<Violation>,
}

impl Watch {
    fn look(&mut self, w: &mut World) {
        let now = w.now();
        for n in 0..self.nodes {
            let stopped = w.nodes[n as usize].alive
                && w.cluster_core(n, 1).is_some_and(|c| c.has_stopped_group());
            // A restarted node applies the poison again and stops at once: each life counts apart.
            let life = w.nodes[n as usize].life;
            let since = &mut self.stopped_since[n as usize];
            match (stopped, *since) {
                (false, _) => *since = None,
                (true, Some((l, _))) if l != life => *since = Some((life, now)),
                (true, None) => {
                    *since = Some((life, now));
                    self.stops += 1;
                }
                (true, Some((_, at))) if now.saturating_since(at) > STOPPED_AT_MOST => {
                    let detail = format!("n{n} kept a stopped group from {at:?} to {now:?}");
                    self.violation.get_or_insert(Violation::safety("supervisor", detail));
                }
                (true, Some(_)) => {}
            }
        }
    }
}

impl Workload for Watch {
    fn name(&self) -> &'static str {
        "poison"
    }

    fn cluster(&self) -> bool {
        true
    }

    fn start(&mut self, w: &mut World) {
        w.registry.register(KIND, poisoned).unwrap();
        let voters: Vec<NodeId> = (1..=u64::from(self.nodes)).map(NodeId).collect();
        w.add_group(GroupDesc { group: G, voters, learners: vec![] }, SmKind::Registered(KIND));
        let at = w.now() + LOOK_EVERY;
        w.schedule(at, Event::Client { client: 0 });
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        self.look(w);
        if !self.proposed && w.now() >= POISON_AT {
            let leader = (0..self.nodes).find(|&n| {
                w.cluster_core(n, 1)
                    .and_then(|c| c.status(G))
                    .is_some_and(|s| s.role == teseladb_raft::Role::Leader)
            });
            if let Some(l) = leader {
                let now = w.local(l);
                let c = w.cluster_core(l, 1).expect("alive");
                c.propose(G, POISON, now).expect("the leader takes it");
                w.settle(l, 1);
                self.proposed = true;
            }
        }
        let at = w.now() + LOOK_EVERY;
        w.schedule(at, Event::Client { client });
    }

    fn on_completions(&mut self, w: &mut World, _: u32, _: u16, _: Completions) {
        w.events.clear();
    }

    fn on_crash(&mut self, _: &mut World, _: u32) {}

    fn on_restart(&mut self, _: &mut World, _: u32) -> Result<(), Violation> {
        Ok(())
    }

    fn on_heal(&mut self, _: &mut World) {}

    fn finish(&mut self, _: &mut World) -> Result<(), Violation> {
        assert!(self.proposed, "the poison was proposed");
        assert!(self.stops > 0, "the poison stopped the group");
        self.violation.take().map_or(Ok(()), Err)
    }
}

#[test]
fn a_group_stopped_by_a_frame_from_the_leader_gets_its_node_restarted() {
    let cfg = SimConfig { faults: FaultPlan::None, ..SimConfig::default() };
    for seed in 1..=3 {
        let wl = Watch {
            nodes: 3,
            proposed: false,
            stopped_since: vec![None; 3],
            stops: 0,
            violation: None,
        };
        let r = run_workload(&cfg, seed, Box::new(wl));
        assert_eq!(r.verdict, Verdict::Pass, "seed {seed}");
    }
}
