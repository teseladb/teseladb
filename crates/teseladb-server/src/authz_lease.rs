//! The authorization fence on core 0 (013 FR-012, `spec/tla/AuthzFence.tla`,
//! `spec/tla/AuthzFenceLeaders.tla`): core 0 runs the protocol of [`LeaseDriver`] over its
//! replica of Group 0, carrying its messages as `AuthLeaseAsk` and `AuthLeaseGrant`. Every
//! [`NODE_HEARTBEAT`] the node asks Group 0's leader for an authorization lease; the leader
//! answers once a majority of the voters has acknowledged a heartbeat round it sent after the
//! request arrived, so a leader cut off from the others grants nothing, not even to itself. The
//! node authorizes commands only while its lease holds, which its client workers read from the
//! [`AuthGate`]; a change that takes privileges away answers once its index is fenced.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use teseladb_auth::lease_driver::{FenceMsg, Group0Host, Group0View, LeaseDriver};
use teseladb_hlc::guard::MAX_CLOCK_OFFSET_MS;
use teseladb_io::peer::PeerMsg;
use teseladb_meta::group0::{GROUP0, Group0Sm};
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

/// The lease of this node and, while it leads Group 0, the table of the others'
/// ([`LeaseDriver`]), publishing the lease in the node's [`AuthGate`].
#[derive(Debug)]
pub struct AuthLeaseDriver {
    driver: LeaseDriver,
    gate: Arc<AuthGate>,
}

impl Default for AuthLeaseDriver {
    fn default() -> AuthLeaseDriver {
        AuthLeaseDriver::with_gate(Arc::default())
    }
}

/// Core 0's replica of Group 0, for the protocol.
struct Host<'a, S: StateMachine + Group0Holder> {
    core: &'a mut MultiRaftCore<S>,
    now: Nanos,
}

fn group0<S: StateMachine + Group0Holder>(core: &MultiRaftCore<S>) -> Option<&Group0Sm> {
    core.state_machine(GROUP0).and_then(Group0Holder::group0)
}

fn applied<S: StateMachine + Group0Holder>(core: &MultiRaftCore<S>) -> u64 {
    group0(core).map_or(0, |sm| sm.applied)
}

impl<S: StateMachine + Group0Holder> Group0Host for Host<'_, S> {
    fn view(&self) -> Option<Group0View> {
        let st = self.core.status(GROUP0)?;
        Some(Group0View {
            leads: st.role == Role::Leader && self.core.leader_ready(GROUP0),
            leader: st.leader.0,
            term: st.hard_state.term,
            commit: st.hard_state.commit,
            applied: applied(self.core),
        })
    }

    fn known_nodes(&self) -> Vec<u64> {
        let recorded = group0(self.core).map(|sm| sm.nodes.keys().map(|n| n.0).collect());
        let mut all: Vec<u64> = recorded.unwrap_or_default();
        if let Some(c) = self.core.conf(GROUP0) {
            all.extend(c.voters.iter().chain(&c.learners).map(|n| n.0));
        }
        all.sort_unstable();
        all.dedup();
        all
    }

    fn quorum_heard_at_ns(&self) -> Option<u64> {
        self.core.quorum_heard_at(GROUP0).map(|n| n.0)
    }

    fn send_confirm_round(&mut self, _now_ns: u64) {
        self.core.send_confirm_round(GROUP0, self.now);
    }
}

fn to_peer(m: FenceMsg) -> (NodeId, PeerMsg) {
    match m {
        FenceMsg::Ask { to, node, seq, applied } => {
            (NodeId(to), PeerMsg::AuthLeaseAsk { node: NodeId(node), seq, applied })
        }
        FenceMsg::Grant { to, seq, commit, fenced } => {
            let node = NodeId(to);
            (node, PeerMsg::AuthLeaseGrant { node, seq, commit, fenced })
        }
    }
}

impl AuthLeaseDriver {
    /// A driver that publishes its lease in `gate`.
    pub fn with_gate(gate: Arc<AuthGate>) -> AuthLeaseDriver {
        AuthLeaseDriver { driver: LeaseDriver::new(NODE_HEARTBEAT.0), gate }
    }

    /// Whether this node holds its authorization lease at `now`.
    pub fn holds(&self, now: Nanos) -> bool {
        self.driver.holds(now.0)
    }

    /// The catalog reads of the node's running operations.
    pub fn pins(&self) -> &CatalogPins {
        &self.gate.pins
    }

    /// The index up to which changes of users and roles are fenced.
    pub fn fenced(&self) -> u64 {
        self.driver.fenced()
    }

