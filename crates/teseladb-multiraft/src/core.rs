//! `MultiRaftCore`: the Raft groups of one core over its `CoreStore`, for any [`StateMachine`]
//! (contract `state-machine-api.md`). It does no I/O: the core loop passes it the time, the disk
//! results and the frames from other nodes, runs the disk operations `poll_io` hands out and sends
//! what `drain_actions` returns.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use teseladb_hlc::{ClockGuard, Hlc, LOGICAL_BITS, Next};

use teseladb_raft::{
    ConfChange, ConfState, Entry, EntryKind, LogStore, Message, MessageType, ProposeError,
    RaftConfig, RawNode, Ready, Role, Snapshot, Status,
};
use teseladb_raftlog::{
    CoreStore, IoDone, IoOp, LogEntry, ReadToken, ReleaseProvider, SnapshotMeta, Ticket,
    format::wal::EntryKind as WalKind,
};
use teseladb_types::{
    Bytes, DecodeError, Decoder, Durability, Encoder, GroupId, Nanos, NodeId, Rng,
};

use crate::chunks::{Reassembly, Received, split};
use crate::config::{CoreConfig, GroupDesc, ReadMode, snapshot_timeout};
use crate::envelope;
use crate::error::OpError;
use crate::frame::{
    GroupHeartbeat, PeerFrame, coalesce, expand_heartbeat_resps, expand_heartbeats,
};
use crate::lease_clock::read_cache_minimum;
use crate::log_store::{
    GroupStore, decode_conf, encode_conf, hs_from_store, hs_to_store, to_store,
};
use crate::state_machine::{ApplyCtx, StateMachine};
use crate::terms::TermRuns;

mod handover;

pub use handover::{Durable, GroupImport};

/// Identifier of an operation in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OpId(pub u64);

/// How an operation ended well.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Done<O> {
    /// The proposed entry was committed and applied here, with the state machine's output.
    Applied(O),
    /// The read may be served from the local state machine, which applied `index`.
    Read {
        /// Commit index at the moment of the read.
        index: u64,
    },
    /// The configuration change was committed: the resulting configuration.
    ConfChanged(ConfState),
    /// The answer to a forwarded request.
    Forwarded(Bytes),
}

/// Why [`MultiRaftCore::propose_at`] proposed nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProposeAt<E> {
    /// The core refused the proposal.
    Op(OpError),
    /// The payload could not be built.
    Build(E),
}

impl<E> From<OpError> for ProposeAt<E> {
    fn from(e: OpError) -> ProposeAt<E> {
        ProposeAt::Op(e)
    }
}

/// A request another node forwarded to this one; the layer above answers it with
/// [`MultiRaftCore::answer_forward`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Forwarded {
    /// Node that forwarded it.
    pub from: NodeId,
    /// Its number on that node.
    pub req: u64,
    /// Milliseconds it waits for the answer.
    pub deadline_ms: u32,
    /// Request.
    pub body: Bytes,
}

/// What the core loop has to do.
#[derive(Debug)]
pub struct Actions<O> {
    /// Frames for other nodes.
    pub send: Vec<(NodeId, PeerFrame)>,
    /// Operations that ended.
    pub done: Vec<(OpId, Result<Done<O>, OpError>)>,
    /// Requests forwarded by other nodes.
    pub forwarded: Vec<Forwarded>,
    /// Snapshots `(group, index)` waiting for their data (`StateMachine::has_data_for`).
    pub snapshot_data: Vec<(GroupId, u64)>,
}

/// What a core records with [`CoreConfig::observe`] for the simulator's Raft checker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Observation {
    /// An entry applied (or skipped) here, with the CRC32C of its data.
    Applied {
        /// Group.
        group: GroupId,
        /// Index.
        index: u64,
        /// Term.
        term: u64,
        /// CRC32C of the entry's data.
        crc: u32,
    },
    /// This replica won an election.
    Elected {
        /// Group.
        group: GroupId,
        /// Term.
        term: u64,
    },
    /// This replica's commit index.
    Committed {
        /// Group.
        group: GroupId,
        /// Commit index.
        commit: u64,
    },
    /// The leader takes `index` (its commit index) for read `ctx`.
    ReadRegistered {
        /// Group.
        group: GroupId,
        /// Read context, unique in this core's life.
        ctx: u64,
        /// Index the read reflects.
        index: u64,
        /// The leader's term.
        term: u64,
        /// Term of the entry at `index` (`None` if the leader's log no longer has it: it applied and
        /// compacted beyond its commit index, which the simulator's checker reports).
        index_term: Option<u64>,
        /// Served right away through the lease.
        lease: bool,
    },
    /// Read `ctx` was confirmed.
    ReadServed {
        /// Group.
        group: GroupId,
        /// Read context.
        ctx: u64,
    },
}

/// First number of a request counter: far from zero and different on every start, so that an
/// answer to a request of an earlier life of the core is never taken for one of this life.
pub fn first_request_id(rng: &mut Rng) -> u64 {
    (1 << 32) + rng.next_u64() % (1 << 62)
}

struct Group<S> {
    raw: RawNode,
    sm: S,
    /// Its snapshot is being read at start-up: nothing is applied until it is restored.
    loading: bool,
    /// The log refused a write, or an entry or a snapshot could not be applied.
    stopped: bool,
    /// A received snapshot is being made durable: no new `Ready` until then.
    installing: bool,
    /// A snapshot from the leader waiting for its data, with the rest of its `Ready`.
    parked: Option<(Snapshot, Deferred)>,
    /// The hard state of a parked `Ready` that a newer snapshot replaced: it goes with the next
    /// `Ready` that has none of its own.
    replaced_hs: Option<teseladb_raftlog::HardState>,
    /// The commit index on disk when a leader's snapshot came that is not durable yet (parked for
    /// its data, being saved, or replaced by a newer one). Raft took the snapshot's index as
    /// committed in memory; until a snapshot is durable, a hard state written with any other
    /// `Ready` carries at most this commit, or a restart would take the old entries the snapshot
    /// replaces for committed and apply them (seed 4058 of `txn-bank`).
    commit_cap: Option<u64>,
    /// When a replica that needs a snapshot last refused the leader's entries.
    refused_at: Nanos,
    /// After a restart, its own snapshot `(index, data)` waiting for the state machine's data.
    /// Meanwhile the replica votes, appends and commits but applies nothing (007 T016).
    own: Option<(u64, Bytes)>,
    /// No replica will serve the data of `own`: it asks the leader for its snapshot.
    own_stale: bool,
    /// Highest HLC mark of the group's entries.
    last_mark: Hlc,
    /// The term this replica leads and the cluster time it won it at, for a state machine with a
    /// read cache ([`StateMachine::read_cache_margin_ms`]).
    lease_mark: Option<(u64, Hlc)>,
    /// Own proposals: index → (term, operation).
    proposals: BTreeMap<u64, (u64, OpId)>,
    /// ReadIndex reads: context → operation.
    reads: BTreeMap<u64, OpId>,
    /// Persistence tickets in flight, in order, with the messages that leave once each one is
    /// durable.
    persisting: VecDeque<(Ticket, Vec<Message>)>,
    /// Index of the latest snapshot taken or installed.
    snap_index: u64,
    /// The index whose state that snapshot holds (`StateMachine::snapshot_state_index`): its own,
    /// or more for one taken while the log replayed entries the state machine held already. It is
    /// what the state machine's floor is compared with.
    snap_state: u64,
    /// Terms of the log up to the latest snapshot taken, installed or restored (written in it).
    terms: TermRuns,
    /// A snapshot taken at an entry the state machine still holds in memory (`HOLDS_LOG`): it is
    /// saved once the release floor reaches it.
    pending_snap: Option<(SnapshotMeta, Bytes)>,
    /// The index whose state `pending_snap` holds.
    pending_state: u64,
    /// Reads served `(with the lease, with ReadIndex)` (005 FR-022).
    read_counts: (u64, u64),
    /// As leader: the term, the mark it closed and its last index then, sent in the heartbeats of
    /// that term (007 FR-012).
    closed_out: Option<(u64, Hlc, u64)>,
    /// The highest closed mark heard from a leader, with its index.
    closed_in: Option<(Hlc, u64)>,
}

impl<S: StateMachine> Group<S> {
    /// Raft learns whether the state machine waits for the data of its own snapshot after a
    /// restart (007 T016). A replica opened without its range's data (`needs_snapshot`) takes no
    /// entries at all, and the founder of a range, which also starts without them, has to apply
    /// its first entry to stop needing them.
    /// The index whose state the snapshot at `index` holds: the latest snapshot's state, or the
    /// index itself for an older one.
    fn state_of(&self, index: u64) -> u64 {
        if index == self.snap_index { self.snap_state } else { index }
    }

    fn sync_awaiting(&mut self) {
        let on = self.own.is_some();
        self.raw.set_awaiting_snapshot(on);
    }
}

#[derive(Clone, Copy, Debug)]
enum OpKind {
    Propose {
        group: GroupId,
    },
    Conf {
        group: GroupId,
    },
    /// `retry`: ReadIndex refused (no leader or no entry of its term committed yet), tried again
    /// on the next tick. `waiting`: confirmed at that index, waits for it to be applied.
    Read {
        group: GroupId,
        retry: bool,
        waiting: Option<u64>,
    },
    Forward {
        req: u64,
        /// Node it was sent to.
        to: NodeId,
    },
}

impl OpKind {
    fn group(&self) -> Option<GroupId> {
        match *self {
            OpKind::Propose { group } | OpKind::Conf { group } | OpKind::Read { group, .. } => {
                Some(group)
            }
            OpKind::Forward { .. } => None,
        }
    }
}

struct Op {
    kind: OpKind,
    deadline: Nanos,
}

/// The rest of a `Ready` that brought a snapshot, persisted once the snapshot is durable.
struct Deferred {
    number: u64,
    entries: Vec<LogEntry>,
    hs: Option<teseladb_raftlog::HardState>,
    after: Vec<Message>,
    /// Node that sent the snapshot and its size, for the acknowledgement.
    from: (NodeId, u64),
    index: u64,
}

enum Wait {
    Persist {
        group: GroupId,
        number: u64,
        last: u64,
        last_term: u64,
    },
    Install {
        group: GroupId,
        deferred: Box<Deferred>,
    },
    /// Group creation and deletion: nothing waits for them.
    Nothing,
    /// A snapshot the core took: nothing waits for it, but the group does not close meanwhile
    /// (reopened, it would not know the snapshot that compacts its log).
    Saved {
        group: GroupId,
    },
    /// The snapshot of a group being imported (007 FR-011): once durable, its hard state and
    /// the entries past it go to the log.
    Import {
        group: GroupId,
        hard: teseladb_raftlog::HardState,
        entries: Vec<LogEntry>,
    },
    /// A hand-over step the layer above waits for (007 FR-011).
    Note(Durable),
}

impl Wait {
    fn group(&self) -> Option<GroupId> {
        match self {
            Wait::Persist { group, .. }
            | Wait::Install { group, .. }
            | Wait::Import { group, .. }
            | Wait::Saved { group } => Some(*group),
            Wait::Nothing | Wait::Note(_) => None,
        }
    }
}

enum Load {
    Restore { group: GroupId },
    Send { group: GroupId, to: NodeId },
}

impl Load {
    fn group(&self) -> GroupId {
        match self {
            Load::Restore { group } | Load::Send { group, .. } => *group,
        }
    }
}

/// How often a core tells the peers of its groups that it is alive ([`MultiRaftCore::on_tick`]).
pub const PING_EVERY: Nanos = Nanos::from_millis(100);
/// Silence after which a peer is suspect: several pings lost, below the election timeouts that
/// follow it (150 to 300 ms by default), so a quiet group elects within about a second.
pub const SILENT_AFTER: Nanos = Nanos::from_millis(500);

