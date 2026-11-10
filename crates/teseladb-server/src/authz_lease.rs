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
use teseladb_meta::indexes::{IndexEntry, Indexes, SearchIndexEntry};
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

/// Until when the node may authorize, for its client workers. Core 0 publishes how long its
/// lease has left and the gate closes that long after the publication on its own clock, which
/// comes later than core 0's reading: a lease ends here no later than core 0 counts it, whatever
/// the origin of either clock. (An end in core 0's clock compared with this one, whose origin
/// comes before the cores start, made every lease end here already when core 0 took longer than
/// a lease to start: seen on 06-11-2026, a node that replayed a large log authorized nothing.)
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
    /// The indexes of the catalog core 0 published for the connection threads, and the Group 0
    /// index they are at (009 FR-021): a thread reads and pins them without a message.
    published: Mutex<Option<(u64, Arc<Indexes>)>>,
    /// The index of [`CatalogPins::published`], which a thread reads again after pinning.
    published_at: AtomicU64,
}

#[derive(Debug, Default)]
struct Pins {
    next: u64,
    at: BTreeMap<u64, u64>,
    /// When each pin released late goes, on core 0's clock.
    until: BTreeMap<u64, Nanos>,
    /// The collection each pin read and when, and whether a stall of it was reported.
    about: BTreeMap<u64, (u32, std::time::Instant, bool)>,
}

/// A pin that holds the node's acknowledgement back longer than [`PIN_STALL`]: what the log says of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StalledPin {
    /// Group 0's index it read at.
    pub at: u64,
    /// The collection whose indexes it read.
    pub collection: u32,
    /// How long it has been held, in milliseconds.
    pub held_ms: u128,
    /// Whether its operation ended and it waits for its late release.
    pub late: bool,
}

/// A pin held longer than this while the node's copy is past it is written to the log once.
pub const PIN_STALL: std::time::Duration = std::time::Duration::from_secs(5);

impl CatalogPins {
    /// Core 0 publishes its copy of the indexes at Group 0 index `at` (009 FR-021). It does so
    /// before it acknowledges `at`: [`CatalogPins::acked_with`] goes no higher than what it
    /// published, so a thread that read an older publication and pinned it is seen
    /// (`CatalogPins.tla`).
    pub fn publish(&self, at: u64, indexes: Arc<Indexes>) {
        *self.published.lock().unwrap_or_else(PoisonError::into_inner) = Some((at, indexes));
        self.published_at.store(at, Ordering::SeqCst);
    }

    /// The indexes of `collection` as core 0 published them, read and pinned without a message
    /// (009 FR-021): the thread pins the publication's index, then reads the publication again
    /// and starts over if it moved. `None` while nothing is published.
    pub fn pin_published(&self, collection: u32) -> Option<(Vec<IndexEntry>, u64)> {
        loop {
            let (at, indexes) =
                self.published.lock().unwrap_or_else(PoisonError::into_inner).clone()?;
            let pin = self.pin_for(at, collection);
            if self.published_at.load(Ordering::SeqCst) == at {
                return Some((indexes.of(collection).cloned().collect(), pin));
            }
            self.release(pin);
        }
    }

    /// The search indexes of `collection` as core 0 published them, without a pin: no write
    /// waits for their changes (009 FR-021, H-039). `None` while nothing is published.
    pub fn published_search(&self, collection: u32) -> Option<Vec<SearchIndexEntry>> {
        let published = self.published.lock().unwrap_or_else(PoisonError::into_inner);
        let (_, indexes) = published.as_ref()?;
        Some(indexes.search_of(collection).cloned().collect())
    }

    /// Pins a read of the catalog at index `at`; the pin's number.
    pub fn pin(&self, at: u64) -> u64 {
        let mut p = self.lock();
        p.next += 1;
        let pin = p.next;
        p.at.insert(pin, at);
        pin
    }

    /// Pins a read of the indexes of `collection` at index `at`, as [`CatalogPins::pin`], noting the collection and
    /// when for [`CatalogPins::stalled`].
    pub fn pin_for(&self, at: u64, collection: u32) -> u64 {
        let pin = self.pin(at);
        self.lock().about.insert(pin, (collection, std::time::Instant::now(), false));
        pin
    }

    /// Releases pin `pin`; a pin already released changes nothing.
    pub fn release(&self, pin: u64) {
        let mut p = self.lock();
        p.at.remove(&pin);
        p.until.remove(&pin);
        p.about.remove(&pin);
    }

    /// The pins below `applied` held longer than [`PIN_STALL`] and not reported yet; each is reported once.
    pub fn stalled(&self, applied: u64) -> Vec<StalledPin> {
        self.stalled_with(applied, &|_| Some(u64::MAX))
    }

