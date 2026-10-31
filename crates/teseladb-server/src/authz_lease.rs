//! The authorization fence on core 0 (013 FR-012, `spec/tla/AuthzFence.tla`): every
//! [`NODE_HEARTBEAT`] a node asks Group 0's leader for an authorization lease with the index of
//! its copy (`AuthLeaseAsk`); the leader records it and answers with its commit index and the
//! index up to which every change of users and roles is fenced (`AuthLeaseGrant`). The node
//! authorizes commands only while its lease holds, which its client workers read from the
//! [`AuthGate`]; a change that takes privileges away answers once its index is fenced.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use teseladb_auth::fence::{AuthLease, FenceTable};
use teseladb_hlc::guard::MAX_CLOCK_OFFSET_MS;
use teseladb_io::peer::PeerMsg;
use teseladb_meta::group0::GROUP0;
use teseladb_meta::liveness::NODE_HEARTBEAT;
use teseladb_multiraft::{MultiRaftCore, StateMachine};
use teseladb_raft::Role;
use teseladb_types::{Nanos, NodeId};

use crate::coresm::Group0Holder;

/// Nanoseconds in a millisecond.
const NANOS_PER_MS: u64 = 1_000_000;

/// What a write's deadline adds to its wait end (009 FR-006): two clocks of the cluster, each
/// within [`MAX_CLOCK_OFFSET_MS`] of the others' reading, differ by at most twice that, so a
/// leader whose clock runs ahead does not refuse a write its sender still waits for.
pub const DEADLINE_SKEW: Duration = Duration::from_millis(2 * MAX_CLOCK_OFFSET_MS.unsigned_abs());

/// How long after a write's wait end a range's leader may still evaluate it (009 FR-006): the
/// write's deadline is [`DEADLINE_SKEW`] past it on the sender's clock; the leader compares it
/// with its own, behind by at most twice [`MAX_CLOCK_OFFSET_MS`], and twice again covers the
/// mark of another node the sender's clock took.
pub const LATE_EVALUATION: Duration = Duration::from_millis(6 * MAX_CLOCK_OFFSET_MS.unsigned_abs());

/// Until when the node may authorize, for its client workers. Its origin is taken before the
/// node's runtime starts, so its clock reads at least the runtime's: a lease ends here no later
/// than core 0 counts it.
#[derive(Debug)]
pub struct AuthGate {
    origin: Instant,
    until_ns: AtomicU64,
    /// The catalog reads of the operations running on the node (009 FR-006).
    pub pins: CatalogPins,
}

impl Default for AuthGate {
    fn default() -> AuthGate {
        AuthGate {
            origin: Instant::now(),
            until_ns: AtomicU64::new(0),
            pins: CatalogPins::default(),
        }
    }
}

/// The indexes of Group 0 at which the node's running operations read the secondary indexes
/// they write with (009 FR-006). Core 0 pins a read when it answers it, so no acknowledgement
/// can pass it in between; the operation releases it when it ends. The node acknowledges to
/// the fence no index above the oldest pin: a change of the indexes answers only once no
/// operation still writes with what the catalog said before it (the barrier of
/// `OnlineIndexBuild.tla`).
#[derive(Debug, Default)]
pub struct CatalogPins {
    held: Mutex<Pins>,
}

#[derive(Debug, Default)]
struct Pins {
    next: u64,
    at: BTreeMap<u64, u64>,
    /// When each pin released late goes, on core 0's clock.
    until: BTreeMap<u64, Nanos>,
}

impl CatalogPins {
    /// Pins a read of the catalog at index `at`; the pin's number.
    pub fn pin(&self, at: u64) -> u64 {
        let mut p = self.lock();
        p.next += 1;
        let pin = p.next;
        p.at.insert(pin, at);
        pin
    }

    /// Releases pin `pin`; a pin already released changes nothing.
    pub fn release(&self, pin: u64) {
        let mut p = self.lock();
        p.at.remove(&pin);
        p.until.remove(&pin);
    }

    /// Releases pin `pin` once core 0's clock reaches `at`: a write of the operation may still
    /// be evaluated until then.
    pub fn release_at(&self, pin: u64, at: Nanos) {
        let mut p = self.lock();
        if p.at.contains_key(&pin) {
            p.until.insert(pin, at);
        }
    }

    /// The index the node may acknowledge with its copy at `applied`, at `now` on core 0's
    /// clock.
    pub fn acked(&self, applied: u64, now: Nanos) -> u64 {
        let mut p = self.lock();
        let gone: Vec<u64> =
            p.until.iter().filter(|(_, at)| **at <= now).map(|(k, _)| *k).collect();
        for pin in gone {
            p.at.remove(&pin);
            p.until.remove(&pin);
        }
        p.at.values().copied().fold(applied, u64::min)
    }

