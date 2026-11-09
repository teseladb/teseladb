//! The logic of a core over the I/O runtime (001 FR-018, 005 FR-015 and FR-016, 007 FR-003): a
//! [`MultiRaftCore`] whose core 0 holds Group 0 and whose every core holds the data ranges Group 0
//! places on it ([`DataCore`]); core 0 hands them Group 0's ranges and publishes the routing
//! table the client workers read. The founder proposes `Bootstrap`; Group 0's leader takes the joins (`AddNode`,
//! then `AddLearner`) and promotes a learner to voter once it has caught up, up to
//! [`GROUP0_MAX_VOTERS`]. A node whose name or addresses changed updates them with `SetNodeAddrs`
//! (FR-023), and every node records its start with `NodeBoot`, forwarding them to the leader
//! when it does not lead. Group 0's leader creates the data range (`CreateRange`) and, every
//! [`BALANCE_EVERY`], runs the balancer, whose replica changes the range's leader carries out.
//!
//! The core's disk operations, the log's, the rollback files', the LSM's and the snapshot
//! transfers', go to one disk thread whose results come back in submission order: a queue
//! remembers whose each one is. The entries of a range this replica drops in a rollback go to
//! the rollback files ([`RollbackWriter`]).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::Sender;

use teseladb_io::handshake::JoinRequest;
use teseladb_io::mailbox::SendError;
use teseladb_io::peer::{JoinAccepted, PeerMsg};
use teseladb_io::runtime::{CoreInput, CoreLogic, CoreSender};
use teseladb_meta::balancer::{BALANCE_EVERY, Decision, View, plan};
use teseladb_meta::bootstrap::{create_data_range, data_range, needs_data_range};
use teseladb_meta::command::{Group0Command, NodeAddrs, NodeState};
use teseladb_meta::group0::{GROUP0, Group0Result, Group0Sm, RangeDesc};
use teseladb_meta::liveness::{DEAD_AFTER, DEAD_AFTER_SETTING, dead_after};
use teseladb_meta::meta2::Meta2Command;
use teseladb_multiraft::{Done, MultiRaftCore, OpError, OpId, PeerFrame, route_stray};
use teseladb_range::host::RangeError;
use teseladb_range::rollback::rollback_files;
use teseladb_wire::commands::Shared;
use teseladb_wire::commands::session::{KillScope, kill_local};
use teseladb_raft::{ConfChange, ConfChangeKind, Role};
use teseladb_raftlog::{IoDone, IoOp};
use teseladb_types::{Bytes, GroupId, Nanos, NodeId};

use crate::authz_lease::{AuthGate, AuthLeaseDriver};
use crate::catalog_ops::{CatalogAnswer, CatalogOps, CatalogReq, CatalogSender};
use crate::clustertime_keys::KeyDriver;
use crate::coresm::{CoreOutput, CoreSm, Group0Holder};
use crate::data::{DataCore, DataReq, DataSender, IoOwner};
use crate::forward::{GATE_BEHIND, GATE_HELD, Request};
use crate::frames::{from_wire, to_wire};
use crate::liveness::{LivenessDriver, SharedLoads, core_load};
use crate::node::FOUNDER;
use crate::ranges::Ranges;
use crate::relocations::RelocateMsg;
use crate::rollback::RollbackWriter;
use crate::rollback::ref_layout;
use crate::routes::SharedRoutes;
use crate::sessions_ops::{AnswersSender, KillSender, NodeQueries, SessionOps};
use teseladb_meta::drain::DrainError;

use crate::client_listener::NodePresence;
use teseladb_meta::security::{Expect, Principal, SecurityOp};

/// Time between two proposals of the `root` user while the cluster has none (013).
const ROOT_RETRY: Nanos = Nanos::from_millis(2_000);
/// How soon the balancer looks again after a round that saw a replica change in progress.
const SETTLE_CHECK: Nanos = Nanos::from_millis(100);

/// Whose a disk operation in flight is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IoSlot {
    /// The log's.
    Log,
    /// The rollback files'.
    Rollback,
    /// The data range's (its LSM or its transfers).
    Data(IoOwner),
}

/// Group 0 voters at most; the other nodes stay learners.
pub const GROUP0_MAX_VOTERS: usize = 5;
/// The states a node's `topologyVersion` counter tells apart (active, draining, removed); the
/// presentation's changes count in multiples of it.
const TOPOLOGY_STATES: i64 = 3;

/// Passes over the reads at the end of a turn: a pass can answer reads that register more; what
/// is left goes at the next turn.
const READ_ROUND_PASSES: usize = 4;

/// Certificate fingerprint of a node in `--insecure` mode: derived from its inter-node address, so
/// that joining again from the same address gives back the same node.
pub fn insecure_fingerprint(peer_addr: &str) -> [u8; 32] {
    teseladb_hash::sha256(peer_addr.as_bytes())
}

/// What a core's copy of Group 0 shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Group0View {
    /// The cluster exists (`Bootstrap` applied).
    pub bootstrapped: bool,
    /// Cluster identifier.
    pub cluster_id: [u8; 16],
    /// Role of this replica (`None` if this core has no Group 0).
    pub role: Option<Role>,
    /// Known leader (0 = none).
    pub leader: NodeId,
    /// Voters.
    pub voters: Vec<NodeId>,
    /// Learners.
    pub learners: Vec<NodeId>,
    /// Nodes of the cluster.
    pub nodes: Vec<NodeId>,
    /// The state of each node of the cluster, removed ones too.
    pub node_states: Vec<(NodeId, NodeState)>,
    /// Group 0 has this node's current name and addresses (always true without them).
    pub addrs_current: bool,
    /// Group 0's descriptor of the data range, once created (005 FR-016).
    pub data_range: Option<RangeDesc>,
    /// The `root` user this node creates on a secure cluster's first start is not in Group 0
    /// yet: a client that logs in with its credentials would fail (013).
    pub root_pending: bool,
}

impl Group0View {
    /// The node serves clients: the cluster exists, Group 0 has this node's addresses and the
    /// `root` user this node creates, if any.
    pub fn ready(&self) -> bool {
        self.bootstrapped && self.addrs_current && !self.root_pending
    }
}

/// Messages a core takes from the rest of the process.
#[derive(Debug)]
pub enum NodeMsg {
    /// A node asks to join the cluster (core 0, which holds Group 0).
    Join(JoinRequest, Sender<Result<JoinAccepted, String>>),
    /// This core's view of Group 0.
    View(Sender<Group0View>),
    /// The Group 0 command that drains (`true`) or removes a node, from core 0's Group 0 (007
    /// FR-015); the port proposes it. `None`: this core has no Group 0.
    Drain(NodeId, bool, Sender<Option<Result<Group0Command, DrainError>>>),
    /// A request of the node's handle for a data range (on the range's core).
    Data(DataReq, DataSender),
    /// The mailboxes of the node's cores, in core order (sent once at start).
    Cores(Vec<CoreSender<NodeMsg>>),
    /// Group 0's ranges, from core 0 to the other cores.
    Ranges(Ranges),
    /// A Group 0 command of a range's core for core 0 (`ReplicasSettled`, `SplitRange`,
    /// `CompleteSplit`).
    Group0(Group0Command),
    /// The inter-node addresses of the cluster's nodes, from core 0 to the cores without Group 0.
    Peers(BTreeMap<NodeId, String>),
    /// A catalog request of the node's handle (core 0, 006 FR-027).
    Catalog(CatalogReq, CatalogSender),
    /// Sessions to kill on the other nodes (core 0, 006 FR-016).
    KillSessions(KillScope, KillSender),
    /// Whether every other node not removed applies write gates `want`; one that cannot be
    /// reached does not count (core 0, 007 FR-029).
    GateEverywhere(teseladb_wire::commands::write_block::WriteGate, KillSender),
    /// The operations in progress on every other node, each node's answer (core 0, 010
    /// FR-016).
    CurrentOps(AnswersSender),
    /// A request of the HTTP port for node `.0`, which opened its cursor or session (core 0,
    /// 019 FR-008): its answer, or none when the node cannot be reached.
    ForwardHttp(NodeId, Vec<u8>, AnswersSender),
    /// The answer to the HTTP request node `.0` forwarded as its request `.1` (core 0).
    HttpAnswer(NodeId, u64, Vec<u8>),
    /// A core change between two cores of the node (007 FR-011).
    Relocate(RelocateMsg),
    /// A frame another core of the node received for a range this core runs (007 FR-011: the
    /// range moved between cores), with the node that sent it.
    Stray(NodeId, PeerFrame),
}

/// Requests other nodes forward to the node's HTTP port: the node that sent each, its request
/// number and the request (019 FR-008).
pub type HttpForwards = std::sync::mpsc::Sender<(NodeId, u64, Vec<u8>)>;

/// Step of a join waiting for an operation.
#[derive(Debug)]
enum JoinStep {
    /// Waiting for `AddNode`.
    AddNode,
    /// Waiting for the `AddLearner` of this node.
    Learner(NodeId),
}

type JoinReply = Sender<Result<JoinAccepted, String>>;

/// Logic of one core.
pub struct NodeLogic {
    core: MultiRaftCore<CoreSm>,
    /// Pending `Bootstrap` of the founder: the cluster and the first node.
    founder: Option<([u8; 16], NodeAddrs)>,
    bootstrap: Option<OpId>,
    /// This node and its current name and addresses (core 0).
    me: Option<(NodeId, NodeAddrs)>,
    /// Pending `SetNodeAddrs`, proposed or forwarded.
    addr_update: Option<OpId>,
    /// Without a known leader, `SetNodeAddrs` goes to Group 0's voters in turn: the next one.
    addr_probe: usize,
    joins: BTreeMap<OpId, (JoinStep, JoinReply)>,
    promoting: Option<OpId>,
    outbound: Vec<(NodeId, PeerMsg)>,
    /// Node heartbeats and clock reports (core 0).
    liveness: LivenessDriver,
    /// The authorization lease and fence (core 0, 013).
    authz: AuthLeaseDriver,
    /// The `root` user to create while the cluster has none, and when to try next (core 0, 013).
    root_user: Option<(Vec<u8>, Nanos)>,
    /// The `$clusterTime` signing keys (core 0 of a node with authentication, 013).
    clustertime_keys: Option<KeyDriver>,
    now: Nanos,
    /// Wall clock at the last tick (Unix ms).
    wall_ms: u64,
    /// The data ranges of this core.
    data: Option<DataCore>,
    /// Whose each disk operation in flight is.
    io: VecDeque<IoSlot>,
    /// Rollback files being written.
    rollback: RollbackWriter,
    /// The node's cores (empty until [`NodeMsg::Cores`]).
    cores: Vec<CoreSender<NodeMsg>>,
    /// This start's `boot_epoch` (core 0), recorded in Group 0 with `NodeBoot`.
    boot_epoch: Option<u32>,
    boot_op: Option<OpId>,
    /// The tags this node started with (`--tags`, core 0), recorded with `SetNodeTags` (007
    /// FR-013).
    tags: Vec<(String, String)>,
    tags_op: Option<OpId>,
    /// The record of this node's versions in Group 0 (007 FR-027), in flight.
    versions_op: Option<OpId>,
    /// The nodes' tags last published for the client workers.
    sent_tags: Option<crate::routes::NodeTags>,
    /// The tenants' states last published for the client workers (007 FR-034), and the index of
    /// Group 0 they were read at: they are read again only after Group 0 applies something.
    sent_tenants: Option<(u64, crate::routes::TenantStates)>,
    create_op: Option<OpId>,
    balance_op: Option<OpId>,
    /// Group 0's automatic reassignment of the designated primary in flight (018 FR-007).
    reassign_op: Option<OpId>,
    balance_at: Nanos,
    /// The nodes the balancer last saw, with their state and whether they started: a change runs
    /// it before its next round.
    balanced_nodes: Vec<(NodeId, NodeState, bool)>,
    /// When the balancer looks again after a round that saw a replica change in progress.
    settle_check_at: Option<Nanos>,
    /// When the balancer last looked.
    balance_round: Nanos,
    /// Group 0 commands of the range's core, proposed or forwarded.
    group0_ops: BTreeSet<OpId>,
    /// Group 0's ranges last given to the cores (core 0).
    sent_ranges: Option<Ranges>,
    /// The catalog last published in the routes (H-017).
    published_catalog: Option<Arc<teseladb_meta::catalog::Catalog>>,
    /// The Group 0 index at which the settings were last looked at, and the settings published.
    published_settings: (u64, Option<Arc<crate::routes::Settings>>),
    /// The routing table the client workers read (core 0 writes it).
    routes: Option<SharedRoutes>,
    /// The nodes' inter-node addresses: given to the other cores by core 0, which has them in
    /// Group 0.
    peers: BTreeMap<NodeId, String>,
    /// Catalog requests in flight (core 0).
    catalog: CatalogOps,
    /// Kills of sessions sent to the other nodes (core 0).
    kills: SessionOps,
    /// Questions to the other nodes about the write gates they apply (core 0, 007 FR-029).
    gates: SessionOps,
    /// Questions to the other nodes about their operations in progress (core 0, 010 FR-016).
    op_lists: NodeQueries,
    /// The node's cursors and sessions, where the other nodes' kills land (core 0).
    sessions: Option<Arc<Shared>>,
    /// This core's index and where it publishes its load for core 0's heartbeat (007 FR-010).
    loads: Option<(usize, Arc<SharedLoads>)>,
    /// Where core 0 tells the clients' port the node's state in Group 0 (007 FR-015).
    presence: Option<Arc<NodePresence>>,
    /// Where core 0 hands the HTTP requests other nodes forward to the node's HTTP port.
    http_forwards: Option<HttpForwards>,
    /// The duties of core 0 for the `Meta2` ranges (007 FR-014).
    meta2: crate::meta2_ops::Meta2Ops,
    /// The node's time zone, where the balancer reads its window (core 0, 007 FR-017).
    zone: Option<teseladb_tz::Zone>,
}

impl NodeLogic {
    /// Logic over `core`; `founder` is set on core 0 of the node that creates the cluster, and `me`
    /// on core 0 of every node.
    pub fn new(
        core: MultiRaftCore<CoreSm>,
        founder: Option<([u8; 16], NodeAddrs)>,
        me: Option<(NodeId, NodeAddrs)>,
    ) -> NodeLogic {
        let mut core = core;
        // The ReadIndex reads of a whole turn of the loop share one heartbeat round per range
        // (005 FR-009): `take_outbound`, at the end of the turn, sends it.
        core.hold_read_rounds(true);
        NodeLogic {
            core,
            founder,
            bootstrap: None,
            me,
            addr_update: None,
            addr_probe: 0,
            joins: BTreeMap::new(),
            promoting: None,
            outbound: Vec::new(),
            liveness: LivenessDriver::default(),
            authz: AuthLeaseDriver::default(),
            root_user: None,
            clustertime_keys: None,
            now: Nanos::ZERO,
            wall_ms: 0,
            data: None,
            io: VecDeque::new(),
            rollback: RollbackWriter::default(),
            cores: Vec::new(),
            boot_epoch: None,
            boot_op: None,
            tags: Vec::new(),
            tags_op: None,
            versions_op: None,
            sent_tags: None,
            sent_tenants: None,
            create_op: None,
            balance_op: None,
            reassign_op: None,
            balance_at: Nanos::ZERO,
            balanced_nodes: Vec::new(),
            settle_check_at: None,
            balance_round: Nanos::ZERO,
            meta2: crate::meta2_ops::Meta2Ops::default(),
            zone: None,
            group0_ops: BTreeSet::new(),
            sent_ranges: None,
            published_catalog: None,
            published_settings: (0, None),
            routes: None,
            peers: BTreeMap::new(),
            catalog: CatalogOps::default(),
            kills: SessionOps::default(),
            gates: SessionOps::default(),
            op_lists: NodeQueries::default(),
            sessions: None,
            loads: None,
            presence: None,
            http_forwards: None,
        }
    }

    /// Core 0 publishes the node's state in Group 0 for the clients' port.
    pub fn with_presence(self, presence: Arc<NodePresence>) -> NodeLogic {
        NodeLogic { presence: Some(presence), ..self }
    }

    /// Where core 0 hands the HTTP requests other nodes forward (019 FR-008).
    pub fn with_http_forwards(self, forwards: HttpForwards) -> NodeLogic {
        NodeLogic { http_forwards: Some(forwards), ..self }
    }

    /// The node's `topologyVersion` counter, and whether it drained (it holds nothing and was
    /// asked to drain, or it is removed): then its clients' port closes (007 FR-015).
    fn publish_presence(&self) {
        let (Some(presence), Some((me, _)), Some(sm)) = (&self.presence, &self.me, self.group0())
        else {
            return;
        };
        let Some(state) = sm.nodes.get(me).map(|n| n.state) else { return };
        let voters = self.core.conf(GROUP0).map(|c| c.voters.clone()).unwrap_or_default();
        // A node that left Group 0 may never learn it: the port closes once its data is gone.
        let closed = match state {
            NodeState::Active => false,
            NodeState::Draining => teseladb_meta::drain::progress(sm, &voters, *me).replicas == 0,
            NodeState::Removed => true,
        };
        // Each change of the presentation raises its term or its `setVersion`, and each change of
        // a member's hold its version, all of which only grow: the counter grows with them and wakes the waiting `hello`s (018 FR-015).
        let shown = crate::config_db::presentation(sm);
        let version = shown.as_ref().map_or(0, |p| p.term.saturating_add(p.set_version));
        // A member that enters or leaves maintenance mode changes its `hello` too.
        let held: i64 = crate::config_db::holds(sm).iter().map(|(_, h)| h.version).sum();
        let version = version.saturating_add(held);
        let counter = teseladb_meta::drain::topology_counter(state)
            .saturating_add(version.saturating_mul(TOPOLOGY_STATES));
        if shown.is_some() {
            presence.set_replica(crate::config_db::replica_member(sm, *me));
            presence.set_replica_port(crate::config_db::replica_port_member(sm, *me));
        }
        presence.set(counter, closed);
    }

