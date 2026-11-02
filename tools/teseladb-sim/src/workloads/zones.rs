//! `zones` workload (018 T002, FR-003, FR-013): Group 0's state and its balancer
//! (`teseladb_meta::balancer::plan`) over a cluster of the run's nodes, with one range of keys at
//! the start on three of them. An administrator changes the zones as `addShardToZone`,
//! `removeShardFromZone` and `updateZoneKeyRange` do (the Group 0 settings of
//! `teseladb_meta::zones`, with the same refusals), and every turn of the balancer applies its
//! decisions to Group 0's table: replica changes (settled at once, as the ranges' leaders do) and
//! the cuts at the edges of the zones' intervals. The fault plan's crashes are the nodes the
//! balancer sees dead.
//!
//! The `zones` checker: no range is ever left without a replica, and once the faults end and the
//! balancer has nothing left to do, no range crosses the edge of an interval and every range
//! inside one has its replicas only on the nodes of its zone.

use std::collections::BTreeMap;

use teseladb_meta::balancer::{Decision, View, plan};
use teseladb_meta::command::{Group0Command, NodeAddrs, NodeState, SettingChange};
use teseladb_meta::group0::{Group0Result, Group0Sm, NodeInfo, RangeDesc, ReplicaRole};
use teseladb_meta::liveness::LiveState;
use teseladb_meta::zones::{Placement, ZoneRange, Zones, shard_key};
use teseladb_raftlog::Completions;
use teseladb_types::{GroupId, Nanos, NodeId};

use crate::checkers::Violation;
use crate::workloads::Workload;
use crate::world::{Event, World};

/// Name of the checker.
pub const CHECKER: &str = "zones";
/// The administrator and the balancer.
const ADMIN: u32 = 0;
const BALANCER: u32 = 1;
/// The zones.
const ZONES: [&str; 2] = ["A", "B"];
/// Keys `\x01 · b` for `b` below this.
const KEYS: u64 = 16;
/// Balancer rounds after the faults end before the end state is checked.
const SETTLE_ROUNDS: usize = 200;
/// The first range.
const FIRST: GroupId = GroupId(10);

fn key(b: u64) -> Vec<u8> {
    vec![1, u8::try_from(b).unwrap_or(u8::MAX)]
}

/// The state of the `zones` workload.
pub struct ZonesWorkload {
    nodes: u32,
    sm: Group0Sm,
    index: u64,
    states: BTreeMap<NodeId, LiveState>,
    seq: u64,
    violation: Option<Violation>,
    healed: bool,
}

impl ZonesWorkload {
    /// The workload over `nodes` nodes.
    pub fn boxed(nodes: u32, _cores: u16) -> Box<dyn Workload> {
        Box::new(Self::new(nodes))
    }

    fn new(nodes: u32) -> ZonesWorkload {
        let mut sm = Group0Sm { cluster_id: [1; 16], ..Group0Sm::default() };
        for i in 1..=u64::from(nodes) {
            let info = NodeInfo {
                id: NodeId(i),
                addrs: NodeAddrs { name: vec![i as u8], cores: 1, ..NodeAddrs::default() },
                state: NodeState::Active,
                joined_index: i,
                cert_fingerprint: [0; 32],
                boot_epoch: 1,
                tags: Vec::new(),
            };
            sm.nodes.insert(NodeId(i), info);
        }
        let replicas = (1..=3.min(u64::from(nodes))).map(|i| (NodeId(i), ReplicaRole::Voter));
        sm.insert_range(RangeDesc {
            group: FIRST,
            start: vec![1],
            end: Vec::new(),
            core: 0,
            replicas: replicas.collect(),
            replica_epoch: 1,
            generation: 1,
            leader_hint: None,
            leader_term: 0,
            pending_split: None,
            settling: false,
        });
        let states = sm.nodes.keys().map(|n| (*n, LiveState::Alive)).collect();
        ZonesWorkload { nodes, sm, index: 100, states, seq: 0, violation: None, healed: false }
    }

    fn apply(&mut self, cmd: &Group0Command) -> Group0Result {
        self.index += 1;
        self.sm.apply_command(self.index, cmd)
    }

    fn zones(&self) -> Zones {
        Zones::from_settings(&self.sm.settings)
    }

    fn set(&mut self, changes: Vec<SettingChange>) {
        self.apply(&Group0Command::SetSettings(changes));
    }

    /// One change of the zones, as the commands make it.
    fn admin(&mut self, w: &mut World) {
        let zone = ZONES[w.rng.range(0, ZONES.len() as u64) as usize];
        let node = NodeId(w.rng.range(1, u64::from(self.nodes) + 1));
        // Weighted so that intervals stay and zones keep fewer nodes than the replicas.
        let pick = match w.rng.range(0, 20) {
            0..=6 => 0,
            7..=9 => 1,
            10..=17 => 2,
            _ => 3,
        };
        // The draws of each change, in the order the seeds have always made them.
        let (mut keys, mut nth) = ((0, 1), 0);
        if pick == 2 {
            let lo = w.rng.range(0, KEYS - 1);
            keys = (lo, w.rng.range(lo + 1, KEYS));
        } else if pick == 3 && !self.zones().ranges.is_empty() {
            nth = w.rng.range(0, self.zones().ranges.len() as u64);
        }
        self.admin_act(pick, zone, node, keys, nth);
    }