/// Snapshot file: the group's highest cluster time, the terms of the log up to the snapshot and
/// the state machine's snapshot.
fn wrap_snapshot(last_ms: u64, terms: &TermRuns, sm: &[u8]) -> Bytes {
    let mut e = Encoder::new();
    e.u64(last_ms);
    terms.encode(&mut e);
    e.raw(sm);
    Bytes::from_vec(e.finish())
}

fn unwrap_snapshot(b: &[u8]) -> Result<(u64, TermRuns, &[u8]), DecodeError> {
    let mut d = Decoder::new(b);
    let ms = d.u64()?;
    let terms = TermRuns::decode(&mut d)?;
    Ok((ms, terms, d.raw(d.remaining())?))
}

/// The group, if it can take operations.
fn usable<S>(
    groups: &mut BTreeMap<GroupId, Group<S>>,
    g: GroupId,
) -> Result<&mut Group<S>, OpError> {
    let grp = groups.get_mut(&g).ok_or(OpError::GroupNotFound)?;
    if grp.stopped || grp.loading {
        return Err(OpError::Unavailable);
    }
    Ok(grp)
}

/// How often a replica that needs a snapshot refuses the leader's entries, which makes the
/// leader send its snapshot (or its entries again, while its log is not compacted).
const NEEDS_SNAPSHOT_EVERY: Nanos = Nanos::from_millis(100);
/// Stray frames kept for the layer above at most: one that never takes them keeps no more.
const MAX_STRAYS: usize = 256;

/// Multi-Raft facade of one core.
pub struct MultiRaftCore<S: StateMachine> {
    cfg: CoreConfig,
    store: CoreStore,
    rng: Rng,
    groups: BTreeMap<GroupId, Group<S>>,
    ops: BTreeMap<OpId, Op>,
    next_op: u64,
    next_ctx: u64,
    next_forward: u64,
    waits: BTreeMap<Ticket, Wait>,
    loads: BTreeMap<ReadToken, Load>,
    /// Raft messages for other nodes.
    outbox: Vec<(GroupId, Message)>,
    /// Groups whose leader registered ReadIndex reads that wait for a heartbeat round: one round
    /// per group and step confirms them all (005 FR-009).
    read_rounds: BTreeSet<GroupId>,
    /// The caller sends those rounds itself at the end of its turn ([`MultiRaftCore::flush_read_rounds`])
    /// instead of at each [`MultiRaftCore::drain_actions`].
    hold_read_rounds: bool,
    /// Other frames for other nodes.
    frames: Vec<(NodeId, PeerFrame)>,
    done: Vec<(OpId, Result<Done<S::Output>, OpError>)>,
    forwarded: Vec<Forwarded>,
    snapshot_data: Vec<(GroupId, u64)>,
    observations: Vec<Observation>,
    /// Entries of this replica's log a later leader's log overwrote (005 FR-010).
    rolled_back: Vec<(GroupId, LogEntry)>,
    /// Entries ever handed to `rolled_back`, ever taken by the layer above, and ever confirmed
    /// in durable rollback files: no disk operation runs while one is not confirmed, so the log
    /// never drops an entry before its rollback file is on disk.
    rolled_counts: (u64, u64, u64),
    reassembly: Reassembly,
    /// Snapshots received whole and `SnapshotReq`s, which arrive in either order.
    received: BTreeMap<GroupId, (NodeId, Received)>,
    snapshot_reqs: BTreeMap<GroupId, Message>,
    /// Snapshot being stepped into Raft: who sent it and its size.
    installing_from: BTreeMap<GroupId, (NodeId, u64)>,
    /// Transfers this core is sending, per group and destination, with their start and size (0
    /// until the snapshot is loaded).
    sending: BTreeMap<GroupId, BTreeMap<NodeId, (Nanos, u64)>>,
    /// Groups whose leader took a snapshot for a replica that asked for one: while it waits for
    /// the release floor, a flush is owed ([`MultiRaftCore::snapshot_awaits_flush`]).
    floor_waits: BTreeSet<GroupId>,
    /// Nodes whose connection dropped and that sent nothing since: a request forwarded to one
    /// ends at once with `Unreachable` instead of waiting for its deadline.
    unreachable: BTreeSet<NodeId>,
    /// When each peer of this core's groups was last heard from (any frame).
    heard: BTreeMap<NodeId, Nanos>,
    /// When this core next tells its peers it is alive.
    next_ping: Nanos,
    /// Snapshots not sent because they were below their state machine's floor, by group and
    /// destination: they go once a newer one is saved.
    refused: BTreeSet<(GroupId, NodeId)>,
    now: Nanos,
    wall_ms: u64,
    /// The local clock rule (005 FR-006).
    clock_rule: ClockGuard,
    /// Attempts to hand over a leadership while the clock is out of margin.
    aside_turn: usize,
    /// Hand-over steps durable and not yet taken (007 FR-011).
    durable: Vec<Durable>,
    /// Frames of groups this core does not run, by the node that sent them: another core of the
    /// node may hold the group (007 FR-011, a range on its way between cores).
    strays: Vec<(NodeId, PeerFrame)>,
    /// The frame being stepped came from another core of the node: what it carries for groups
    /// this core does not run is dropped, never sent on again.
    from_core: bool,
}

impl<S: StateMachine> MultiRaftCore<S> {
    /// Core over a freshly recovered store. Its groups stay closed until the layer above gives
    /// each one its state machine ([`MultiRaftCore::recovered_groups`],
    /// [`MultiRaftCore::create_group`]).
    pub fn new(cfg: CoreConfig, store: CoreStore, seed: u64) -> MultiRaftCore<S> {
        let mut ids = Rng::new(seed.rotate_left(32) ^ cfg.instance);
        let (next_ctx, next_forward) = (first_request_id(&mut ids), first_request_id(&mut ids));
        MultiRaftCore {
            cfg,
            store,
            rng: Rng::new(seed),
            groups: BTreeMap::new(),
            ops: BTreeMap::new(),
            next_op: 1,
            next_ctx,
            next_forward,
            waits: BTreeMap::new(),
            loads: BTreeMap::new(),
            outbox: Vec::new(),
            read_rounds: BTreeSet::new(),
            hold_read_rounds: false,
            frames: Vec::new(),
            done: Vec::new(),
            forwarded: Vec::new(),
            snapshot_data: Vec::new(),
            observations: Vec::new(),
            rolled_back: Vec::new(),
            rolled_counts: (0, 0, 0),
            reassembly: Reassembly::default(),
            received: BTreeMap::new(),
            snapshot_reqs: BTreeMap::new(),
            installing_from: BTreeMap::new(),
            sending: BTreeMap::new(),
            floor_waits: BTreeSet::new(),
            unreachable: BTreeSet::new(),
            heard: BTreeMap::new(),
            next_ping: Nanos::ZERO,
            refused: BTreeSet::new(),
            now: Nanos::ZERO,
            wall_ms: 0,
            clock_rule: ClockGuard::default(),
            aside_turn: 0,
            durable: Vec::new(),
            strays: Vec::new(),
            from_core: false,
        }
    }

    /// Groups of the log that have no state machine yet.
    pub fn recovered_groups(&self) -> Vec<GroupId> {
        self.store.group_ids().into_iter().filter(|g| !self.groups.contains_key(g)).collect()
    }

    /// Creates a group with its state machine, or gives a recovered group its state machine (then
    /// `desc` is not used: the configuration is the one in the log).
    /// A new group whose configuration is not valid is refused with `InvalidConfChange`.
    pub fn create_group(&mut self, desc: GroupDesc, sm: S) -> Result<(), OpError> {
        let g = desc.group;
        if self.groups.contains_key(&g) {
            return Err(OpError::GroupExists);
        }
        if self.store.group(g).is_none() {
            let mut conf = ConfState::voters(&desc.voters);
            conf.learners = desc.learners.clone();
            conf.learners.sort_unstable();
            // What reaches the log has to read back (a node is not voter and learner at once).
            let encoded = encode_conf(&conf);
            decode_conf(&encoded).map_err(|_| OpError::InvalidConfChange)?;
            let t = self
                .store
                .create_group(g, encoded, Durability::Strict)
                .map_err(|_| OpError::Unavailable)?;
            self.waits.insert(t, Wait::Nothing);
        }
        self.open_group(g, sm);
        Ok(())
    }

    fn raft_cfg(&self) -> RaftConfig {
        RaftConfig { id: self.cfg.node_id, ..self.cfg.raft.clone() }
    }

    fn open_group(&mut self, g: GroupId, sm: S) {
        let log = self.store.group(g).expect("a group of the store");
        // The store checks the CRC of what it reads: an unreadable configuration is our own bug.
        let conf = decode_conf(&log.conf).expect("group configuration");
        let single = conf.voters == [self.cfg.node_id] && conf.learners.is_empty();
        let hs = hs_from_store(&log.hard);
        let snap_index = log.snapshot_index;
        let view = GroupStore::new(&self.store, g);
        let mut raw = RawNode::new(self.raft_cfg(), g, conf, hs, &view, &mut self.rng);
        // A single replica leads without waiting for the election timeout.
        if single {
            raw.campaign_now(&view, &mut self.rng);
        }
        let loading = snap_index > 0;
        if loading {
            let (_, token) = self.store.load_snapshot(g).expect("a group with a snapshot index");
            self.loads.insert(token, Load::Restore { group: g });
        }
        let grp = Group {
            raw,
            sm,
            loading,
            stopped: false,
            installing: false,
            parked: None,
            replaced_hs: None,
            commit_cap: None,
            refused_at: Nanos::ZERO,
            own: None,
            own_stale: false,
            last_mark: Hlc { physical: 0, logical: 0 },
            lease_mark: None,
            proposals: BTreeMap::new(),
            reads: BTreeMap::new(),
            persisting: VecDeque::new(),
            snap_index,
            snap_state: snap_index,
            pending_snap: None,
            pending_state: 0,
            terms: TermRuns::default(),
            read_counts: (0, 0),
            closed_out: None,
            closed_in: None,
        };
        self.groups.insert(g, grp);
        self.drive(g);
    }

    /// Removes a group from this core and its log; its operations end with `GroupNotFound`.
    pub fn remove_group(&mut self, g: GroupId) -> Result<(), OpError> {
        if self.groups.remove(&g).is_none() {
            return Err(OpError::GroupNotFound);
        }
        // Refused only if the core stopped: the group is gone from this core anyway.
        if let Some(t) = self.forget_group(g) {
            self.waits.insert(t, Wait::Nothing);
        }
        Ok(())
    }

    /// Deletes group `g` from the store and forgets what this core keeps of it; the ticket of the
    /// deletion, or `None` if the store refused it (the core stopped).
    fn forget_group(&mut self, g: GroupId) -> Option<Ticket> {
        let ticket = self.store.delete_group(g).ok();
        self.received.remove(&g);
        self.snapshot_reqs.remove(&g);
        self.installing_from.remove(&g);
        self.sending.remove(&g);
        self.refused.retain(|(x, _)| *x != g);
        // What was waiting for its disk operations is no longer awaited.
        self.waits.retain(|_, w| w.group() != Some(g));
        self.loads.retain(|_, l| l.group() != g);
        self.fail_group(g, OpError::GroupNotFound);
        ticket
    }

    /// Closes a group on this core and keeps its log, so that `create_group` opens it again as a
    /// recovered group: from its snapshot and its log, as after a restart (005 FR-014, a replica
    /// whose data is rebuilt). Its operations end with `GroupNotFound` and its frames are dropped
    /// until then. Refused with `Busy` while something of the group is in flight with the disk
    /// (a write, a snapshot read or received, its own snapshot waiting for data), which the
    /// reopened group would not know of. A snapshot from the leader parked for its data is not on
    /// disk: it goes with the group, as in a crash (007: no replica will serve its data and no
    /// leader sends a newer one, so the replica, which votes for no one while it waits, opens
    /// again from its log).
    pub fn close_group(&mut self, g: GroupId) -> Result<(), OpError> {
        let grp = self.groups.get(&g).ok_or(OpError::GroupNotFound)?;
        if self.close_refused(g, grp) {
            return Err(OpError::Busy);
        }
        self.groups.remove(&g);
        self.snapshot_reqs.remove(&g);
        self.sending.remove(&g);
        self.fail_group(g, OpError::GroupNotFound);
        Ok(())
    }