    /// Core `core` publishes its load in `loads` at every tick; core 0 sends the node's.
    pub fn with_loads(self, core: u16, loads: Arc<SharedLoads>) -> NodeLogic {
        NodeLogic { loads: Some((usize::from(core), loads)), ..self }
    }

    /// The node's load for the heartbeat, after publishing this core's: the operations of its
    /// data ranges' leaders count.
    fn node_load(&self) -> teseladb_meta::liveness::NodeLoad {
        let ops = self.data.as_ref().map_or(0, |d| d.ops(&self.core));
        let mine = core_load(&self.core, ops);
        match &self.loads {
            Some((core, loads)) => {
                loads.publish(*core, mine);
                loads.node()
            }
            None => mine,
        }
    }

    /// The node's cursors and sessions, for the kills other nodes send (core 0).
    pub fn with_sessions(self, sessions: Arc<Shared>) -> NodeLogic {
        NodeLogic { sessions: Some(sessions), ..self }
    }

    /// Sends the sessions of `scope` to every other node of the cluster (an operation, to its
    /// node); `reply` learns when all answered.
    fn kill_sessions(&mut self, scope: KillScope, reply: KillSender) {
        let me = self.me.as_ref().map_or(NodeId(0), |(id, _)| *id);
        // An operation goes to its node alone.
        let only = match &scope {
            KillScope::Op { node, .. } => Some(NodeId(*node)),
            KillScope::All | KillScope::Sessions(_) => None,
        };
        let body = Request::KillSessions(scope).encode();
        let others: Vec<NodeId> = self
            .peers
            .keys()
            .copied()
            .filter(|n| *n != me && only.is_none_or(|o| o == *n))
            .collect();
        let ops = others
            .into_iter()
            .map(|to| self.core.forward(to, Bytes::from_vec(body.clone()), self.now))
            .collect();
        self.kills.start(ops, reply);
    }

    /// Asks every other node for its operations in progress (010 FR-016).
    fn current_ops(&mut self, reply: AnswersSender) {
        let me = self.me.as_ref().map_or(NodeId(0), |(id, _)| *id);
        let body = Request::CurrentOps.encode();
        let others: Vec<NodeId> = self.group0().map_or_else(Vec::new, |sm| {
            sm.nodes
                .values()
                .filter(|n| n.id != me && n.state != NodeState::Removed)
                .map(|n| n.id)
                .collect()
        });
        let ops = others
            .into_iter()
            .map(|to| self.core.forward(to, Bytes::from_vec(body.clone()), self.now))
            .collect();
        self.op_lists.start(ops, reply);
    }

    /// The node's operations in progress as `currentOp` describes them, in the answer to
    /// [`Request::CurrentOps`]: a document whose `ops` holds them.
    fn own_operations(&self) -> Vec<u8> {
        let (node, host) = self.me.as_ref().map_or((0, String::new()), |(id, a)| {
            (id.0, String::from_utf8_lossy(&a.native_addr).into_owned())
        });
        let ops: Vec<teseladb_bson::Value> = self.sessions.as_ref().map_or_else(Vec::new, |s| {
            teseladb_wire::commands::admin::node_operations(s, node, &host, crate::clock::wall_ms())
                .into_iter()
                .map(teseladb_bson::Value::Document)
                .collect()
        });
        teseladb_bson::Document::new()
            .with("ops", teseladb_bson::Value::Array(ops))
            .to_bytes()
            .unwrap_or_default()
    }

    /// Asks every other node not removed whether it applies write gates `want` (007 FR-029).
    fn gate_everywhere(
        &mut self,
        want: teseladb_wire::commands::write_block::WriteGate,
        reply: KillSender,
    ) {
        let me = self.me.as_ref().map_or(NodeId(0), |(id, _)| *id);
        let others: Vec<NodeId> = self.group0().map_or_else(Vec::new, |sm| {
            sm.nodes
                .values()
                .filter(|n| n.id != me && n.state != NodeState::Removed)
                .map(|n| n.id)
                .collect()
        });
        let body = Request::Gate(want).encode();
        let ops = others
            .into_iter()
            .map(|to| self.core.forward(to, Bytes::from_vec(body.clone()), self.now))
            .collect();
        self.gates.start(ops, reply);
    }

    /// This core holds data ranges.
    pub fn with_data(self, data: DataCore) -> NodeLogic {
        NodeLogic { data: Some(data), ..self }
    }

    /// Core 0 writes the routing table the client workers read.
    pub fn with_routes(self, routes: SharedRoutes) -> NodeLogic {
        NodeLogic { routes: Some(routes), ..self }
    }

    /// This node started with `boot_epoch` (core 0): Group 0 records it (`NodeBoot`), which makes
    /// the node eligible for replicas.
    pub fn with_boot_epoch(self, boot_epoch: u32) -> NodeLogic {
        NodeLogic { boot_epoch: Some(boot_epoch), ..self }
    }

    /// This node started with `tags`, sorted by name (core 0): Group 0 records them
    /// (`SetNodeTags`), and reads with a `readPreference` match them (007 FR-013).
    pub fn with_tags(self, tags: Vec<(String, String)>) -> NodeLogic {
        NodeLogic { tags, ..self }
    }

    /// The node's time zone (core 0), where the balancer reads the window of `config.settings`
    /// (007 FR-017).
    pub fn with_zone(self, zone: Option<teseladb_tz::Zone>) -> NodeLogic {
        NodeLogic { zone, ..self }
    }

    /// Core 0 creates user `doc` (the BSON of `admin.system.users`) while the cluster has no
    /// user: the `root` of the first start (013 FR-004).
    pub fn with_root_user(self, doc: Vec<u8>) -> NodeLogic {
        NodeLogic { root_user: Some((doc, Nanos::ZERO)), ..self }
    }

    /// Proposes the `root` user while Group 0 is bootstrapped and has no user; a proposal that
    /// fails or races with another node's is tried again after [`ROOT_RETRY`].
    fn create_root(&mut self) {
        let Some((doc, at)) = &self.root_user else { return };
        let Some(sm) = self.group0() else { return };
        if !sm.bootstrapped() {
            return;
        }
        if !sm.security.list(Principal::User, None).is_empty() {
            self.root_user = None;
            return;
        }
        if self.now < *at {
            return;
        }
        let Ok(op) = SecurityOp::put(Principal::User, Expect::Absent, doc.clone()) else {
            self.root_user = None;
            return;
        };
        self.root_user = Some((doc.clone(), self.now + ROOT_RETRY));
        let (tx, _rx) = std::sync::mpsc::channel();
        self.catalog_change(Group0Command::Security(vec![op]).encode(), tx);
    }

    /// Core 0 publishes the `$clusterTime` signing keys with `keys` and, while it leads Group 0,
    /// makes and drops them (013 FR-014).
    pub fn with_clustertime_keys(self, keys: KeyDriver) -> NodeLogic {
        NodeLogic { clustertime_keys: Some(keys), ..self }
    }

    /// Publishes Group 0's signing keys when they changed and, on its leader, proposes the keys
    /// due and the drops of the old ones; Group 0 takes a key only on the state it was made on,
    /// so a lost race is made again at the next look.
    fn clustertime_keys(&mut self) {
        let Some(mut driver) = self.clustertime_keys.take() else { return };
        let leads = self.is_leader();
        let (now, wall_ms) = (self.now, self.wall_ms);
        let mut proposals = Vec::new();
        if let Some(sm) = self.group0().filter(|sm| sm.bootstrapped()) {
            let unopened = driver.publish(&sm.clustertime_keys);
            if unopened > 0 {
                // TeselaDB's own line: it has no RefDb identifier.
                crate::log::defer(crate::log::Pending {
                    id: 0,
                    msg: "$clusterTime signing keys that this node's master key does not open"
                        .to_owned(),
                    attr: format!("{{\"keys\":{unopened}}}"),
                });
            }
            if leads {
                proposals = driver.propose(&sm.clustertime_keys, now, wall_ms);
            }
        }
        for cmd in proposals {
            // A proposal that fails is made again at the next look.
            let _ = self.core.propose(GROUP0, &cmd.encode(), now);
        }
        self.clustertime_keys = Some(driver);
    }

    /// Core 0 publishes the node's authorization lease in `gate` for its client workers (013).
    pub fn with_auth_gate(self, gate: Arc<AuthGate>) -> NodeLogic {
        NodeLogic { authz: AuthLeaseDriver::with_gate(gate), ..self }
    }

    /// Group 0's state, on the core that holds it.
    fn group0(&self) -> Option<&Group0Sm> {
        self.core.state_machine(GROUP0).and_then(Group0Holder::group0)
    }

    /// This core's view of Group 0.
    pub fn view(&self) -> Group0View {
        let Some(sm) = self.group0() else { return Group0View::default() };
        let status = self.core.status(GROUP0);
        let conf = self.core.conf(GROUP0);
        Group0View {
            bootstrapped: sm.bootstrapped(),
            cluster_id: sm.cluster_id,
            role: status.as_ref().map(|s| s.role),
            leader: status.map_or(NodeId(0), |s| s.leader),
            voters: conf.map(|c| c.voters.clone()).unwrap_or_default(),
            learners: conf.map(|c| c.learners.clone()).unwrap_or_default(),
            nodes: sm.nodes.keys().copied().collect(),
            node_states: sm.nodes.iter().map(|(id, info)| (*id, info.state)).collect(),
            addrs_current: self.addrs_current(),
            data_range: data_range(sm).cloned(),
            root_pending: self.root_user.is_some(),
        }
    }

    /// Whether Group 0 has this node's current name and addresses (true without `me`).
    fn addrs_current(&self) -> bool {
        let Some((id, mine)) = &self.me else { return true };
        let Some(n) = self.group0().and_then(|sm| sm.nodes.get(id)) else {
            return false;
        };
        let a = &n.addrs;
        (&a.name, &a.peer_addr, &a.http_addr, &a.native_addr)
            == (&mine.name, &mine.peer_addr, &mine.http_addr, &mine.native_addr)
    }

    /// Proposes this node's `SetNodeAddrs` when Group 0 has other ones, or forwards it to the
    /// leader; one at a time, tried again on the next tick if it did not take. Without a known
    /// leader it goes to the voters in turn: a node back on another port that cannot be reached
    /// (a learner does not campaign) dials them, and the leader learns its port from the
    /// connection.
    fn update_addrs(&mut self) {
        let Some((id, mine)) = &self.me else { return };
        let known = self.group0().is_some_and(|sm| sm.nodes.contains_key(id));
        if self.addr_update.is_some() || !known || self.addrs_current() {
            return;
        }
        let cmd = Group0Command::SetNodeAddrs { node: *id, addrs: mine.clone() };
        self.addr_update = self.send_group0(cmd);
    }

    /// Proposes `cmd` on Group 0's leader, or forwards it there; without a known leader it goes to
    /// the voters in turn.
    fn send_group0(&mut self, cmd: Group0Command) -> Option<OpId> {
        // After the handoff, an order on a descriptor goes to the `Meta2` range that holds it.
        if let Some(g) = crate::meta2_ops::Meta2Ops::holder(&self.core, &cmd) {
            return self.send_meta2(g, Meta2Command::Descriptor(cmd));
        }
        if self.is_leader() {
            return self.core.propose(GROUP0, &cmd.encode(), self.now).ok();
        }
        let leader = self.core.status(GROUP0).map_or(NodeId(0), |s| s.leader);
        let me = self.me.as_ref().map_or(NodeId(0), |(id, _)| *id);
        let to = if leader != NodeId(0) { Some(leader) } else { self.next_voter(me) };
        let body = Bytes::from_vec(Request::Group0(cmd).encode());
        to.map(|to| self.core.forward(to, body, self.now))
    }

    /// Sends `cmd` to the leader of `Meta2` range `g`: proposed here if this node leads it,
    /// forwarded to its leader or, without a known one, to a voter.
    fn send_meta2(&mut self, g: GroupId, cmd: Meta2Command) -> Option<OpId> {
        let status = self.core.status(g)?;
        if status.role == Role::Leader {
            return self.core.propose(g, &cmd.encode(), self.now).ok();
        }
        let me = self.me.as_ref().map_or(NodeId(0), |(id, _)| *id);
        let voter = self.core.conf(g).and_then(|c| c.voters.iter().copied().find(|v| *v != me));
        let to = Some(status.leader).filter(|l| *l != NodeId(0) && *l != me).or(voter)?;
        let body = Request::Meta2 { group: g, entry: cmd.encode() }.encode();
        Some(self.core.forward(to, Bytes::from_vec(body), self.now))
    }

    /// The duties of core 0 for the `Meta2` ranges: their replicas, seeds, `Meta2Seeded`, cluster
    /// facts and the handoff (007 FR-014).
    fn meta2_step(&mut self) {
        if self.me.is_none() {
            return;
        }
        self.meta2.open(&mut self.core);
        for send in self.meta2.step(&mut self.core, self.now) {
            let op = match send {
                crate::meta2_ops::Send::Group0(cmd) => self.send_group0(cmd),
                crate::meta2_ops::Send::Meta2(g, cmd) => self.send_meta2(g, cmd),
            };
            if let Some(op) = op {
                self.group0_ops.insert(op);
            }
        }
    }

    /// Records this start in Group 0 (`NodeBoot`) once Group 0 knows the node; one at a time.
    fn announce_boot(&mut self) {
        let (Some(epoch), Some((id, _))) = (self.boot_epoch, &self.me) else { return };
        let id = *id;
        let behind =
            self.group0().and_then(|sm| sm.nodes.get(&id)).is_some_and(|n| n.boot_epoch < epoch);
        if behind && self.boot_op.is_none() {
            self.boot_op =
                self.send_group0(Group0Command::NodeBoot { node: id, boot_epoch: epoch });
        }
    }

    /// Records the versions this node runs in Group 0 while they differ there (007 FR-027); one
    /// at a time.
    fn announce_versions(&mut self) {
        let Some((id, _)) = &self.me else { return };
        let (key, value) = (crate::node_versions::key(*id), crate::node_versions::ours());
        let differ = self.group0().is_some_and(|sm| sm.settings.get(&key) != Some(&value));
        if differ && self.versions_op.is_none() {
            self.versions_op = self.send_group0(Group0Command::SetSetting { key, value });
        }
    }

    /// Records this node's tags in Group 0 (`SetNodeTags`) while they differ there; one at a time.
    fn announce_tags(&mut self) {
        let Some((id, _)) = &self.me else { return };
        let id = *id;
        let differ =
            self.group0().and_then(|sm| sm.nodes.get(&id)).is_some_and(|n| n.tags != self.tags);
        if differ && self.tags_op.is_none() {
            let cmd = Group0Command::SetNodeTags { node: id, tags: self.tags.clone() };
            self.tags_op = self.send_group0(cmd);
        }
    }

    /// Group 0's leader creates the data range once the cluster exists (005 FR-016).
    fn create_range(&mut self) {
        let leads = self.is_leader() && self.create_op.is_none();
        if leads && self.group0().is_some_and(needs_data_range) {
            let cmd = create_data_range(FOUNDER).encode();
            self.create_op = self.core.propose(GROUP0, &cmd, self.now).ok();
        }
    }

    /// Every [`BALANCE_EVERY`], Group 0's leader runs the balancer and proposes its first replica,
    /// node-state or leader decision; the ranges' leaders carry out the leader moves (007 FR-010).
    /// Between two rounds it looks again for replicas only ([`NodeLogic::balance_due`]). Group 0's
    /// own membership stays with [`NodeLogic::promote`] (001), which also takes a draining node
    /// out of the voters.
    fn balance(&mut self) {
        let Some((regular, nodes)) = self.balance_due() else { return };
        self.balance_round = self.now;
        let me = self.me.as_ref().map_or(NodeId(0), |(id, _)| *id);
        let now = self.now;
        let conf = self.core.conf(GROUP0).cloned().unwrap_or_default();
        let conf_in_flight = self.promoting.is_some();
        let sm = self
            .core
            .state_machine(GROUP0)
            .and_then(Group0Holder::group0)
            .expect("Group 0's leader holds it");
        let Some(table) = self.liveness.table_mut() else { return };
        let limit = dead_after(sm.settings.get(DEAD_AFTER_SETTING).map(Vec::as_slice), DEAD_AFTER);
        table.check(&nodes, now, limit);
        let states = table.states().into_iter().collect();
        let loads = nodes.iter().filter_map(|n| Some((*n, table.get(*n)?.load))).collect();
        // After the handoff the descriptors are in the `Meta2` ranges' copies too.
        let merged: BTreeMap<GroupId, RangeDesc>;
        let ranges = match sm.meta1 {
            None => &sm.ranges,
            Some(_) => {
                let all = Ranges::of_all(sm, &crate::meta2_ops::copies(&self.core));
                merged = all.descs.into_iter().map(|d| (d.group, d)).collect();
                &merged
            }
        };
        let view = View {
            group0: sm,
            ranges,
            voters: &conf.voters,
            learners: &conf.learners,
            leader: me,
            states: &states,
            loads: &loads,
            conf_in_flight,
            skewed: &[],
            local_offset_s: crate::local_zone::offset_s(self.zone, sm.cluster_time_ms),
        };
        let settling = ranges.values().any(|d| d.settling);
        let cmd = plan(&view).into_iter().find_map(|d| match d {
            Decision::Command(c @ Group0Command::SetReplicas { .. }) => Some(c),
            Decision::Command(
                c @ (Group0Command::SetNodeState { .. } | Group0Command::SetLeaderHint { .. }),
            ) if regular => Some(c),
            Decision::Command(_) | Decision::Group0Conf(_) => None,
        });
        self.settle_check_at = settling.then_some(now + SETTLE_CHECK);
        if let Some(cmd) = cmd {
            self.balance_op = self.send_group0(cmd);
        }
    }

