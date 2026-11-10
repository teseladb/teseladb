//! Node heartbeats and clock reports on core 0 (005 FR-006, 001 FR-018): every
//! [`NODE_HEARTBEAT`] each node sends its wall clock and load to Group 0's leader
//! (`NodeHeartbeat`), and the leader answers every node with its offset from the median clock of
//! Group 0's voters (`ClockReport`). A node out of margin hands over its leaderships and takes
//! none until a report brings it back.
//!
//! The load a heartbeat carries is the node's (007 FR-010): every core publishes its own in
//! [`SharedLoads`] once per [`NODE_HEARTBEAT`], and core 0 adds them up with the space of the data
//! directory's disk when its heartbeat is due, not at every tick.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use teseladb_io::peer::PeerMsg;
use teseladb_meta::clock::ClockSample;
use teseladb_meta::group0::GROUP0;
use teseladb_meta::liveness::{Liveness, NODE_HEARTBEAT, NodeLoad};
use teseladb_multiraft::{MultiRaftCore, StateMachine};
use teseladb_raft::Role;
use teseladb_types::{Nanos, NodeId};

use crate::coresm::Group0Holder;

/// The heartbeats a core sends and, while it leads Group 0, the table of the others'.
#[derive(Debug, Default)]
pub struct LivenessDriver {
    /// When the next heartbeat (or round of reports) is due.
    next: Nanos,
    /// The table, while this core leads Group 0.
    table: Option<Liveness>,
}

/// What a core carries: its groups, those it leads, the bytes of their logs and the operations
/// per second its ranges served (`ops`, from the data ranges' leaders).
pub fn core_load<S: StateMachine>(core: &MultiRaftCore<S>, ops: u64) -> NodeLoad {
    let ids = core.group_ids();
    let leaders =
        ids.iter().filter(|&&g| core.status(g).is_some_and(|s| s.role == Role::Leader)).count();
    let bytes = ids.iter().filter_map(|&g| core.store().group(g)).map(|l| l.bytes()).sum();
    NodeLoad {
        groups: ids.len() as u64,
        leaders: leaders as u64,
        bytes,
        ops,
        ..NodeLoad::default()
    }
}

/// Fields of a core's slot in [`SharedLoads`].
const SLOT: usize = 4;

/// The loads the node's cores publish for core 0's heartbeat, and the data directory whose disk
/// it measures (none: its space stays unknown).
#[derive(Debug)]
pub struct SharedLoads {
    cores: Vec<[AtomicU64; SLOT]>,
    data_dir: Option<PathBuf>,
}

impl SharedLoads {
    /// Slots for `cores` cores.
    pub fn new(cores: usize, data_dir: Option<PathBuf>) -> SharedLoads {
        let cores = (0..cores).map(|_| std::array::from_fn(|_| AtomicU64::new(0))).collect();
        SharedLoads { cores, data_dir }
    }

    /// Core `core` carries `l` now.
    pub fn publish(&self, core: usize, l: NodeLoad) {
        if let Some(slot) = self.cores.get(core) {
            for (a, v) in slot.iter().zip([l.groups, l.leaders, l.bytes, l.ops]) {
                a.store(v, Ordering::Relaxed);
            }
        }
    }

    /// The node's load: the sum of its cores' and the space of its disk.
    pub fn node(&self) -> NodeLoad {
        let mut sum = NodeLoad::default();
        for slot in &self.cores {
            let [groups, leaders, bytes, ops] = slot.each_ref().map(|a| a.load(Ordering::Relaxed));
            sum = sum.plus(NodeLoad { groups, leaders, bytes, ops, ..NodeLoad::default() });
        }
        let space = self.data_dir.as_ref().and_then(|d| teseladb_sys::host::fs_usage(d).ok());
        if let Some((used, total_bytes)) = space {
            sum = NodeLoad { free_bytes: total_bytes.saturating_sub(used), total_bytes, ..sum };
        }
        sum
    }
}

/// A clock sample of `node`'s heartbeat. No round trip is measured on this path: the heartbeat's
/// flight counts as offset, as in the rule with Raft's heartbeats.
fn sample(node: NodeId, sent_ms: u64, received_ms: u64) -> ClockSample {
    let ms = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    ClockSample { node, sent_ms: ms(sent_ms), received_ms: ms(received_ms), rtt_ms: 0 }
}