    /// Whether `close_group` refuses group `g` (`grp`) now: something of it is in flight with
    /// the disk.
    fn close_refused(&self, g: GroupId, grp: &Group<S>) -> bool {
        grp.loading
            || (grp.installing && grp.parked.is_none())
            || grp.own.is_some()
            || grp.pending_snap.is_some()
            || self.waits.values().any(|w| w.group() == Some(g))
            || self.loads.values().any(|l| l.group() == g)
            || self.received.contains_key(&g)
            || self.installing_from.contains_key(&g)
    }

    // ----- operations -----

    fn new_op(&mut self, kind: OpKind) -> OpId {
        let id = OpId(self.next_op);
        self.next_op += 1;
        self.ops.insert(id, Op { kind, deadline: self.now + self.cfg.op_timeout });
        id
    }

    fn finish(&mut self, id: OpId, r: Result<Done<S::Output>, OpError>) {
        // Every caller ends an operation still in progress (a finished one is in no list).
        self.ops.remove(&id).expect("an operation in progress");
        // Its read indexes and proposals are no longer awaited: an index that arrives later is
        // dropped, and an entry of its that commits is applied without answering anyone.
        for grp in self.groups.values_mut() {
            grp.reads.retain(|_, o| *o != id);
            grp.proposals.retain(|_, (_, o)| *o != id);
        }
        self.done.push((id, r));
    }

    fn fail_group(&mut self, g: GroupId, err: OpError) {
        let ids: Vec<OpId> =
            self.ops.iter().filter(|(_, o)| o.kind.group() == Some(g)).map(|(id, _)| *id).collect();
        for id in ids {
            self.finish(id, Err(err));
        }
    }

    /// Proposes an entry for the group's state machine; the result arrives as `Applied`.
    pub fn propose(&mut self, g: GroupId, payload: &[u8], now: Nanos) -> Result<OpId, OpError> {
        let zero = Hlc { physical: 0, logical: 0 };
        let build = |_| Ok::<_, std::convert::Infallible>(payload.to_vec());
        match self.propose_at(g, zero, now, build) {
            Ok((op, _)) => Ok(op),
            Err(ProposeAt::Op(e)) => Err(e),
            Err(ProposeAt::Build(never)) => match never {},
        }
    }

    /// The mark of a read served now (005 FR-006, FR-009): from the wall clock the clock rule
    /// checked at the last tick; `None` while the clock is out of margin or the mark has to wait.
    pub fn read_mark(&self, now: Nanos) -> Option<Hlc> {
        if !self.clock_rule.in_margin(now) {
            return None;
        }
        match self.cfg.clock.0.now(self.wall_ms) {
            Next::Mark(ts) => Some(ts),
            Next::WaitUntil(_) | Next::AboveBound => None,
        }
    }

    /// Group 0's `ClockReport` (005 FR-006): this node's offset from the median of the voters.
    /// Out of margin, the node hands over its leaderships at once.
    pub fn on_clock_report(&mut self, offset_ms: i64, now: Nanos) {
        self.clock_rule.report(offset_ms, now);
        self.apply_clock_rule(now);
    }

    /// The clock rule (005 FR-006): out of margin, no group of this node campaigns and the
    /// leadership of each one it leads goes to another voter, a different one on each attempt in
    /// case that voter may not lead either.
    fn apply_clock_rule(&mut self, now: Nanos) {
        let ok = self.clock_rule.in_margin(now);
        let me = self.cfg.node_id;
        let ids: Vec<GroupId> = self.groups.keys().copied().collect();
        for g in ids {
            let grp = self.groups.get_mut(&g).expect("listed");
            // A replica that waits for a snapshot lacks the data of its log's start (a range
            // born on a node that did not apply its parent's cut): leading, it would serve what
            // an older life of the range left on its disk. It still votes and follows.
            grp.raw.set_may_lead(ok && !grp.sm.needs_snapshot());
            let view = GroupStore::new(&self.store, g);
            if ok || grp.stopped || grp.raw.status(&view).role != Role::Leader {
                continue;
            }
            let others: Vec<NodeId> =
                grp.raw.conf().voters.iter().copied().filter(|&v| v != me).collect();
            if others.is_empty() {
                continue;
            }
            self.aside_turn = self.aside_turn.wrapping_add(1);
            let to = others[self.aside_turn % others.len()];
            grp.raw.transfer_leader(to, &view);
            self.drive(g);
        }
    }

    /// Hands the leadership of `g` to voter `to` (007 FR-010, the balancer's leader moves): once
    /// `to` is up to date it campaigns at once. `false` if this replica does not lead `g` or `to`
    /// is not another voter of it; a transfer that does not end in time leaves this one leader.
    pub fn transfer_leader(&mut self, g: GroupId, to: NodeId) -> bool {
        let me = self.cfg.node_id;
        let Some(grp) = self.groups.get_mut(&g) else { return false };
        let view = GroupStore::new(&self.store, g);
        let leads = !grp.stopped && grp.raw.status(&view).role == Role::Leader;
        if !leads || to == me || !grp.raw.conf().voters.contains(&to) {
            return false;
        }
        grp.raw.transfer_leader(to, &view);
        self.drive(g);
        true
    }

    /// Proposes an entry whose mark is above `floor` (the reads of its keys, 005 FR-004):
    /// `build` gets the mark and returns the payload, so a write can be evaluated at its own mark.
    /// It runs only on the leader; if it fails, or the entry is too large or the queue is full,
    /// nothing is proposed.
    pub fn propose_at<E>(
        &mut self,
        g: GroupId,
        floor: Hlc,
        now: Nanos,
        build: impl FnOnce(Hlc) -> Result<Vec<u8>, E>,
    ) -> Result<(OpId, Hlc), ProposeAt<E>> {
        self.now = now;
        let max_entry = self.cfg.raft.max_entry_bytes;
        let max_pending = self.cfg.max_pending_proposals;
        let wall = self.wall_ms;
        let clock = self.cfg.clock.0.clone();
        // A clock out of margin assigns no mark (005 FR-006).
        if !self.clock_rule.in_margin(now) {
            return Err(OpError::Overloaded.into());
        }
        let grp = usable(&mut self.groups, g)?;
        let view = GroupStore::new(&self.store, g);
        let st = grp.raw.status(&view);
        if st.role != Role::Leader {
            return Err(OpError::NotLeader { hint: st.leader }.into());
        }
        // With other voters, no mark while no other clock vouches for this one (005 FR-006).
        if grp.raw.conf().voters.len() > 1 && !self.clock_rule.vouched(now) {
            return Err(OpError::Overloaded.into());
        }
        // Above every entry of the group this replica applied or proposed, and above the floor
        // (FR-004). An entry of an earlier term reaches `last_mark` when it is applied, so the
        // caller proposes only once those are applied (the range host waits for it).
        // Faulty variant (005 T017): the clock does not take the group's marks, so after a restart
        // a node whose clock is behind writes below what was written.
        #[cfg(not(teseladb_bug = "hlc_regress_restart"))]
        clock.update(grp.last_mark);
        clock.update(floor);
        let ts = match clock.now(wall) {
            Next::Mark(ts) => ts,
            // The sequence of the millisecond ran out, or the mark would reach the bound on disk
            // before its renewal is written: the proposal comes back later (D-003, FR-003).
            Next::WaitUntil(_) | Next::AboveBound => return Err(OpError::Overloaded.into()),
        };
        let payload = build(ts).map_err(ProposeAt::Build)?;
        let data = envelope::encode(ts, &payload);
        if data.len() > max_entry {
            return Err(OpError::TooLarge.into());
        }
        if grp.proposals.len() >= max_pending {
            return Err(OpError::Overloaded.into());
        }
        let id = grp.raw.propose(Bytes::from_vec(data), &view).map_err(|e| match e {
            ProposeError::NotLeader { hint } => OpError::NotLeader { hint },
            ProposeError::TooLarge => OpError::TooLarge,
            _ => OpError::Overloaded,
        })?;
        grp.last_mark = ts;
        let term = st.hard_state.term;
        let op = self.new_op(OpKind::Propose { group: g });
        self.groups.get_mut(&g).expect("checked").proposals.insert(id.0, (term, op));
        self.drive(g);
        Ok((op, ts))
    }

    /// Proposes a configuration change; the result arrives as `ConfChanged`.
    pub fn propose_conf_change(
        &mut self,
        g: GroupId,
        cc: ConfChange,
        now: Nanos,
    ) -> Result<OpId, OpError> {
        self.now = now;
        let grp = usable(&mut self.groups, g)?;
        let view = GroupStore::new(&self.store, g);
        let id = grp.raw.propose_conf_change(cc, &view).map_err(|e| match e {
            ProposeError::NotLeader { hint } => OpError::NotLeader { hint },
            ProposeError::ConfChangePending => OpError::ConfChangePending,
            ProposeError::NotReady => OpError::NotReady,
            _ => OpError::InvalidConfChange,
        })?;
        let term = grp.raw.status(&view).hard_state.term;
        let op = self.new_op(OpKind::Conf { group: g });
        self.groups.get_mut(&g).expect("checked").proposals.insert(id.0, (term, op));
        self.drive(g);
        Ok(op)
    }

    /// Linearizable read on the leader; it ends as `Read { index }` once the local state machine
    /// applied the commit index of the moment of the read.
    pub fn read(&mut self, g: GroupId, mode: ReadMode, now: Nanos) -> Result<OpId, OpError> {
        self.now = now;
        let grp = usable(&mut self.groups, g)?;
        let st = grp.raw.status(&GroupStore::new(&self.store, g));
        if st.role != Role::Leader {
            // Faulty variant (002 T014): a follower that knows the leader answers from its own
            // state, which may miss writes the leader already acknowledged.
            #[cfg(teseladb_bug = "follower_local_read")]
            if st.leader.0 != 0 {
                let op = self.new_op(OpKind::Read { group: g, retry: false, waiting: None });
                self.finish(op, Ok(Done::Read { index: st.applied }));
                return Ok(op);
            }
            return Err(OpError::NotLeader { hint: st.leader });
        }
        // A clock out of margin serves no lease read (005 FR-006): ReadIndex does not need it.
        let lease = mode == ReadMode::Lease
            && self.clock_rule.in_margin(now)
            && grp.raw.can_lease_read(now);
        if lease {
            grp.read_counts.0 += 1;
        } else {
            grp.read_counts.1 += 1;
        }
        let op = self.new_op(OpKind::Read { group: g, retry: false, waiting: None });
        if lease {
            if self.cfg.observe {
                let ctx = self.next_ctx;
                self.next_ctx += 1;
                self.observe_read(g, ctx, &st, true);
                self.observations.push(Observation::ReadServed { group: g, ctx });
            }
            self.confirmed(op, g, st.hard_state.commit);
        } else {
            self.read_index(op, g);
        }
        Ok(op)
    }

    fn read_index(&mut self, op: OpId, g: GroupId) {
        let ctx = self.next_ctx;
        self.next_ctx += 1;
        let grp = self.groups.get_mut(&g).expect("a read's group");
        let view = GroupStore::new(&self.store, g);
        let st = grp.raw.status(&view);
        let registered = grp.raw.read_index(ctx, &view).is_ok();
        if registered && self.cfg.observe {
            self.observe_read(g, ctx, &st, false);
        }
        let grp = self.groups.get_mut(&g).expect("a read's group");
        self.ops.get_mut(&op).expect("a read in progress").kind =
            OpKind::Read { group: g, retry: !registered, waiting: None };
        if registered {
            grp.reads.insert(ctx, op);
            self.drive(g);
        }
    }