    /// Group 0's leader designates another primary of the replica set presentation when the
    /// designated one's node stops being alive in its heartbeat table (018 FR-007, T019,
    /// `PrimaryReassign.tla`): `Suspect` already, as a member of RefDb calls an election after
    /// `electionTimeoutMillis` without hearing from the primary.
    fn reassign_primary(&mut self) {
        if !self.is_leader() || self.reassign_op.is_some() {
            return;
        }
        let now = self.now;
        let Some(sm) = self.core.state_machine(GROUP0).and_then(Group0Holder::group0) else {
            return;
        };
        if crate::config_db::presentation(sm).is_none() {
            return;
        }
        let nodes: Vec<NodeId> =
            sm.nodes.values().filter(|n| n.state != NodeState::Removed).map(|n| n.id).collect();
        let Some(table) = self.liveness.table_mut() else { return };
        let limit = dead_after(sm.settings.get(DEAD_AFTER_SETTING).map(Vec::as_slice), DEAD_AFTER);
        table.check(&nodes, now, limit);
        let alive = |n: NodeId| {
            table.get(n).is_some_and(|e| e.state == teseladb_meta::liveness::LiveState::Alive)
        };
        let Some((key, value)) = crate::config_db::reassignment(sm, &alive) else { return };
        self.reassign_op = self.send_group0(Group0Command::SetSetting { key, value });
    }

    /// Whether the balancer looks now: `Some(true)` at its round, `Some(false)` between two rounds
    /// when a node joined, started or left, or [`SETTLE_CHECK`] after a round that saw a replica
    /// change in progress, with the nodes not removed. A node changes the replication factor and
    /// the replicas wanted, and a change that settles lets the next one go: until they are done a
    /// range may keep its only voter on one node, and losing that node would leave the range
    /// without a leader for good (005 SC-004). Those looks only take replica changes, so the
    /// moves for balance and the leader moves keep the pace of the rounds.
    fn balance_due(&mut self) -> Option<(bool, Vec<NodeId>)> {
        if !self.is_leader() || self.balance_op.is_some() {
            return None;
        }
        let sm = self.group0()?;
        let seen: Vec<(NodeId, NodeState, bool)> =
            sm.nodes.values().map(|n| (n.id, n.state, n.boot_epoch > 0)).collect();
        let regular = self.now >= self.balance_at;
        let settle = self.settle_check_at.is_some_and(|at| self.now >= at);
        if !regular && !settle && seen == self.balanced_nodes {
            return None;
        }
        if regular {
            self.balance_at = self.now + BALANCE_EVERY;
        }
        let nodes =
            seen.iter().filter(|(_, st, _)| *st != NodeState::Removed).map(|(n, ..)| *n).collect();
        self.balanced_nodes = seen;
        Some((regular, nodes))
    }

    /// Gives the other cores the nodes' addresses when they change (core 0).
    fn sync_peers(&mut self) {
        let Some(sm) = self.group0() else { return };
        let peers: BTreeMap<NodeId, String> = sm
            .nodes
            .values()
            .map(|n| (n.id, String::from_utf8_lossy(&n.addrs.peer_addr).into_owned()))
            .collect();
        if peers == self.peers || self.cores.is_empty() {
            return;
        }
        // A core that is stopping drops it; one whose mailbox is full gets them again on the next
        // turn (012 FR-005: a core never waits for another one).
        let full = self.cores[1..]
            .iter()
            .filter(|tx| {
                matches!(
                    tx.send(CoreInput::App(NodeMsg::Peers(peers.clone()))),
                    Err(SendError::Full(_))
                )
            })
            .count();
        if full == 0 {
            self.peers = peers;
        }
    }

    /// Publishes the nodes' tags for the client workers when Group 0's change (core 0, FR-013),
    /// and the cluster's write gates (FR-029).
    fn sync_tags(&mut self) {
        let (Some(routes), Some(sm)) = (&self.routes, self.group0()) else { return };
        let all = crate::backend::parameters_of(&sm.settings);
        let gate = teseladb_wire::commands::write_block::gate_of(&all);
        if routes.gate() != gate {
            routes.publish_gate(gate);
        }
        let limit = crate::node_versions::fcv_limit(sm);
        if routes.fcv_limit() != limit {
            routes.publish_fcv_limit(limit);
        }
        routes.publish_ordered_ids(crate::node_versions::ordered_ids(sm));
        let tags: crate::routes::NodeTags =
            sm.nodes.iter().map(|(id, n)| (*id, n.tags.clone())).collect();
        let applied = sm.applied;
        let stale = self.sent_tenants.as_ref().is_none_or(|(at, _)| *at != applied);
        let tenants: Option<crate::routes::TenantStates> = stale.then(|| {
            sm.tenants
                .list()
                .into_iter()
                .filter_map(|(id, _)| sm.tenants.state(id).map(|s| (id, s.clone())))
                .collect()
        });
        if self.sent_tags.as_ref() != Some(&tags) {
            routes.publish_tags(tags.clone());
            self.sent_tags = Some(tags);
        }
        if let Some(tenants) = tenants {
            if self.sent_tenants.as_ref().is_none_or(|(_, sent)| *sent != tenants) {
                routes.publish_tenants(Arc::new(tenants.clone()));
                // The connections' threads look at the new states now (007 FR-034).
                if let Some(p) = &self.presence {
                    p.wake();
                }
            }
            self.sent_tenants = Some((applied, tenants));
            if let Some(sm) = self.group0() {
                let names =
                    sm.tenants.list().into_iter().map(|(id, n)| (id, n.to_owned())).collect();
                routes.publish_tenant_names(Arc::new(names));
                let live = sm.nodes.values().filter(|n| n.state != NodeState::Removed).count();
                routes.publish_nodes(live);
            }
        }
    }

    /// Gives every core Group 0's ranges when they change, and publishes the routing table
    /// (core 0).
    fn sync_desc(&mut self) {
        let copies = crate::meta2_ops::copies(&self.core);
        let Some(ranges) = self.group0().map(|sm| Ranges::of_all(sm, &copies)) else { return };
        if self.sent_ranges.as_ref() == Some(&ranges) {
            return;
        }
        if let Some(routes) = &self.routes {
            routes.publish(&ranges.descs);
        }
        if let Some(data) = &mut self.data {
            data.set_ranges(&ranges);
        }
        // As with the peers: sent again on the next turn to a core whose mailbox is full.
        let full = self
            .cores
            .iter()
            .skip(1)
            .filter(|tx| {
                matches!(
                    tx.send(CoreInput::App(NodeMsg::Ranges(ranges.clone()))),
                    Err(SendError::Full(_))
                )
            })
            .count();
        if full == 0 {
            self.sent_ranges = Some(ranges);
        }
    }

    /// The core changes' messages go to their cores; one whose mailbox is full goes on a later
    /// turn.
    fn forward_relocations(&mut self) {
        let Some(data) = &mut self.data else { return };
        for (to, msg) in data.take_relocate_msgs() {
            let Some(tx) = self.cores.get(usize::from(to)) else {
                data.requeue_relocate(to, msg);
                continue;
            };
            if let Err(SendError::Full(CoreInput::App(NodeMsg::Relocate(msg)))) =
                tx.send(CoreInput::App(NodeMsg::Relocate(msg)))
            {
                data.requeue_relocate(to, msg);
            }
        }
    }

    /// The frames this core received for ranges another core of the node runs go to that core;
    /// a core that is stopping or whose mailbox is full drops them, as the network may.
    fn forward_strays(&mut self) {
        let strays = self.core.take_strays();
        let Some(data) = &self.data else { return };
        for (from, frame) in strays {
            for (core, f) in route_stray(frame, |g| data.holder(g)) {
                if let Some(tx) = self.cores.get(usize::from(core)) {
                    let _ = tx.send(CoreInput::App(NodeMsg::Stray(from, f)));
                }
            }
        }
    }

    /// The data range's Group 0 commands go to Group 0: from here on core 0, or through core 0.
    fn forward_group0(&mut self) {
        let Some(data) = &mut self.data else { return };
        for cmd in data.take_group0() {
            if self.me.is_some() {
                if let Some(op) = self.send_group0(cmd) {
                    self.group0_ops.insert(op);
                }
            } else if let Some(tx) = self.cores.first() {
                // A core that is stopping or whose mailbox is full drops it; the range sends it
                // again.
                let _ = tx.send(CoreInput::App(NodeMsg::Group0(cmd)));
            }
        }
    }

    /// Moves the data range and sends what it produced.
    fn data_step(&mut self) {
        if let Some(data) = &mut self.data {
            data.poll(&mut self.core, (self.now, self.wall_ms));
        }
        self.pump();
        self.forward_group0();
        self.forward_relocations();
        let applied = self.group0().map_or(0, |sm| sm.applied);
        // Before any change of the catalog is answered, the workers can read it (H-017).
        self.publish_catalog();
        self.catalog.poll(applied, self.authz.fenced());
    }

    /// Publishes this node's copies of the catalog and of the cluster settings for the workers
    /// when they changed (H-017).
    fn publish_catalog(&mut self) {
        let (Some(routes), Some(sm)) = (self.routes.clone(), self.group0()) else { return };
        if !sm.bootstrapped() {
            return;
        }
        let same = self.published_catalog.as_ref().is_some_and(|c| Arc::ptr_eq(c, &sm.catalog));
        let catalog = (!same).then(|| Arc::clone(&sm.catalog));
        // The settings are compared only when Group 0 applied something since the last look.
        let applied = sm.applied;
        let settings = (self.published_settings.0 != applied
            && self.published_settings.1.as_deref() != Some(&sm.settings))
        .then(|| Arc::new(sm.settings.clone()));
        if let Some(catalog) = catalog {
            routes.publish_catalog(Arc::clone(&catalog));
            self.published_catalog = Some(catalog);
        }
        self.published_settings.0 = applied;
        if let Some(settings) = settings {
            routes.publish_settings(Arc::clone(&settings));
            self.published_settings.1 = Some(settings);
        }
    }

    /// A catalog request of the handle: read the local copy, or change it through Group 0's
    /// leader.
    fn catalog_request(&mut self, req: CatalogReq, reply: CatalogSender) {
        match req {
            CatalogReq::Read => {
                let a = match self.group0() {
                    Some(sm) if sm.bootstrapped() => {
                        CatalogAnswer::Catalog(Arc::clone(&sm.catalog))
                    }
                    Some(_) | None => CatalogAnswer::Unavailable,
                };
                let _ = reply.send(a);
            }
            CatalogReq::Dump => {
                let a = match self.group0() {
                    Some(sm) if sm.bootstrapped() => {
                        let mut e = teseladb_types::Encoder::new();
                        // The tenants and the security state go with the catalog (007 FR-037).
                        let security = teseladb_meta::command::RestoredSecurity {
                            tenants: (*sm.tenants).clone(),
                            security: sm.security.clone(),
                        };
                        let (catalog, indexes) = (&sm.catalog, &sm.indexes);
                        teseladb_meta::command::encode_restore(
                            &mut e,
                            catalog,
                            indexes,
                            Some(&security),
                        );
                        CatalogAnswer::Dump(e.finish())
                    }
                    Some(_) | None => CatalogAnswer::Unavailable,
                };
                let _ = reply.send(a);
            }
            CatalogReq::Tenants => {
                let a = match self.group0() {
                    Some(sm) if sm.bootstrapped() => CatalogAnswer::Tenants(
                        sm.tenants
                            .list()
                            .into_iter()
                            .map(|(id, n)| {
                                let state = sm.tenants.state(id).cloned().unwrap_or_default();
                                (id, n.to_owned(), state)
                            })
                            .collect(),
                        teseladb_meta::node_versions::all_run(
                            sm,
                            teseladb_meta::group0::SNAPSHOT_VERSION,
                        ),
                    ),
                    Some(_) | None => CatalogAnswer::Unavailable,
                };
                let _ = reply.send(a);
            }
            CatalogReq::Settings => {
                let a = match self.group0() {
                    Some(sm) if sm.bootstrapped() => CatalogAnswer::Settings(sm.settings.clone()),
                    Some(_) | None => CatalogAnswer::Unavailable,
                };
                let _ = reply.send(a);
            }
            CatalogReq::Indexes(collection) => {
                let a = match self.group0() {
                    // The read is pinned here, before any acknowledgement can pass its index
                    // (009 FR-006).
                    Some(sm) if sm.bootstrapped() => CatalogAnswer::Indexes(
                        sm.indexes.of(collection).cloned().collect(),
                        self.authz.pins().pin_for(sm.applied, collection),
                    ),
                    Some(_) | None => CatalogAnswer::Unavailable,
                };
                let _ = reply.send(a);
            }
            CatalogReq::SearchIndexes(collection) => {
                let a = match self.group0() {
                    Some(sm) if sm.bootstrapped() => CatalogAnswer::SearchIndexes(
                        sm.indexes.search_of(collection).cloned().collect(),
                    ),
                    Some(_) | None => CatalogAnswer::Unavailable,
                };
                let _ = reply.send(a);
            }
            CatalogReq::Principal { kind, db, name } => {
                let a = match self.group0() {
                    Some(sm) if sm.bootstrapped() => CatalogAnswer::Principals(
                        sm.security.get(kind, &db, &name).map(<[u8]>::to_vec).into_iter().collect(),
                    ),
                    Some(_) | None => CatalogAnswer::Unavailable,
                };
                let _ = reply.send(a);
            }
            CatalogReq::PrincipalById { kind, id } => {
                let a = match self.group0() {
                    Some(sm) if sm.bootstrapped() => CatalogAnswer::Principals(
                        sm.security.get_by_id(kind, &id).map(<[u8]>::to_vec).into_iter().collect(),
                    ),
                    Some(_) | None => CatalogAnswer::Unavailable,
                };
                let _ = reply.send(a);
            }
            CatalogReq::Principals { kind, db } => {
                let a = match self.group0() {
                    Some(sm) if sm.bootstrapped() => CatalogAnswer::Principals(
                        sm.security
                            .list(kind, db.as_deref())
                            .into_iter()
                            .map(<[u8]>::to_vec)
                            .collect(),
                    ),
                    Some(_) | None => CatalogAnswer::Unavailable,
                };
                let _ = reply.send(a);
            }
            CatalogReq::Config(coll) => {
                let a = match self.group0() {
                    Some(sm) if sm.bootstrapped() => {
                        let copies = crate::meta2_ops::copies(&self.core);
                        let docs = crate::config_db::documents_with(sm, &copies, &coll);
                        CatalogAnswer::Documents(
                            docs.iter().filter_map(|d| d.to_bytes().ok()).collect(),
                        )
                    }
                    Some(_) | None => CatalogAnswer::Unavailable,
                };
                let _ = reply.send(a);
            }
            CatalogReq::Security(cmd) => {
                let fence = takes_away(&cmd);
                self.catalog_change(cmd.encode(), reply);
                if fence {
                    self.catalog.fence_last();
                }
            }
            CatalogReq::Write(cmd) => self.catalog_change(CatalogOps::payload(cmd), reply),
            // Every change of the indexes waits for the barrier (009 FR-006): no node may still
            // run a write that read the indexes before it.
            CatalogReq::Index(cmd) => {
                self.catalog_change(Group0Command::Indexes(cmd).encode(), reply);
                self.catalog.fence_last();
            }
            CatalogReq::SetSettings(changes) => {
                self.catalog_change(Group0Command::SetSettings(changes).encode(), reply);
            }
            CatalogReq::SetSetting { key, value } => {
                self.catalog_change(Group0Command::SetSetting { key, value }.encode(), reply);
            }
            CatalogReq::Ddl(from) => {
                let zero = teseladb_hlc::Hlc { physical: 0, logical: 0 };
                let a = match self.group0() {
                    Some(sm) if sm.bootstrapped() => CatalogAnswer::Ddl(
                        sm.ddl.after(from),
                        sm.ddl.floor,
                        sm.ddl.applied_mark.unwrap_or(zero),
                    ),
                    Some(_) | None => CatalogAnswer::Unavailable,
                };
                let _ = reply.send(a);
            }
            CatalogReq::Tick => {
                // `cluster_time_ms` 0 leaves the cluster time as it is.
                self.catalog_change(
                    Group0Command::ClusterTick { cluster_time_ms: 0 }.encode(),
                    reply,
                );
            }
        }
    }

    /// A change of Group 0 of the handle (the catalog or a setting): proposed as its leader, or
    /// forwarded to it.
    fn catalog_change(&mut self, payload: Vec<u8>, reply: CatalogSender) {
        if self.is_leader() {
            let op = self.core.propose(GROUP0, &payload, self.now);
            self.catalog.local(op, false, reply);
            return;
        }
        let leader = self.core.status(GROUP0).map_or(NodeId(0), |s| s.leader);
        let me = self.me.as_ref().map_or(NodeId(0), |(id, _)| *id);
        let to = if leader != NodeId(0) { Some(leader) } else { self.next_voter(me) };
        match to {
            Some(to) => {
                let body = Bytes::from_vec(Request::Catalog(payload).encode());
                let op = self.core.forward(to, body, self.now);
                self.catalog.local(Ok(op), true, reply);
            }
            None => drop(reply.send(CatalogAnswer::Unavailable)),
        }
    }

    /// The next voter of Group 0 other than `me`, in turn.
    fn next_voter(&mut self, me: NodeId) -> Option<NodeId> {
        let voters: Vec<NodeId> =
            self.core.conf(GROUP0)?.voters.iter().copied().filter(|v| *v != me).collect();
        let to = *voters.get(self.addr_probe % voters.len().max(1))?;
        self.addr_probe = self.addr_probe.wrapping_add(1);
        Some(to)
    }