    /// The table: a thread that panicked holding it left it whole, since every change is one
    /// map operation.
    fn lock(&self) -> std::sync::MutexGuard<'_, Pins> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl AuthGate {
    /// The gate's clock, which reads at least core 0's.
    pub fn now(&self) -> Nanos {
        Nanos(u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX))
    }

    /// Releases pin `pin` once a write sent with wait end `until` can no longer be evaluated by
    /// any leader (009 FR-006): [`LATE_EVALUATION`] after it, on core 0's clock.
    pub fn release_pin_after(&self, pin: u64, until: Instant) {
        let at = (until + LATE_EVALUATION).saturating_duration_since(self.origin);
        self.pins.release_at(pin, Nanos(u64::try_from(at.as_nanos()).unwrap_or(u64::MAX)));
    }

    /// Whether the node holds an authorization lease now.
    pub fn valid(&self) -> bool {
        let now = u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX);
        now < self.until_ns.load(Ordering::Acquire)
    }

    fn set(&self, until_ms: u64) {
        self.until_ns.store(until_ms.saturating_mul(NANOS_PER_MS), Ordering::Release);
    }
}

/// The lease of this node and, while it leads Group 0, the table of the others'.
#[derive(Debug, Default)]
pub struct AuthLeaseDriver {
    next: Nanos,
    lease: AuthLease,
    table: Option<FenceTable>,
    /// Every change of users and roles up to this index of Group 0 is fenced.
    fenced: u64,
    gate: Arc<AuthGate>,
}

fn ms(n: Nanos) -> u64 {
    n.0 / NANOS_PER_MS
}

fn applied<S: StateMachine + Group0Holder>(core: &MultiRaftCore<S>) -> u64 {
    core.state_machine(GROUP0).and_then(Group0Holder::group0).map_or(0, |sm| sm.applied)
}

impl AuthLeaseDriver {
    /// A driver that publishes its lease in `gate`.
    pub fn with_gate(gate: Arc<AuthGate>) -> AuthLeaseDriver {
        AuthLeaseDriver { gate, ..AuthLeaseDriver::default() }
    }

    /// Whether this node holds its authorization lease at `now`.
    pub fn holds(&self, now: Nanos) -> bool {
        self.lease.valid(ms(now))
    }

    /// The catalog reads of the node's running operations.
    pub fn pins(&self) -> &CatalogPins {
        &self.gate.pins
    }

    /// The index up to which changes of users and roles are fenced.
    pub fn fenced(&self) -> u64 {
        self.fenced
    }