    /// Records a read the leader registers at its commit index (a leader's committed entries are in
    /// its own log unless it applied beyond them).
    fn observe_read(&mut self, g: GroupId, ctx: u64, st: &Status, lease: bool) {
        let index = st.hard_state.commit;
        let index_term = GroupStore::new(&self.store, g).term(index).ok();
        let term = st.hard_state.term;
        self.observations.push(Observation::ReadRegistered {
            group: g,
            ctx,
            index,
            term,
            index_term,
            lease,
        });
    }

    /// What was recorded with [`CoreConfig::observe`] since the last call.
    pub fn take_observations(&mut self) -> Vec<Observation> {
        core::mem::take(&mut self.observations)
    }

    /// The entries this replica drops from its log because a later leader's entries took their
    /// indexes (005 FR-010, D-034: a rollback), handed out once, in log order. None was
    /// committed; the layer above writes their writes to its rollback files and then calls
    /// [`MultiRaftCore::rollback_durable`]: until then the core runs no disk operation, so the
    /// entries stay in the log on disk if the node crashes first.
    pub fn take_rolled_back(&mut self) -> Vec<(GroupId, LogEntry)> {
        self.rolled_counts.1 = self.rolled_counts.0;
        core::mem::take(&mut self.rolled_back)
    }

    /// The rollback files of every entry [`MultiRaftCore::take_rolled_back`] handed out so far
    /// are durable (or held nothing): the disk operations that drop those entries may run.
    pub fn rollback_durable(&mut self) {
        self.rolled_counts.2 = self.rolled_counts.1;
    }

    /// The connection to `node` dropped: its groups stop counting on what was in flight to it (a
    /// snapshot transfer starts again), and the requests forwarded to it end with `Unavailable`,
    /// since no answer comes on a connection that is gone (005 SC-004: a request forwarded to a
    /// leader that died does not wait for its deadline).
    pub fn report_unreachable(&mut self, node: NodeId) {
        self.unreachable.insert(node);
        let lost: Vec<OpId> = self
            .ops
            .iter()
            .filter(|(_, o)| matches!(o.kind, OpKind::Forward { to, .. } if to == node))
            .map(|(id, _)| *id)
            .collect();
        for id in lost {
            self.finish(id, Err(OpError::Unavailable));
        }
        let ids: Vec<GroupId> = self.groups.keys().copied().collect();
        for g in ids {
            let grp = self.groups.get_mut(&g).expect("listed");
            grp.raw.report_unreachable(node);
            // A follower of that node wakes from quiescence: it campaigns if it does not hear its
            // leader in time (an idle group would otherwise wait for a request forever, 007). One
            // that is awake keeps counting: every failed redial of a dead leader reports it again,
            // and starting its count over each time held the election back (005 SC-004).
            let view = GroupStore::new(&self.store, g);
            let st = grp.raw.status(&view);
            if grp.raw.is_quiesced() && st.role != Role::Leader && st.leader == node {
                grp.raw.wake(&view);
            }
            let was_sending = self.sending.get_mut(&g).is_some_and(|s| s.remove(&node).is_some());
            if was_sending {
                grp.raw.report_snapshot(node, false);
            }
            self.drive(g);
        }
    }

    /// Read `op` was confirmed at `index`: it ends now or once `index` is applied.
    fn confirmed(&mut self, op: OpId, g: GroupId, index: u64) {
        let applied = self.groups[&g].raw.status(&GroupStore::new(&self.store, g)).applied;
        if applied >= index {
            self.finish(op, Ok(Done::Read { index }));
        } else {
            self.ops.get_mut(&op).expect("a read in progress").kind =
                OpKind::Read { group: g, retry: false, waiting: Some(index) };
        }
    }

    /// Forwards a request to another node; its answer arrives as `Forwarded`. To a node whose
    /// connection dropped and that sent nothing since, nothing is sent and it ends at once with
    /// `Unreachable`: the caller tries another node or later.
    pub fn forward(&mut self, to: NodeId, body: Bytes, now: Nanos) -> OpId {
        self.now = now;
        let req = self.next_forward;
        self.next_forward += 1;
        let op = self.new_op(OpKind::Forward { req, to });
        if self.unreachable.contains(&to) {
            self.finish(op, Err(OpError::Unreachable));
            return op;
        }
        let deadline_ms = u32::try_from(self.cfg.op_timeout.as_millis()).unwrap_or(u32::MAX);
        self.frames.push((to, PeerFrame::Forward { req, deadline_ms, body }));
        op
    }

    /// Answers a request `to` forwarded to this node.
    pub fn answer_forward(&mut self, to: NodeId, req: u64, body: Bytes) {
        self.frames.push((to, PeerFrame::ForwardResp { req, body }));
    }

    // ----- time, network and disk -----

    /// Global tick of the core; `wall_ms` is the node's wall clock.
    pub fn on_tick(&mut self, now: Nanos, wall_ms: u64) {
        self.now = now;
        self.wall_ms = wall_ms;
        self.clock_rule.tick(now, wall_ms);
        // The HLC is not below the highest time the node read while in margin (005 FR-005,
        // FR-006), so the bound on disk covers that time across a restart: a node whose wall
        // clock went back waits to pass it also after a restart.
        self.cfg.clock.0.update(Hlc { physical: self.clock_rule.peak_ms(), logical: 0 });
        self.apply_clock_rule(now);
        self.keep_lease_clock();
        self.ping_and_suspect(now);
        let ids: Vec<GroupId> = self.groups.keys().copied().collect();
        for g in ids {
            let grp = self.groups.get_mut(&g).expect("group");
            if grp.stopped {
                continue;
            }
            grp.raw.tick(&GroupStore::new(&self.store, g), &mut self.rng, now);
            self.declare_floor(g);
            self.save_pending_snapshot(g);
            self.drive(g);
        }
        self.retry_reads();
        let stale: Vec<(GroupId, NodeId)> = self
            .sending
            .iter()
            .flat_map(|(g, to)| to.iter().map(move |(n, sent)| (*g, *n, *sent)))
            .filter(|(_, _, (at, size))| *at + snapshot_timeout(*size) <= now)
            .map(|(g, n, _)| (g, n))
            .collect();
        for (g, to) in stale {
            self.end_sending(g, to);
            let grp = self.groups.get_mut(&g).expect("removed with its group");
            grp.raw.retry_snapshot(to, &GroupStore::new(&self.store, g));
            self.drive(g);
        }
        // Snapshots refused below a floor go once a snapshot past it is saved.
        let due: Vec<(GroupId, NodeId)> = self
            .refused
            .iter()
            .filter(|(g, _)| {
                self.groups.get(g).is_none_or(|grp| grp.snap_state >= grp.sm.snapshot_floor())
            })
            .copied()
            .collect();
        for (g, to) in due {
            self.refused.remove(&(g, to));
            if let Some(grp) = self.groups.get_mut(&g) {
                grp.raw.retry_snapshot(to, &GroupStore::new(&self.store, g));
                self.drive(g);
            }
        }
        let late: Vec<OpId> =
            self.ops.iter().filter(|(_, o)| o.deadline <= now).map(|(id, _)| *id).collect();
        for id in late {
            self.finish(id, Err(OpError::Timeout));
        }
        self.store.on_tick(now);
    }

    /// Node-level liveness between quiet groups (005 FR-016). A quiescent group's leader sends no
    /// heartbeats, and only a dropped connection used to tell its followers that it was gone; a
    /// partition that drops packets closes no connection, and the group never elected again.
    /// Every [`PING_EVERY`] this core sends each peer of its groups an empty `Heartbeats` frame;
    /// a peer not heard from in [`SILENT_AFTER`] is suspect: the quiescent followers of a group it
    /// leads wake (and campaign if they still do not hear from it), and requests forwarded to it
    /// end at once until it is heard from again.
    fn ping_and_suspect(&mut self, now: Nanos) {
        let me = self.cfg.node_id;
        let mut peers = BTreeSet::new();
        for grp in self.groups.values() {
            let conf = grp.raw.conf();
            peers.extend(conf.voters.iter().chain(&conf.learners).copied().filter(|n| *n != me));
        }
        for p in &peers {
            // A peer starts counting from when this core first has it.
            self.heard.entry(*p).or_insert(now);
        }
        if now >= self.next_ping {
            self.next_ping = now + PING_EVERY;
            let hlc = self.cfg.clock.0.peek();
            for p in &peers {
                let beat = PeerFrame::Heartbeats { wall_ms: self.wall_ms, hlc, beats: Vec::new() };
                self.frames.push((*p, beat));
            }
        }
        let silent: BTreeSet<NodeId> = peers
            .into_iter()
            .filter(|p| self.heard.get(p).is_some_and(|at| *at + SILENT_AFTER <= now))
            .collect();
        if silent.is_empty() {
            return;
        }
        self.unreachable.extend(silent.iter().copied());
        let ids: Vec<GroupId> = self.groups.keys().copied().collect();
        for g in ids {
            let grp = self.groups.get_mut(&g).expect("listed");
            let view = GroupStore::new(&self.store, g);
            let st = grp.raw.status(&view);
            if grp.raw.is_quiesced() && st.role != Role::Leader && silent.contains(&st.leader) {
                grp.raw.wake(&view);
                self.drive(g);
            }
        }
    }

    fn retry_reads(&mut self) {
        // A leader cut off from its quorum confirms no read until it hears from it again: its
        // reads end now, as on a node that does not lead, rather than at their deadline.
        let unconfirmed: Vec<OpId> = self
            .ops
            .iter()
            .filter(|(_, o)| {
                matches!(o.kind, OpKind::Read { group, waiting: None, .. }
                    if self.groups.get(&group).is_some_and(|g| g.raw.lost_quorum()))
            })
            .map(|(id, _)| *id)
            .collect();
        for op in unconfirmed {
            self.finish(op, Err(OpError::NotLeader { hint: NodeId(0) }));
        }
        let again: Vec<(OpId, GroupId)> = self
            .ops
            .iter()
            .filter_map(|(id, o)| match o.kind {
                OpKind::Read { group, retry: true, .. } => Some((*id, group)),
                _ => None,
            })
            .collect();
        for (op, g) in again {
            let grp = &self.groups[&g];
            let st = grp.raw.status(&GroupStore::new(&self.store, g));
            if st.role == Role::Leader {
                self.read_index(op, g);
            } else {
                self.finish(op, Err(OpError::NotLeader { hint: st.leader }));
            }
        }
    }

    /// The clocks of a frame from `from`: its HLC moves this node's clock past it (005 FR-003);
    /// its wall clock is a sample for the clock rule (FR-006) only from a voter of one of this
    /// core's groups, as the median is the voters'.
    fn observe_clock(&mut self, from: NodeId, wall_ms: u64, hlc: Hlc, now: Nanos) {
        // Faulty variant `hlc_regress_restart`: the clock takes no mark it receives, from an
        // entry or from a frame.
        #[cfg(not(teseladb_bug = "hlc_regress_restart"))]
        self.cfg.clock.0.update(hlc);
        #[cfg(teseladb_bug = "hlc_regress_restart")]
        let _ = hlc;
        if self.groups.values().any(|g| g.raw.conf().voters.contains(&from)) {
            self.clock_rule.observe(from, wall_ms, self.wall_ms, now);
        }
    }