    /// A forwarded Group 0 command: its leader takes a node's `SetNodeAddrs` and `NodeBoot` for
    /// that node only, and what a range's leader asks of Group 0, which checks it.
    fn take_forward(&mut self, from: NodeId, cmd: Group0Command) {
        if forward_allowed(&cmd, from) && self.is_leader() {
            // A failed proposal is sent again by the node on its next tick.
            let _ = self.core.propose(GROUP0, &cmd.encode(), self.now);
        }
    }

    fn is_leader(&self) -> bool {
        self.core.status(GROUP0).is_some_and(|s| s.role == Role::Leader)
    }

    /// Proposes the founder's `Bootstrap` until it is applied (a failed proposal is tried again on
    /// the next tick; applying it twice only answers `AlreadyBootstrapped`).
    fn bootstrap(&mut self) {
        let Some((cluster_id, first_node)) = &self.founder else { return };
        if self.group0().is_some_and(Group0Sm::bootstrapped) {
            self.founder = None;
            return;
        }
        if self.bootstrap.is_some() {
            return;
        }
        let cmd = Group0Command::Bootstrap {
            cluster_id: *cluster_id,
            first_node: first_node.clone(),
            cert_fingerprint: insecure_fingerprint(&String::from_utf8_lossy(&first_node.peer_addr)),
        };
        self.bootstrap = self.core.propose(GROUP0, &cmd.encode(), self.now).ok();
    }

    /// Group 0's leader promotes a learner that has caught up with the commit index.
    fn promote(&mut self) {
        if self.promoting.is_some() {
            return;
        }
        let Some(status) = self.core.status(GROUP0).filter(|s| s.role == Role::Leader) else {
            return;
        };
        let conf =
            self.core.conf(GROUP0).expect("a group with a status has a configuration").clone();
        if self.drain_vote(&conf.voters) {
            return;
        }
        if conf.voters.len() >= GROUP0_MAX_VOTERS {
            return;
        }
        let commit = status.hard_state.commit;
        let active = |n: &NodeId| {
            self.group0()
                .is_some_and(|sm| sm.nodes.get(n).is_some_and(|i| i.state == NodeState::Active))
        };
        let ready = conf
            .learners
            .iter()
            .copied()
            .filter(|l| active(l))
            .find(|l| status.progress.iter().any(|(n, matched)| n == l && *matched >= commit));
        if let Some(node) = ready {
            let cc = ConfChange { kind: ConfChangeKind::PromoteLearner, node };
            self.promoting = self.core.propose_conf_change(GROUP0, cc, self.now).ok();
        }
    }

    /// A draining or removed node leaves Group 0's voters once it holds no range replica (007
    /// FR-015), if an active voter is left; Group 0's leader hands its own leadership to an active
    /// voter first. `true` if it did something.
    fn drain_vote(&mut self, voters: &[NodeId]) -> bool {
        let me = self.me.as_ref().map_or(NodeId(0), |(id, _)| *id);
        let Some(sm) = self.group0() else { return false };
        let active = |n: &NodeId| sm.nodes.get(n).is_some_and(|i| i.state == NodeState::Active);
        let holds = |n: NodeId| teseladb_meta::drain::progress(sm, voters, n).replicas > 0;
        let leaving = voters.iter().copied().find(|v| !active(v) && !holds(*v));
        let Some(n) = leaving else { return false };
        let Some(&other) = voters.iter().find(|v| active(v)) else { return false };
        if n == me {
            return self.core.transfer_leader(GROUP0, other);
        }
        let cc = ConfChange { kind: ConfChangeKind::Remove, node: n };
        self.promoting = self.core.propose_conf_change(GROUP0, cc, self.now).ok();
        self.promoting.is_some()
    }

    /// Starts a join: only Group 0's leader takes it; the others answer `not_leader` with the
    /// leader's address when they know it.
    fn join(&mut self, req: JoinRequest, reply: JoinReply) {
        let Some(sm) = self.group0().filter(|_| self.is_leader()) else {
            let leader = self.core.status(GROUP0).map_or(NodeId(0), |s| s.leader);
            let hint = self
                .group0()
                .and_then(|sm| sm.nodes.get(&leader))
                .map(|n| String::from_utf8_lossy(&n.addrs.peer_addr).into_owned());
            let _ = reply.send(Err(match hint {
                Some(a) => format!("not_leader {a}"),
                None => "not_leader".to_owned(),
            }));
            return;
        };
        // Every node of a cluster has the founder's cores (data model §1).
        let cores = sm.nodes.values().next().map_or(req.cores, |n| n.addrs.cores);
        let node = NodeAddrs {
            name: req.name.into_bytes(),
            peer_addr: req.peer_addr.clone().into_bytes(),
            http_addr: req.http_addr.into_bytes(),
            native_addr: req.native_addr.into_bytes(),
            cores,
        };
        let cmd =
            Group0Command::AddNode { node, cert_fingerprint: insecure_fingerprint(&req.peer_addr) };
        match self.core.propose(GROUP0, &cmd.encode(), self.now) {
            Ok(op) => {
                self.joins.insert(op, (JoinStep::AddNode, reply));
            }
            Err(e) => {
                let _ = reply.send(Err(format!("{e:?}")));
            }
        }
    }

    /// Next step of a join once its operation ended.
    fn join_step(
        &mut self,
        step: JoinStep,
        reply: JoinReply,
        result: Result<Done<CoreOutput>, OpError>,
    ) {
        let node = match (step, result) {
            (JoinStep::AddNode, Ok(Done::Applied(CoreOutput::Group0(Group0Result::Node(n))))) => {
                let member = self.core.conf(GROUP0).is_some_and(|c| c.members().any(|m| m == n));
                if !member {
                    let cc = ConfChange { kind: ConfChangeKind::AddLearner, node: n };
                    match self.core.propose_conf_change(GROUP0, cc, self.now) {
                        Ok(op) => {
                            self.joins.insert(op, (JoinStep::Learner(n), reply));
                        }
                        Err(e) => {
                            let _ = reply.send(Err(format!("{e:?}")));
                        }
                    }
                    return;
                }
                n
            }
            (JoinStep::Learner(n), Ok(Done::ConfChanged(_))) => n,
            (_, other) => {
                let _ = reply.send(Err(format!("{other:?}")));
                return;
            }
        };
        let _ = reply.send(Ok(self.accepted(node)));
    }

    /// The answer to a node that joined: its identifier, the cluster and Group 0's voters.
    fn accepted(&self, node: NodeId) -> JoinAccepted {
        let sm = self.group0().expect("Group 0 on the leader of a join");
        let voters = self
            .core
            .conf(GROUP0)
            .map(|c| c.voters.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|v| (v, sm.nodes.get(&v).map_or([0; 32], |n| n.cert_fingerprint)))
            .collect();
        JoinAccepted {
            node_id: node,
            cluster_id: sm.cluster_id,
            cores: sm.nodes.get(&node).map_or(0, |n| n.addrs.cores),
            voters,
            denied: Vec::new(),
        }
    }

    /// Takes what the core produced: frames for other nodes and operations that ended. Handling an
    /// operation can produce more (the `AddLearner` of a join, the answer to a forward), so it
    /// goes on until the core has nothing left.
    fn pump(&mut self) {
        loop {
            let actions = self.core.drain_actions();
            let idle = actions.send.is_empty()
                && actions.done.is_empty()
                && actions.forwarded.is_empty()
                && actions.snapshot_data.is_empty();
            if idle {
                return;
            }
            self.outbound.extend(actions.send.into_iter().map(|(to, f)| (to, to_wire(f))));
            self.roll_back();
            // Only a range parks a snapshot (Group 0's data is its snapshot).
            if let Some(data) = &mut self.data {
                for (g, index) in actions.snapshot_data {
                    data.snapshot_data(&self.core, g, index, self.now);
                }
            }
            for (op, result) in actions.done {
                self.op_done(op, result);
            }
            for f in actions.forwarded {
                match (Request::decode(&f.body), &mut self.data) {
                    // The range's core answers once it has the piece.
                    (Ok(Request::Fetch(fetch)), Some(data)) => {
                        data.on_fetch(&mut self.core, (f.from, f.req), fetch, self.now);
                    }
                    // An empty answer ends the sender's wait.
                    (Ok(Request::Group0(cmd)), _) => {
                        self.take_forward(f.from, cmd);
                        self.core.answer_forward(f.from, f.req, Bytes::new());
                    }
                    (Ok(Request::Data(body)), Some(data)) => {
                        let at = (self.now, self.wall_ms);
                        data.request_remote(&mut self.core, (f.from, f.req), &body, at);
                    }
                    (Ok(Request::Catalog(body)), _) => {
                        let op = match self.is_leader() {
                            true => self.core.propose(GROUP0, &body, self.now).ok(),
                            false => None,
                        };
                        if let Some(answer) = self.catalog.remote(op, (f.from, f.req)) {
                            self.core.answer_forward(f.from, f.req, Bytes::from_vec(answer));
                        }
                    }
                    (Ok(Request::KillSessions(scope)), _) => {
                        if let Some(sessions) = &self.sessions {
                            kill_local(sessions, &scope);
                        }
                        self.core.answer_forward(f.from, f.req, Bytes::new());
                    }
                    (Ok(Request::Checksum(r)), Some(data)) => {
                        data.on_checksum_report(&r);
                        self.core.answer_forward(f.from, f.req, Bytes::new());
                    }
                    (Ok(Request::Rebuild(g)), Some(data)) => {
                        data.rebuild(g);
                        self.core.answer_forward(f.from, f.req, Bytes::new());
                    }
                    // An empty answer is a "no" (a core without the ranges).
                    (Ok(Request::MergeProbe { range, merge }), Some(data)) => {
                        data.on_merge_probe(&mut self.core, (f.from, f.req), (range, merge));
                    }
                    // A `Meta2` range's leader takes it; anyone else drops it and the sender
                    // tries again.
                    (Ok(Request::Meta2 { group, entry }), _) => {
                        if self.core.status(group).is_some_and(|s| s.role == Role::Leader)
                            && Meta2Command::decode(&entry).is_ok()
                        {
                            let _ = self.core.propose(group, &entry, self.now);
                        }
                        self.core.answer_forward(f.from, f.req, Bytes::new());
                    }
                    // The node's HTTP port answers it, then core 0 sends the answer
                    // ([`NodeMsg::HttpAnswer`]); without a port, an empty answer.
                    (Ok(Request::Http(body)), _) => {
                        let handed = self
                            .http_forwards
                            .as_ref()
                            .is_some_and(|h| h.send((f.from, f.req, body)).is_ok());
                        if !handed {
                            self.core.answer_forward(f.from, f.req, Bytes::new());
                        }
                    }
                    (Ok(Request::CurrentOps), _) => {
                        let answer = self.own_operations();
                        self.core.answer_forward(f.from, f.req, Bytes::from_vec(answer));
                    }
                    (Ok(Request::Gate(want)), _) => {
                        let held = self.routes.as_ref().is_some_and(|r| r.gate() == want);
                        let answer = vec![if held { GATE_HELD } else { GATE_BEHIND }];
                        self.core.answer_forward(f.from, f.req, Bytes::from_vec(answer));
                    }
                    (
                        Ok(
                            Request::Fetch(_)
                            | Request::Data(_)
                            | Request::Checksum(_)
                            | Request::Rebuild(_)
                            | Request::MergeProbe { .. },
                        )
                        | Err(_),
                        _,
                    ) => {
                        self.core.answer_forward(f.from, f.req, Bytes::new());
                    }
                }
            }
        }
    }

    /// Writes what the ranges of this core dropped in a rollback to their rollback files; Group 0
    /// has none (its entries are no data).
    fn roll_back(&mut self) {
        let rolled = self.core.take_rolled_back();
        if rolled.is_empty() {
            return;
        }
        let ranges: Vec<_> = rolled
            .into_iter()
            .filter(|(g, _)| self.core.state_machine(*g).and_then(CoreSm::range).is_some())
            .collect();
        let internal = rollback_files(&ranges, self.wall_ms);
        let empty = BTreeMap::new();
        let uuids = self.data.as_ref().map_or(&empty, DataCore::collections);
        self.rollback.add(ref_layout(internal, uuids));
    }

    /// An operation of the core ended.
    fn op_done(&mut self, op: OpId, result: Result<Done<CoreOutput>, OpError>) {
        let mine = |slot: &mut Option<OpId>| {
            let hit = *slot == Some(op);
            if hit {
                *slot = None;
            }
            hit
        };
        if mine(&mut self.bootstrap)
            || mine(&mut self.promoting)
            || mine(&mut self.addr_update)
            || mine(&mut self.boot_op)
            || mine(&mut self.tags_op)
            || mine(&mut self.versions_op)
            || mine(&mut self.create_op)
            || mine(&mut self.balance_op)
            || mine(&mut self.reassign_op)
            || self.group0_ops.remove(&op)
        {
            return;
        }
        if self.kills.owns(op) {
            self.kills.on_done(op, matches!(result, Ok(Done::Forwarded(_))));
            return;
        }
        if self.op_lists.owns(op) {
            let answer = match &result {
                Ok(Done::Forwarded(b)) => Some(b[..].to_vec()),
                _ => None,
            };
            self.op_lists.on_done(op, answer);
            return;
        }
        // A node that answers 0 is behind; one that does not answer is down, and one that answers
        // nothing speaks a protocol without the question: neither counts.
        if self.gates.owns(op) {
            let behind = matches!(&result, Ok(Done::Forwarded(b)) if b[..] == [GATE_BEHIND]);
            self.gates.on_done(op, !behind);
            return;
        }
        if let Some((step, reply)) = self.joins.remove(&op) {
            self.join_step(step, reply, result);
            return;
        }
        if self.catalog.owns(op) {
            let commit = self.core.status(GROUP0).map_or(0, |s| s.hard_state.commit);
            let applied = self.group0().map_or(0, |sm| sm.applied);
            if let Some(Some((node, req, body))) = self.catalog.on_done(op, result, commit, applied)
            {
                self.core.answer_forward(node, req, body);
            }
            return;
        }
        if let Some(data) = &mut self.data {
            data.on_done(&mut self.core, op, result, self.now);
        }
    }
}

/// Whether a change of users and roles may take privileges away (013 FR-012): any that changes
/// or removes an existing user or role, so that it answers only once fenced. Creating one does not.
fn takes_away(cmd: &Group0Command) -> bool {
    match cmd {
        Group0Command::Idempotent { cmd, .. } => takes_away(cmd),
        Group0Command::Security(ops) => ops.iter().any(|op| op.expect != Expect::Absent),
        _ => false,
    }
}

/// Whether Group 0's leader takes `cmd` forwarded by node `from`: a node's own `SetNodeAddrs`,
/// `NodeBoot` and `SetNodeTags`, a range leader's `ReplicasSettled`, `SplitRange` and `CompleteSplit` (Group 0
/// checks the epoch, the bounds and the recorded intent), and a range leader's publication of
/// itself (`SetLeaderHint` with its term; a request, term 0, is the balancer's).
fn forward_allowed(cmd: &Group0Command, from: NodeId) -> bool {
    match cmd {
        Group0Command::SetNodeAddrs { node, .. }
        | Group0Command::NodeBoot { node, .. }
        | Group0Command::SetNodeTags { node, .. } => *node == from,
        // A node records its own versions only (007 FR-027).
        Group0Command::SetSetting { key, .. } => *key == crate::node_versions::key(from),
        Group0Command::SetLeaderHint { node, term, .. } => *node == from && *term > 0,
        Group0Command::ReplicasSettled { .. }
        | Group0Command::SplitRange { .. }
        | Group0Command::CompleteSplit { .. } => true,
        _ => false,
    }
}

impl CoreLogic for NodeLogic {
    type Msg = NodeMsg;

    fn tick(&mut self, now: Nanos, wall_ms: u64) {
        self.now = now;
        self.wall_ms = wall_ms;
        self.core.on_tick(now, wall_ms);
        let load = self.node_load();
        self.publish_presence();
        if let Some((me, _)) = &self.me {
            let out = self.liveness.tick_with(&mut self.core, (*me, load), now, wall_ms);
            self.outbound.extend(out);
            let out = self.authz.tick(&mut self.core, *me, now);
            self.outbound.extend(out);
        }
        self.bootstrap();
        self.create_root();
        self.clustertime_keys();
        self.update_addrs();
        self.announce_boot();
        self.announce_tags();
        self.announce_versions();
        self.sync_tags();
        self.create_range();
        self.balance();
        self.reassign_primary();
        self.promote();
        self.meta2_step();
        self.rollback.tick(now);
        self.data_step();
        self.sync_desc();
        self.sync_peers();
    }

    fn poll_io(&mut self, now: Nanos) -> Option<IoOp> {
        self.now = now;
        // The log drops the entries of a rollback only once their files are durable (005
        // FR-010): until then the core issues no disk operation.
        if self.rollback.idle() {
            self.core.rollback_durable();
        }
        if let Some(op) = self.core.poll_io(now) {
            self.io.push_back(IoSlot::Log);
            return Some(op);
        }
        if let Some(op) = self.rollback.poll_io() {
            self.io.push_back(IoSlot::Rollback);
            return Some(op);
        }
        let data = self.data.as_mut()?;
        let next = data.poll_io(&mut self.core, now);
        // Completions that came with it (an export, an ingestion) may have moved the core.
        self.pump();
        let (op, owner) = next?;
        self.io.push_back(IoSlot::Data(owner));
        Some(op)
    }

    fn on_io_done(&mut self, done: IoDone, now: Nanos) {
        self.now = now;
        match self.io.pop_front().expect("a disk operation in flight") {
            IoSlot::Log => self.core.on_io_done(done, now),
            IoSlot::Rollback => self.rollback.on_io_done(&done, now),
            IoSlot::Data(owner) => {
                let data = self.data.as_mut().expect("the owner of a data operation");
                data.on_io_done(&mut self.core, owner, done, now);
            }
        }
        self.data_step();
    }

    fn next_deadline(&self) -> Option<Nanos> {
        self.core.next_deadline()
    }