    /// An administrator's change: 0 adds `node` to `zone`, 1 takes it out (never the last node of
    /// a zone with ranges), 2 adds the keys `[lo, hi)` to the zone, 3 removes the `nth` range
    /// (modulo their number).
    fn admin_act(&mut self, pick: u8, zone: &str, node: NodeId, (lo, hi): (u64, u64), nth: u64) {
        let z = self.zones();
        match pick {
            0 => {
                if !z.shards.get(&node).is_some_and(|zs| zs.iter().any(|x| x == zone)) {
                    self.seq += 1;
                    let value = Some(self.seq.to_be_bytes().to_vec());
                    let key = shard_key(node, zone);
                    self.set(vec![SettingChange { key, expect: None, value }]);
                }
            }
            1 => {
                let key = shard_key(node, zone);
                let Some(v) = self.sm.settings.get(&key).cloned() else { return };
                let last = z.nodes_of(zone).len() == 1;
                if !(last && z.ranges.iter().any(|r| r.zone == zone)) {
                    self.set(vec![SettingChange { key, expect: Some(v), value: None }]);
                }
            }
            2 => {
                let (s, e) = (key(lo), key(hi));
                let free = z
                    .ranges
                    .iter()
                    .filter_map(|r| r.keys.as_ref())
                    .all(|(rs, re)| e <= *rs || *re <= s);
                if z.exists(zone) && free {
                    self.seq += 1;
                    let r = ZoneRange {
                        ns: "db.c".into(),
                        id: [u8::try_from(self.seq % 256).unwrap_or(0); 12],
                        min: Vec::new(),
                        max: Vec::new(),
                        zone: zone.into(),
                        keys: Some((s, e)),
                    };
                    let taken = self.sm.settings.contains_key(&r.key());
                    if !taken {
                        self.set(vec![SettingChange {
                            key: r.key(),
                            expect: None,
                            value: Some(r.encode()),
                        }]);
                    }
                }
            }
            _ => {
                if z.ranges.is_empty() {
                    return;
                }
                let r = &z.ranges[(nth % z.ranges.len() as u64) as usize];
                let change = SettingChange { key: r.key(), expect: Some(r.encode()), value: None };
                self.set(vec![change]);
            }
        }
    }

    /// One round of the balancer, applied to Group 0's table.
    fn balance(&mut self) -> bool {
        let voters: Vec<NodeId> = self.sm.nodes.keys().copied().collect();
        let decisions = plan(&View {
            group0: &self.sm,
            ranges: &self.sm.ranges,
            voters: &voters,
            learners: &[],
            leader: NodeId(1),
            states: &self.states,
            loads: &BTreeMap::new(),
            conf_in_flight: false,
            skewed: &[],
            local_offset_s: 0,
        });
        let mut moved = false;
        for d in decisions {
            let Decision::Command(cmd) = d else { continue };
            match &cmd {
                Group0Command::SetReplicas { group, .. } => {
                    let group = *group;
                    if !matches!(self.apply(&cmd), Group0Result::Ok) {
                        continue;
                    }
                    moved = true;
                    if let Some(epoch) = self.sm.ranges.get(&group).map(|r| r.replica_epoch) {
                        self.apply(&Group0Command::ReplicasSettled { group, epoch });
                    }
                }
                Group0Command::SplitRange { group, at } => {
                    let (group, at) = (*group, at.clone());
                    if let Group0Result::Range(_) = self.apply(&cmd) {
                        moved = true;
                        let at_index = self.index;
                        self.apply(&Group0Command::CompleteSplit { group, at, at_index });
                    }
                }
                _ => {}
            }
        }
        moved
    }

    /// No range without a replica.
    fn check_replicas(&mut self) {
        if let Some(r) = self.sm.ranges.values().find(|r| r.replicas.is_empty()) {
            let v = Violation::safety(CHECKER, format!("range {} has no replica", r.group.0));
            self.violation.get_or_insert(v);
        }
    }