    /// A frame from another node.
    pub fn on_peer_message(&mut self, from: NodeId, frame: PeerFrame, now: Nanos) {
        self.now = now;
        self.unreachable.remove(&from);
        self.heard.insert(from, now);
        let me = self.cfg.node_id;
        match frame {
            PeerFrame::Raft { wall_ms, hlc, msgs } => {
                self.observe_clock(from, wall_ms, hlc, now);
                self.step_all(from, (wall_ms, hlc), msgs);
            }
            PeerFrame::Heartbeats { wall_ms, hlc, beats } => {
                self.observe_clock(from, wall_ms, hlc, now);
                self.hear_closed(&beats);
                self.step_all(from, (wall_ms, hlc), expand_heartbeats(from, me, &beats));
            }
            PeerFrame::HeartbeatResps { wall_ms, hlc, resps } => {
                self.observe_clock(from, wall_ms, hlc, now);
                self.step_all(from, (wall_ms, hlc), expand_heartbeat_resps(from, me, &resps));
            }
            PeerFrame::SnapshotChunk(c) if self.stray(c.group) => {
                self.keep_stray(from, PeerFrame::SnapshotChunk(c));
            }
            PeerFrame::SnapshotAck { group, index, offset, ok } if self.stray(group) => {
                self.keep_stray(from, PeerFrame::SnapshotAck { group, index, offset, ok });
            }
            PeerFrame::SnapshotChunk(c) => {
                let (group, index, offset) = (c.group, c.index, c.offset);
                match self.reassembly.push(c) {
                    Ok(None) => {}
                    Ok(Some(r)) => {
                        self.received.insert(group, (from, r));
                        self.try_install(group);
                    }
                    Err(_) => {
                        let ack = PeerFrame::SnapshotAck { group, index, offset, ok: false };
                        self.frames.push((from, ack));
                    }
                }
            }
            PeerFrame::SnapshotAck { group, index, ok, .. } => {
                // A request for a snapshot (index 0) ends no transfer: one in flight keeps its
                // timeout, so that one lost on its way (the follower restarted) goes again, since
                // Raft answers the request with nothing while it counts one in flight (007 T016).
                if ok || index != 0 {
                    self.end_sending(group, from);
                }
                // A follower that needs a snapshot (index 0) from a leader whose log was never
                // compacted: the leader takes one now, and sends it when it is saved (007 T016:
                // a range born from a split, with a replica that never applied the cut).
                let leads = self.groups.get(&group).is_some_and(|grp| {
                    grp.raw.status(&GroupStore::new(&self.store, group)).role == Role::Leader
                });
                if !ok && index == 0 && leads && self.snapshot_index(group) == Some(0) {
                    self.snapshot_now(group);
                    self.floor_waits.insert(group);
                }
                if let Some(grp) = self.groups.get_mut(&group) {
                    // A failed transfer goes again at once: the follower may be waiting for the
                    // data of an older snapshot and answer nothing else.
                    // Index 0: the follower waits for a snapshot and cannot use its log.
                    let view = GroupStore::new(&self.store, group);
                    match (ok, index) {
                        (true, _) => grp.raw.snapshot_installed(from, index),
                        (false, 0) => grp.raw.request_snapshot(from, &view),
                        (false, _) => grp.raw.retry_snapshot(from, &view),
                    }
                    self.drive(group);
                }
            }
            PeerFrame::Forward { req, deadline_ms, body } => {
                self.forwarded.push(Forwarded { from, req, deadline_ms, body });
            }
            PeerFrame::ForwardResp { req, body } => {
                let op = self.ops.iter().find_map(|(id, o)| match o.kind {
                    OpKind::Forward { req: r, .. } if r == req => Some(*id),
                    _ => None,
                });
                if let Some(op) = op {
                    self.finish(op, Ok(Done::Forwarded(body)));
                }
            }
        }
    }

    /// Whether a frame of group `g` goes to another core of the node: this core does not run
    /// the group, and the frame did not come from another core.
    fn stray(&self, g: GroupId) -> bool {
        !self.from_core && !self.groups.contains_key(&g)
    }

    /// Keeps a stray frame for the layer above; past [`MAX_STRAYS`] untaken ones it is dropped,
    /// as the network may drop it.
    fn keep_stray(&mut self, from: NodeId, frame: PeerFrame) {
        if self.strays.len() < MAX_STRAYS {
            self.strays.push((from, frame));
        }
    }

    /// Whether group `g` is halted with a state it can hand over (007 FR-011): its state machine
    /// has its state (not loading, nor waiting for the data of its own snapshot) and that state's
    /// index is in its log. One that cannot be handed over yet goes on taking the leader's
    /// messages, a snapshot among them.
    pub(crate) fn hands_over(&self, g: GroupId) -> bool {
        let (Some(grp), Some(log)) = (self.groups.get(&g), self.store.group(g)) else {
            return false;
        };
        let index = grp.sm.snapshot_state_index();
        grp.sm.halted()
            && !grp.loading
            && grp.own.is_none()
            && (log.snapshot_index..=log.last_index()).contains(&index)
    }

    fn step_all(
        &mut self,
        from: NodeId,
        (wall_ms, hlc): (u64, Hlc),
        batch: Vec<(GroupId, Message)>,
    ) {
        let (strays, batch): (Vec<_>, Vec<_>) =
            batch.into_iter().partition(|(g, _)| self.stray(*g));
        if !strays.is_empty() {
            self.keep_stray(from, PeerFrame::Raft { wall_ms, hlc, msgs: strays });
        }
        for (g, m) in batch {
            // A replica that moved to another core keeps the log and the state it hands over
            // (007 FR-011): it takes no more entries, snapshots or votes and does not campaign,
            // so that nothing of it is in flight with the disk when its core closes it. Its copy
            // there does all that; the answers still reach a halted leader handing its role on.
            let to_halted = matches!(
                m.ty,
                MessageType::AppendReq
                    | MessageType::Heartbeat
                    | MessageType::SnapshotReq
                    | MessageType::VoteReq
                    | MessageType::PreVoteReq
                    | MessageType::TimeoutNow
            );
            if to_halted && self.hands_over(g) {
                continue;
            }
            if m.ty == MessageType::SnapshotReq {
                // The data travels apart, in chunks.
                self.snapshot_reqs.insert(g, m);
                self.try_install(g);
            } else {
                self.step_raft(g, m);
            }
        }
    }

    fn step_raft(&mut self, g: GroupId, m: Message) {
        let Some(grp) = self.groups.get_mut(&g) else { return };
        if grp.stopped {
            return;
        }
        // A replica whose snapshot waits for its data takes nothing but a newer snapshot: an
        // entry appended past the parked one would leave the log behind it with a gap if a newer
        // snapshot replaces it, and the replica cannot answer anything until its data is here.
        if grp.parked.is_some() && m.ty != MessageType::SnapshotReq {
            return;
        }
        grp.sync_awaiting();
        // Its own snapshot's data will not come: each round of the leader's entries asks for the
        // leader's snapshot, and the entries are still taken, so that the group commits.
        if grp.own_stale
            && m.ty == MessageType::AppendReq
            && self.now >= grp.refused_at + NEEDS_SNAPSHOT_EVERY
        {
            grp.refused_at = self.now;
            let ask = PeerFrame::SnapshotAck { group: g, index: 0, offset: 0, ok: false };
            self.frames.push((m.from, ask));
        }
        if grp.sm.needs_snapshot() && m.ty == MessageType::AppendReq {
            // Once per round of the leader's: each refusal makes it try again at once.
            if self.now >= grp.refused_at + NEEDS_SNAPSHOT_EVERY {
                grp.refused_at = self.now;
                let ack = PeerFrame::SnapshotAck { group: g, index: 0, offset: 0, ok: false };
                self.frames.push((m.from, ack));
            }
            return;
        }
        // A misaddressed message or one from ourselves is dropped: it does not belong to this
        // group's conversation.
        let _ = grp.raw.step(m, &GroupStore::new(&self.store, g), &mut self.rng);
        self.drive(g);
    }

    /// Steps a `SnapshotReq` once its data arrived whole.
    fn try_install(&mut self, g: GroupId) {
        let (Some(m), Some((_, r))) = (self.snapshot_reqs.get(&g), self.received.get(&g)) else {
            return;
        };
        if m.index != r.index {
            // Keep the newer one: the older transfer will not be completed.
            if r.index < m.index {
                self.received.remove(&g);
            } else {
                self.snapshot_reqs.remove(&g);
            }
            return;
        }
        let mut m = self.snapshot_reqs.remove(&g).expect("checked");
        let (from, r) = self.received.remove(&g).expect("checked");
        // A newer snapshot replaces one still waiting for its data, which may never come (007:
        // no replica serves a snapshot from before the range's last split). The parked `Ready`
        // is dropped but for its hard state, which goes with the next one.
        let parked = self.groups.get(&g).and_then(|grp| grp.parked.as_ref());
        if parked.is_some_and(|(s, _)| s.meta.index < m.index) {
            let grp = self.groups.get_mut(&g).expect("it parked a snapshot");
            let (_, old) = grp.parked.take().expect("checked");
            grp.installing = false;
            grp.replaced_hs = old.hs;
        }
        self.installing_from.insert(g, (from, r.data.len() as u64));
        m.snapshot = m.snapshot.map(|s| Snapshot { data: r.data, ..s });
        self.step_raft(g, m);
    }

    /// Next disk operation to run; none while a rollback file is not durable yet.
    pub fn poll_io(&mut self, now: Nanos) -> Option<IoOp> {
        self.now = now;
        if self.rollback_pending() {
            return None;
        }
        self.store.poll_io(now)
    }

    /// Result of a disk operation.
    pub fn on_io_done(&mut self, done: IoDone, now: Nanos) {
        self.now = now;
        let c = self.store.on_io_done(done, now);
        let mut ready = BTreeSet::new();
        for (t, r) in c.tickets {
            // A removed group's tickets were forgotten with it.
            let Some(w) = self.waits.remove(&t) else { continue };
            // An error stops the store (fail-stop): nothing that depends on it is acknowledged.
            if r.is_err() {
                continue;
            }
            match w {
                Wait::Persist { group, number, last, last_term } => {
                    let grp = self.groups.get_mut(&group).expect("its waits go with it");
                    // The store completes a group's tickets in order.
                    let (_, after) = grp.persisting.pop_front().expect("the ticket in flight");
                    grp.raw.on_persisted(
                        number,
                        last,
                        last_term,
                        &GroupStore::new(&self.store, group),
                    );
                    self.outbox.extend(after.into_iter().map(|m| (group, m)));
                    ready.insert(group);
                }
                Wait::Install { group, deferred: d } => {
                    let grp = self.groups.get_mut(&group).expect("its waits go with it");
                    // The snapshot is durable: the commit of its `Ready` may go to disk now.
                    (grp.installing, grp.commit_cap) = (false, None);
                    let (index, (node, total)) = (d.index, d.from);
                    if self.persist(group, d.number, d.entries, d.hs, d.after) {
                        let ack = PeerFrame::SnapshotAck { group, index, offset: total, ok: true };
                        self.frames.push((node, ack));
                    }
                    ready.insert(group);
                }
                Wait::Import { group, hard, entries } => self.imported(group, hard, &entries),
                Wait::Note(d) => self.durable.push(d),
                Wait::Nothing | Wait::Saved { .. } => {}
            }
        }
        for (token, r) in c.snapshots {
            match (self.loads.remove(&token), r) {
                (Some(Load::Restore { group }), Ok((meta, data))) => {
                    if self.missing_data(group, &data) {
                        // Its data comes another way (a range cleared before a crash): the group
                        // asks for it and meanwhile votes, appends and commits, but applies
                        // nothing; the leader's snapshot may come first (007 T016).
                        self.snapshot_data.push((group, meta.index));
                        let grp = self.groups.get_mut(&group).expect("loading");
                        grp.own = Some((meta.index, data));
                        grp.loading = false;
                        ready.insert(group);
                    } else if self.restore(group, &data) {
                        self.restored_own(group);
                        ready.insert(group);
                    }
                }
                (Some(Load::Restore { group }), Err(_)) => self.stop(group),
                // Read before the state machine's floor passed it: it does not go (see
                // `start_snapshot_send`).
                (Some(Load::Send { group, to }), Ok((meta, _)))
                    if self.groups[&group].state_of(meta.index)
                        < self.groups[&group].sm.snapshot_floor() =>
                {
                    self.end_sending(group, to);
                    let grp = self.groups.get_mut(&group).expect("its loads go with it");
                    grp.raw.report_snapshot(to, false);
                    self.refused.insert((group, to));
                }
                (Some(Load::Send { group, to }), Ok((meta, data))) => {
                    if let Some(sent) = self.sending.get_mut(&group).and_then(|s| s.get_mut(&to)) {
                        sent.1 = data.len() as u64;
                    }
                    for c in split(group, meta.index, meta.term, &data) {
                        self.frames.push((to, PeerFrame::SnapshotChunk(c)));
                    }
                }
                (Some(Load::Send { group, to }), Err(_)) => {
                    self.end_sending(group, to);
                    let grp = self.groups.get_mut(&group).expect("its loads go with it");
                    grp.raw.report_snapshot(to, false);
                }
                // A removed group's reads were forgotten with it.
                (None, _) => {}
            }
        }
        for g in ready {
            self.drive(g);
        }
    }