    /// A tick of core 0 of node `me` ([`LeaseDriver::tick`]). Returns what to send.
    pub fn tick<S: StateMachine + Group0Holder>(
        &mut self,
        core: &mut MultiRaftCore<S>,
        me: NodeId,
        now: Nanos,
    ) -> Vec<(NodeId, PeerMsg)> {
        let pins = &self.gate.pins;
        // What the node acknowledges waits for its running operations (009 FR-006).
        let acked = |applied: u64| pins.acked(applied, now);
        let out = self.driver.tick(&mut Host { core, now }, me.0, now.0, &acked);
        self.publish();
        out.into_iter().map(to_peer).collect()
    }

    /// A message for core 0: a request on the leader (held until a majority confirms it still
    /// leads), a grant on its node. Any other message is given back.
    pub fn on_peer<S: StateMachine + Group0Holder>(
        &mut self,
        core: &MultiRaftCore<S>,
        msg: PeerMsg,
        now: Nanos,
    ) -> Result<Vec<(NodeId, PeerMsg)>, PeerMsg> {
        match msg {
            PeerMsg::AuthLeaseAsk { node, seq, applied } => {
                self.driver.on_ask(node.0, seq, applied, now.0);
                Ok(Vec::new())
            }
            PeerMsg::AuthLeaseGrant { seq, commit, fenced, .. } => {
                self.driver.on_grant(seq, commit, fenced, applied(core));
                self.publish();
                Ok(Vec::new())
            }
            other => Err(other),
        }
    }

    fn publish(&self) {
        self.gate.set(self.driver.until_ms());
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use teseladb_auth::fence::{AUTH_LEASE_MS, DRIFT_MARGIN_MS};
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

        /// The node other than `out` that leads Group 0, if any.
        fn leader_without(&self, out: NodeId) -> Option<NodeId> {
            self.nodes
                .iter()
                .find(|(id, n)| {
                    **id != out && n.core.status(GROUP0).is_some_and(|s| s.role == Role::Leader)
                })
                .map(|(id, _)| *id)
        }

        /// Cuts off Group 0's leader for 30 s while the other two elect a leader and commit a
        /// change through it. Returns how long after the cut the old leader still authorized,
        /// and checks at every step that it never authorizes once the change has passed the
        /// fence.
        fn cut_off_the_leader(&mut self) -> Nanos {
            let old = self.leader();
            let cut_at = self.now;
            self.cut.insert(old);
            let mut last_held = Nanos::ZERO;
            let mut change: Option<(NodeId, u64)> = None;
            while self.now < cut_at + Nanos::from_secs(30) {
                self.step();
                let now = self.now;
                if self.nodes[&old].driver.holds(now) {
                    last_held = now.saturating_since(cut_at);
                }
                let Some(new) = self.leader_without(old) else { continue };
                match change {
                    None => {
                        let cmd = Group0Command::SetSetting { key: b"k".to_vec(), value: vec![] };
                        let n = self.nodes.get_mut(&new).unwrap();
                        if n.core.propose(GROUP0, &cmd.encode(), now).is_ok() {
                            let index = n.core.status(GROUP0).unwrap().last_index;
                            change = Some((new, index));
                        }
                    }
                    Some((by, index)) => {
                        if self.nodes[&by].driver.fenced() >= index {
                            assert!(
                                applied(&self.nodes[&old].core) >= index
                                    || !self.nodes[&old].driver.holds(now),
                                "the change passed the fence while the old leader authorizes \
                                 without it, {} ms after the cut",
                                now.saturating_since(cut_at).as_millis()
                            );
                        }
                    }
                }
            }
            let (by, index) = change.expect("the other two elect a leader and commit");
            assert!(self.nodes[&by].driver.fenced() >= index, "the change passed the fence");
            last_held
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

    /// The longest a node cut off may still authorize: a lease taken at the cut, plus the tick
    /// in which the test sees it.
    const CUT_OFF_LIMIT: Nanos = Nanos::from_millis(AUTH_LEASE_MS - DRIFT_MARGIN_MS + 10);

    /// Group 0's leader cut off from the rest keeps no lease of its own past one lease after the
    /// last round a quorum acknowledged, which it sent before the cut (013 FR-012, D-073).
    #[test]
    fn a_leader_cut_off_stops_authorizing_within_a_lease() {
        let mut c = Cluster::new();
        c.steps(300);
        let held = c.cut_off_the_leader();
        assert!(
            held <= CUT_OFF_LIMIT,
            "the leader cut off authorized {} ms after the cut",
            held.as_millis()
        );
    }

    /// The same with Group 0 quiet: a quiescent leader no longer counts ticks towards stepping
    /// down, so its role alone would keep its lease for good.
    #[test]
    fn a_quiet_leader_cut_off_stops_authorizing_within_a_lease() {
        let mut c = Cluster::new();
        c.steps(300);
        for _ in 0..1000 {
            if c.nodes[&c.leader()].core.is_quiesced(GROUP0) {
                break;
            }
            c.step();
        }
        let held = c.cut_off_the_leader();
        assert!(
            held <= CUT_OFF_LIMIT,
            "the quiet leader cut off authorized {} ms after the cut",
            held.as_millis()
        );
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