    /// As [`CatalogPins::stalled`], only the pins that hold a change back: those after which a
    /// change of their collection's indexes came, as `last_change` says.
    pub fn stalled_with(
        &self,
        applied: u64,
        last_change: &dyn Fn(u32) -> Option<u64>,
    ) -> Vec<StalledPin> {
        let mut p = self.lock();
        let Pins { at, until, about, .. } = &mut *p;
        let mut out = Vec::new();
        for (pin, (collection, since, told)) in about.iter_mut() {
            let Some(index) = at.get(pin).copied() else { continue };
            let holds = last_change(*collection).is_some_and(|changed| changed > index);
            if *told || !holds || index >= applied || since.elapsed() < PIN_STALL {
                continue;
            }
            *told = true;
            out.push(StalledPin {
                at: index,
                collection: *collection,
                held_ms: since.elapsed().as_millis(),
                late: until.contains_key(pin),
            });
        }
        out
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
    /// clock, every pin holding it back.
    pub fn acked(&self, applied: u64, now: Nanos) -> u64 {
        self.acked_with(applied, now, &|_| Some(u64::MAX))
    }

    /// The collections of the pins held.
    pub fn collections(&self) -> std::collections::BTreeSet<u32> {
        self.lock().about.values().map(|(c, _, _)| *c).collect()
    }

    /// The index the node may acknowledge with its copy at `applied`, at `now` on core 0's
    /// clock (009 FR-006): a read of the indexes of a collection holds it back only once a change
    /// of that collection's indexes came after it, as `last_change` says; one of another
    /// collection writes nothing the change needs. A pin whose collection is not known holds it
    /// back always.
    pub fn acked_with(
        &self,
        applied: u64,
        now: Nanos,
        last_change: &dyn Fn(u32) -> Option<u64>,
    ) -> u64 {
        // Nothing above what the threads can read (009 FR-021), once anything is published.
        let published = self.published_at.load(Ordering::SeqCst);
        let applied = if published == 0 { applied } else { applied.min(published) };
        let mut p = self.lock();
        let gone: Vec<u64> =
            p.until.iter().filter(|(_, at)| **at <= now).map(|(k, _)| *k).collect();
        for pin in gone {
            p.at.remove(&pin);
            p.until.remove(&pin);
            p.about.remove(&pin);
        }
        p.at.iter()
            .filter(|(pin, at)| {
                let Some((collection, _, _)) = p.about.get(pin) else { return true };
                last_change(*collection).is_some_and(|changed| changed > **at)
            })
            .map(|(_, at)| *at)
            .fold(applied, u64::min)
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

    /// The lease has `left` from now: zero closes the gate.
    fn set(&self, left: Duration) {
        let left = u64::try_from(left.as_nanos()).unwrap_or(u64::MAX);
        let until = if left == 0 { 0 } else { self.now().0.saturating_add(left) };
        self.until_ns.store(until, Ordering::Release);
    }
}

/// The lease of this node and, while it leads Group 0, the table of the others'
/// ([`LeaseDriver`]), publishing the lease in the node's [`AuthGate`].
#[derive(Debug)]
pub struct AuthLeaseDriver {
    driver: LeaseDriver,
    gate: Arc<AuthGate>,
    /// Requests sent and answers received, for the line of a node without a lease.
    asked: u64,
    answered: u64,
    /// When that line may be written again.
    report_at: Nanos,
}

/// How often a node without a lease says why (013 FR-012): its client commands get 13436.
const REPORT_EVERY: Nanos = Nanos(10_000_000_000);

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
        AuthLeaseDriver {
            driver: LeaseDriver::new(NODE_HEARTBEAT.0),
            gate,
            asked: 0,
            answered: 0,
            report_at: Nanos(0),
        }
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
        // The last change of the indexes of each pinned collection, read before the driver
        // borrows the core (009 FR-006).
        let changes: BTreeMap<u32, u64> = group0(core).map_or_else(BTreeMap::new, |sm| {
            pins.collections()
                .into_iter()
                .filter_map(|c| sm.index_changes.last(c).map(|i| (c, i)))
                .collect()
        });
        let last_change = |c: u32| changes.get(&c).copied();
        for s in pins.stalled_with(applied(core), &last_change) {
            // TeselaDB's own line: it has no RefDb identifier.
            crate::log::defer(crate::log::Pending {
                id: 0,
                msg: "a running operation holds the catalog changes of the indexes".to_owned(),
                attr: format!(
                    "{{\"node\":{},\"at\":{},\"collection\":{},\"heldMillis\":{},\"late\":{}}}",
                    me.0, s.at, s.collection, s.held_ms, s.late
                ),
            });
        }
        // What the node acknowledges waits for the running operations of the collections whose
        // indexes changed after they read them (009 FR-006).
        let acked = |applied: u64| pins.acked_with(applied, now, &last_change);
        let out = self.driver.tick(&mut Host { core, now }, me.0, now.0, &acked);
        self.publish(now);
        self.asked += out.iter().filter(|m| matches!(m, FenceMsg::Ask { .. })).count() as u64;
        self.report(core, now);
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
                self.answered += 1;
                self.driver.on_grant(seq, commit, fenced, applied(core));
                self.publish(now);
                Ok(Vec::new())
            }
            other => Err(other),
        }
    }

    /// Publishes what is left at `now`, on core 0's clock, of the lease.
    fn publish(&self, now: Nanos) {
        let until_ns = self.driver.until_ms().saturating_mul(NANOS_PER_MS);
        self.gate.set(Duration::from_nanos(until_ns.saturating_sub(now.0)));
    }

    /// The line of a node without a lease, every [`REPORT_EVERY`]: what it knows of Group 0 and
    /// how many requests and answers went, so that a node that keeps refusing says why.
    fn report<S: StateMachine + Group0Holder>(&mut self, core: &MultiRaftCore<S>, now: Nanos) {
        if self.holds(now) || now < self.report_at {
            return;
        }
        let first = self.report_at == Nanos(0);
        self.report_at = Nanos(now.0.saturating_add(REPORT_EVERY.0));
        if first {
            // A node that has just started has no lease yet: the line waits one period.
            return;
        }
        let Some(st) = core.status(GROUP0) else { return };
        crate::log::defer(crate::log::Pending {
            id: 0,
            msg: "no authorization lease: the node authorizes no command".to_owned(),
            attr: format!(
                "{{\"leader\":{},\"role\":\"{:?}\",\"commit\":{},\"applied\":{},\"asked\":{},\"answered\":{}}}",
                st.leader.0,
                st.role,
                st.hard_state.commit,
                applied(core),
                self.asked,
                self.answered,
            ),
        });
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

    /// A buffer the log writes to.
    #[derive(Clone, Default)]
    struct Lines(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Lines {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A node that keeps refusing says why every ten seconds: what it knows of Group 0 and the
    /// requests and answers that went (013 FR-012).
    #[test]
    fn a_node_without_a_lease_says_why() {
        let mut c = Cluster::new();
        c.steps(300);
        let held = c.cut_off_the_leader();
        assert!(held <= CUT_OFF_LIMIT);
        let lines = Lines::default();
        let log = crate::log::Logger::new(
            crate::log::Level::Info,
            crate::log::Format::Text,
            Box::new(lines.clone()),
        );
        crate::log::drain(&log);
        let text = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
        assert!(text.contains("no authorization lease"), "{text}");
        assert!(text.contains("asked"), "{text}");
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
    fn a_pin_that_holds_the_node_back_too_long_is_reported_once() {
        let pins = CatalogPins::default();
        let old = pins.pin_for(4, 21);
        let late = pins.pin_for(5, 22);
        let fresh = pins.pin_for(6, 23);
        let above = pins.pin_for(9, 24);
        let long_ago = std::time::Instant::now().checked_sub(PIN_STALL).unwrap();
        for pin in [old, late, above] {
            pins.lock().about.get_mut(&pin).unwrap().1 = long_ago;
        }
        pins.release_at(late, Nanos::from_millis(1_000));
        let stalled = pins.stalled(9);
        let seen: Vec<(u64, u32, bool)> =
            stalled.iter().map(|s| (s.at, s.collection, s.late)).collect();
        assert_eq!(
            seen,
            [(4, 21, false), (5, 22, true)],
            "the fresh one and the one at the copy are not"
        );
        assert!(stalled.iter().all(|s| s.held_ms >= PIN_STALL.as_millis()));
        assert!(pins.stalled(9).is_empty(), "once");
        pins.release(old);
        pins.release(fresh);
        assert!(!pins.lock().about.contains_key(&old) && !pins.lock().about.contains_key(&fresh));
        assert_eq!(pins.acked(9, Nanos::from_millis(1_000)), 9, "the late one went at its time");
        assert!(!pins.lock().about.contains_key(&late));
    }

    #[test]
    fn a_thread_pins_the_published_indexes_and_the_node_acknowledges_no_more_than_it_published() {
        // 009 FR-021: core 0 publishes the indexes at a Group 0 index; a thread reads and pins
        // them without a message, and the node acknowledges nothing above what it published.
        let pins = CatalogPins::default();
        let now = Nanos::ZERO;
        assert!(pins.pin_published(7).is_none(), "nothing published yet: the message path");
        let mut idx = Indexes::default();
        idx.indexes.insert(
            3,
            teseladb_meta::indexes::IndexEntry {
                id: 3,
                collection: 7,
                name: "a_1".to_owned(),
                spec: Vec::new(),
                buckets: 0,
                state: teseladb_meta::indexes::IndexState::Ready,
            },
        );
        pins.publish(5, Arc::new(idx));
        assert_eq!(pins.acked(9, now), 5, "not above what the threads can read");
        let (entries, pin) = pins.pin_published(7).expect("published");
        assert_eq!(entries.iter().map(|e| e.id).collect::<Vec<_>>(), [3]);
        assert!(pins.pin_published(8).expect("published").0.is_empty(), "another collection");
        // A change of collection 7 after index 5 waits for the pinned read.
        pins.publish(9, Arc::new(Indexes::default()));
        assert_eq!(pins.acked_with(9, now, &|c| (c == 7).then_some(8)), 5);
        pins.release(pin);
        assert_eq!(pins.acked_with(9, now, &|c| (c == 7).then_some(8)), 9);
    }

    #[test]
    fn the_published_search_indexes_are_read_without_a_message_nor_a_pin() {
        let pins = CatalogPins::default();
        assert!(pins.published_search(7).is_none(), "nothing published yet: the message path");
        let mut idx = Indexes::default();
        let entry = SearchIndexEntry {
            id: [1; 12],
            collection: 7,
            name: "default".to_owned(),
            kind: teseladb_meta::indexes::SearchIndexKind::Search,
            definition: Vec::new(),
            status: teseladb_meta::indexes::SearchIndexStatus::Ready,
            version: 2,
            defined_ms: 0,
        };
        idx.search.insert((7, "default".to_owned()), entry.clone());
        pins.publish(5, Arc::new(idx));
        assert_eq!(pins.published_search(7), Some(vec![entry]));
        assert_eq!(pins.published_search(8), Some(Vec::new()), "another collection");
        assert_eq!(pins.acked(9, Nanos::ZERO), 5, "no pin held");
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

    /// A read of the indexes holds back only a later change of its own collection's indexes (009
    /// FR-006): a long write on one collection does not hold back the changes of another.
    #[test]
    fn a_pin_holds_back_only_a_later_change_of_its_collection() {
        let pins = CatalogPins::default();
        let now = Nanos::ZERO;
        let x = pins.pin_for(4, 21);
        let y = pins.pin_for(5, 22);
        let changes = |c: u32| match c {
            21 => Some(7),
            22 => Some(3),
            _ => None,
        };
        assert_eq!(pins.acked_with(9, now, &changes), 4, "x's collection changed after it");
        assert_eq!(pins.acked(9, now), 4, "every pin holds without the changes");
        let unknown = pins.pin(2);
        assert_eq!(pins.acked_with(9, now, &changes), 2, "a pin of no known collection holds");
        pins.release(unknown);
        pins.release(x);
        assert_eq!(pins.acked_with(9, now, &changes), 9, "y read after its collection changed");
        assert_eq!(pins.acked_with(9, now, &|_| None), 9, "no change, nothing held");
        let long_ago = std::time::Instant::now().checked_sub(PIN_STALL).unwrap();
        pins.lock().about.get_mut(&y).unwrap().1 = long_ago;
        assert!(pins.stalled_with(9, &changes).is_empty(), "y holds nothing back");
        assert_eq!(pins.stalled(9).len(), 1, "without the changes it would be reported");
        assert_eq!(pins.collections(), [22].into_iter().collect());
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
        gate.set(Duration::from_secs(3_600));
        assert!(gate.valid());
        gate.set(Duration::ZERO);
        assert!(!gate.valid());
        gate.set(Duration::from_nanos(1));
        std::thread::sleep(Duration::from_millis(1));
        assert!(!gate.valid(), "a lease that ran out");
    }

    /// The gate's origin comes before the cores start; core 0's clock starts with its runtime,
    /// after replaying its log. A lease core 0 holds holds at the gate however long that took.
    #[test]
    fn a_lease_holds_at_the_gate_whatever_core_0_took_to_start() {
        let gate = Arc::new(AuthGate {
            origin: Instant::now() - Duration::from_secs(10),
            ..AuthGate::default()
        });
        let d = AuthLeaseDriver::with_gate(Arc::clone(&gate));
        // Without a lease the gate stays closed.
        d.publish(Nanos::from_millis(2));
        assert!(!gate.valid());
        // A lease of core 0 that ends 1.75 s after its 2 ms holds at the gate, whose clock reads 10 s.
        gate.set(Duration::from_millis(1_750));
        assert!(gate.valid(), "core 0 at 2 ms, the gate at 10 s: the lease still holds");
    }
}