    /// Next deadline of the store.
    pub fn next_deadline(&self) -> Option<Nanos> {
        // While a rollback file is not durable no disk operation runs: nothing is due.
        if self.rollback_pending() {
            return None;
        }
        self.store.next_deadline()
    }

    /// Whether a rollback file is not durable yet, so that no disk operation runs (005 FR-010).
    pub fn rollback_pending(&self) -> bool {
        self.rolled_counts.2 < self.rolled_counts.0
    }

    /// What the core loop has to do: frames (Raft messages grouped per node), operations that
    /// ended and forwarded requests.
    pub fn drain_actions(&mut self) -> Actions<S::Output> {
        if !self.hold_read_rounds {
            self.flush_read_rounds();
        }
        let hlc = self.cfg.clock.0.peek();
        // A received mark passed the bound on disk: no frame leaves, an answer to a leader
        // included, until a bound above it is written (005 FR-003, research R13); a voter that
        // restarted would otherwise carry less than it acknowledged. They go at the next call.
        let mut send = Vec::new();
        if self.cfg.clock.0.covered() {
            let out = core::mem::take(&mut self.outbox);
            let groups = &self.groups;
            let closed = |g: GroupId, term: u64| {
                let (t, mark, index) = groups.get(&g)?.closed_out?;
                (t == term).then_some((mark, index))
            };
            send = coalesce(self.cfg.node_id, out, self.wall_ms, hlc, &closed);
            send.append(&mut self.frames);
        }
        Actions {
            send,
            done: core::mem::take(&mut self.done),
            forwarded: core::mem::take(&mut self.forwarded),
            snapshot_data: core::mem::take(&mut self.snapshot_data),
        }
    }

    // ----- Raft -----

    /// A replica of `g` won an election: with a read cache, the node's clock goes to its minimum
    /// now, before the messages of the term leave (005 FR-003, FR-005, research R12). The
    /// previous leader read until its lease ended, at most a lease after the last round its voters
    /// acknowledged, and at a mark no higher than its wall clock then plus that lease: each round
    /// carried its clock, at least its wall clock (`keep_lease_clock`), and the voters handed it
    /// on. So the minimum is the millisecond after a lease above the clock this node heard
    /// (`read_cache_minimum`), and at least the margin above its own wall clock. It does not run
    /// away: a new term comes only after the previous lease ended, so the wall clocks advance at
    /// least a lease between two elections, and the cluster time stays within the margin plus a
    /// lease and a millisecond of the wall time (seed 4086 of `splits`; seed 730 of `range-docs`
    /// for the reads at the wall clock late in a term).
    fn raise_to_read_cache(&mut self, g: GroupId) {
        let grp = self.groups.get_mut(&g).expect("group");
        let Some(margin) = grp.sm.read_cache_margin_ms() else { return };
        let clock = &self.cfg.clock.0;
        let lease_ms = self.cfg.raft.lease_dur.0.div_ceil(1_000_000);
        let minimum = read_cache_minimum(clock.peek(), self.wall_ms, lease_ms, margin);
        // The read cache starts at the lease mark plus the margin: at the minimum.
        let lease = Hlc { physical: minimum.physical - margin, logical: 0 };
        // Faulty variant (005 T017): the clock stays where it was; with the write's read floor
        // gone too (see `RangeHost::propose`), nothing keeps the new leader's writes above the
        // reads the previous one served.
        #[cfg(not(teseladb_bug = "write_below_read"))]
        clock.update(minimum);
        let term = grp.raw.status(&GroupStore::new(&self.store, g)).hard_state.term;
        grp.lease_mark = Some((term, lease));
    }

    /// While this node leads a group with a read cache, its clock is at least its wall clock, so
    /// the frames of its lease rounds carry that wall clock to its voters (005 FR-005, research
    /// R12): its reads until the lease ends stay within a lease of what they hand to the next
    /// leader.
    fn keep_lease_clock(&mut self) {
        // Faulty variant (005 T017): see `raise_to_read_cache`.
        if cfg!(teseladb_bug = "write_below_read") {
            return;
        }
        let leads = self
            .groups
            .values()
            .any(|grp| grp.raw.role() == Role::Leader && grp.sm.read_cache_margin_ms().is_some());
        if leads {
            self.cfg.clock.0.update(Hlc { physical: self.wall_ms, logical: 0 });
        }
    }

    fn drive(&mut self, g: GroupId) {
        for _ in 0..1000 {
            let grp = self.groups.get_mut(&g).expect("group");
            grp.sync_awaiting();
            if grp.stopped || grp.installing || grp.loading || !grp.raw.has_ready() {
                break;
            }
            let rd = grp.raw.ready(&GroupStore::new(&self.store, g), &mut self.rng);
            self.handle_ready(g, rd);
        }
        if self.groups.get(&g).is_some_and(|grp| grp.raw.read_round_due()) {
            self.read_rounds.insert(g);
        }
    }

    /// The caller sends the heartbeat rounds of the ReadIndex reads itself, once per turn, with
    /// [`MultiRaftCore::flush_read_rounds`]: the reads of every event of the turn share one round
    /// per group. Off by default: each [`MultiRaftCore::drain_actions`] sends them.
    pub fn hold_read_rounds(&mut self, hold: bool) {
        self.hold_read_rounds = hold;
    }

    /// Whether ReadIndex reads wait for a round that has not been sent.
    pub fn read_rounds_due(&self) -> bool {
        !self.read_rounds.is_empty()
    }

    /// Sends one heartbeat round per group for the ReadIndex reads registered since its last
    /// round (005 FR-009): each read was registered at the commit index when it arrived and is
    /// answered once a majority confirms a round sent after it.
    pub fn flush_read_rounds(&mut self) {
        for g in core::mem::take(&mut self.read_rounds) {
            let Some(grp) = self.groups.get_mut(&g) else { continue };
            if grp.stopped || grp.installing || grp.loading {
                continue;
            }
            grp.raw.send_read_round(&GroupStore::new(&self.store, g));
            self.drive(g);
        }
    }

    fn handle_ready(&mut self, g: GroupId, rd: Ready) {
        if rd.soft_state.is_some_and(|s| s.role == Role::Leader) {
            self.raise_to_read_cache(g);
        }
        if self.cfg.observe {
            if rd.soft_state.is_some_and(|s| s.role == Role::Leader) {
                let grp = &self.groups[&g];
                let term = grp.raw.status(&GroupStore::new(&self.store, g)).hard_state.term;
                self.observations.push(Observation::Elected { group: g, term });
            }
            if let Some(hs) = &rd.hard_state {
                self.observations.push(Observation::Committed { group: g, commit: hs.commit });
            }
        }
        // A replica that stops leading cannot confirm the reads it registered: they end now (the
        // caller retries at the new leader) instead of at their deadline.
        if let Some(s) = rd.soft_state.filter(|s| s.role != Role::Leader) {
            let grp = self.groups.get_mut(&g).expect("group");
            for op in std::mem::take(&mut grp.reads).into_values() {
                self.finish(op, Err(OpError::NotLeader { hint: s.leader }));
            }
        }
        let snapshot_to: Vec<NodeId> =
            rd.messages.iter().filter(|m| m.ty == MessageType::SnapshotReq).map(|m| m.to).collect();
        let refused: Vec<NodeId> =
            snapshot_to.into_iter().filter(|to| !self.start_snapshot_send(g, *to)).collect();
        let sent = |m: &Message| m.ty != MessageType::SnapshotReq || !refused.contains(&m.to);
        self.outbox.extend(rd.messages.into_iter().filter(sent).map(|m| (g, m)));
        let entries: Vec<LogEntry> = rd.entries_to_persist.iter().map(to_store).collect();
        let replaced = self.groups.get_mut(&g).expect("group").replaced_hs.take();
        let hs = rd.hard_state.as_ref().map(hs_to_store).or(replaced);
        if rd.snapshot_to_apply.is_some() {
            let on_disk = self.store.hard_state(g).map_or(0, |h| h.commit);
            self.groups.get_mut(&g).expect("group").commit_cap.get_or_insert(on_disk);
        }
        let after = rd.messages_after_persist;
        let ok = match rd.snapshot_to_apply {
            // A snapshot from the leader: the state is restored now and the rest of the `Ready`
            // is persisted once the snapshot is durable.
            Some(snap) => {
                let from = self.installing_from.remove(&g).expect("a snapshot comes whole");
                let d = Deferred { number: rd.number, entries, hs, after, from, index: 0 };
                self.install(g, snap, d)
            }
            None => self.persist(g, rd.number, entries, hs, after),
        };
        if !ok {
            return;
        }
        let mut last = None;
        for e in &rd.committed {
            // A halted state machine is handed nothing more (007 FR-011): the entries stay in the
            // log, which goes with the group to another core.
            if self.groups[&g].sm.halted() {
                break;
            }
            if !self.apply(g, e) {
                return;
            }
            last = Some(e);
        }
        if let Some(e) = last {
            self.groups.get_mut(&g).expect("group").raw.on_applied(e.index);
            self.applied_reads(g, e.index);
            self.note_log_floor(g);
            self.declare_floor(g);
            self.maybe_snapshot(g, e.index, e.term);
        }
        for rs in rd.read_states {
            // Only the contexts this core gave Raft arrive; one whose read already ended is dropped.
            if let Some(op) = self.groups.get_mut(&g).expect("group").reads.remove(&rs.ctx) {
                if self.cfg.observe {
                    self.observations.push(Observation::ReadServed { group: g, ctx: rs.ctx });
                }
                self.confirmed(op, g, rs.index);
            }
        }
    }

    /// Reads of `g` that were waiting for an index up to `applied`.
    fn applied_reads(&mut self, g: GroupId, applied: u64) {
        let ready: Vec<(OpId, u64)> = self
            .ops
            .iter()
            .filter_map(|(id, o)| match o.kind {
                OpKind::Read { group, waiting: Some(i), .. } if group == g && i <= applied => {
                    Some((*id, i))
                }
                _ => None,
            })
            .collect();
        for (op, index) in ready {
            self.finish(op, Ok(Done::Read { index }));
        }
    }

    /// Keeps the entries of `g`'s log that appending `entries` drops (the store discards its
    /// suffix from the first one): those another leader's entry replaces at their index, and those
    /// after the last new one. Only proposals count: neither a leader's `Noop` nor a
    /// configuration change carries a write.
    fn note_overwritten(&mut self, g: GroupId, entries: &[LogEntry]) {
        let Some(first) = entries.first() else { return };
        let log = self.store.group(g).expect("a group of this core");
        let last = log.last_index();
        if first.index > last {
            return;
        }
        // Appended entries are above the snapshot, so the log has those they replace.
        let old = log.entries(first.index, last + 1, u64::MAX).expect("entries above the snapshot");
        let replaced = old.iter().filter(|o| {
            let new = o.index.checked_sub(first.index).and_then(|k| entries.get(k as usize));
            new.is_none_or(|n| n.term != o.term)
        });
        let writes = replaced.filter(|o| o.kind == WalKind::Normal && !o.data.is_empty());
        let writes = writes.cloned().collect();
        self.note_rolled(g, writes);
    }