    fn io_settled(&self) -> bool {
        // The log waits for the rollback files (005 FR-010), so both go to disk before a stop.
        self.core.writes_settled() && self.rollback.idle()
    }

    fn on_peer(&mut self, from: NodeId, msg: PeerMsg, now: Nanos) {
        self.now = now;
        let msg = match &self.me {
            Some((me, _)) => {
                let me = *me;
                let msg = match self.authz.on_peer(&self.core, msg, now) {
                    Ok(out) => {
                        self.outbound.extend(out);
                        return self.pump();
                    }
                    Err(m) => m,
                };
                match self.liveness.on_peer(&mut self.core, me, msg, now, self.wall_ms) {
                    Some(m) => m,
                    None => return self.pump(),
                }
            }
            None => msg,
        };
        if let Some(frame) = from_wire(msg) {
            self.core.on_peer_message(from, frame, now);
            self.forward_strays();
            self.data_step();
        }
    }

    fn on_message(&mut self, msg: NodeMsg, now: Nanos) {
        self.now = now;
        match msg {
            NodeMsg::Join(req, reply) => self.join(req, reply),
            NodeMsg::View(reply) => {
                let _ = reply.send(self.view());
            }
            NodeMsg::Drain(node, drain, reply) => {
                let voters = self.core.conf(GROUP0).map(|c| c.voters.clone()).unwrap_or_default();
                let cmd = self.group0().map(|sm| match drain {
                    true => teseladb_meta::drain::drain(sm, node),
                    false => teseladb_meta::drain::remove(sm, &voters, node),
                });
                let _ = reply.send(cmd);
            }
            NodeMsg::Data(req, reply) => match &mut self.data {
                Some(data) => data.request(&mut self.core, req, reply, (now, self.wall_ms)),
                None => {
                    let _ = reply.send(Err(RangeError::OutOfRange));
                }
            },
            NodeMsg::Cores(cores) => {
                self.cores = cores;
                self.sent_ranges = None;
                self.sync_desc();
                self.sync_peers();
            }
            NodeMsg::Peers(peers) => self.peers = peers,
            NodeMsg::Catalog(req, reply) => self.catalog_request(req, reply),
            NodeMsg::KillSessions(scope, reply) => self.kill_sessions(scope, reply),
            NodeMsg::GateEverywhere(want, reply) => self.gate_everywhere(want, reply),
            NodeMsg::CurrentOps(reply) => self.current_ops(reply),
            NodeMsg::ForwardHttp(to, body, reply) => {
                let b = Bytes::from_vec(crate::forward::Request::Http(body).encode());
                let op = self.core.forward(to, b, self.now);
                self.op_lists.start(vec![op], reply);
            }
            NodeMsg::HttpAnswer(to, req, body) => {
                self.core.answer_forward(to, req, Bytes::from_vec(body));
            }
            NodeMsg::Ranges(ranges) => {
                if let Some(data) = &mut self.data {
                    data.set_ranges(&ranges);
                }
            }
            NodeMsg::Group0(cmd) => {
                if let Some(op) = self.send_group0(cmd) {
                    self.group0_ops.insert(op);
                }
            }
            NodeMsg::Relocate(msg) => {
                if let Some(data) = &mut self.data {
                    data.on_relocate(&mut self.core, msg);
                }
            }
            NodeMsg::Stray(from, frame) => self.core.on_stray(from, frame, now),
        }
        self.data_step();
    }

    fn take_outbound(&mut self, out: &mut Vec<(NodeId, PeerMsg)>) {
        // End of the turn: one round per range for the reads its events registered. A single
        // voter answers them at once, and what that moves may register more.
        for _ in 0..READ_ROUND_PASSES {
            if !self.core.read_rounds_due() {
                break;
            }
            self.core.flush_read_rounds();
            self.data_step();
        }
        out.append(&mut self.outbound);
    }

    fn peer_addr(&self, node: NodeId) -> Option<String> {
        match self.group0() {
            Some(sm) => {
                let n = sm.nodes.get(&node)?;
                Some(String::from_utf8_lossy(&n.addrs.peer_addr).into_owned())
            }
            None => self.peers.get(&node).cloned(),
        }
    }

    fn peer_unreachable(&mut self, node: NodeId) {
        self.core.report_unreachable(node);
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::mpsc::{Receiver, channel};

    use teseladb_auth::fence::AUTH_LEASE_MS;
    use teseladb_meta::group0::{GROUP0, Group0Error};
    use teseladb_meta::indexes::{IndexCommand, IndexState};
    use teseladb_meta::liveness::NODE_HEARTBEAT;
    use teseladb_meta::security::{Expect, Principal, SecurityOp};
    use teseladb_multiraft::{CoreConfig, GroupDesc};
    use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};

    use super::*;

    const TICK: Nanos = Nanos::from_millis(10);

    fn addr(id: u64) -> String {
        format!("127.0.0.1:{}", 7000 + id)
    }

    fn addrs(id: u64) -> NodeAddrs {
        NodeAddrs {
            name: addr(id).into_bytes(),
            peer_addr: addr(id).into_bytes(),
            cores: 1,
            ..NodeAddrs::default()
        }
    }

    fn request(id: u64, name: &str) -> JoinRequest {
        JoinRequest {
            name: name.to_owned(),
            peer_addr: addr(id),
            http_addr: String::new(),
            native_addr: String::new(),
            cores: 0,
        }
    }

    /// Nodes of one core each, over `MemDisk`, in logical time; frames go straight to their node,
    /// except to a node restarted on another port: as with the real transport, only a node that has
    /// its new address in Group 0, or that heard from it (its `Hello` on an incoming connection),
    /// reaches it.
    struct Cluster {
        nodes: Vec<(NodeId, NodeLogic, MemDisk)>,
        now: Nanos,
        /// Nodes restarted on another port, with that port.
        moved: BTreeMap<NodeId, String>,
        /// Nodes cut off from the rest: their frames, both ways, are dropped without closing any
        /// connection (a partition with iptables `DROP`).
        cut: BTreeSet<NodeId>,
        /// (who, whom): `who` has had a message from `whom`, so it knows where `whom` listens.
        heard: BTreeSet<(NodeId, NodeId)>,
    }

    impl Cluster {
        fn new() -> Cluster {
            Cluster {
                nodes: Vec::new(),
                now: Nanos::ZERO,
                moved: BTreeMap::new(),
                cut: BTreeSet::new(),
                heard: BTreeSet::new(),
            }
        }

        fn add(&mut self, id: u64, group0: Option<GroupDesc>, founder: bool) {
            let mut disk = MemDisk::new();
            let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
            let mut core = MultiRaftCore::new(CoreConfig::new(NodeId(id), 0), store, id);
            if let Some(desc) = group0 {
                core.create_group(desc, CoreSm::Group0(Group0Sm::default())).unwrap();
            }
            let founder = founder.then(|| ([7; 16], addrs(id)));
            self.nodes.push((NodeId(id), NodeLogic::new(core, founder, None), disk));
        }

        /// Adds node `id` with a data core over its disk (one core: Group 0 and the ranges).
        fn add_with_data(&mut self, id: u64, group0: GroupDesc, founder: bool) {
            use std::cell::RefCell;
            use std::rc::Rc;
            let mut disk = MemDisk::new();
            let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
            let cfg = CoreConfig::new(NodeId(id), 0);
            let clock = cfg.clock.clone();
            let mut core = MultiRaftCore::new(cfg, store, id);
            core.create_group(group0, CoreSm::Group0(Group0Sm::default())).unwrap();
            let lcfg = teseladb_lsm::LsmConfig { dir: "core-00/lsm".into(), ..Default::default() };
            let lsm =
                Rc::new(RefCell::new(teseladb_lsm::Lsm::recover(&lcfg, &mut disk).unwrap().0));
            let transfers = crate::transfer::Transfers::new(NodeId(id), "core-00/t".into(), id);
            let data = DataCore::new(NodeId(id), 0, lsm, clock, transfers, 1 << 30);
            let founder = founder.then(|| ([7; 16], addrs(id)));
            let logic = NodeLogic::new(core, founder, None).with_data(data);
            self.nodes.push((NodeId(id), logic, disk));
        }

        /// Asks node `id`'s data core and runs the cluster until it answers.
        fn data(&mut self, id: u64, req: DataReq) -> Result<crate::data::DataReply, RangeError> {
            let (tx, rx) = channel();
            let now = self.now;
            self.logic(id).on_message(NodeMsg::Data(req, tx), now);
            for _ in 0..500 {
                if let Ok(r) = rx.try_recv() {
                    return r;
                }
                self.step();
            }
            panic!("no answer");
        }

        /// Restarts node `id` from its disk, listening now at `peer` (`me` set, as in a node).
        fn restart(&mut self, id: u64, peer: &str) {
            let at = self.nodes.iter().position(|(n, ..)| n.0 == id).expect("a node");
            let (_, _, mut disk) = self.nodes.remove(at);
            let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
            let mut core = MultiRaftCore::new(CoreConfig::new(NodeId(id), 0), store, id + 50);
            let recovered = GroupDesc { group: GROUP0, voters: Vec::new(), learners: Vec::new() };
            core.create_group(recovered, CoreSm::Group0(Group0Sm::default())).unwrap();
            let me = NodeAddrs {
                name: addr(id).into_bytes(),
                peer_addr: peer.into(),
                cores: 1,
                ..NodeAddrs::default()
            };
            self.nodes.push((NodeId(id), NodeLogic::new(core, None, Some((NodeId(id), me))), disk));
            self.moved.insert(NodeId(id), peer.to_owned());
            self.heard.retain(|(_, whom)| whom.0 != id);
        }

        /// The inter-node address Group 0 has for `of`, as node `at` sees it.
        fn peer_of(&mut self, at: u64, of: u64) -> Option<String> {
            self.logic(at).peer_addr(NodeId(of))
        }

        fn found(&mut self) {
            let desc = GroupDesc { group: GROUP0, voters: vec![NodeId(1)], learners: Vec::new() };
            self.add(1, Some(desc), true);
        }

        fn logic(&mut self, id: u64) -> &mut NodeLogic {
            &mut self.nodes.iter_mut().find(|(n, ..)| n.0 == id).expect("a node").1
        }

        /// Delivers a message to a node and runs its I/O.
        fn deliver(&mut self, to: NodeId, msg: PeerMsg, from: NodeId) {
            if self.cut.contains(&to) || self.cut.contains(&from) {
                return;
            }
            if let Some(port) = self.moved.get(&to).cloned()
                && !self.heard.contains(&(from, to))
                && self.peer_of(from.0, to.0).as_deref() != Some(port.as_str())
            {
                return;
            }
            self.heard.insert((to, from));
            let now = self.now;
            if let Some((_, l, _)) = self.nodes.iter_mut().find(|(n, ..)| *n == to) {
                l.on_peer(from, msg, now);
            }
        }

        fn step(&mut self) {
            self.now = self.now + TICK;
            let now = self.now;
            let mut sent = Vec::new();
            for (id, l, disk) in &mut self.nodes {
                l.tick(now, now.0 / 1_000_000);
                while let Some(op) = l.poll_io(now) {
                    let done = disk.execute(op);
                    l.on_io_done(done, now);
                }
                let mut out = Vec::new();
                l.take_outbound(&mut out);
                sent.extend(out.into_iter().map(|(to, m)| (*id, to, m)));
            }
            for (from, to, m) in sent {
                self.deliver(to, m, from);
            }
        }

        fn run_until(&mut self, mut ok: impl FnMut(&mut Cluster) -> bool) {
            for _ in 0..2_000 {
                if ok(self) {
                    return;
                }
                self.step();
            }
            panic!("never happened");
        }

        fn join(&mut self, at: u64, req: JoinRequest) -> Receiver<Result<JoinAccepted, String>> {
            let (tx, rx) = channel();
            let now = self.now;
            self.logic(at).on_message(NodeMsg::Join(req, tx), now);
            rx
        }

        fn answer(
            &mut self,
            rx: &Receiver<Result<JoinAccepted, String>>,
        ) -> Result<JoinAccepted, String> {
            let mut got = None;
            self.run_until(|_| {
                got = rx.try_recv().ok();
                got.is_some()
            });
            got.expect("an answer")
        }

        /// Joins node `id` through `at` and adds it with the voters it was given.
        fn join_node(&mut self, at: u64, id: u64) -> JoinAccepted {
            let rx = self.join(at, request(id, &addr(id)));
            let a = self.answer(&rx).expect("accepted");
            let voters = a.voters.iter().map(|(n, _)| *n).collect();
            self.add(
                id,
                Some(GroupDesc { group: GROUP0, voters, learners: vec![a.node_id] }),
                false,
            );
            a
        }
    }

    /// A core that wakes nobody: the test drains its mailbox by hand.
    struct Quiet;

    impl teseladb_io::mailbox::Notify for Quiet {
        fn notify(&self) {}
    }

    /// Core 0 never waits for another core (012 FR-005): when the other core's mailbox is full it
    /// keeps what did not go and sends it on a later turn.
    #[test]
    fn core_zero_sends_again_what_a_full_mailbox_gave_back() {
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        let (own, _own_rx) = teseladb_io::mailbox::channel(std::sync::Arc::new(Quiet));
        // Room for one message: the ranges go, the peers come back full.
        let (other, mut other_rx) =
            teseladb_io::mailbox::channel_with(1, std::sync::Arc::new(Quiet));
        let now = c.now;
        c.logic(1).on_message(NodeMsg::Cores(vec![own, other]), now);
        let mut got = Vec::new();
        other_rx.drain_into(&mut got);
        assert!(matches!(got.as_slice(), [CoreInput::App(NodeMsg::Ranges(_))]), "{got:?}");
        got.clear();
        c.run_until(|_| {
            other_rx.drain_into(&mut got);
            got.iter().any(|m| matches!(m, CoreInput::App(NodeMsg::Peers(_))))
        });
    }

    #[test]
    fn every_node_records_its_tags_in_group_0() {
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        let east = vec![("dc".to_owned(), "east".to_owned())];
        // The harness starts nodes without knowing their own number, as a node that joins does
        // until it is accepted.
        c.logic(1).me = Some((NodeId(1), addrs(1)));
        c.logic(1).tags = east.clone();
        c.run_until(|c| c.logic(1).group0().is_some_and(|sm| sm.nodes[&NodeId(1)].tags == east));
        // A node that is not Group 0's leader forwards its own.
        c.join_node(1, 2);
        c.run_until(|c| c.logic(2).view().bootstrapped);
        let west = vec![("dc".to_owned(), "west".to_owned()), ("rack".to_owned(), "r2".to_owned())];
        c.logic(2).me = Some((NodeId(2), addrs(2)));
        c.logic(2).tags = west.clone();
        c.run_until(|c| {
            c.logic(1)
                .group0()
                .and_then(|sm| sm.nodes.get(&NodeId(2)))
                .is_some_and(|n| n.tags == west)
        });
    }

    #[test]
    fn the_founder_creates_the_cluster_and_nodes_join_and_become_voters() {
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        let v = c.logic(1).view();
        assert_eq!((v.cluster_id, v.role, v.leader), ([7; 16], Some(Role::Leader), NodeId(1)));
        assert_eq!(c.logic(1).peer_addr(NodeId(1)), Some(addr(1)));
        assert_eq!(c.logic(1).peer_addr(NodeId(9)), None);

        let a = c.join_node(1, 2);
        assert_eq!((a.node_id, a.cluster_id, a.cores), (NodeId(2), [7; 16], 1));
        assert_eq!(a.voters, vec![(NodeId(1), insecure_fingerprint(&addr(1)))]);
        c.run_until(|c| c.logic(1).view().voters == [NodeId(1), NodeId(2)]);
        c.run_until(|c| c.logic(2).view().bootstrapped);

        // Joining again from the same address gives back the same node, already a member.
        let rx = c.join(1, request(2, &addr(2)));
        assert_eq!(c.answer(&rx).expect("accepted").node_id, NodeId(2));
    }

    #[test]
    fn each_node_records_the_versions_it_runs_in_group0() {
        use crate::node_versions::{key, ours};
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        c.join_node(1, 2);
        c.run_until(|c| c.logic(2).view().bootstrapped);
        let recorded = |c: &mut Cluster, n: u64| {
            let sm = c.logic(1).group0().cloned();
            sm.and_then(|sm| sm.settings.get(&key(NodeId(n))).cloned()) == Some(ours())
        };
        assert!(!recorded(&mut c, 2), "a node that does not know itself records nothing");
        // Started as a node does, knowing who it is: it records them through Group 0's leader.
        c.restart(2, &addr(2));
        c.run_until(|c| recorded(c, 2));
        assert!(forward_allowed(
            &Group0Command::SetSetting { key: key(NodeId(2)), value: ours() },
            NodeId(2)
        ));
        assert!(!forward_allowed(
            &Group0Command::SetSetting { key: key(NodeId(1)), value: ours() },
            NodeId(2)
        ));
    }

    #[test]
    fn the_catalog_is_published_for_the_workers_before_a_change_of_it_is_answered() {
        use crate::catalog_ops::CatalogAnswer;
        // The workers read the catalog from the node's slot instead of asking core 0 for it on
        // every command (H-017 of the native tests: a round trip between threads each time). A
        // change is answered only once the slot holds it, so a client that creates a database and
        // uses it at once never reads the catalog before it.
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        let routes = SharedRoutes::default();
        c.logic(1).routes = Some(routes.clone());
        c.run_until(|_| routes.catalog().is_some());
        let own = Arc::clone(&c.logic(1).group0().unwrap().catalog);
        assert!(Arc::ptr_eq(&routes.catalog().unwrap(), &own));
        let (tx, rx) = channel();
        let create = CatalogReq::Write(teseladb_meta::catalog::CatalogCommand::CreateDatabase {
            db: "probe".into(),
        });
        let now = c.now;
        c.logic(1).on_message(NodeMsg::Catalog(create, tx), now);
        let mut answered = None;
        c.run_until(|_| {
            answered = rx.try_recv().ok();
            if answered.is_some() {
                // At the moment the answer leaves, the slot already has the database.
                assert!(routes.catalog().unwrap().dbs.contains_key("probe"));
            }
            answered.is_some()
        });
        assert!(matches!(answered, Some(CatalogAnswer::Done(_))), "{answered:?}");
    }