impl LivenessDriver {
    /// A tick of core 0 of node `me`: the heartbeat to Group 0's leader or, on the leader, the
    /// round of clock reports (its own goes straight to its core). Returns what to send.
    pub fn tick<S: StateMachine + Group0Holder>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        me: NodeId,
        now: Nanos,
        wall_ms: u64,
    ) -> Vec<(NodeId, PeerMsg)> {
        let load = core_load(core, 0);
        self.tick_with(core, (me, load), now, wall_ms)
    }

    /// Whether a tick at `now` sends the heartbeat (or the round of reports): only then does
    /// core 0 need the node's load, which reads every core's slot and asks the file system for
    /// its space.
    pub fn due(&self, now: Nanos) -> bool {
        now >= self.next
    }

    /// [`LivenessDriver::tick`] with the node's load `load`, from [`SharedLoads::node`]; a tick
    /// that is not [`due`](LivenessDriver::due) does not read it.
    pub fn tick_with<S: StateMachine + Group0Holder>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        (me, load): (NodeId, NodeLoad),
        now: Nanos,
        wall_ms: u64,
    ) -> Vec<(NodeId, PeerMsg)> {
        let Some(st) = core.status(GROUP0) else { return Vec::new() };
        if st.role != Role::Leader {
            self.table = None;
        }
        if now < self.next {
            return Vec::new();
        }
        self.next = now + NODE_HEARTBEAT;
        if st.role != Role::Leader {
            if st.leader.0 == 0 {
                return Vec::new();
            }
            let NodeLoad { groups, leaders, bytes, free_bytes, total_bytes, ops } = load;
            let hb = PeerMsg::NodeHeartbeat {
                node: me,
                now_ms: wall_ms,
                groups,
                leaders,
                bytes,
                free_bytes,
                total_bytes,
                ops,
            };
            return vec![(st.leader, hb)];
        }
        let nodes: Vec<NodeId> = core
            .state_machine(GROUP0)
            .and_then(Group0Holder::group0)
            .map(|sm| sm.nodes.keys().copied().collect())
            .unwrap_or_default();
        let voters = core.conf(GROUP0).map(|c| c.voters.clone()).unwrap_or_default();
        let table = self.table.get_or_insert_with(|| Liveness::start(nodes, now));
        table.heartbeat(me, now, load, sample(me, wall_ms, wall_ms));
        let mut out = Vec::new();
        for (node, offset_ms) in table.clock_reports(me, &voters) {
            if node == me {
                core.on_clock_report(offset_ms, now);
            } else {
                out.push((node, PeerMsg::ClockReport { node, offset_ms }));
            }
        }
        out
    }

    /// A message for core 0 of node `me`: a heartbeat goes into the table (on Group 0's leader) and
    /// this node's report into its clock rule. Any other message is given back.
    pub fn on_peer<S: StateMachine>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        me: NodeId,
        msg: PeerMsg,
        now: Nanos,
        wall_ms: u64,
    ) -> Option<PeerMsg> {
        match msg {
            PeerMsg::NodeHeartbeat {
                node,
                now_ms,
                groups,
                leaders,
                bytes,
                free_bytes,
                total_bytes,
                ops,
            } => {
                if let Some(table) = &mut self.table {
                    let load = NodeLoad { groups, leaders, bytes, free_bytes, total_bytes, ops };
                    table.heartbeat(node, now, load, sample(node, now_ms, wall_ms));
                }
                None
            }
            PeerMsg::ClockReport { node, offset_ms } => {
                if node == me {
                    core.on_clock_report(offset_ms, now);
                }
                None
            }
            other => Some(other),
        }
    }

    /// Group 0's leader's table, if this core leads it.
    pub fn table(&self) -> Option<&Liveness> {
        self.table.as_ref()
    }

    /// Group 0's leader's table, to apply its timeouts (the balancer, 005 FR-016).
    pub fn table_mut(&mut self) -> Option<&mut Liveness> {
        self.table.as_mut()
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::collections::BTreeMap;

    use teseladb_meta::group0::Group0Sm;
    use teseladb_multiraft::{CoreConfig, GroupDesc};
    use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};

    use super::*;
    use crate::frames::{from_wire, to_wire};

    const TICK: Nanos = Nanos::from_millis(10);

    #[test]
    fn the_node_load_adds_up_its_cores_and_measures_its_disk() {
        let core =
            |groups, ops| NodeLoad { groups, leaders: 1, bytes: 10, ops, ..NodeLoad::default() };
        let blind = SharedLoads::new(2, None);
        blind.publish(0, core(3, 100));
        blind.publish(1, core(2, 50));
        // A core past the slots publishes nothing.
        blind.publish(2, core(9, 9));
        let n = blind.node();
        assert_eq!((n.groups, n.leaders, n.bytes, n.ops), (5, 2, 20, 150));
        assert_eq!((n.free_bytes, n.total_bytes), (0, 0), "no directory: unknown");
        let dir = std::env::temp_dir();
        let seen = SharedLoads::new(1, Some(dir.clone())).node();
        let (_, total) = teseladb_sys::host::fs_usage(&dir).unwrap();
        assert_eq!(seen.total_bytes, total);
        assert!(seen.free_bytes > 0 && seen.free_bytes < total, "{seen:?}");
        // A directory that is not there: unknown.
        let gone = SharedLoads::new(1, Some(dir.join("teseladb-no-such-dir"))).node();
        assert_eq!(gone.total_bytes, 0);
    }
    const WALL0: u64 = 1_800_000_000_000;

    struct Node {
        core: MultiRaftCore<Group0Sm>,
        disk: MemDisk,
        driver: LivenessDriver,
        /// Milliseconds its wall clock is ahead of the others'.
        skew: u64,
    }

    /// Three nodes holding Group 0, in logical time; every message arrives at the next step.
    struct Cluster {
        nodes: BTreeMap<NodeId, Node>,
        now: Nanos,
        /// The driver's messages, to look at.
        sent: Vec<(NodeId, NodeId, PeerMsg)>,
    }

    impl Cluster {
        fn new() -> Cluster {
            let voters: Vec<NodeId> = (1..=3).map(NodeId).collect();
            let mut nodes = BTreeMap::new();
            for &n in &voters {
                let mut disk = MemDisk::new();
                let store = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap().0;
                let mut core = MultiRaftCore::new(CoreConfig::new(n, 0), store, n.0);
                let desc = GroupDesc { group: GROUP0, voters: voters.clone(), learners: vec![] };
                core.create_group(desc, Group0Sm::default()).unwrap();
                nodes.insert(n, Node { core, disk, driver: LivenessDriver::default(), skew: 0 });
            }
            Cluster { nodes, now: Nanos::ZERO, sent: Vec::new() }
        }

        fn step(&mut self) {
            self.now = self.now + TICK;
            let now = self.now;
            let mut out = Vec::new();
            for (&id, n) in &mut self.nodes {
                let wall = WALL0 + now.as_millis() + n.skew;
                n.core.on_tick(now, wall);
                for (to, m) in n.driver.tick(&mut n.core, id, now, wall) {
                    self.sent.push((id, to, m.clone()));
                    out.push((id, to, m));
                }
                while let Some(op) = n.core.poll_io(now) {
                    let d = n.disk.execute(op);
                    n.core.on_io_done(d, now);
                }
                let a = n.core.drain_actions();
                out.extend(a.send.into_iter().map(|(to, f)| (id, to, to_wire(f))));
            }
            for (from, to, m) in out {
                let n = self.nodes.get_mut(&to).unwrap();
                let wall = WALL0 + now.as_millis() + n.skew;
                if let Some(m) = n.driver.on_peer(&mut n.core, to, m, now, wall)
                    && let Some(f) = from_wire(m)
                {
                    n.core.on_peer_message(from, f, now);
                }
            }
        }

        fn steps(&mut self, k: u32) {
            for _ in 0..k {
                self.step();
            }
        }

        fn leader(&self) -> Option<NodeId> {
            self.nodes
                .iter()
                .find(|(_, n)| n.core.status(GROUP0).is_some_and(|s| s.role == Role::Leader))
                .map(|(id, _)| *id)
        }
    }

    #[test]
    fn every_node_sends_its_heartbeat_and_the_leader_answers_its_offset() {
        let mut c = Cluster::new();
        c.steps(300);
        let l = c.leader().expect("Group 0 elects a leader");
        c.sent.clear();
        c.steps(50);
        // Every 500 ms each follower sends one heartbeat, to the leader.
        let beats: Vec<NodeId> = c
            .sent
            .iter()
            .filter(|(_, _, m)| matches!(m, PeerMsg::NodeHeartbeat { .. }))
            .map(|(from, to, _)| {
                assert_eq!(*to, l);
                *from
            })
            .collect();
        assert_eq!(beats.len(), 2, "{beats:?}");
        assert!(!beats.contains(&l));
        // The leader answers each follower with its offset; all clocks agree.
        for (from, to, m) in &c.sent {
            if let PeerMsg::ClockReport { node, offset_ms } = m {
                assert_eq!((*from, *node), (l, *to));
                assert!(offset_ms.abs() <= 10, "{offset_ms}");
            }
        }
        let table = c.nodes[&l].driver.table().expect("the leader keeps the table");
        let f = c.nodes.keys().copied().find(|&n| n != l).unwrap();
        let load = table.get(f).unwrap().load;
        assert_eq!((load.groups, load.leaders), (1, 0));
        let held = c.nodes[&f].core.store().group(GROUP0).unwrap().bytes();
        assert_eq!(load.bytes, held, "the data of the entries its log keeps");
        assert!(c.nodes.iter().all(|(&n, x)| (n == l) == x.driver.table().is_some()));
        // A report about another node is used up and changes nothing here.
        let now = c.now;
        let n = c.nodes.get_mut(&f).unwrap();
        let other = PeerMsg::ClockReport { node: l, offset_ms: 400 };
        assert!(n.driver.on_peer(&mut n.core, f, other, now, WALL0 + now.as_millis()).is_none());
        assert!(n.core.status(GROUP0).is_some_and(|s| s.role != Role::Leader));
        c.steps(10);
        assert_eq!(c.leader(), Some(l), "its own clock rule did not take it");
    }

    #[test]
    fn a_follower_is_due_exactly_when_it_sends_its_heartbeat() {
        let mut c = Cluster::new();
        c.steps(300);
        let l = c.leader().expect("Group 0 elects a leader");
        let mut beats = 0;
        for _ in 0..50 {
            let next = c.now + TICK;
            let due: BTreeMap<NodeId, bool> =
                c.nodes.iter().map(|(&id, n)| (id, n.driver.due(next))).collect();
            c.sent.clear();
            c.step();
            for (&id, &was_due) in due.iter().filter(|(id, _)| **id != l) {
                let sent = c
                    .sent
                    .iter()
                    .any(|(from, _, m)| *from == id && matches!(m, PeerMsg::NodeHeartbeat { .. }));
                assert_eq!(sent, was_due, "node {id:?} at {next:?}");
                beats += usize::from(sent);
            }
        }
        // 50 ticks of 10 ms are 500 ms: one heartbeat per follower.
        assert_eq!(beats, 2);
    }

    #[test]
    fn a_node_400_ms_ahead_is_reported_and_leads_nothing() {
        let mut c = Cluster::new();
        c.steps(300);
        let l = c.leader().expect("Group 0 elects a leader");
        // The leader's clock moves 400 ms ahead: it hands Group 0 over within a second, and the
        // new leader's reports say it is out of margin.
        c.nodes.get_mut(&l).unwrap().skew = 400;
        let mut moved = None;
        for t in 1..=100u32 {
            c.step();
            if c.leader().is_some_and(|n| n != l) {
                moved = Some(t);
                break;
            }
        }
        let t = moved.expect("another node leads within a second");
        assert!(u64::from(t) * TICK.as_millis() <= 1_000);
        c.sent.clear();
        c.steps(100);
        let reports: Vec<i64> = c
            .sent
            .iter()
            .filter_map(|(_, _, m)| match m {
                PeerMsg::ClockReport { node, offset_ms } if *node == l => Some(*offset_ms),
                _ => None,
            })
            .collect();
        assert!(!reports.is_empty() && reports.iter().all(|&o| o >= 390), "{reports:?}");
        assert_ne!(c.leader(), Some(l));
        // Messages that are not the driver's go on to the core.
        let other = PeerMsg::RangeRetire { group: GROUP0 };
        let n = c.nodes.get_mut(&l).unwrap();
        assert!(n.driver.on_peer(&mut n.core, l, other, c.now, WALL0).is_some());
    }
}