    /// Keeps the entries of `g`'s log up to the installed snapshot's `index` whose term is not the
    /// committed log's term at their index, as the snapshot's terms give it: the store drops them
    /// when the snapshot compacts the log, and they were never committed. Those whose term
    /// matches are the committed entries. Entries above `index` stay in the log until a later
    /// leader's entries replace them ([`MultiRaftCore::note_overwritten`]).
    fn note_superseded(&mut self, g: GroupId, index: u64) {
        let log = self.store.group(g).expect("a group of this core");
        let last = log.last_index().min(index);
        let old = log.entries(log.first_index(), last + 1, u64::MAX).expect("the log's entries");
        let terms = &self.groups[&g].terms;
        let superseded = old.iter().filter(|o| {
            o.kind == WalKind::Normal
                && !o.data.is_empty()
                && terms.term_of(o.index).is_some_and(|t| t != o.term)
        });
        let superseded = superseded.cloned().collect();
        self.note_rolled(g, superseded);
    }

    /// Hands `entries` of `g` to [`MultiRaftCore::take_rolled_back`].
    fn note_rolled(&mut self, g: GroupId, entries: Vec<LogEntry>) {
        self.rolled_counts.0 += entries.len() as u64;
        self.rolled_back.extend(entries.into_iter().map(|o| (g, o)));
    }

    /// Persists a `Ready`'s entries and hard state; `after` leaves once they are durable. `false`
    /// if the store refused them (the group stops).
    fn persist(
        &mut self,
        g: GroupId,
        number: u64,
        entries: Vec<LogEntry>,
        hs: Option<teseladb_raftlog::HardState>,
        after: Vec<Message>,
    ) -> bool {
        // Faulty variant (005 T033): the entries another leader's log replaces leave no record.
        #[cfg(not(teseladb_bug = "rollback_unrecorded"))]
        self.note_overwritten(g, &entries);
        let grp = self.groups.get_mut(&g).expect("group");
        if !entries.is_empty() || hs.is_some() {
            let hs = match grp.commit_cap {
                Some(cap) => {
                    hs.map(|h| teseladb_raftlog::HardState { commit: h.commit.min(cap), ..h })
                }
                None => hs,
            };
            let Ok(t) = self.store.append_raft(g, &entries, hs, Durability::Strict) else {
                // Disk full or store stopped: nothing is persisted or answered, and the group
                // stops before an answer that depends on these entries can leave.
                self.stop(g);
                return false;
            };
            let log = self.store.group(g).expect("group");
            let last = log.last_index();
            let last_term = log.term(last).unwrap_or(0);
            grp.persisting.push_back((t, after));
            self.waits.insert(t, Wait::Persist { group: g, number, last, last_term });
        } else if let Some((_, queued)) = grp.persisting.back_mut() {
            // Answers that depend on what is still being persisted.
            queued.extend(after);
        } else {
            let log = self.store.group(g).expect("group");
            let last = log.last_index();
            let last_term = log.term(last).unwrap_or(0);
            grp.raw.on_persisted(number, last, last_term, &GroupStore::new(&self.store, g));
            self.outbox.extend(after.into_iter().map(|m| (g, m)));
        }
        true
    }

    /// Applies a committed entry; `false` if the group stopped.
    fn apply(&mut self, g: GroupId, e: &Entry) -> bool {
        // Every mark the node receives moves its clock past it (005 FR-003); the faulty variant
        // `hlc_regress_restart` skips it.
        #[cfg(not(teseladb_bug = "hlc_regress_restart"))]
        let clock = self.cfg.clock.0.clone();
        if self.cfg.observe {
            let crc = teseladb_hash::crc32c(&e.data);
            self.observations.push(Observation::Applied {
                group: g,
                index: e.index,
                term: e.term,
                crc,
            });
        }
        let grp = self.groups.get_mut(&g).expect("group");
        // If another leader's entry ended up at that index, the proposal was lost.
        let op = match grp.proposals.remove(&e.index) {
            Some((t, op)) if t == e.term => Some(op),
            Some((_, op)) => {
                self.finish(op, Err(OpError::ProposalDropped));
                None
            }
            None => None,
        };
        let grp = self.groups.get_mut(&g).expect("group");
        if e.kind == EntryKind::ConfChange {
            // Raft applied the change when it handed the entry out.
            grp.sm.skip(e.index);
            if let Some(op) = op {
                let conf = grp.raw.conf().clone();
                self.finish(op, Ok(Done::ConfChanged(conf)));
            }
            return true;
        }
        if e.data.is_empty() {
            // The `Noop` of a new leader.
            grp.sm.skip(e.index);
            return true;
        }
        let applied = envelope::decode(&e.data).map_err(|_| ()).and_then(|env| {
            grp.last_mark = grp.last_mark.max(env.ts);
            #[cfg(not(teseladb_bug = "hlc_regress_restart"))]
            clock.update(env.ts);
            let ctx = ApplyCtx { group: g, index: e.index, term: e.term, ts: env.ts };
            grp.sm.apply(env.payload, &ctx).map_err(|_| ())
        });
        match applied {
            Ok(out) => {
                if let Some(op) = op {
                    self.finish(op, Ok(Done::Applied(out)));
                }
                true
            }
            // An entry this version cannot apply: the group stops instead of guessing.
            Err(()) => {
                self.stop(g);
                false
            }
        }
    }

    /// Stops a group: no ticks, messages, proposals or reads until the node restarts.
    fn stop(&mut self, g: GroupId) {
        self.groups.get_mut(&g).expect("a group of this core").stopped = true;
        self.fail_group(g, OpError::Unavailable);
    }

    fn end_sending(&mut self, g: GroupId, to: NodeId) {
        if let Some(s) = self.sending.get_mut(&g) {
            s.remove(&to);
        }
    }

    // ----- snapshots -----

    /// Takes a snapshot at the last applied entry (`index`, `term`) once enough entries were applied
    /// past the latest one; the store compacts the log when it is durable.
    /// The log learns what a state machine that holds it still needs (004 FR-011): before a
    /// snapshot, so that one never covers entries it has not made durable.
    fn declare_floor(&mut self, g: GroupId) {
        if S::HOLDS_LOG {
            let floor = self.groups.get(&g).expect("group").sm.release_floor();
            self.store.set_release_floor(g, ReleaseProvider::Lsm, floor);
        }
    }

    /// The snapshot is the state at `index`. A state machine that holds the log has that entry in
    /// memory still (under continuous writes the floor never reaches the entry just applied): the
    /// snapshot waits, taken at `index`, until the floor passes it, and no new one is taken
    /// meanwhile.
    /// A state machine's snapshot floor above the latest snapshot, waiting or saved, takes one
    /// now: the one that waits is replaced.
    fn maybe_snapshot(&mut self, g: GroupId, index: u64, term: u64) {
        self.save_pending_snapshot(g);
        let grp = self.groups.get_mut(&g).expect("group");
        let latest = grp.pending_snap.as_ref().map_or(grp.snap_state, |_| grp.pending_state);
        let below_floor = latest < grp.sm.snapshot_floor();
        let due = grp.pending_snap.is_none() && index >= grp.snap_index + self.cfg.snapshot_entries;
        if below_floor || due {
            self.take_snapshot(g, index, term);
        }
    }

    /// Takes the snapshot of `g` at its applied entry `index` of term `term`.
    fn take_snapshot(&mut self, g: GroupId, index: u64, term: u64) {
        let grp = self.groups.get_mut(&g).expect("group");
        // The terms of the entries since the last snapshot join those below it; the log has them
        // all, since it is only compacted up to a snapshot.
        let mut terms = grp.terms.clone();
        let log = self.store.group(g).expect("group");
        terms.extend(index, |i| log.term(i).expect("an entry above the last snapshot"));
        let meta = SnapshotMeta { index, term, conf: encode_conf(grp.raw.conf()) };
        let data = wrap_snapshot(grp.last_mark.physical, &terms, &grp.sm.snapshot());
        grp.terms = terms;
        grp.pending_state = grp.sm.snapshot_state_index().max(index);
        grp.pending_snap = Some((meta, data));
        self.save_pending_snapshot(g);
    }

    /// Saves the snapshot of `g` that waits for the release floor once the floor reaches it; one
    /// that an installed snapshot passed is dropped.
    fn save_pending_snapshot(&mut self, g: GroupId) {
        let floor = self.store.release_floor_of(g);
        let grp = self.groups.get_mut(&g).expect("group");
        let Some((meta, _)) = &grp.pending_snap else { return };
        if meta.index > grp.snap_index && floor.is_some_and(|f| meta.index > f) {
            return;
        }
        let (meta, data) = grp.pending_snap.take().expect("checked");
        self.floor_waits.remove(&g);
        if meta.index <= grp.snap_index {
            return;
        }
        let index = meta.index;
        // Refused only if the store stopped or a newer one is being written: a later applied
        // entry takes another.
        if let Ok(t) = self.store.save_snapshot(g, meta, data) {
            grp.snap_index = index;
            grp.snap_state = grp.pending_state;
            self.waits.insert(t, Wait::Saved { group: g });
        }
    }

    /// Hands Raft the state machine's log floor, which an applied entry or a restored snapshot
    /// may have moved.
    fn note_log_floor(&mut self, g: GroupId) {
        let grp = self.groups.get_mut(&g).expect("group");
        grp.raw.set_log_floor(grp.sm.log_floor());
    }

    /// Restores a group's state machine from a snapshot file; `false` if it stopped.
    fn restore(&mut self, g: GroupId, data: &[u8]) -> bool {
        let grp = self.groups.get_mut(&g).expect("a group of this core");
        let restored = unwrap_snapshot(data)
            .map_err(|_| ())
            .and_then(|(ms, terms, sm)| grp.sm.restore(sm).map(|()| (ms, terms)).map_err(|_| ()));
        match restored {
            Ok((ms, terms)) => {
                grp.terms = terms;
                // The snapshot restored is the latest one (at open, or the one being installed).
                grp.snap_state = grp.sm.snapshot_state_index();
                grp.raw.set_log_floor(grp.sm.log_floor());
                // The snapshot keeps the milliseconds: the last mark of that millisecond bounds
                // every mark of the entries it covers.
                let bound = Hlc { physical: ms, logical: (1 << LOGICAL_BITS) - 1 };
                grp.last_mark = grp.last_mark.max(bound);
                self.cfg.clock.0.update(bound);
                true
            }
            // A snapshot this version cannot restore: the group stops instead of guessing.
            Err(()) => {
                self.stop(g);
                false
            }
        }
    }

    /// The state machine of `g` is restored from its own snapshot: the entries held back go out.
    fn restored_own(&mut self, g: GroupId) {
        let grp = self.groups.get_mut(&g).expect("restored");
        grp.loading = false;
        grp.own_stale = false;
    }