    #[test]
    fn the_settings_are_published_for_the_workers_before_a_change_of_them_is_answered() {
        use crate::catalog_ops::CatalogAnswer;
        // As the catalog (H-017): every command read the cluster settings from core 0 with a
        // round trip between threads; the workers read them from the node's slot.
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        let routes = SharedRoutes::default();
        c.logic(1).routes = Some(routes.clone());
        c.run_until(|_| routes.settings().is_some());
        let (tx, rx) = channel();
        let set = CatalogReq::SetSetting { key: b"probe".to_vec(), value: b"1".to_vec() };
        let now = c.now;
        c.logic(1).on_message(NodeMsg::Catalog(set, tx), now);
        let mut answered = None;
        c.run_until(|_| {
            answered = rx.try_recv().ok();
            if answered.is_some() {
                let settings = routes.settings().unwrap();
                assert_eq!(
                    settings.get(b"probe".as_slice()).map(Vec::as_slice),
                    Some(b"1".as_slice())
                );
            }
            answered.is_some()
        });
        assert!(matches!(answered, Some(CatalogAnswer::Done(_))), "{answered:?}");
    }

    #[test]
    fn catalog_requests_without_a_cluster_or_a_leader_and_forwards_to_the_leader() {
        use crate::catalog_ops::CatalogAnswer;
        // A replica of Group 0 that has not seen the cluster created answers that it cannot.
        let mut c = Cluster::new();
        let lone = GroupDesc {
            group: GROUP0,
            voters: vec![NodeId(1), NodeId(9)],
            learners: vec![NodeId(5)],
        };
        c.add(5, Some(lone), false);
        let now = c.now;
        let user = teseladb_meta::security::Principal::User;
        let unbootstrapped = [
            CatalogReq::Read,
            CatalogReq::Settings,
            CatalogReq::Indexes(1),
            CatalogReq::SearchIndexes(1),
            CatalogReq::Principal { kind: user, db: "admin".into(), name: "u".into() },
            CatalogReq::Principals { kind: user, db: None },
            CatalogReq::Config("shards".into()),
            CatalogReq::Ddl(teseladb_hlc::Hlc { physical: 0, logical: 0 }),
        ];
        for req in unbootstrapped {
            let (tx, rx) = channel();
            c.logic(5).on_message(NodeMsg::Catalog(req, tx), now);
            assert!(matches!(rx.try_recv(), Ok(CatalogAnswer::Unavailable)));
        }
        // Without a known leader a change goes to a voter in turn.
        let (tx, _rx) = channel();
        let set = CatalogReq::SetSetting { key: b"k".to_vec(), value: b"v".to_vec() };
        c.logic(5).on_message(NodeMsg::Catalog(set.clone(), tx), now);
        let mut out = Vec::new();
        c.logic(5).take_outbound(&mut out);

        // On a cluster, a change asked on another node is forwarded to the leader, which answers
        // once it applied it; sessions killed on a node without any are answered all the same.
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        c.join_node(1, 2);
        c.run_until(|c| c.logic(2).view().bootstrapped);
        let (tx, rx) = channel();
        let now = c.now;
        c.logic(2).on_message(NodeMsg::Catalog(set, tx), now);
        let mut done = None;
        c.run_until(|_| {
            done = rx.try_recv().ok();
            done.is_some()
        });
        assert!(matches!(done, Some(CatalogAnswer::Done(None))), "{done:?}");
        // A change stream's empty entry moves node 2's last mark, which its catalog changes give
        // (014 FR-014).
        let zero = teseladb_hlc::Hlc { physical: 0, logical: 0 };
        let mark_of = |c: &mut Cluster| {
            let (tx, rx) = channel();
            c.logic(2).on_message(NodeMsg::Catalog(CatalogReq::Ddl(zero), tx), now);
            match rx.try_recv() {
                Ok(CatalogAnswer::Ddl(_, floor, mark)) => (floor, mark),
                other => panic!("{other:?}"),
            }
        };
        let (floor, before) = mark_of(&mut c);
        assert_eq!(floor, zero);
        let (tx, rx) = channel();
        c.logic(2).on_message(NodeMsg::Catalog(CatalogReq::Tick, tx), now);
        let mut ticked = None;
        c.run_until(|_| {
            ticked = rx.try_recv().ok();
            ticked.is_some()
        });
        assert!(matches!(ticked, Some(CatalogAnswer::Done(None))), "{ticked:?}");
        assert!(mark_of(&mut c).1 > before);
        let (tx, rx) = channel();
        c.logic(1).on_message(NodeMsg::KillSessions(KillScope::All, tx), now);
        c.run_until(|_| rx.try_recv().is_ok());
        // An operation's kill goes to its node alone, which answers.
        let (tx, rx) = channel();
        c.logic(1).on_message(NodeMsg::KillSessions(KillScope::Op { node: 2, op: 7 }, tx), now);
        c.run_until(|_| rx.try_recv().is_ok());
        // `currentOp` asks every other node for its operations: node 2 answers with its own.
        let shared = Arc::new(Shared::default());
        teseladb_wire::commands::lock(&shared).ops.begin(teseladb_wire::ops::Op {
            conn: 4,
            client: None,
            metadata: None,
            app_name: None,
            op: "query",
            ns: "d.c".to_owned(),
            command: teseladb_bson::Document::new().with("find", "c"),
            started_ms: 0,
            deadline_ms: None,
            killed: false,
            interrupt: None,
            user: None,
            tenant: 0,
            privileges: None,
            roles: None,
            desc: None,
            progress: None,
            metrics: None,
        });
        c.logic(2).sessions = Some(shared);
        c.logic(2).me = Some((NodeId(2), addrs(2)));
        let (tx, rx) = channel();
        c.logic(1).on_message(NodeMsg::CurrentOps(tx), now);
        let mut answers = None;
        c.run_until(|_| {
            answers = rx.try_recv().ok();
            answers.is_some()
        });
        let answers = answers.flatten().expect("every node answered");
        let ops: Vec<teseladb_bson::Value> = answers
            .iter()
            .flat_map(|a| {
                let doc = teseladb_bson::Document::from(
                    teseladb_bson::RawDoc::from_bytes(a).expect("a document"),
                );
                match doc.get("ops") {
                    Some(teseladb_bson::Value::Array(ops)) => ops.clone(),
                    other => panic!("{other:?}"),
                }
            })
            .collect();
        let [teseladb_bson::Value::Document(op)] = &ops[..] else { panic!("{ops:?}") };
        assert_eq!(op.get("opid"), Some(&teseladb_bson::Value::from("node2:1")));
        assert_eq!(op.get("op"), Some(&teseladb_bson::Value::from("query")));
        // Users and roles: a user created through Group 0 is read and listed on every node.
        let user_doc = |name: &str| {
            teseladb_bson::Document::new()
                .with("_id", format!("admin.{name}"))
                .with("user", name)
                .with("db", "admin")
                .to_bytes()
                .unwrap()
        };
        let user = SecurityOp::put(Principal::User, Expect::Absent, user_doc("ana")).unwrap();
        let (tx, rx) = channel();
        let create = CatalogReq::Security(Group0Command::Security(vec![user.clone()]));
        c.logic(1).on_message(NodeMsg::Catalog(create, tx), now);
        c.run_until(|_| rx.try_recv().is_ok());
        let one = || CatalogReq::Principal {
            kind: Principal::User,
            db: "admin".to_owned(),
            name: "ana".to_owned(),
        };
        let all = || CatalogReq::Principals { kind: Principal::User, db: None };
        c.run_until(|c| {
            let (tx, rx) = channel();
            let now = c.now;
            c.logic(2).on_message(NodeMsg::Catalog(all(), tx), now);
            matches!(rx.try_recv(), Ok(CatalogAnswer::Principals(d)) if d.len() == 1)
        });
        let (tx, rx) = channel();
        c.logic(2).on_message(NodeMsg::Catalog(one(), tx), now);
        assert_eq!(rx.try_recv().unwrap(), CatalogAnswer::Principals(vec![user_doc("ana")]));
        // Removing a user takes privileges away: its answer waits for the authorization fence,
        // proposed on the leader or forwarded to it (013 FR-012).
        // Core 0 of every real node knows itself; the nodes of this harness do not.
        for id in [1, 2] {
            if c.logic(id).me.is_none() {
                c.logic(id).me = Some((NodeId(id), NodeAddrs::default()));
            }
        }
        let start = c.now;
        for (node, name) in [(1, "bea"), (2, "ana")] {
            if name == "bea" {
                let add = SecurityOp::put(Principal::User, Expect::Absent, user_doc(name)).unwrap();
                let (tx, rx) = channel();
                let req = CatalogReq::Security(Group0Command::Security(vec![add]));
                c.logic(1).on_message(NodeMsg::Catalog(req, tx), now);
                c.run_until(|_| rx.try_recv().is_ok());
            }
            let drop = SecurityOp::remove(Principal::User, user_doc(name)).unwrap();
            let (tx, rx) = channel();
            let req = CatalogReq::Security(Group0Command::Security(vec![drop]));
            let now = c.now;
            c.logic(node).on_message(NodeMsg::Catalog(req, tx), now);
            let mut answer = None;
            c.run_until(|_| {
                answer = rx.try_recv().ok();
                answer.is_some()
            });
            assert!(matches!(answer, Some(CatalogAnswer::Done(None))), "{answer:?}");
        }
        // Both nodes acknowledge at their next request: no lease has to run out.
        assert!(c.now < start + Nanos::from_millis(2 * AUTH_LEASE_MS), "{:?}", c.now);
        // A change of the indexes waits for the fence too, and a node holds the fence back while
        // an operation that read the indexes of the same collection before the change still runs
        // (009 FR-006). The collection and its index first.
        let collection = {
            let (tx, rx) = channel();
            let ns = teseladb_meta::catalog::Namespace { db: "db".into(), coll: "c".into() };
            let create = teseladb_meta::catalog::CatalogCommand::Create {
                ns,
                uuid: [3; 16],
                options: vec![],
                if_absent: false,
            };
            let now = c.now;
            c.logic(1).on_message(NodeMsg::Catalog(CatalogReq::Write(create), tx), now);
            let mut answer = None;
            c.run_until(|_| {
                answer = rx.try_recv().ok();
                answer.is_some()
            });
            match answer {
                Some(CatalogAnswer::Done(Some(entry))) => entry.id,
                other => panic!("the collection: {other:?}"),
            }
        };
        let index_id = {
            let (tx, rx) = channel();
            let create = IndexCommand::Create {
                collection,
                name: "x_1".into(),
                spec: vec![1, 2, 3],
                buckets: 0,
            };
            let now = c.now;
            c.logic(1).on_message(NodeMsg::Catalog(CatalogReq::Index(create), tx), now);
            c.run_until(|_| rx.try_recv().is_ok());
            c.logic(1).group0().map(|sm| sm.indexes.indexes.keys().copied().max().unwrap()).unwrap()
        };
        c.run_until(|c| {
            c.logic(2).group0().is_some_and(|sm| sm.indexes.indexes.contains_key(&index_id))
        });
        let now = c.now;
        let (tx, rx) = channel();
        c.logic(2).on_message(NodeMsg::Catalog(CatalogReq::Indexes(collection), tx), now);
        let Ok(CatalogAnswer::Indexes(_, pin)) = rx.try_recv() else { panic!("an answer") };
        // The search indexes are read from the same copy, without a pin (011).
        let (search_tx, search_rx) = channel();
        c.logic(2).on_message(NodeMsg::Catalog(CatalogReq::SearchIndexes(1), search_tx), now);
        let search = search_rx.try_recv();
        assert!(
            matches!(&search, Ok(CatalogAnswer::SearchIndexes(v)) if v.is_empty()),
            "{search:?}"
        );
        let (tx, rx) = channel();
        let index = IndexCommand::SetState { id: index_id, state: IndexState::Ready };
        let now = c.now;
        c.logic(1).on_message(NodeMsg::Catalog(CatalogReq::Index(index), tx), now);
        let held = c.now;
        while c.now < held + NODE_HEARTBEAT + NODE_HEARTBEAT + NODE_HEARTBEAT {
            c.step();
            assert!(rx.try_recv().is_err(), "answered while a read before it runs");
        }
        c.logic(2).authz.pins().release(pin);
        let mut answer = None;
        c.run_until(|_| {
            answer = rx.try_recv().ok();
            answer.is_some()
        });
        assert!(matches!(answer, Some(CatalogAnswer::Done(_))), "{answer:?}");
        // A read taken after its collection's last change holds back nothing, and a change that
        // touches no collection's indexes (an index Group 0 does not know) does not wait for it.
        let (tx, rx) = channel();
        let now = c.now;
        c.logic(2).on_message(NodeMsg::Catalog(CatalogReq::Indexes(collection), tx), now);
        let Ok(CatalogAnswer::Indexes(_, pin)) = rx.try_recv() else { panic!("an answer") };
        let (tx, rx) = channel();
        let other = IndexCommand::SetState { id: 999, state: IndexState::Ready };
        c.logic(1).on_message(NodeMsg::Catalog(CatalogReq::Index(other), tx), now);
        let mut answer = None;
        c.run_until(|_| {
            answer = rx.try_recv().ok();
            answer.is_some()
        });
        assert!(
            matches!(answer, Some(CatalogAnswer::Refused(_))),
            "answered with the read held: {answer:?}"
        );
        c.logic(2).authz.pins().release(pin);
        // A catalog change or a kill forwarded to a node that does not lead Group 0 (and has no
        // sessions) is answered at once.
        let l = c.logic(2);
        let change = Request::Catalog(Group0Command::ClusterTick { cluster_time_ms: 1 }.encode());
        for (req, body) in
            [(7, change.encode()), (8, Request::KillSessions(KillScope::All).encode())]
        {
            l.on_peer(
                NodeId(1),
                PeerMsg::Forward { req, deadline_ms: 100, hlc: None, body: Bytes::from_vec(body) },
                now,
            );
            let mut out = Vec::new();
            l.take_outbound(&mut out);
            assert!(
                out.iter().any(|(to, m)| *to == NodeId(1)
                    && matches!(m, PeerMsg::ForwardResp { req: r, .. } if *r == req)),
                "{out:?}"
            );
        }
        // Without a cluster, users and roles cannot be read.
        let mut c = Cluster::new();
        c.add(5, None, false);
        for req in [one(), all()] {
            let (tx, rx) = channel();
            c.logic(5).on_message(NodeMsg::Catalog(req, tx), now);
            assert_eq!(rx.try_recv().unwrap(), CatalogAnswer::Unavailable);
        }
    }

    #[test]
    fn a_balancer_decision_in_flight_holds_the_next_round() {
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        let l = c.logic(1);
        l.balance_op = Some(OpId(4242));
        l.balance_at = Nanos::ZERO;
        l.balance();
        assert_eq!((l.balance_op, l.balance_at), (Some(OpId(4242)), Nanos::ZERO));
    }

