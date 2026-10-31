//! The simulated node's state machines (002 T002): every kind applies, snapshots and restores
//! through `NodeSm`, the registry builds the registered ones by name, and a cluster world runs
//! Group 0 on core 0 of every node next to the `testkv` groups, which has to survive the final
//! crash and restart of every node.

mod common;

use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_multiraft::{ApplyCtx, ApplyError, RestoreError, StateMachine};
use teseladb_sim::faults::FaultPlan;
use teseladb_sim::node::{NodeOutput, RegisteredSm, Registry, SmKind};
use teseladb_sim::{SimConfig, Verdict, run_seed};
use teseladb_testkv::Command;
use teseladb_types::{Bytes, GroupId};

/// Counts the entries it applies; its snapshot is the count and the applied index.
#[derive(Default)]
struct Counter {
    n: u64,
    applied: u64,
}

impl RegisteredSm for Counter {
    fn apply(&mut self, _: &[u8], ctx: &ApplyCtx) -> Result<Bytes, ApplyError> {
        self.n += 1;
        self.applied = ctx.index;
        Ok(Bytes::from_vec(self.n.to_le_bytes().to_vec()))
    }
    fn skip(&mut self, index: u64) {
        self.applied = index;
    }
    fn snapshot(&self) -> Vec<u8> {
        [self.n.to_le_bytes(), self.applied.to_le_bytes()].concat()
    }
    fn restore(&mut self, snap: &[u8]) -> Result<(), RestoreError> {
        if snap.len() != 16 {
            return Err(RestoreError("16 bytes".into()));
        }
        self.n = u64::from_le_bytes(snap[..8].try_into().unwrap());
        self.applied = u64::from_le_bytes(snap[8..].try_into().unwrap());
        Ok(())
    }
    fn applied_index(&self) -> u64 {
        self.applied
    }
}

fn counter() -> Box<dyn RegisteredSm> {
    Box::new(Counter::default())
}

fn ctx(index: u64) -> ApplyCtx {
    ApplyCtx {
        group: GroupId(9),
        index,
        term: 1,
        ts: teseladb_hlc::Hlc { physical: 0, logical: 0 },
    }
}

/// Applies, skips, snapshots into a fresh one of the same kind and restores.
fn round_trip(reg: &Registry, kind: SmKind, payload: &[u8]) -> NodeOutput {
    let mut sm = reg.make(kind).unwrap();
    let out = sm.apply(payload, &ctx(1)).unwrap();
    sm.skip(2);
    assert_eq!(sm.applied_index(), 2);
    let mut again = reg.make(kind).unwrap();
    again.restore(&sm.snapshot()).unwrap();
    assert_eq!(again.applied_index(), 2);
    assert_eq!(again.snapshot(), sm.snapshot());
    out
}

#[test]
fn every_kind_applies_snapshots_and_restores_through_the_enum() {
    let mut reg = Registry::default();
    reg.register("counter", counter).unwrap();
    assert!(reg.register("counter", counter).unwrap_err().contains("already registered"));
    assert_eq!(reg.names(), ["counter"]);
    let boot = Group0Command::Bootstrap {
        cluster_id: [1; 16],
        first_node: NodeAddrs::default(),
        cert_fingerprint: [0; 32],
    };
    assert!(matches!(round_trip(&reg, SmKind::Group0, &boot.encode()), NodeOutput::Group0(_)));
    let put = Command::Cas { key: b"k".to_vec(), expected: None, new: Some(b"v".to_vec()) };
    assert!(matches!(round_trip(&reg, SmKind::Kv, &put.encode()), NodeOutput::Kv(_)));
    assert_eq!(
        round_trip(&reg, SmKind::Registered("counter"), b"x"),
        NodeOutput::Bytes(Bytes::from_vec(1u64.to_le_bytes().to_vec()))
    );
    assert!(reg.make(SmKind::Registered("nobody")).is_none());
    // Each kind refuses what is not its own.
    let mut g0 = reg.make(SmKind::Group0).unwrap();
    assert!(g0.apply(&[0xff], &ctx(1)).is_err() && g0.restore(&[0xff]).is_err());
    let mut kv = reg.make(SmKind::Kv).unwrap();
    assert!(kv.apply(&[0xff], &ctx(1)).is_err());
    let mut c = reg.make(SmKind::Registered("counter")).unwrap();
    assert!(c.restore(b"short").is_err());
}

#[test]
fn accessors_and_debug_name_the_kind() {
    let mut reg = Registry::default();
    reg.register("counter", counter).unwrap();
    let g0 = reg.make(SmKind::Group0).unwrap();
    let kv = reg.make(SmKind::Kv).unwrap();
    let r = reg.make(SmKind::Registered("counter")).unwrap();
    assert!(g0.group0().is_some() && g0.kv().is_none());
    assert!(kv.kv().is_some() && kv.group0().is_none());
    assert!(r.group0().is_none() && r.kv().is_none());
    assert_eq!(format!("{g0:?} {kv:?} {r:?}"), "Group0 Kv Registered");
}

#[test]
fn a_cluster_world_runs_group0_on_core_0_of_every_node() {
    for plan in [FaultPlan::None, FaultPlan::Heavy] {
        let cfg = SimConfig { workload: "testkv".into(), faults: plan, ..SimConfig::default() };
        for seed in 1..=5 {
            let r = run_seed(&cfg, seed);
            assert_eq!(r.verdict, Verdict::Pass, "seed {seed} {plan:?}");
            // Group 0 was created by node 1 and every node that is not out applied it.
            assert_eq!(r.group0_bootstrapped, 3 - r.lost.len() as u32, "seed {seed} {plan:?}");
        }
    }
}

#[test]
fn group0_lost_by_every_node_fails_the_seed_as_durability() {
    use teseladb_raftlog::{SimDisk, SimDiskConfig};
    let cfg =
        SimConfig { workload: "testkv".into(), faults: FaultPlan::None, ..SimConfig::default() };
    let kv = teseladb_sim::registry::Registry::builtin().workload("testkv", 3, 2).unwrap();
    let mut wl = common::Around::new(kv);
    // Without faults the only crashes are the final ones: every disk comes back empty. The empty
    // nodes elect again in terms that already had a leader, which the Raft checker reports first;
    // forgetting what it saw leaves the Group 0 check alone in front of the loss.
    wl.crashing = Box::new(|w, node| {
        for c in &mut w.nodes[node as usize].cores {
            c.disk = SimDisk::new(SimDiskConfig::default());
        }
        w.raft_safety = Default::default();
    });
    let r = teseladb_sim::run_workload(&cfg, 3, Box::new(wl));
    let Verdict::Fail(v) = r.verdict else { panic!("{:?}", r.verdict) };
    assert_eq!(v.checker, "durability");
    assert!(v.detail.contains("Group 0 on 0 of 3 nodes"), "{}", v.detail);
    // The same seed without the damage passes.
    assert_eq!(run_seed(&cfg, 3).verdict, Verdict::Pass);
}