    /// Every range inside an interval on its zone's nodes, none across an edge.
    fn check_placement(&self) -> Result<(), Violation> {
        let z = self.zones();
        for r in self.sm.ranges.values() {
            match z.placement(&r.start, &r.end) {
                Placement::Anywhere => {}
                Placement::CutAt(at) => {
                    let msg = format!("range {} still crosses an interval at {at:?}", r.group.0);
                    return Err(Violation::liveness(CHECKER, msg));
                }
                Placement::On(nodes) => {
                    let outside: Vec<NodeId> =
                        r.nodes().into_iter().filter(|n| !nodes.contains(n)).collect();
                    if !nodes.is_empty() && !outside.is_empty() {
                        let msg = format!(
                            "range {} [{:?}, {:?}) keeps replicas on {outside:?}, outside its \
                             zone's nodes {nodes:?}",
                            r.group.0, r.start, r.end
                        );
                        return Err(Violation::safety(CHECKER, msg));
                    }
                }
            }
        }
        Ok(())
    }

    fn next_turn(w: &mut World, client: u32) {
        let at = w.now() + Nanos::from_millis(w.rng.range(20, 80));
        w.schedule(at, Event::Client { client });
    }
}

impl Workload for ZonesWorkload {
    fn name(&self) -> &'static str {
        "zones"
    }

    fn cluster(&self) -> bool {
        false
    }

    fn start(&mut self, w: &mut World) {
        for client in [ADMIN, BALANCER] {
            Self::next_turn(w, client);
        }
    }

    fn on_client(&mut self, w: &mut World, client: u32) {
        if client == ADMIN && !self.healed {
            self.admin(w);
        } else if client == BALANCER {
            self.balance();
            self.check_replicas();
        }
        Self::next_turn(w, client);
    }

    fn on_completions(&mut self, _w: &mut World, _node: u32, _core: u16, _c: Completions) {}

    fn on_crash(&mut self, _w: &mut World, node: u32) {
        self.states.insert(NodeId(u64::from(node) + 1), LiveState::Dead);
    }

    fn on_restart(&mut self, _w: &mut World, node: u32) -> Result<(), Violation> {
        self.states.insert(NodeId(u64::from(node) + 1), LiveState::Alive);
        Ok(())
    }

    fn on_heal(&mut self, _w: &mut World) {
        self.healed = true;
    }

    fn finish(&mut self, _w: &mut World) -> Result<(), Violation> {
        if let Some(v) = self.violation.take() {
            return Err(v);
        }
        for s in self.states.values_mut() {
            *s = LiveState::Alive;
        }
        for _ in 0..SETTLE_ROUNDS {
            if !self.balance() {
                break;
            }
            self.check_replicas();
        }
        if let Some(v) = self.violation.take() {
            return Err(v);
        }
        self.check_placement()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_zone(wl: &ZonesWorkload, zone: &str, node: u64) -> bool {
        wl.zones().shards.get(&NodeId(node)).is_some_and(|zs| zs.iter().any(|z| z == zone))
    }

    #[test]
    fn the_last_node_of_a_zone_with_ranges_stays_in_it() {
        let mut wl = ZonesWorkload::new(3);
        // Taking out a node that is not in the zone changes nothing.
        wl.admin_act(1, "A", NodeId(1), (0, 1), 0);
        wl.admin_act(0, "A", NodeId(1), (0, 1), 0);
        wl.admin_act(2, "A", NodeId(1), (0, 4), 0);
        assert_eq!(wl.zones().ranges.len(), 1);
        wl.admin_act(1, "A", NodeId(1), (0, 1), 0);
        assert!(in_zone(&wl, "A", 1), "the last node of a zone with a range");
        wl.admin_act(0, "A", NodeId(2), (0, 1), 0);
        wl.admin_act(1, "A", NodeId(1), (0, 1), 0);
        assert!(!in_zone(&wl, "A", 1) && in_zone(&wl, "A", 2));
        // A range whose identifier is taken is not added; then the ranges go one by one.
        wl.seq = 1;
        wl.admin_act(2, "A", NodeId(1), (8, 9), 0);
        assert_eq!(wl.zones().ranges.len(), 1);
        wl.admin_act(3, "A", NodeId(1), (0, 1), 5);
        assert!(wl.zones().ranges.is_empty());
        wl.admin_act(3, "A", NodeId(1), (0, 1), 5);
    }

    #[test]
    fn a_range_without_replicas_or_outside_its_zone_fails() {
        let mut wl = ZonesWorkload::new(3);
        wl.admin_act(0, "A", NodeId(1), (0, 1), 0);
        wl.admin_act(2, "A", NodeId(1), (0, 16), 0);
        let r = wl.sm.ranges.get_mut(&FIRST).expect("the first range");
        (r.start, r.end) = (key(0), key(16));
        let v = wl.check_placement().expect_err("replicas on nodes 2 and 3");
        assert!(!v.liveness && v.detail.contains("outside its zone's nodes"), "{}", v.detail);
        wl.sm.ranges.get_mut(&FIRST).expect("there").replicas.clear();
        wl.check_replicas();
        let mut w = World::new(1, 3, 1, false);
        let v = wl.finish(&mut w).expect_err("no replica");
        assert!(v.detail.contains("has no replica"), "{}", v.detail);
    }
}