    /// A node that joins or starts makes the balancer look at once, not at its next round: until
    /// then the founder may hold the only voter of every range, and losing it would leave them
    /// without a leader for good (007 FR-010, 005 SC-004).
    #[test]
    fn a_node_that_joins_or_starts_makes_the_balancer_look_before_its_round() {
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped && c.logic(1).balance_at > Nanos::ZERO);
        let looked = c.logic(1).balance_round;
        let rx = c.join(1, request(2, &addr(2)));
        let a = c.answer(&rx).expect("accepted");
        let soon = c.now + Nanos::from_millis(100);
        c.run_until(|c| c.logic(1).balance_round > looked);
        assert!(c.now < soon, "the balancer waited for its round after the join");
        // The node can take replicas once it records its start: the balancer looks again.
        let voters = a.voters.iter().map(|(n, _)| *n).collect();
        c.add(2, Some(GroupDesc { group: GROUP0, voters, learners: vec![NodeId(2)] }), false);
        c.logic(2).boot_epoch = Some(1);
        c.logic(2).me = Some((NodeId(2), addrs(2)));
        let booted = |c: &mut Cluster| {
            let sm = c.logic(1).group0().expect("Group 0");
            sm.nodes.get(&NodeId(2)).is_some_and(|n| n.boot_epoch > 0)
        };
        c.run_until(booted);
        let (started, soon) = (c.now, c.now + Nanos::from_millis(100));
        c.run_until(|c| c.logic(1).balance_round >= started);
        assert!(c.now < soon, "the balancer waited for its round after the start");
    }

    /// Between two rounds the balancer looks again [`SETTLE_CHECK`] after a round that saw a
    /// replica change in progress, and not before.
    #[test]
    fn a_replica_change_in_progress_makes_the_balancer_look_again_soon() {
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped && c.logic(1).balance_at > Nanos::ZERO);
        let l = c.logic(1);
        l.balance_at = l.now + BALANCE_EVERY;
        l.balanced_nodes.clear();
        assert_eq!(l.balance_due().map(|(regular, _)| regular), Some(false), "a node changed");
        l.settle_check_at = Some(l.now + SETTLE_CHECK);
        assert!(l.balance_due().is_none());
        l.settle_check_at = Some(l.now);
        assert_eq!(l.balance_due().map(|(regular, _)| regular), Some(false));
        l.settle_check_at = None;
        assert!(l.balance_due().is_none());
        l.balance_at = l.now;
        assert_eq!(l.balance_due(), Some((true, vec![NodeId(1)])));
    }

    #[test]
    fn group0_takes_what_a_range_leader_on_another_node_asks() {
        let g = teseladb_types::GroupId(4);
        let split = Group0Command::SplitRange { group: g, at: vec![1, 5] };
        let complete = Group0Command::CompleteSplit { group: g, at: vec![1, 5], at_index: 9 };
        let settled = Group0Command::ReplicasSettled { group: g, epoch: 2 };
        for cmd in [split, complete, settled] {
            assert!(forward_allowed(&cmd, NodeId(3)), "{cmd:?}");
        }
        let hint = Group0Command::SetLeaderHint { group: g, node: NodeId(3), term: 0 };
        assert!(!forward_allowed(&hint, NodeId(3)), "a request is the balancer's");
        let mine = Group0Command::SetLeaderHint { group: g, node: NodeId(3), term: 5 };
        assert!(forward_allowed(&mine, NodeId(3)), "a leader publishes itself");
        assert!(!forward_allowed(&mine, NodeId(2)), "only itself");
    }

    #[test]
    fn requests_reach_the_range_leader_from_a_node_without_a_replica() {
        use crate::data::DataReply;
        use crate::data_wire::{RemoteReply, RemoteReq};
        use teseladb_meta::bootstrap::DATA_RANGE;
        use teseladb_range::read::ReadConcern;
        use teseladb_types::GroupId;
        let mut c = Cluster::new();
        let one = GroupDesc { group: GROUP0, voters: vec![NodeId(1)], learners: Vec::new() };
        c.add_with_data(1, one, true);
        c.run_until(|c| {
            let r = c.data(1, DataReq::Status(DATA_RANGE));
            matches!(r, Ok(DataReply::Status(s)) if s.role == Some(Role::Leader))
        });
        let wc =
            teseladb_range::ack::WriteConcern { w: teseladb_range::ack::W::Nodes(1), j: false };
        let put = DataReq::Put { key: vec![1, 7], value: vec![9], wc };
        assert!(matches!(c.data(1, put), Ok(DataReply::Put(_))));
        assert_eq!(c.data(1, DataReq::Get { key: vec![1, 7] }), Ok(DataReply::Get(Some(vec![9]))));
        // Keys outside every range of the core, and a range nobody holds.
        assert_eq!(c.data(1, DataReq::Get { key: vec![9] }), Err(RangeError::OutOfRange));
        let put = DataReq::Put { key: vec![9], value: vec![1], wc };
        assert_eq!(c.data(1, put), Err(RangeError::OutOfRange));
        let scan = RemoteReq::Scan {
            start: vec![1],
            end: vec![2],
            limit: 10,
            concern: ReadConcern::Local,
            backward: false,
        };
        let nobody = c.data(1, DataReq::Remote(GroupId(77), scan.clone()));
        assert_eq!(nobody, Err(RangeError::NotLeader { hint: NodeId(0) }));

        // A second node without a replica (one replica below three nodes) sends its requests to
        // the range's leader, and answers a forwarded request for a range it lacks with the
        // leader it knows.
        let rx = c.join(1, request(2, &addr(2)));
        let a = c.answer(&rx).expect("accepted");
        let voters = a.voters.iter().map(|(n, _)| *n).collect();
        let desc = GroupDesc { group: GROUP0, voters, learners: vec![NodeId(2)] };
        c.add_with_data(2, desc, false);
        c.run_until(|c| c.logic(2).sent_ranges.as_ref().is_some_and(|r| !r.descs.is_empty()));
        let rows = c.data(2, DataReq::Remote(DATA_RANGE, scan.clone()));
        assert_eq!(rows, Ok(DataReply::Remote(RemoteReply::Rows(vec![(vec![1, 7], vec![9])]))));
        // With its connection to the leader just dropped, node 2 sends nothing and says so at
        // once (005 SC-004); once it hears from node 1 again, the request goes.
        c.logic(2).core.report_unreachable(NodeId(1));
        let now = c.now;
        let (tx, rx) = channel();
        let req = DataReq::Remote(DATA_RANGE, scan.clone());
        c.logic(2).on_message(NodeMsg::Data(req, tx), now);
        c.step();
        assert_eq!(rx.try_recv(), Ok(Err(RangeError::NotLeader { hint: NodeId(0) })));
        for _ in 0..20 {
            c.step();
        }
        let rows = c.data(2, DataReq::Remote(DATA_RANGE, scan.clone()));
        assert_eq!(rows, Ok(DataReply::Remote(RemoteReply::Rows(vec![(vec![1, 7], vec![9])]))));
        // Forwarded requests node 2 cannot run: a range it lacks, and an undecodable body.
        let now = (c.now, 0);
        let l = c.logic(2);
        let data = l.data.as_mut().expect("a data core");
        let body = crate::data_wire::encode_for(DATA_RANGE, &scan);
        data.request_remote(&mut l.core, (NodeId(1), 900), &body, now);
        data.request_remote(&mut l.core, (NodeId(1), 901), &[0xff], now);
        c.step();
    }

    /// The state of node `id`'s replica of the data range.
    fn range_status(c: &mut Cluster, id: u64) -> crate::data::DataStatus {
        use crate::data::DataReply;
        match c.data(id, DataReq::Status(teseladb_meta::bootstrap::DATA_RANGE)) {
            Ok(DataReply::Status(s)) => s,
            other => panic!("{other:?}"),
        }
    }

    /// Three nodes with data cores whose data range has a voter on each.
    fn three_with_data() -> Cluster {
        use teseladb_meta::bootstrap::DATA_RANGE;
        let mut c = Cluster::new();
        let one = GroupDesc { group: GROUP0, voters: vec![NodeId(1)], learners: Vec::new() };
        c.add_with_data(1, one, true);
        c.run_until(|c| range_status(c, 1).role == Some(Role::Leader));
        for id in [2, 3] {
            let rx = c.join(1, request(id, &addr(id)));
            let a = c.answer(&rx).expect("accepted");
            let voters = a.voters.iter().map(|(n, _)| *n).collect();
            let desc = GroupDesc { group: GROUP0, voters, learners: vec![NodeId(id)] };
            c.add_with_data(id, desc, false);
        }
        // The two new nodes join the range (as the balancer of a cluster whose nodes boot does).
        c.run_until(|c| c.logic(3).sent_ranges.as_ref().is_some_and(|r| !r.descs.is_empty()));
        let l = c.logic(1);
        let sm = l.core.state_machine(GROUP0).and_then(Group0Holder::group0).unwrap();
        let epoch = sm.ranges[&DATA_RANGE].replica_epoch;
        let add = vec![NodeId(2), NodeId(3)];
        let cmd = Group0Command::SetReplicas { group: DATA_RANGE, epoch, add, remove: Vec::new() };
        let now = c.now;
        c.logic(1).core.propose(GROUP0, &cmd.encode(), now).unwrap();
        c.run_until(|c| {
            let s = range_status(c, 1);
            s.voters.len() == 3 && s.learners.is_empty()
        });
        c
    }

    #[test]
    fn group0_hands_the_descriptors_to_a_meta2_range_that_every_node_holds() {
        use crate::data::DataReply;
        use teseladb_meta::bootstrap::DATA_RANGE;
        use teseladb_meta::meta2::SNAPSHOT_BYTES_SETTING;
        use teseladb_range::ack::{W, WriteConcern};
        let mut c = three_with_data();
        for id in 1..=3 {
            c.logic(id).me = Some((NodeId(id), addrs(id)));
        }
        let wc = WriteConcern { w: W::Majority, j: true };
        let put = DataReq::Put { key: vec![1, 1], value: vec![1], wc };
        assert!(matches!(c.data(1, put), Ok(DataReply::Put(_))));
        // A one-byte threshold: Group 0's leader hands its descriptors over at once.
        let set = Group0Command::SetSetting {
            key: SNAPSHOT_BYTES_SETTING.to_vec(),
            value: b"1".to_vec(),
        };
        let now = c.now;
        c.logic(1).core.propose(GROUP0, &set.encode(), now).unwrap();
        let handed = |c: &mut Cluster, id: u64| {
            c.logic(id)
                .group0()
                .and_then(|sm| sm.meta1.as_ref())
                .is_some_and(|m| !m.seeded.is_empty())
        };
        c.run_until(|c| (1..=3).all(|id| handed(c, id)));
        let m2 = c.logic(1).group0().unwrap().meta1.as_ref().unwrap().ranges[0].group;
        // Group 0 no longer has the descriptors; every node holds the Meta2 range, seeded, and
        // routes with it.
        c.run_until(|c| {
            (1..=3).all(|id| {
                let l = c.logic(id);
                let seeded =
                    l.core.state_machine(m2).and_then(CoreSm::meta2).is_some_and(|m| m.seeded);
                seeded
                    && l.group0().is_some_and(|sm| sm.ranges.is_empty())
                    && l.sent_ranges
                        .as_ref()
                        .is_some_and(|r| r.descs.iter().any(|d| d.group == DATA_RANGE))
            })
        });
        assert!(c.logic(2).sent_ranges.as_ref().unwrap().meta2_groups.contains(&m2));
        // An order on the descriptor goes to the Meta2 range: the range splits there.
        // As a range's leader does, the order is sent again until it is applied.
        let split = Group0Command::SplitRange { group: DATA_RANGE, at: vec![1, 4] };
        let halves = |c: &mut Cluster, id: u64| {
            c.logic(id).core.state_machine(m2).and_then(CoreSm::meta2).map_or(0, |m| m.ranges.len())
        };
        let mut turn = 0u32;
        c.run_until(|c| {
            turn += 1;
            if turn % 50 == 1 {
                c.logic(2).send_group0(split.clone());
            }
            (1..=3).all(|id| halves(c, id) == 2)
        });
        c.run_until(|c| c.logic(3).sent_ranges.as_ref().is_some_and(|r| r.descs.len() == 2));
        // The split is in the history `config.changelog` shows.
        let (tx, rx) = channel();
        let now = c.now;
        c.logic(1).on_message(NodeMsg::Catalog(CatalogReq::Config("changelog".into()), tx), now);
        let Ok(crate::catalog_ops::CatalogAnswer::Documents(docs)) = rx.try_recv() else {
            panic!()
        };
        // The replica change of the start in Group 0, then the split in the Meta2 range.
        let what = |b: &Vec<u8>| {
            let d = teseladb_bson::Document::from(teseladb_bson::RawDoc::from_bytes(b).unwrap());
            d.get("what").cloned()
        };
        let whats: Vec<_> = docs.iter().map(what).collect();
        assert_eq!(whats.last(), Some(&Some(teseladb_bson::Value::from("split"))), "{whats:?}");
        assert_eq!(whats.len(), 2, "{whats:?}");
        // The data still reads, through the range's leader.
        let get = DataReq::Get { key: vec![1, 1] };
        c.run_until(|c| matches!(c.data(1, get.clone()), Ok(DataReply::Get(Some(_)))));
    }

    #[test]
    fn a_scan_with_a_read_preference_runs_on_its_candidates_in_turn() {
        use crate::data::DataReply;
        use crate::data_wire::{RemoteReply, RemoteReq};
        use teseladb_meta::bootstrap::DATA_RANGE;
        use teseladb_range::ack::{W, WriteConcern};
        let mut c = three_with_data();
        let wc = WriteConcern { w: W::Majority, j: true };
        for k in 0..4u8 {
            let put = DataReq::Put { key: vec![1, k], value: vec![k], wc };
            assert!(matches!(c.data(1, put), Ok(DataReply::Put(_))));
        }
        // Past the lag and a mark the log carries: the followers read the writes.
        for _ in 0..1_000 {
            c.step();
        }
        let l = (1..=3).find(|id| range_status(&mut c, *id).role == Some(Role::Leader)).unwrap();
        let f = (1..=3).find(|id| *id != l).unwrap();
        let scan = |nodes: Vec<(u64, bool)>, max_stale_ms| {
            let nodes = nodes.into_iter().map(|(n, leads)| (NodeId(n), leads)).collect();
            let r = RemoteReq::FollowerScan {
                start: vec![1],
                end: vec![2],
                limit: 10,
                max_stale_ms,
                nodes,
                backward: false,
            };
            DataReq::Remote(DATA_RANGE, r)
        };
        let rows = |r: Result<DataReply, RangeError>| match r {
            Ok(DataReply::Remote(RemoteReply::Rows(rows))) => rows.len(),
            other => panic!("{other:?}"),
        };
        // On the follower that asks, and forwarded from the leader to it.
        assert_eq!(rows(c.data(f, scan(vec![(f, false)], None))), 4);
        assert_eq!(rows(c.data(l, scan(vec![(f, false)], None))), 4);
        // Its mark is older than a millisecond: the next candidate, the leader, scans.
        assert_eq!(rows(c.data(f, scan(vec![(f, false), (l, true)], Some(1)))), 4);
        assert_eq!(rows(c.data(l, scan(vec![(f, false), (l, true)], Some(1)))), 4);
        // No candidate left: the last refusal goes back.
        let refused = c.data(f, scan(vec![(f, false)], Some(1)));
        assert!(matches!(refused, Err(RangeError::NotLeader { .. })), "{refused:?}");
        assert!(matches!(c.data(f, scan(vec![], None)), Err(RangeError::NotLeader { .. })));
    }

    #[test]
    fn two_cold_neighbours_merge_and_the_right_ones_replicas_go() {
        use crate::data::DataReply;
        use teseladb_meta::bootstrap::DATA_RANGE;
        use teseladb_range::ack::{W, WriteConcern};
        let mut c = three_with_data();
        // Ranges merge once no write reached them for 2 s; each node knows itself, as a started
        // node does, so that its data core's Group 0 commands go through core 0.
        for id in 1..=3 {
            let l = c.logic(id);
            l.data = l.data.take().map(|d| d.with_merges(16 << 20, Nanos::from_secs(2)));
            l.me = Some((NodeId(id), addrs(id)));
        }
        let wc = WriteConcern { w: W::Majority, j: true };
        for k in 0..8u8 {
            let put = DataReq::Put { key: vec![1, k], value: vec![k], wc };
            assert!(matches!(c.data(1, put), Ok(DataReply::Put(_))));
        }
        let ranges = |c: &mut Cluster| c.logic(1).group0().map_or(0, |sm| sm.ranges.len());
        let settled = |c: &mut Cluster| {
            c.logic(1).group0().is_some_and(|sm| sm.ranges.values().all(|d| !d.settling))
        };
        c.run_until(settled);
        let split = Group0Command::SplitRange { group: DATA_RANGE, at: vec![1, 4] };
        c.logic(1).send_group0(split);
        c.run_until(|c| ranges(c) == 2);
        let right = c.logic(1).group0().unwrap().range_for(&[1, 6]).unwrap().group;
        assert_ne!(right, DATA_RANGE);
        // Cold and small, the two halves merge back: Group 0 keeps the left one, every node
        // drops its replica of the right one, and both sides' documents are read from the left.
        c.run_until(|c| ranges(c) == 1);
        c.run_until(|c| (1..=3).all(|id| c.logic(id).core.state_machine(right).is_none()));
        c.run_until(|c| {
            (1..=3).all(|id| {
                let sm = c.logic(id).group0().unwrap();
                sm.merges.is_empty() && sm.range_for(&[1, 6]).unwrap().group == DATA_RANGE
            })
        });
        for id in 1..=3 {
            assert!(c.logic(id).core.recovered_groups().is_empty());
        }
        // Read and written through the merged range's leader.
        c.run_until(|c| (1..=3).any(|id| range_status(c, id).role == Some(Role::Leader)));
        let l = (1..=3).find(|id| range_status(&mut c, *id).role == Some(Role::Leader)).unwrap();
        for k in [1u8, 6] {
            let got = c.data(l, DataReq::Get { key: vec![1, k] });
            assert_eq!(got, Ok(DataReply::Get(Some(vec![k]))), "{k}");
        }
        let put = DataReq::Put { key: vec![1, 7], value: vec![70], wc };
        assert!(matches!(c.data(l, put), Ok(DataReply::Put(_))));
        assert_eq!(c.data(l, DataReq::Get { key: vec![1, 7] }), Ok(DataReply::Get(Some(vec![70]))));
    }

    #[test]
    fn a_merge_a_cut_off_node_cannot_confirm_is_given_up_and_retried_once_it_is_back() {
        use crate::data::DataReply;
        use teseladb_meta::bootstrap::DATA_RANGE;
        use teseladb_range::ack::{W, WriteConcern};
        let mut c = three_with_data();
        for id in 1..=3 {
            let l = c.logic(id);
            l.data = l.data.take().map(|d| d.with_merges(16 << 20, Nanos::from_secs(2)));
            l.me = Some((NodeId(id), addrs(id)));
        }
        let wc = WriteConcern { w: W::Majority, j: true };
        let put = DataReq::Put { key: vec![1, 6], value: vec![6], wc };
        assert!(matches!(c.data(1, put), Ok(DataReply::Put(_))));
        let ranges = |c: &mut Cluster| c.logic(1).group0().map_or(0, |sm| sm.ranges.len());
        c.run_until(|c| {
            c.logic(1).group0().is_some_and(|sm| sm.ranges.values().all(|d| !d.settling))
        });
        c.logic(1).send_group0(Group0Command::SplitRange { group: DATA_RANGE, at: vec![1, 4] });
        c.run_until(|c| ranges(c) == 2);
        let right = c.logic(1).group0().unwrap().range_for(&[1, 6]).unwrap().group;
        // Node 3 is cut off: its replica of the right range never confirms the freeze, and the
        // left range's leader gives the merge up in its log; Group 0 aborts it and the right range
        // thaws.
        c.cut.insert(NodeId(3));
        let gave_up = |c: &mut Cluster| {
            (1..=2).any(|id| {
                let l = c.logic(id);
                l.core
                    .state_machine(DATA_RANGE)
                    .and_then(CoreSm::range)
                    .is_some_and(|sm| sm.abandoned() > 0)
            })
        };
        let thawed = |c: &mut Cluster| {
            (1..=2).all(|id| {
                let l = c.logic(id);
                l.core
                    .state_machine(right)
                    .and_then(CoreSm::range)
                    .is_some_and(|sm| sm.frozen_for().is_none())
            })
        };
        for _ in 0..3 {
            if gave_up(&mut c) {
                break;
            }
            for _ in 0..2_000 {
                if gave_up(&mut c) {
                    break;
                }
                c.step();
            }
        }
        assert!(gave_up(&mut c));
        c.run_until(|c| thawed(c) || ranges(c) == 1);
        assert_eq!(ranges(&mut c), 2);
        // Back, node 3 confirms the next intent and the ranges merge.
        c.cut.clear();
        c.run_until(|c| ranges(c) == 1);
        c.run_until(|c| (1..=3).all(|id| c.logic(id).core.state_machine(right).is_none()));
    }

    #[test]
    fn an_idle_range_whose_leader_is_cut_off_without_a_dropped_connection_elects_again() {
        use crate::data::DataReply;
        use teseladb_range::ack::{W, WriteConcern};
        let mut c = three_with_data();
        let wc = WriteConcern { w: W::Majority, j: true };
        let put = DataReq::Put { key: vec![1, 1], value: vec![1], wc };
        assert!(matches!(c.data(1, put), Ok(DataReply::Put(_))));
        // Idle long enough for the range and Group 0 to go quiet.
        for _ in 0..400 {
            c.step();
        }
        let leader = (1..=3).find(|id| range_status(&mut c, *id).role == Some(Role::Leader));
        let leader = leader.expect("a leader");
        c.cut.insert(NodeId(leader));
        let others: Vec<u64> = (1..=3).filter(|id| *id != leader).collect();
        // Within about a second (100 steps of 10 ms) one of the other two leads.
        let mut elected = None;
        for _ in 0..150 {
            c.step();
            elected = others
                .iter()
                .copied()
                .find(|id| range_status(&mut c, *id).role == Some(Role::Leader));
            if elected.is_some() {
                break;
            }
        }
        let n = elected.expect("the majority elects a new leader");
        let put = DataReq::Put { key: vec![1, 2], value: vec![2], wc };
        assert!(matches!(c.data(n, put), Ok(DataReply::Put(_))));
    }

    #[test]
    fn a_replica_whose_checksum_differs_is_rebuilt_from_the_others() {
        use crate::data::DataReply;
        use teseladb_hlc::Hlc;
        use teseladb_lsm::Lookup;
        use teseladb_meta::bootstrap::DATA_RANGE;
        use teseladb_range::ack::{W, WriteConcern};
        const STRAY: &[u8] = &[1, 99];
        let status = |c: &mut Cluster, id: u64| match c.data(id, DataReq::Status(DATA_RANGE)) {
            Ok(DataReply::Status(s)) => s,
            other => panic!("{other:?}"),
        };
        // The stray version, read straight from node `id`'s LSM.
        let stray = |c: &mut Cluster, id: u64| loop {
            let lsm = c.logic(id).data.as_ref().expect("a data core").lsm().clone();
            let view = lsm.borrow().view();
            let newest = Hlc { physical: u64::MAX, logical: u32::MAX };
            match view.get(&mut lsm.borrow_mut(), STRAY, newest) {
                Lookup::Hit(_) => return true,
                Lookup::NotFound => return false,
                Lookup::Miss(_) => c.step(),
                Lookup::Failed(e) => panic!("{e:?}"),
            }
        };
        let mut c = three_with_data();
        let wc = WriteConcern { w: W::Majority, j: true };
        for k in 0..8u8 {
            let put = DataReq::Put { key: vec![1, k], value: vec![k], wc };
            assert!(matches!(c.data(1, put), Ok(DataReply::Put(_))));
        }
        let leader = (1..=3).find(|id| status(&mut c, *id).role == Some(Role::Leader)).unwrap();
        let victim = (1..=3).find(|id| *id != leader).unwrap();
        c.run_until(|c| status(c, victim).applied == status(c, leader).applied);
        // A version only the victim has: its sum differs from the other two.
        let applied = status(&mut c, victim).applied;
        let lsm = c.logic(victim).data.as_ref().unwrap().lsm().clone();
        let old = Hlc { physical: 1, logical: 0 };
        lsm.borrow_mut().put(DATA_RANGE, applied, STRAY, old, b"stray").unwrap();
        assert!(stray(&mut c, victim));
        let l = c.logic(leader);
        let data = l.data.as_mut().unwrap();
        data.check(&l.core, DATA_RANGE).expect("the leader takes a checksum");
        c.run_until(|c| {
            let s = status(c, victim);
            s.rebuilt == 1 && !s.rebuilding && s.role.is_some() && !s.waiting_data
        });
        c.run_until(|c| status(c, victim).applied >= status(c, leader).applied);
        assert!(!stray(&mut c, victim), "the stray version went with the rebuild");
        for k in 0..8u8 {
            let lsm = c.logic(victim).data.as_ref().unwrap().lsm().clone();
            let view = lsm.borrow().view();
            let newest = Hlc { physical: u64::MAX, logical: u32::MAX };
            assert!(matches!(view.get(&mut lsm.borrow_mut(), &[1, k], newest), Lookup::Hit(_)));
        }
        // The replicas agree now: another checksum rebuilds nothing, and the cluster still writes.
        let l = c.logic(leader);
        let data = l.data.as_mut().unwrap();
        data.check(&l.core, DATA_RANGE).expect("the leader takes a checksum");
        for _ in 0..300 {
            c.step();
        }
        assert!((1..=3).all(|id| status(&mut c, id).rebuilt == u64::from(id == victim)));
        let put = DataReq::Put { key: vec![1, 50], value: vec![1], wc };
        assert!(matches!(c.data(leader, put), Ok(DataReply::Put(_))));
        // The leader may be the one marked: it rebuilds its own replica, and the others elect.
        let applied = status(&mut c, leader).applied;
        let lsm = c.logic(leader).data.as_ref().unwrap().lsm().clone();
        lsm.borrow_mut().put(DATA_RANGE, applied, STRAY, old, b"stray").unwrap();
        let l = c.logic(leader);
        let data = l.data.as_mut().unwrap();
        data.check(&l.core, DATA_RANGE).expect("the leader takes a checksum");
        c.run_until(|c| {
            let s = status(c, leader);
            s.rebuilt == 1 && !s.rebuilding && s.role.is_some() && !s.waiting_data
        });
        assert!(!stray(&mut c, leader));
    }

    #[test]
    fn group0_keeps_five_voters_and_the_rest_learners() {
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        for id in 2..=6 {
            c.join_node(1, id);
            let want = (id as usize).min(GROUP0_MAX_VOTERS);
            c.run_until(|c| c.logic(1).view().voters.len() == want);
        }
        for _ in 0..200 {
            c.step();
        }
        let v = c.logic(1).view();
        assert_eq!(v.voters.len(), GROUP0_MAX_VOTERS);
        assert_eq!(v.learners, [NodeId(6)]);
        assert_eq!(v.nodes.len(), 6);
    }

    #[test]
    fn joins_are_refused_where_they_cannot_be_taken() {
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        c.join_node(1, 2);
        // A follower that knows the leader but has not applied Group 0 yet has no address for it.
        c.run_until(|c| c.logic(2).view().leader == NodeId(1));
        if !c.logic(2).view().bootstrapped {
            let rx = c.join(2, request(3, &addr(3)));
            assert_eq!(rx.try_recv().unwrap(), Err("not_leader".to_owned()));
        }
        // Once applied, a follower names the leader.
        c.run_until(|c| c.logic(2).view().bootstrapped);
        let rx = c.join(2, request(3, &addr(3)));
        assert_eq!(rx.try_recv().unwrap(), Err(format!("not_leader {}", addr(1))));
        // A core without Group 0 knows no leader.
        c.add(9, None, false);
        assert_eq!(c.logic(9).view(), Group0View::default());
        assert_eq!(c.logic(9).peer_addr(NodeId(1)), None);
        let rx = c.join(9, request(3, &addr(3)));
        assert_eq!(rx.try_recv().unwrap(), Err("not_leader".to_owned()));
        // A name another node has is refused by Group 0.
        let rx = c.join(1, request(4, &addr(1)));
        let e = c.answer(&rx).unwrap_err();
        assert!(e.contains(&format!("{:?}", Group0Error::NameOrAddrTaken)), "{e}");
    }

    #[test]
    fn messages_outside_the_core_and_forwards_are_answered() {
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        let now = c.now;
        let l = c.logic(1);
        l.on_peer(NodeId(2), PeerMsg::RangeRetire { group: GROUP0 }, now);
        let (tx, rx) = channel();
        l.on_message(NodeMsg::View(tx), now);
        assert!(rx.try_recv().unwrap().bootstrapped);
        // An idle core waits for nothing.
        assert_eq!(l.next_deadline(), None);
        l.peer_unreachable(NodeId(2));
        // A forwarded request gets an empty answer.
        let fwd = PeerMsg::Forward { req: 3, deadline_ms: 100, hlc: None, body: Bytes::new() };
        l.on_peer(NodeId(2), fwd, now);
        let mut out = Vec::new();
        l.take_outbound(&mut out);
        assert!(
            out.iter().any(|(to, m)| *to == NodeId(2)
                && matches!(m, PeerMsg::ForwardResp { req: 3, body, .. } if body.is_empty())),
            "{out:?}"
        );
    }

    #[test]
    fn a_core_without_group0_or_the_range_drops_what_is_not_its() {
        let mut c = Cluster::new();
        c.add(9, None, false);
        let now = c.now;
        let l = c.logic(9);
        let desc = RangeDesc {
            group: teseladb_meta::bootstrap::DATA_RANGE,
            start: vec![1],
            end: vec![4],
            core: 0,
            replicas: Vec::new(),
            replica_epoch: 1,
            generation: 1,
            leader_hint: None,
            leader_term: 0,
            pending_split: None,
            settling: false,
            moved_ms: 0,
        };
        l.on_message(NodeMsg::Ranges(Ranges { descs: vec![desc], ..Ranges::default() }), now);
        // No leader and no voters to send it to.
        let cmd = Group0Command::ReplicasSettled { group: teseladb_types::GroupId(1), epoch: 1 };
        l.on_message(NodeMsg::Group0(cmd), now);
        assert!(l.group0_ops.is_empty());
        let (tx, rx) = channel();
        l.on_message(NodeMsg::Data(DataReq::Status(teseladb_types::GroupId(1)), tx), now);
        assert_eq!(rx.try_recv().unwrap(), Err(RangeError::OutOfRange));
        l.on_message(NodeMsg::Peers(BTreeMap::from([(NodeId(1), addr(1))])), now);
        assert_eq!(l.peer_addr(NodeId(1)), Some(addr(1)));
    }

    /// Three nodes whose Group 0 has every node as a voter.
    fn three_voters() -> Cluster {
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        c.join_node(1, 2);
        c.join_node(1, 3);
        let all = [NodeId(1), NodeId(2), NodeId(3)];
        c.run_until(|c| (1..=3).all(|i| c.logic(i).view().voters == all));
        c
    }

    #[test]
    fn a_follower_back_on_another_port_updates_its_address_through_the_leader() {
        let mut c = three_voters();
        c.restart(2, "127.0.0.1:9002");
        assert!(!c.logic(2).view().addrs_current, "Group 0 still has the old port");
        c.run_until(|c| c.peer_of(1, 2).as_deref() == Some("127.0.0.1:9002"));
        c.run_until(|c| c.logic(2).view().addrs_current);
        assert_eq!(c.peer_of(3, 2).as_deref(), Some("127.0.0.1:9002"));
    }

    #[test]
    fn a_learner_back_on_another_port_that_knows_no_leader_finds_it() {
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        c.join_node(1, 2);
        c.run_until(|c| (1..=2).all(|i| c.logic(i).view().voters == [NodeId(1), NodeId(2)]));
        c.join_node(1, 3);
        // Node 3 has applied Group 0 up to its own entry but stops before it applies its
        // promotion: it comes back a learner, on another port, without a leader; the leader still
        // dials the old port.
        c.run_until(|c| c.logic(3).view().nodes.contains(&NodeId(3)));
        assert_eq!(c.logic(3).view().role, Some(Role::Learner));
        c.restart(3, "127.0.0.1:9003");
        let v = c.logic(3).view();
        assert_eq!((v.role, v.leader, v.addrs_current), (Some(Role::Learner), NodeId(0), false));
        c.run_until(|c| c.logic(3).view().addrs_current);
        c.run_until(|c| (1..=3).all(|i| c.peer_of(i, 3).as_deref() == Some("127.0.0.1:9003")));
    }

    #[test]
    fn the_founder_back_on_another_port_updates_its_address() {
        let mut c = three_voters();
        c.restart(1, "127.0.0.1:9001");
        c.run_until(|c| (1..=3).all(|i| c.peer_of(i, 1).as_deref() == Some("127.0.0.1:9001")));
        assert!(c.logic(1).view().addrs_current);
    }

    #[test]
    fn a_node_only_changes_its_own_addresses() {
        let mut c = three_voters();
        let mut other = addrs(3);
        other.peer_addr = b"127.0.0.1:6666".to_vec();
        let cmd =
            Request::Group0(Group0Command::SetNodeAddrs { node: NodeId(3), addrs: other }).encode();
        let fwd =
            PeerMsg::Forward { req: 1, deadline_ms: 100, hlc: None, body: Bytes::from_vec(cmd) };
        c.deliver(NodeId(1), fwd, NodeId(2));
        for _ in 0..50 {
            c.step();
        }
        assert_eq!(c.peer_of(1, 3), Some(addr(3)));
    }

    #[test]
    fn core_zero_creates_root_while_the_cluster_has_no_user() {
        let mut c = Cluster::new();
        c.found();
        let doc = teseladb_bson::Document::new()
            .with("_id", "admin.root")
            .with("user", "root")
            .with("db", "admin")
            .to_bytes()
            .unwrap();
        c.logic(1).root_user = Some((doc.clone(), Nanos::ZERO));
        // Bootstrapped, the node is not ready while its `root` user is not in Group 0.
        let v = c.logic(1).view();
        assert!(v.root_pending && !v.ready(), "{v:?}");
        c.run_until(|c| {
            c.logic(1).group0().is_some_and(|sm| {
                sm.security.get(Principal::User, "admin", crate::pki::ROOT_USER).is_some()
            })
        });
        let users = |c: &mut Cluster| {
            c.logic(1).group0().map_or(0, |sm| sm.security.list(Principal::User, None).len())
        };
        c.run_until(|c| c.logic(1).root_user.is_none());
        assert_eq!(users(&mut c), 1);
        assert!(c.logic(1).view().ready());
        // With a user already there, another node's `root` is not created.
        c.logic(1).root_user = Some((doc, Nanos::ZERO));
        c.run_until(|c| c.logic(1).root_user.is_none());
        assert_eq!(users(&mut c), 1);
        // A document without `_id` is not proposed.
        let mut c = Cluster::new();
        c.found();
        c.logic(1).root_user = Some((vec![5, 0, 0, 0, 0], Nanos::ZERO));
        c.run_until(|c| c.logic(1).root_user.is_none());
        assert_eq!(users(&mut c), 0);
    }

    #[test]
    fn group0s_leader_makes_the_clustertime_keys_and_every_node_opens_them() {
        use crate::clustertime_keys::{KeyDriver, KeySlot};
        use teseladb_auth::clustertime::KeySealer;
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        c.join_node(1, 2);
        let slots = [Arc::new(KeySlot::default()), Arc::new(KeySlot::default())];
        for (id, slot) in [1u64, 2].into_iter().zip(&slots) {
            let sealer = KeySealer::new(&[4; 32]).unwrap();
            let driver = KeyDriver::new(sealer, slot.clone(), &[id as u8; 48]);
            c.logic(id).clustertime_keys = Some(driver);
        }
        c.run_until(|_| slots.iter().all(|s| s.ring().keys.len() == 2));
        // Two keys only, the same on both nodes, sealed in Group 0.
        for _ in 0..300 {
            c.step();
        }
        let sm_keys = c.logic(2).group0().map(|sm| sm.clustertime_keys.clone()).unwrap();
        assert_eq!(sm_keys.keys.len(), 2);
        assert_eq!(*slots[0].ring(), *slots[1].ring());
        let ring = slots[0].ring();
        for k in &ring.keys {
            let sealed = &sm_keys.keys[&k.id].sealed;
            assert!(!sealed.windows(4).any(|w| k.key.windows(4).any(|x| x == w)));
        }
        // A node with another master key opens none and signs with zeros.
        let stranger = Arc::new(KeySlot::default());
        let sealer = KeySealer::new(&[9; 32]).unwrap();
        c.logic(2).clustertime_keys = Some(KeyDriver::new(sealer, stranger.clone(), &[3; 48]));
        c.step();
        assert!(stranger.ring().keys.is_empty());
        assert_eq!(c.logic(1).group0().unwrap().clustertime_keys.keys.len(), 2);
    }

    #[test]
    fn a_node_not_in_group0_yet_proposes_nothing() {
        let mut c = Cluster::new();
        c.found();
        c.run_until(|c| c.logic(1).view().bootstrapped);
        c.restart(1, "127.0.0.1:9001");
        // A learner of a cluster it is not in: no record, no proposal, not current.
        let desc = GroupDesc { group: GROUP0, voters: vec![NodeId(1)], learners: vec![NodeId(8)] };
        c.add(8, Some(desc), false);
        let at = c.nodes.iter().position(|(n, ..)| n.0 == 8).unwrap();
        c.nodes[at].1.me = Some((NodeId(8), addrs(8)));
        for _ in 0..30 {
            c.step();
        }
        let v = c.logic(8).view();
        assert!(!v.addrs_current);
        assert_eq!(c.logic(8).addr_update, None);
    }
}