    /// A tick of core 0 of node `me`: the request to Group 0's leader, answered in place on the
    /// leader itself. Returns what to send.
    pub fn tick<S: StateMachine + Group0Holder>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        me: NodeId,
        now: Nanos,
    ) -> Vec<(NodeId, PeerMsg)> {
        let Some(st) = core.status(GROUP0) else { return Vec::new() };
        let commit = st.hard_state.commit;
        if st.role == Role::Leader {
            if self.table.is_none() {
                let nodes: Vec<u64> = core
                    .state_machine(GROUP0)
                    .and_then(Group0Holder::group0)
                    .map(|sm| sm.nodes.keys().map(|n| n.0).collect())
                    .unwrap_or_default();
                self.table = Some(FenceTable::take_over(&nodes, ms(now)));
            }
        } else {
            self.table = None;
        }
        if let Some(table) = &self.table {
            self.fenced = self.fenced.max(table.fenced(commit, ms(now)));
        }
        if now < self.next {
            return Vec::new();
        }
        self.next = now + NODE_HEARTBEAT;
        let seq = self.lease.ask(ms(now));
        let applied = applied(core);
        // What the node acknowledges waits for its running operations (009 FR-006).
        let acked = self.gate.pins.acked(applied, now);
        if let Some(table) = &mut self.table {
            let commit = table.request(me.0, acked, commit, ms(now));
            self.lease.granted(seq, commit, applied);
            self.publish();
            return Vec::new();
        }
        if st.leader.0 == 0 {
            return Vec::new();
        }
        vec![(st.leader, PeerMsg::AuthLeaseAsk { node: me, seq, applied: acked })]
    }

    /// A message for core 0: a request on the leader (answered), a grant on its node. Any other
    /// message is given back.
    pub fn on_peer<S: StateMachine + Group0Holder>(
        &mut self,
        core: &MultiRaftCore<S>,
        msg: PeerMsg,
        now: Nanos,
    ) -> Result<Vec<(NodeId, PeerMsg)>, PeerMsg> {
        match msg {
            PeerMsg::AuthLeaseAsk { node, seq, applied } => {
                let Some(table) = &mut self.table else { return Ok(Vec::new()) };
                let commit = core.status(GROUP0).map_or(0, |s| s.hard_state.commit);
                let commit = table.request(node.0, applied, commit, ms(now));
                let fenced = table.fenced(commit, ms(now));
                Ok(vec![(node, PeerMsg::AuthLeaseGrant { node, seq, commit, fenced })])
            }
            PeerMsg::AuthLeaseGrant { seq, commit, fenced, .. } => {
                self.lease.granted(seq, commit, applied(core));
                self.fenced = self.fenced.max(fenced);
                self.publish();
                Ok(Vec::new())
            }
            other => Err(other),
        }
    }

    fn publish(&self) {
        self.gate.set(self.lease.until_ms());
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use teseladb_auth::fence::AUTH_LEASE_MS;
    use teseladb_meta::command::Group0Command;
    use teseladb_meta::group0::Group0Sm;
    use teseladb_multiraft::{CoreConfig, GroupDesc};
    use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};

    use super::*;
    use crate::frames::{from_wire, to_wire};

    const TICK: Nanos = Nanos::from_millis(10);

    struct Node {
        core: MultiRaftCore<Group0Sm>,
        disk: MemDisk,
        driver: AuthLeaseDriver,
    }

    /// Three nodes holding Group 0, in logical time; every message arrives at the next step,
    /// except those to or from a node cut off.
    struct Cluster {
        nodes: BTreeMap<NodeId, Node>,
        now: Nanos,
        cut: BTreeSet<NodeId>,
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
                let driver = AuthLeaseDriver::with_gate(Arc::new(AuthGate::default()));
                nodes.insert(n, Node { core, disk, driver });
            }
            Cluster { nodes, now: Nanos::ZERO, cut: BTreeSet::new() }
        }

        fn step(&mut self) {
            self.now = self.now + TICK;
            let now = self.now;
            let mut out = Vec::new();
            for (&id, n) in &mut self.nodes {
                n.core.on_tick(now, 0);
                // Clocks in margin, as the liveness reports keep them.
                n.core.on_clock_report(0, now);
                for (to, m) in n.driver.tick(&mut n.core, id, now) {
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
                if self.cut.contains(&from) || self.cut.contains(&to) {
                    continue;
                }
                let n = self.nodes.get_mut(&to).unwrap();
                match n.driver.on_peer(&n.core, m, now) {
                    Ok(answers) => {
                        for (back, a) in answers {
                            let b = self.nodes.get_mut(&back).unwrap();
                            assert!(b.driver.on_peer(&b.core, a, now).unwrap().is_empty());
                        }
                    }
                    Err(m) => {
                        if let Some(f) = from_wire(m) {
                            n.core.on_peer_message(from, f, now);
                        }
                    }
                }
            }
        }

        fn steps(&mut self, k: u32) {
            for _ in 0..k {
                self.step();
            }
        }

        fn leader(&self) -> NodeId {
            *self
                .nodes
                .iter()
                .find(|(_, n)| n.core.status(GROUP0).is_some_and(|s| s.role == Role::Leader))
                .expect("Group 0 elects a leader")
                .0
        }

        fn commit(&self) -> u64 {
            self.nodes[&self.leader()].core.status(GROUP0).unwrap().hard_state.commit
        }

        /// Commits a change in Group 0 through its leader.
        fn change(&mut self) {
            let l = self.leader();
            let cmd = Group0Command::SetSetting { key: b"k".to_vec(), value: b"v".to_vec() };
            let now = self.now;
            self.nodes.get_mut(&l).unwrap().core.propose(GROUP0, &cmd.encode(), now).unwrap();
        }
    }

    #[test]
    fn every_node_holds_a_lease_and_changes_are_fenced() {
        let mut c = Cluster::new();
        c.steps(300);
        let now = c.now;
        assert!(c.nodes.values().all(|n| n.driver.holds(now)), "every node holds its lease");
        c.change();
        c.steps(300);
        let commit = c.commit();
        let l = c.leader();
        assert_eq!(c.nodes[&l].driver.fenced(), commit);
        for n in c.nodes.values() {
            assert_eq!(n.driver.fenced(), commit, "the grants carry it");
        }
        // A request to a node that does not lead, and a message of another kind.
        let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
        let x = c.nodes.get_mut(&f).unwrap();
        let ask = PeerMsg::AuthLeaseAsk { node: l, seq: 1, applied: 0 };
        assert!(x.driver.on_peer(&x.core, ask, now).unwrap().is_empty());
        let other = PeerMsg::NodeSuspect { node: l, suspect: false };
        assert!(x.driver.on_peer(&x.core, other, now).is_err());
    }

    #[test]
    fn a_node_cut_off_loses_its_lease_before_the_fence_passes() {
        let mut c = Cluster::new();
        c.steps(300);
        let l = c.leader();
        let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
        c.cut.insert(f);
        c.change();
        c.steps(5);
        let index = c.commit();
        assert!(c.nodes[&l].driver.fenced() < index, "the node cut off has not applied it");
        let cut_at = c.now;
        loop {
            c.step();
            let now = c.now;
            let fenced = c.nodes[&l].driver.fenced() >= index;
            if fenced {
                assert!(!c.nodes[&f].driver.holds(now), "fenced while the node still authorizes");
                break;
            }
            assert!(now < cut_at + Nanos::from_millis(2 * AUTH_LEASE_MS), "the fence never passed");
        }
        assert!(!c.nodes[&f].driver.holds(c.now));
        c.cut.clear();
        c.steps(200);
        assert!(c.nodes[&f].driver.holds(c.now), "back in the cluster, it holds a lease again");
    }

    #[test]
    fn pins_hold_back_what_a_node_acknowledges() {
        let pins = CatalogPins::default();
        let now = Nanos::ZERO;
        assert_eq!(pins.acked(9, now), 9);
        let a = pins.pin(4);
        let b = pins.pin(6);
        assert_ne!(a, b);
        assert_eq!(pins.acked(9, now), 4);
        pins.release(a);
        assert_eq!(pins.acked(9, now), 6);
        assert_eq!(pins.acked(5, now), 5, "never above the copy");
        pins.release(a);
        pins.release(b);
        assert_eq!(pins.acked(9, now), 9);
    }

    /// A write that may still reach its range's leader keeps its operation's read of the catalog
    /// pinned until its deadline has passed (009 FR-006).
    #[test]
    fn a_pin_released_late_holds_until_its_time() {
        let pins = CatalogPins::default();
        let a = pins.pin(4);
        pins.release_at(a, Nanos::from_millis(10));
        assert_eq!(pins.acked(9, Nanos::from_millis(9)), 4, "still held");
        assert_eq!(pins.acked(9, Nanos::from_millis(10)), 9, "its time came");
        assert_eq!(pins.acked(9, Nanos::ZERO), 9, "gone for good");
        // A pin released at once after a late release, and a late release of a pin already
        // gone, change nothing more.
        let b = pins.pin(5);
        pins.release_at(b, Nanos::from_millis(50));
        pins.release(b);
        assert_eq!(pins.acked(9, Nanos::ZERO), 9);
        pins.release_at(b, Nanos::from_millis(60));
        assert_eq!(pins.acked(9, Nanos::ZERO), 9);
    }

    #[test]
    fn a_pinned_catalog_read_holds_back_the_fence() {
        let mut c = Cluster::new();
        c.steps(300);
        let l = c.leader();
        let f = *c.nodes.keys().find(|&&n| n != l).unwrap();
        let applied = applied(&c.nodes[&f].core);
        let pin = c.nodes[&f].driver.pins().pin(applied);
        c.change();
        c.steps(300);
        let index = c.commit();
        assert!(index > applied);
        assert!(c.nodes[&f].driver.holds(c.now), "the node keeps its lease");
        assert!(c.nodes[&l].driver.fenced() < index, "the running write holds the change back");
        c.nodes[&f].driver.pins().release(pin);
        c.steps(300);
        assert_eq!(c.nodes[&l].driver.fenced(), c.commit(), "released, the change passes");
    }

    #[test]
    fn a_core_without_group_0_asks_nothing() {
        let mut disk = MemDisk::new();
        let store = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap().0;
        let mut core: MultiRaftCore<Group0Sm> =
            MultiRaftCore::new(CoreConfig::new(NodeId(1), 0), store, 1);
        let mut d = AuthLeaseDriver::default();
        assert!(d.tick(&mut core, NodeId(1), Nanos::from_millis(10)).is_empty());
        assert!(!d.holds(Nanos::from_millis(10)));
    }

    #[test]
    fn the_gate_follows_the_lease() {
        let gate = Arc::new(AuthGate::default());
        assert!(!gate.valid());
        gate.set(u64::MAX / NANOS_PER_MS);
        assert!(gate.valid());
        gate.set(0);
        assert!(!gate.valid());
    }
}