    fn install(&mut self, g: GroupId, snap: Snapshot, mut d: Deferred) -> bool {
        let grp = self.groups.get_mut(&g).expect("a group of this core");
        if let Ok((_, _, sm)) = unwrap_snapshot(&snap.data)
            && !grp.sm.has_data_for(sm)
        {
            // The data comes another way: nothing is applied until it is here.
            grp.installing = true;
            self.snapshot_data.push((g, snap.meta.index));
            grp.parked = Some((snap, d));
            return false;
        }
        if !self.restore(g, &snap.data) {
            let (node, total) = d.from;
            let ack = PeerFrame::SnapshotAck {
                group: g,
                index: snap.meta.index,
                offset: total,
                ok: false,
            };
            self.frames.push((node, ack));
            return false;
        }
        d.index = snap.meta.index;
        // The leader's snapshot replaces the own one whose data was missing.
        let grp = self.groups.get_mut(&g).expect("restored");
        (grp.own, grp.own_stale) = (None, false);
        // Faulty variant (005 T033): the entries the snapshot contradicts leave no record.
        #[cfg(not(teseladb_bug = "rollback_unrecorded"))]
        self.note_superseded(g, d.index);
        let meta = SnapshotMeta {
            index: snap.meta.index,
            term: snap.meta.term,
            conf: encode_conf(&snap.meta.conf),
        };
        // The restored state machine may hold the log differently now (005: an ingested range
        // has nothing in memory): the log learns it before the snapshot is saved. It has the data
        // of the whole snapshot (`has_data_for`), so nothing up to the snapshot's index is held
        // back, whatever entries below it it still keeps in memory: a range applies again after
        // a restart the entries its LSM had already flushed, and its floor counts them.
        if S::HOLDS_LOG {
            let floor = self.groups.get(&g).expect("group").sm.release_floor();
            let floor = floor.map(|f| f.max(d.index));
            self.store.set_release_floor(g, ReleaseProvider::Lsm, floor);
        }
        let Ok(t) = self.store.save_snapshot(g, meta, snap.data) else {
            self.stop(g);
            return false;
        };
        let grp = self.groups.get_mut(&g).expect("group");
        grp.installing = true;
        grp.snap_index = d.index;
        self.waits.insert(t, Wait::Install { group: g, deferred: Box::new(d) });
        true
    }

    /// The data the parked snapshot of `g` needed is now here (one from the leader, or its own
    /// after a restart): it is installed or restored, or asked for again if it is still missing.
    pub fn snapshot_data_ready(&mut self, g: GroupId, now: Nanos) {
        self.now = now;
        let Some(grp) = self.groups.get_mut(&g) else { return };
        if let Some((snap, d)) = grp.parked.take() {
            grp.installing = false;
            self.install(g, snap, d);
            return;
        }
        let Some((index, data)) = grp.own.take() else { return };
        if self.missing_data(g, &data) {
            self.snapshot_data.push((g, index));
            self.groups.get_mut(&g).expect("checked").own = Some((index, data));
        } else if self.restore(g, &data) {
            self.restored_own(g);
            self.drive(g);
        }
    }

    /// The state machine's part of the snapshot of `g` that waits for its data
    /// (`Actions::snapshot_data`), if one does.
    pub fn awaited_snapshot(&self, g: GroupId) -> Option<&[u8]> {
        let grp = self.groups.get(&g)?;
        let parked = grp.parked.as_ref().map(|(snap, _)| &snap.data[..]);
        let data = parked.or_else(|| grp.own.as_ref().map(|(_, data)| &data[..]))?;
        unwrap_snapshot(data).ok().map(|(_, _, sm)| sm)
    }

    /// The layer above learnt that no replica will serve the data of the snapshot `g` waits for
    /// (007: the range split past it): its leader is told the transfer failed, and sends its
    /// newest snapshot, which replaces this one. A follower that waits for a snapshot's data
    /// answers nothing else, so the leader would not learn it otherwise. For its own snapshot
    /// after a restart, it asks for the leader's snapshot (index 0), which it installs even below
    /// its commit index.
    pub fn reject_awaited(&mut self, g: GroupId) {
        let Some(grp) = self.groups.get_mut(&g) else { return };
        // Its own snapshot after a restart: it asks the leader for its snapshot, now and with each
        // round of the leader's entries (007 T016).
        let own = grp.parked.is_none() && grp.own.is_some();
        grp.own_stale |= own;
        let index = match (&grp.parked, own) {
            (Some((snap, _)), _) => snap.meta.index,
            (None, true) => 0,
            (None, false) => return,
        };
        // The leader it last heard of may have lost the leadership while it waited: every voter
        // hears it, and only the leader acts on it.
        let me = self.cfg.node_id;
        let to: BTreeSet<NodeId> =
            grp.raw.conf().voters.iter().copied().filter(|n| *n != me).collect();
        for node in to {
            let ack = PeerFrame::SnapshotAck { group: g, index, offset: 0, ok: false };
            self.frames.push((node, ack));
        }
    }

    /// Whether the state machine of `g` lacks the data of snapshot `data`.
    fn missing_data(&self, g: GroupId, data: &[u8]) -> bool {
        let grp = self.groups.get(&g).expect("a group of this core");
        unwrap_snapshot(data).is_ok_and(|(_, _, sm)| !grp.sm.has_data_for(sm))
    }

    /// Raft asked to send a snapshot to `to`: its file is read and sent in chunks.
    /// `false` if the snapshot is below the state machine's floor or not on the disk yet: it is
    /// not sent, and Raft asks again until the newer one, taken when the floor passed it, is
    /// saved.
    fn start_snapshot_send(&mut self, g: GroupId, to: NodeId) -> bool {
        let grp = self.groups.get_mut(&g).expect("a group of this core");
        // Raft asks for one once the log was compacted, which needs one, or for a follower below
        // the log floor, whose snapshot past it may still be on its way to the disk.
        let loaded = if grp.snap_state < grp.sm.snapshot_floor() {
            None
        } else {
            self.store.load_snapshot(g)
        };
        let Some((_, token)) = loaded else {
            grp.raw.report_snapshot(to, false);
            self.refused.insert((g, to));
            return false;
        };
        self.loads.insert(token, Load::Send { group: g, to });
        self.sending.entry(g).or_default().insert(to, (self.now, 0));
        true
    }

    // ----- state -----

    /// `provider` still needs the log of `g` above `floor` (`None`: nothing held back): the log
    /// keeps those entries and periodic snapshots stay at or below the floor (004 FR-011).
    pub fn set_release_floor(&mut self, g: GroupId, provider: ReleaseProvider, floor: Option<u64>) {
        self.store.set_release_floor(g, provider, floor);
    }

    /// Reads of group `g` this core answered `(with the lease, with ReadIndex)` (005 FR-022).
    pub fn read_counts(&self, g: GroupId) -> Option<(u64, u64)> {
        self.groups.get(&g).map(|grp| grp.read_counts)
    }

    /// This node's clock minus the median clock of the voters it heard in the last second, if it
    /// heard any (005 FR-006, FR-022).
    pub fn clock_offset(&self, now: Nanos) -> Option<i64> {
        self.clock_rule.offset(now)
    }

    /// Whether group `g` is quiescent: idle, its leader sends no heartbeats.
    pub fn is_quiesced(&self, g: GroupId) -> bool {
        self.groups.get(&g).is_some_and(|grp| grp.raw.is_quiesced())
    }

    /// Whether a snapshot a replica waits for is held back by the release floor (a state machine
    /// that holds the log, its memtable not flushed yet): the layer above flushes now rather than
    /// when the core is next idle, since the replica may be what the group needs to commit.
    pub fn snapshot_awaits_flush(&self) -> bool {
        self.floor_waits
            .iter()
            .any(|g| self.groups.get(g).is_some_and(|grp| grp.pending_snap.is_some()))
    }

    /// Raft status of a group.
    pub fn status(&self, g: GroupId) -> Option<Status> {
        self.groups.get(&g).map(|grp| grp.raw.status(&GroupStore::new(&self.store, g)))
    }

    /// The cluster time at which this replica won the term of `g` it leads, for a state machine
    /// with a read cache: the node's clock went then to it plus
    /// [`StateMachine::read_cache_margin_ms`] (005 FR-005). `None` if it does not lead `g`.
    pub fn lease_mark(&self, g: GroupId) -> Option<Hlc> {
        let grp = self.groups.get(&g)?;
        let st = grp.raw.status(&GroupStore::new(&self.store, g));
        grp.lease_mark
            .filter(|(term, _)| st.role == Role::Leader && *term == st.hard_state.term)
            .map(|(_, lease)| lease)
    }

    /// As leader of `g`, closes mark `c` (007 FR-012): the layer above writes nothing more at `c`
    /// or below, and every write at or below it was proposed already. The heartbeats of this term
    /// carry `c` with the last index of the log now. `false` if this replica does not lead `g` or
    /// its lease ran out.
    pub fn set_closed(&mut self, g: GroupId, c: Hlc) -> bool {
        let Some(grp) = self.groups.get_mut(&g) else { return false };
        let st = grp.raw.status(&GroupStore::new(&self.store, g));
        // Only within its lease: a follower elects another leader only once it ran out, and that
        // one writes above its own clock, past what this one closed (plan «Marcas cerradas»).
        if st.role != Role::Leader || !grp.raw.can_lease_read(self.now) {
            return false;
        }
        grp.closed_out = Some((st.hard_state.term, c, st.last_index));
        true
    }

    /// The highest mark a leader of `g` closed that this replica heard, with the index up to which
    /// it has to apply to read at it (007 FR-012). A closed mark holds for good: no leader of a
    /// later term writes at or below it either.
    pub fn closed(&self, g: GroupId) -> Option<(Hlc, u64)> {
        self.groups.get(&g)?.closed_in
    }

    /// Takes the closed marks of the heartbeats `beats`.
    fn hear_closed(&mut self, beats: &[GroupHeartbeat]) {
        for b in beats {
            let (Some((c, index)), Some(grp)) = (b.closed, self.groups.get_mut(&b.group)) else {
                continue;
            };
            if grp.closed_in.is_none_or(|(known, _)| known < c) {
                grp.closed_in = Some((c, index));
            }
        }
    }

    /// Index of the last snapshot of a group (0: none, its log starts at its first entry).
    pub fn snapshot_index(&self, g: GroupId) -> Option<u64> {
        self.groups.get(&g).map(|grp| grp.snap_index)
    }

    /// Data entered `g`'s state machine without an entry (018: a restore's ingest): `enter`
    /// records it there, Raft takes the state machine's new log floor, and a snapshot past it is
    /// taken now (see [`MultiRaftCore::snapshot_now`]). `false` if `g` is unknown or no snapshot
    /// was taken.
    pub fn data_entered(&mut self, g: GroupId, enter: impl FnOnce(&mut S)) -> bool {
        let Some(grp) = self.groups.get_mut(&g) else { return false };
        enter(&mut grp.sm);
        self.snapshot_now(g)
    }

    /// Takes a snapshot of `g` at its last applied entry now, however few entries passed since
    /// the last one (007: a range born from a split compacts its log at once, so that a replica
    /// added later receives the data the split left in the core's LSM as a snapshot and never the
    /// log from its first entry, which does not have it). It is saved like any other, once the
    /// release floor passes it. `false` if `g` is unknown, applied nothing past its last
    /// snapshot, or a snapshot is already waiting.
    pub fn snapshot_now(&mut self, g: GroupId) -> bool {
        let Some(grp) = self.groups.get_mut(&g) else { return false };
        // An ingest just before may have raised it (018).
        grp.raw.set_log_floor(grp.sm.log_floor());
        self.save_pending_snapshot_if_any(g);
        let grp = &self.groups[&g];
        let index = grp.sm.applied_index();
        if grp.pending_snap.is_some() || index <= grp.snap_index || grp.loading || grp.own.is_some()
        {
            return false;
        }
        let Ok(term) = GroupStore::new(&self.store, g).term(index) else { return false };
        self.take_snapshot(g, index, term);
        true
    }

    fn save_pending_snapshot_if_any(&mut self, g: GroupId) {
        if self.groups.contains_key(&g) {
            self.save_pending_snapshot(g);
        }
    }

    /// The state machine of a group.
    pub fn state_machine(&self, g: GroupId) -> Option<&S> {
        self.groups.get(&g).map(|grp| &grp.sm)
    }

    /// Configuration of a group.
    pub fn conf(&self, g: GroupId) -> Option<&ConfState> {
        self.groups.get(&g).map(|grp| grp.raw.conf())
    }

    /// Whether a group stopped (the log refused a write, or an entry or snapshot could not be
    /// applied): only a restart brings it back.
    pub fn has_stopped_group(&self) -> bool {
        self.groups.values().any(|g| g.stopped)
    }

    /// Groups with a state machine.
    pub fn group_ids(&self) -> Vec<GroupId> {
        self.groups.keys().copied().collect()
    }

    /// The core's log.
    pub fn store(&self) -> &CoreStore {
        &self.store
    }
}
