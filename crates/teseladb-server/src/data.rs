//! The data ranges of a core (005 FR-015, FR-016, FR-012; 007 FR-003, FR-005, FR-007): the core's
//! LSM and the host of its ranges, the in-process requests of the node's handle ([`DataReq`]),
//! each request sent to the leader of its range through the router, the replicas Group 0 gives
//! this node created and opened ([`crate::ranges`]), the replica changes and splits the ranges'
//! leaders carry out ([`crate::replicas`], [`crate::ranges`]) and the snapshot transfers
//! ([`crate::transfer`]).
//!
//! The core's disk thread runs one stream of operations whose results come back in submission
//! order; [`DataCore::poll_io`] gives the LSM's and the transfers' and says whose each one is, so
//! that the logic routes every result to its owner.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::Sender;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Completions, Lsm};
use teseladb_meta::command::Group0Command;
use teseladb_multiraft::{Done, MultiRaftCore, NodeClock, OpError, OpId, StateMachine};
use teseladb_raft::Role;
use teseladb_raftlog::{IoDone, IoOp};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_range::checksum::Report;
use teseladb_range::command::Put;
use teseladb_range::host::{
    ChangesReq, Mutation, MutationError, RangeError, RangeHost, ReadSpec, Reply, ReqId, ScanReq,
    TxnReq, WriteReq,
};
use teseladb_range::mutate::Outcome;
use teseladb_range::read::ReadConcern;
use teseladb_router::route::{Answer, Router};
use teseladb_types::{Bytes, GroupId, Nanos, NodeId};
use teseladb_wire::commands::mutation::Bound;

use crate::backup::source::Backups;
use crate::checksums::Checksums;
use crate::coresm::{CoreOutput, CoreSm};
use crate::data_wire::{
    RemoteReply, RemoteReq, decode_for, decode_reply, encode_for, encode_reply,
};
use crate::forward::{Fetch, Request};
use crate::hosting::Hosting;
use crate::leaders::Leaders;
use crate::merges::{MERGE_BYTES, MERGE_COLD, Merges};
use crate::ranges::{Ranges, Replicas};
use crate::relocations::{RelocateMsg, Relocations};
use crate::transfer::Transfers;

/// Value type of the values the node's handle writes: a document (as the simulator's
/// `range-docs`).
pub const DOCUMENT: u8 = 1;

/// A range that applied nothing for this long has its memtable flushed: what stays only in a
/// memtable holds the range's log back (`StateMachine::release_floor`), so an idle range would
/// keep its whole log and never take a snapshot.
pub const IDLE_FLUSH: Nanos = Nanos::from_secs(1);

pub use crate::idle_flush::IdleFlush;
/// How long a replica waits for a snapshot that does not come before it asks for its range as
/// its split left it.
pub const UNSERVED_FETCH: Nanos = Nanos::from_secs(2);

/// Attempts the router of a core makes before it leaves a request to the caller's retries.
const ROUTER_ATTEMPTS: u32 = 3;

/// A request of the node's handle for a data range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataReq {
    /// Writes `value` at `key` with write concern `wc`, in the range of this core that holds it.
    Put {
        /// Key (inside `[01, 04)`).
        key: Vec<u8>,
        /// Value.
        value: Vec<u8>,
        /// Write concern.
        wc: WriteConcern,
    },
    /// Reads `key` with read concern `linearizable`, in the range of this core that holds it.
    Get {
        /// Key.
        key: Vec<u8>,
    },
    /// The state of this node's replica of a range.
    Status(GroupId),
    /// A write or a scan of the CRUD commands (006 FR-014) for a range: run here if this replica
    /// leads it, or forwarded to its leader.
    Remote(GroupId, RemoteReq),
    /// A request for the replica of a range on node `.0` (a restore's, 007 FR-021): run here if
    /// it is this node, or forwarded to it.
    At(NodeId, GroupId, RemoteReq),
}

/// What this node's replica of a range looks like.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DataStatus {
    /// Its role (`None`: no replica on this node).
    pub role: Option<Role>,
    /// Known leader (0 = none).
    pub leader: NodeId,
    /// Its term.
    pub term: u64,
    /// Voters of its configuration.
    pub voters: Vec<NodeId>,
    /// Learners of its configuration.
    pub learners: Vec<NodeId>,
    /// Index the range applied.
    pub applied: u64,
    /// The range's flushed index in the LSM: after a restart the log replays from the next one.
    pub flushed: u64,
    /// A snapshot of the range waits for its data.
    pub waiting_data: bool,
    /// Snapshots whose data this core received from another and ingested since it started.
    pub ingested: u64,
    /// Exports this core keeps for others.
    pub exports: usize,
    /// The range's bounds as its state machine applied them (empty: no replica).
    pub bounds: (Vec<u8>, Vec<u8>),
    /// Where a split by size would cut this replica's data: the middle key of its SST files
    /// (`None`: no replica, or no files to cut).
    pub cut: Option<Vec<u8>>,
    /// Replicas of this core whose data a checksum had rebuilt since it started (FR-014).
    pub rebuilt: u64,
    /// This replica is being rebuilt.
    pub rebuilding: bool,
}

/// The answer to a [`DataReq`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataReply {
    /// The write is confirmed at this mark.
    Put(Hlc),
    /// The value at `key`, if any.
    Get(Option<Vec<u8>>),
    /// The replica's state.
    Status(DataStatus),
    /// The answer to a [`DataReq::Remote`].
    Remote(RemoteReply),
}

/// Why a request of the node's handle failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DataError {
    /// The range refused it or failed.
    Range(RangeError),
    /// No answer in time (the core is stopping or busy).
    NoAnswer,
    /// An answer of another kind (a defect of the core).
    Unexpected(String),
}

/// Where the answer to a request goes.
pub type DataSender = Sender<Result<DataReply, RangeError>>;

/// Who waits for a request of the host.
enum Client {
    /// The node's handle; with the range and the request, to forward it to the leader once.
    Local(DataSender, Option<(GroupId, RemoteReq)>),
    /// Another node's forwarded request.
    Remote(NodeId, u64),
}

/// The mutation of a put: the new value, whatever was there.
struct PutValue {
    key: Vec<u8>,
    value: Vec<u8>,
}

impl Mutation for PutValue {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let put = Put { key: self.key.clone(), value_type: DOCUMENT, value: self.value.clone() };
        Ok(Outcome { puts: vec![put], deletes: Vec::new(), change: Vec::new(), result: Vec::new() })
    }
}

/// Whose a disk operation of [`DataCore::poll_io`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoOwner {
    /// The LSM's.
    Lsm,
    /// The snapshot transfers'.
    Transfer,
    /// The core changes' (007 FR-011).
    Relocate,
    /// The backups' and restores' (007 FR-021).
    Backup,
}

/// The data ranges of a core.
pub struct DataCore {
    me: NodeId,
    host: RangeHost,
    lsm: Rc<RefCell<Lsm>>,
    clients: BTreeMap<ReqId, Client>,
    /// Requests forwarded to a range's leader: who waits, the range and the node asked.
    forwarded: BTreeMap<OpId, (DataSender, GroupId, NodeId)>,
    /// Scans with a read preference forwarded to a candidate, with the request whose next
    /// candidate a refusal tries (007 FR-013).
    follower_forwards: BTreeMap<OpId, (DataSender, GroupId, RemoteReq)>,
    /// The wall clock of the last call, for a scan that passes to its next candidate.
    wall_ms: u64,
    /// Where each range's leader is.
    router: Router,
    /// This core's replicas: created and opened as Group 0 gives them, changed and split by their
    /// leaders.
    replicas: Replicas,
    transfers: Transfers,
    /// The sums of the replicas on their way to the leaders, and the rebuilds (FR-014).
    checksums: Checksums,
    /// RefDb's UUID of each collection, by its identifier (from Group 0's catalog).
    collections: BTreeMap<u32, [u8; 16]>,
    /// The merges of this core's ranges (007 FR-009).
    merges: Merges,
    /// What the leaders of this core's ranges do with the leader their descriptor names (007
    /// FR-010).
    leaders: Leaders,
    /// Group 0 commands for core 0 (`ReplicasSettled`, `SplitRange`, `CompleteSplit`, the
    /// merges').
    group0: Vec<Group0Command>,
    /// When the memtable is flushed for an idle core or for a snapshot a replica waits for.
    idle: IdleFlush,
    /// Since when each replica has waited for a snapshot with none on its way.
    unserved: BTreeMap<GroupId, Nanos>,
    /// Since when each replica has waited for a snapshot no replica serves.
    stale: BTreeMap<GroupId, Nanos>,
    /// The core changes of this core's ranges (007 FR-011).
    relocations: Relocations,
    /// Backups and restores of the ranges this core leads (007 FR-021).
    backups: Backups,
    /// Who waits for each answer of `backups`.
    backup_clients: BTreeMap<u64, Client>,
    /// The identifier of the next request of `backups`.
    backup_next: u64,
}

impl DataCore {
    /// The data ranges of core `core` of node `me` over `lsm`; the node's `clock` gives the marks
    /// and `split_bytes` is the size at which a range splits.
    pub fn new(
        me: NodeId,
        core: u16,
        lsm: Rc<RefCell<Lsm>>,
        clock: NodeClock,
        transfers: Transfers,
        split_bytes: u64,
    ) -> DataCore {
        DataCore {
            me,
            host: RangeHost::new(lsm.clone(), clock),
            lsm,
            clients: BTreeMap::new(),
            forwarded: BTreeMap::new(),
            follower_forwards: BTreeMap::new(),
            wall_ms: 0,
            router: Router::new(ROUTER_ATTEMPTS),
            replicas: Replicas::new(me, core, split_bytes),
            transfers,
            checksums: Checksums::new(me),
            merges: Merges::new(me, MERGE_BYTES, MERGE_COLD),
            leaders: Leaders::default(),
            collections: BTreeMap::new(),
            group0: Vec::new(),
            idle: IdleFlush::default(),
            unserved: BTreeMap::new(),
            stale: BTreeMap::new(),
            relocations: Relocations::new(core, PathBuf::from("relocate"), Hosting::default()),
            backups: Backups::new(PathBuf::from("backup"), 0),
            backup_clients: BTreeMap::new(),
            backup_next: 0,
        }
    }

    /// Core changes with their files under `dir` and the node's table of which cores hold each
    /// range (007 FR-011).
    pub fn with_relocations(mut self, dir: PathBuf, hosting: Hosting) -> DataCore {
        let core = self.replicas.core();
        self.replicas.set_hosting(hosting.clone());
        self.relocations = Relocations::new(core, dir, hosting);
        self
    }

    /// Backups and restores with their files under `dir`; `nonce` tells this life's file names
    /// from an earlier one's (007 FR-021).
    pub fn with_backups(mut self, dir: PathBuf, nonce: u64) -> DataCore {
        self.backups = Backups::new(dir, nonce);
        self
    }

    /// Runs a backup's or a restore's request `r` for range `g` for `client`.
    fn backup_request(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        (g, r): (GroupId, RemoteReq),
        client: Client,
        at: (Nanos, u64),
    ) {
        // A replica that still waits for its first snapshot, or for the data of the one it
        // installed, keeps the restore's files staged until it has it (007 FR-021): that
        // snapshot holds the range as it was before the restore and would drop them.
        let incomplete = || {
            core.state_machine(g).is_some_and(StateMachine::needs_snapshot)
                || self.transfers.waiting_for(g)
        };
        if matches!(r, RemoteReq::RestoreIngest { .. }) && incomplete() {
            let not_yet = Ok(RemoteReply::Restored { done: false });
            self.send_answer(core, client, not_yet, at.0);
            return;
        }
        self.backup_next += 1;
        let id = self.backup_next;
        self.backup_clients.insert(id, client);
        self.backups.request((core, &mut self.host), id, (g, r), at);
        self.answer(core, at.0);
    }

    /// A core change message from another core of the node.
    pub fn on_relocate(&mut self, core: &mut MultiRaftCore<CoreSm>, msg: RelocateMsg) {
        let lsm = self.lsm.clone();
        self.relocations.on_message(core, &mut self.replicas, &lsm, msg);
    }

    /// The other core of the node that runs range `g`, if one does (007 FR-011).
    pub fn holder(&self, g: GroupId) -> Option<u16> {
        self.relocations.holder(g)
    }

    /// Core change messages for other cores of the node, with their core.
    pub fn take_relocate_msgs(&mut self) -> Vec<(u16, RelocateMsg)> {
        self.relocations.take_msgs()
    }

    /// A core change message for core `to` that could not leave: it goes on a later turn.
    pub fn requeue_relocate(&mut self, to: u16, msg: RelocateMsg) {
        self.relocations.requeue(to, msg);
    }

    /// Ranges merge below `bytes` of live data each, once no write reached them for `cold`
    /// (FR-009; tests shorten it).
    pub fn with_merges(self, bytes: u64, cold: Nanos) -> DataCore {
        DataCore { merges: Merges::new(self.me, bytes, cold), ..self }
    }

    /// Another node asks whether this node's replica of `range` is frozen for `merge` with its
    /// data on disk (FR-009).
    pub fn on_merge_probe(
        &self,
        core: &mut MultiRaftCore<CoreSm>,
        from: (NodeId, u64),
        (range, merge): (GroupId, u64),
    ) {
        let answer = crate::merges::probe_answer(core, &self.lsm.borrow(), range, merge);
        core.answer_forward(from.0, from.1, answer);
    }

    /// Operations per second the ranges this core leads served in the last whole second (007
    /// FR-010).
    pub fn ops(&self, core: &MultiRaftCore<CoreSm>) -> u64 {
        core.group_ids().into_iter().map(|g| self.host.load(g)).sum()
    }

    /// Group 0's ranges changed (from core 0).
    pub fn set_ranges(&mut self, ranges: &Ranges) {
        self.router.refresh_ranges(ranges.descs.iter());
        self.leaders.set(ranges);
        self.replicas.set(ranges);
        self.relocations.set_intents(&ranges.relocations);
        self.collections.clone_from(&ranges.collections);
        if let Some(ms) = ranges.history_ms {
            self.host.set_gc_ttl_ms(ms);
        }
        if let Some(ops) = ranges.load_split_ops {
            self.host.set_load_split_ops(ops);
        }
        self.merges.set_off(ranges.automerge_off);
    }

    /// The Group 0 commands for core 0 since the last call.
    pub fn take_group0(&mut self) -> Vec<Group0Command> {
        std::mem::take(&mut self.group0)
    }

    /// The range of this core that holds `key` on this node.
    fn local_range(&self, key: &[u8]) -> Option<GroupId> {
        self.router.table().lookup(key).map(|e| e.group).filter(|g| self.host_has(*g))
    }

    fn host_has(&self, g: GroupId) -> bool {
        self.replicas.is_open(g)
    }

    /// A request of the node's handle.
    pub fn request(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        req: DataReq,
        reply: DataSender,
        (now, wall_ms): (Nanos, u64),
    ) {
        self.wall_ms = wall_ms;
        let id = match req {
            DataReq::Status(g) => {
                let _ = reply.send(Ok(DataReply::Status(self.status(core, g))));
                return;
            }
            DataReq::Put { key, value, wc } => {
                let Some(g) = self.local_range(&key) else {
                    let _ = reply.send(Err(RangeError::OutOfRange));
                    return;
                };
                let keys = vec![key.clone()];
                let w = WriteReq { g, keys, m: Box::new(PutValue { key, value }), dedup: None, wc };
                self.host.mutate(core, w, now, wall_ms)
            }
            DataReq::Get { key } => {
                let Some(g) = self.local_range(&key) else {
                    let _ = reply.send(Err(RangeError::OutOfRange));
                    return;
                };
                self.host.get(core, g, &key, ReadConcern::Linearizable, now, wall_ms)
            }
            DataReq::Remote(g, r @ RemoteReq::FollowerScan { .. }) => {
                self.follower_scan(core, g, r, reply, (now, wall_ms));
                return;
            }
            DataReq::At(node, g, r) if node != self.me => {
                // The request names its replica, which may still be a learner the router does
                // not count among the voters: it goes there and nowhere else.
                self.send_to(core, (g, r), reply, node, now);
                return;
            }
            DataReq::At(_, g, r) | DataReq::Remote(g, r) => {
                if !self.host_has(g) {
                    // No replica here (or not open yet): straight to the leader the router knows.
                    self.forward(core, g, r, reply, NodeId(0), now);
                    return;
                }
                let client = Client::Local(reply, Some((g, r.clone())));
                self.run(core, (g, r), client, (now, wall_ms));
                return;
            }
        };
        self.clients.insert(id, Client::Local(reply, None));
        self.answer(core, now);
    }

    /// Forwards a request for range `g` to its leader: `hint` if it is one of the range's voters
    /// other than this node, or the leader the router knows.
    fn forward(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        g: GroupId,
        r: RemoteReq,
        to: DataSender,
        hint: NodeId,
        now: Nanos,
    ) {
        let voters = self.router.table().entry(g).map(|e| e.voters.clone()).unwrap_or_default();
        let target = Some(hint)
            .filter(|n| *n != self.me && voters.contains(n))
            .or_else(|| self.router.route_group(g).map(|t| t.node).filter(|n| *n != self.me))
            .or_else(|| voters.iter().copied().find(|n| *n != self.me));
        let Some(node) = target else {
            // No other replica to ask: the range has no reachable leader yet.
            let _ = to.send(Err(RangeError::NotLeader { hint: NodeId(0) }));
            return;
        };
        self.send_to(core, (g, r), to, node, now);
    }

    /// Sends request `r` for range `g` to `node`; its answer goes to `to`.
    fn send_to(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        (g, r): (GroupId, RemoteReq),
        to: DataSender,
        node: NodeId,
        now: Nanos,
    ) {
        let body = Request::Data(encode_for(g, &r)).encode();
        let op = core.forward(node, Bytes::from_vec(body), now);
        self.forwarded.insert(op, (to, g, node));
    }

    /// A scan with a read preference: on its first candidate, here or forwarded; a candidate that
    /// cannot serve it (no replica here, a closed mark too old) passes it to the next one, and the
    /// last refusal goes back to the caller (007 FR-013).
    fn follower_scan(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        g: GroupId,
        mut r: RemoteReq,
        reply: DataSender,
        (now, wall_ms): (Nanos, u64),
    ) {
        // A request without candidates goes where any other request of the range goes.
        let RemoteReq::FollowerScan { nodes, .. } = &mut r else {
            self.request(core, DataReq::Remote(g, r), reply, (now, wall_ms));
            return;
        };
        while nodes.first().is_some_and(|(n, _)| *n == self.me) && !self.host_has(g) {
            nodes.remove(0);
        }
        let Some(&(node, _)) = nodes.first() else {
            let _ = reply.send(Err(RangeError::NotLeader { hint: NodeId(0) }));
            return;
        };
        if node == self.me {
            let client = Client::Local(reply, Some((g, r.clone())));
            self.run(core, (g, r), client, (now, wall_ms));
            return;
        }
        let body = Request::Data(encode_for(g, &r)).encode();
        let op = core.forward(node, Bytes::from_vec(body), now);
        self.follower_forwards.insert(op, (reply, g, r));
    }

    /// The scan `r` without its first candidate, which refused it.
    fn next_candidate(r: RemoteReq) -> RemoteReq {
        match r {
            RemoteReq::FollowerScan { start, end, limit, max_stale_ms, mut nodes } => {
                if !nodes.is_empty() {
                    nodes.remove(0);
                }
                RemoteReq::FollowerScan { start, end, limit, max_stale_ms, nodes }
            }
            other => other,
        }
    }

    /// Runs a write or a scan on the host.
    fn run(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        (g, r): (GroupId, RemoteReq),
        client: Client,
        (now, wall_ms): (Nanos, u64),
    ) {
        let id = match r {
            RemoteReq::Write { keys, mutation, dedup, wc, deadline } => {
                let m: Box<dyn Mutation> = match Bound::decode(&mutation) {
                    Ok(m) => Box::new(m),
                    // An undecodable mutation fails inside the host as a refused one.
                    Err(e) => Box::new(Refuse(e.to_mutation())),
                };
                let w = WriteReq { g, keys, m, dedup, wc };
                match deadline {
                    Some(d) => self.host.mutate_until(core, w, d, (now, wall_ms)),
                    None => self.host.mutate(core, w, now, wall_ms),
                }
            }
            RemoteReq::Scan { start, end, limit, concern } => {
                let limit = usize::try_from(limit).unwrap_or(usize::MAX);
                let req = ScanReq { g, start, end, concern, limit };
                self.host.scan(core, req, now, wall_ms)
            }
            // The leader scans as for `primary`; a follower at its closed mark.
            RemoteReq::FollowerScan { start, end, limit, max_stale_ms, nodes } => {
                let limit = usize::try_from(limit).unwrap_or(usize::MAX);
                if nodes.first().is_some_and(|(_, leads)| *leads) {
                    let req = ScanReq { g, start, end, concern: ReadConcern::Majority, limit };
                    self.host.scan(core, req, now, wall_ms)
                } else {
                    let view = teseladb_range::push::TxnView::default();
                    let scan = Some((end, limit));
                    let concern = ReadConcern::Local;
                    let spec = ReadSpec {
                        g,
                        key: start,
                        scan,
                        at: None,
                        concern,
                        view,
                        uncertain_to: None,
                    };
                    self.host.read_follower(core, spec, max_stale_ms, now, wall_ms)
                }
            }
            RemoteReq::Read { key, at: Some(at) } => {
                self.host.get_at(core, g, &key, at, now, wall_ms)
            }
            // The leader takes the mark under its clock rule (D-003, D-069).
            RemoteReq::Read { key, at: None } => {
                self.host.get(core, g, &key, ReadConcern::Majority, now, wall_ms)
            }
            // A read that knows about transactions whose intents it met (008 FR-003).
            RemoteReq::ReadWith { key, scan, at, concern, view, uncertain_to } => {
                let scan =
                    scan.map(|(end, limit)| (end, usize::try_from(limit).unwrap_or(usize::MAX)));
                let spec = ReadSpec { g, key, scan, at, concern, view, uncertain_to };
                self.host.read_with(core, spec, now, wall_ms)
            }
            // Every request of a transaction waits for the range's majority (008).
            RemoteReq::Txn { op, deadline } => {
                let wc = WriteConcern { w: W::Majority, j: true };
                let req = TxnReq { g, op, wc };
                match deadline {
                    Some(d) => self.host.txn_until(core, req, d, (now, wall_ms)),
                    None => self.host.txn(core, req, now, wall_ms),
                }
            }
            // A change stream's read, on the leader (014 FR-003).
            RemoteReq::Changes { start, end, from } => {
                self.host.changes(core, ChangesReq { g, start, end, from }, now)
            }
            // A resharding's write fence, on the leader (018 FR-002).
            RemoteReq::Fence { start, end, on } => self.host.fence(core, (g, start, end), on, now),
            // A backup's or a restore's request (007 FR-021).
            r @ (RemoteReq::BackupFetch { .. }
            | RemoteReq::RestorePiece { .. }
            | RemoteReq::RestoreIngest { .. }) => {
                self.backup_request(core, (g, r), client, (now, wall_ms));
                return;
            }
        };
        self.clients.insert(id, client);
        self.answer(core, now);
    }

    /// A request another node forwarded to this replica (as the range's leader). A range this
    /// core does not hold answers `NotLeader` with the leader the router knows.
    pub fn request_remote(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        from: (NodeId, u64),
        body: &[u8],
        (now, wall_ms): (Nanos, u64),
    ) {
        self.wall_ms = wall_ms;
        match decode_for(body) {
            Ok((g, r)) if self.host_has(g) => {
                self.run(core, (g, r), Client::Remote(from.0, from.1), (now, wall_ms));
            }
            Ok((g, _)) => {
                let hint = self.router.route_group(g).map_or(NodeId(0), |t| t.node);
                let hint = if hint == self.me { NodeId(0) } else { hint };
                let body = encode_reply(&Err(RangeError::NotLeader { hint }));
                core.answer_forward(from.0, from.1, Bytes::from_vec(body));
            }
            Err(_) => {
                let body = encode_reply(&Err(RangeError::OutOfRange));
                core.answer_forward(from.0, from.1, Bytes::from_vec(body));
            }
        }
    }

    fn status(&self, core: &MultiRaftCore<CoreSm>, g: GroupId) -> DataStatus {
        let st = core.status(g);
        let conf = core.conf(g);
        let sm = core.state_machine(g).and_then(CoreSm::range);
        DataStatus {
            role: st.as_ref().map(|s| s.role),
            leader: st.as_ref().map_or(NodeId(0), |s| s.leader),
            term: st.as_ref().map_or(0, |s| s.hard_state.term),
            voters: conf.map(|c| c.voters.clone()).unwrap_or_default(),
            learners: conf.map(|c| c.learners.clone()).unwrap_or_default(),
            applied: core.state_machine(g).map_or(0, StateMachine::applied_index),
            flushed: self.lsm.borrow().flushed_index(g),
            waiting_data: self.transfers.waiting_for(g),
            ingested: self.transfers.ingested(),
            exports: self.transfers.exports(),
            bounds: sm.map(|s| (s.bounds().0.to_vec(), s.bounds().1.to_vec())).unwrap_or_default(),
            cut: sm.and_then(|s| self.lsm.borrow().interval_size(s.bounds().0, s.bounds().1).cut),
            rebuilt: self.checksums.rebuilt(),
            rebuilding: self.checksums.rebuilding(g),
        }
    }

    /// Sends the host's answers to whoever asked; a request of the handle that this replica
    /// does not lead goes once to the leader it names, or to the one the router knows.
    fn answer(&mut self, core: &mut MultiRaftCore<CoreSm>, now: Nanos) {
        let replies = self.host.take_replies();
        let lsm = self.lsm.clone();
        let replies: Vec<_> = replies
            .into_iter()
            .filter(|(id, r)| !self.backups.on_host_reply(&mut lsm.borrow_mut(), *id, r))
            .collect();
        let backups: Vec<_> = self.backups.take_replies();
        for (id, r) in backups {
            let Some(client) = self.backup_clients.remove(&id) else { continue };
            self.send_answer(core, client, r, now);
        }
        for (id, r) in replies {
            let Some(client) = self.clients.remove(&id) else { continue };
            match client {
                Client::Local(to, None) => {
                    let r = r.map(|reply| match reply {
                        Reply::Mutated { ts, .. } => DataReply::Put(ts),
                        Reply::Value { value, .. } => DataReply::Get(value.map(|(_, v)| v)),
                        Reply::Batch(rows) => {
                            DataReply::Get(rows.into_iter().next().map(|(_, (_, v))| v))
                        }
                        Reply::Intents { ts } => DataReply::Put(ts),
                        Reply::Record(record) => DataReply::Get(Some(record.encode())),
                        // A stream's read comes as a remote request, never from the handle.
                        Reply::Changes { .. } => DataReply::Get(None),
                        Reply::Fenced { ts } => DataReply::Put(ts),
                    });
                    let _ = to.send(r);
                }
                client => self.send_answer(core, client, r.map(remote_reply), now),
            }
        }
    }

    /// Sends a leader's answer `r` to `client`: a request of the handle that this replica does
    /// not lead goes once to the leader it names, or to the one the router knows.
    fn send_answer(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        client: Client,
        r: Result<RemoteReply, RangeError>,
        now: Nanos,
    ) {
        match client {
            Client::Local(to, Some((g, req @ RemoteReq::FollowerScan { .. }))) => match r {
                Err(RangeError::NotLeader { .. }) => {
                    let wall_ms = self.wall_ms;
                    self.follower_scan(core, g, Self::next_candidate(req), to, (now, wall_ms));
                }
                r => drop(to.send(r.map(DataReply::Remote))),
            },
            Client::Local(to, Some((g, req))) => match r {
                Err(RangeError::NotLeader { hint }) => {
                    self.forward(core, g, req, to, hint, now);
                }
                r => drop(to.send(r.map(DataReply::Remote))),
            },
            Client::Local(to, None) => drop(to.send(r.map(DataReply::Remote))),
            Client::Remote(node, req) => {
                let body = encode_reply(&r);
                core.answer_forward(node, req, Bytes::from_vec(body));
            }
        }
    }

    /// Moves the host's requests, the replicas, the leaders' duties and the transfers.
    pub fn poll(&mut self, core: &mut MultiRaftCore<CoreSm>, (now, wall_ms): (Nanos, u64)) {
        self.wall_ms = wall_ms;
        self.replicas.open(core, &mut self.host, &self.lsm);
        // Each replica follows what its state machine applied: a split's cut, the GC threshold
        // handed to the LSM.
        self.host.follow_state(core);
        self.host.poll(core, now, wall_ms);
        self.checksums.route(core, &mut self.host, now);
        let lsm = self.lsm.clone();
        self.checksums.step(core, &mut self.host, &mut self.replicas, &mut lsm.borrow_mut());
        self.answer(core, now);
        let cmds = self.replicas.lead(core, &mut self.host, &lsm.borrow(), now);
        self.group0.extend(cmds);
        let cmds = self.merges.step(core, &mut self.host, &mut self.replicas, &lsm, now);
        self.group0.extend(cmds);
        let cmds = self.leaders.step(core, self.me, now);
        self.group0.extend(cmds);
        self.relocations.step(core, &mut self.host, &mut self.replicas, &lsm, now);
        self.group0.extend(self.relocations.take_group0());
        self.flush_idle(core, now);
        let nodes = self.transfers.sink_group().map(|g| self.replicas.nodes(g)).unwrap_or_default();
        self.transfers.forget_unawaited(core);
        // A replica frozen for a merge keeps its data: the left range of the node takes it over, and
        // a clearing for an ingestion would run over what the left range writes there. Its
        // snapshot waits for the merge to end (given up: it thaws; done: it is retired).
        let merging = self.transfers.sink_group().is_some_and(|g| {
            let frozen = core
                .state_machine(g)
                .and_then(CoreSm::range)
                .and_then(teseladb_range::sm::RangeSm::frozen_for);
            let absorbing = self.replicas.absorbing(g).is_some_and(|(_, m)| frozen == Some(m));
            absorbing || self.replicas.merged_away(g)
        });
        if !merging {
            self.transfers.tick(core, &nodes, now);
        }
        self.backups.tick(now);
        self.fetch_unserved(core, now);
        self.stale.retain(|g, _| core.awaited_snapshot(*g).is_some());
        for g in self.transfers.take_stale() {
            core.reject_awaited(g);
            // The leader sends a newer snapshot. If none comes (no leader: a replica waiting for
            // data votes for no one), the replica opens again from its log after a while: the
            // snapshot it waits for is not on disk, and no replica will ever serve it.
            let since = *self.stale.entry(g).or_insert(now);
            if now.saturating_since(since) >= UNSERVED_FETCH && core.close_group(g).is_ok() {
                self.stale.remove(&g);
                self.host.remove_range(g);
                self.replicas.hold(g);
                self.replicas.release(g);
            }
        }
    }

    /// A replica opened with nothing of its range waits for a snapshot from the leader, which
    /// the leader takes once it applied an entry. A range born from a split whose other replicas
    /// cannot commit without this one never gets one: after [`UNSERVED_FETCH`] the replica asks a
    /// replica of the range for the range as its split left it (index 0), if one has applied
    /// nothing yet, and opens again with it as a replica born from the split.
    fn fetch_unserved(&mut self, core: &MultiRaftCore<CoreSm>, now: Nanos) {
        let unserved: Vec<GroupId> = self
            .replicas
            .open_groups()
            .filter(|g| core.awaited_snapshot(*g).is_none())
            .filter(|g| {
                core.state_machine(*g).is_some_and(|sm| {
                    use teseladb_multiraft::StateMachine;
                    sm.needs_snapshot()
                })
            })
            .collect();
        self.unserved.retain(|g, _| unserved.contains(g));
        for g in unserved {
            let since = *self.unserved.entry(g).or_insert(now);
            if !self.transfers.waiting() && now.saturating_since(since) >= UNSERVED_FETCH {
                let end =
                    core.state_machine(g).and_then(CoreSm::range).map(|sm| sm.bounds().1.to_vec());
                self.transfers.want(g, 0, end.unwrap_or_default(), now);
                self.unserved.insert(g, now);
            }
        }
    }

    /// Flushes the memtable once the core's ranges applied nothing for [`IDLE_FLUSH`] and one of
    /// them holds its log back, or at once for a snapshot a replica waits for
    /// ([`MultiRaftCore::snapshot_awaits_flush`]).
    fn flush_idle(&mut self, core: &MultiRaftCore<CoreSm>, now: Nanos) {
        let groups = core.group_ids();
        let ranges = groups.iter().filter(|g| self.host_has(**g));
        let applied = ranges
            .clone()
            .filter_map(|g| core.state_machine(*g))
            .map(StateMachine::applied_index)
            .fold(0u64, u64::wrapping_add);
        let held = ranges.clone().any(|g| self.lsm.borrow().release_floor(*g).is_some());
        let (idle, flush) = self.idle.step(applied, held, core.snapshot_awaits_flush(), now);
        self.idle = idle;
        if flush {
            self.lsm.borrow_mut().flush();
        }
    }

    /// The end of one of the core's operations; `false` if it is not this module's.
    pub fn on_done(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        op: OpId,
        r: Result<Done<CoreOutput>, OpError>,
        now: Nanos,
    ) -> bool {
        if self.replicas.on_done(op) || self.checksums.on_done(op) {
            return true;
        }
        let body = match &r {
            Ok(Done::Forwarded(b)) => Some(b.as_ref()),
            Ok(_) | Err(_) => None,
        };
        if self.merges.on_done(op, body) {
            return true;
        }
        let lsm = self.lsm.clone();
        if self.transfers.on_done(core, &mut lsm.borrow_mut(), op, &r, now) {
            return true;
        }
        if let Some((to, g, req)) = self.follower_forwards.remove(&op) {
            let answer = match r {
                Ok(Done::Forwarded(body)) => {
                    decode_reply(&body).unwrap_or(Err(RangeError::Unavailable))
                }
                // Nothing reached it, or it did not answer: the next candidate.
                Ok(_) | Err(_) => Err(RangeError::NotLeader { hint: NodeId(0) }),
            };
            match answer {
                Err(RangeError::NotLeader { .. }) => {
                    let wall_ms = self.wall_ms;
                    self.follower_scan(core, g, Self::next_candidate(req), to, (now, wall_ms));
                }
                r => drop(to.send(r.map(DataReply::Remote))),
            }
            return true;
        }
        if let Some((to, g, node)) = self.forwarded.remove(&op) {
            let answer = match r {
                Ok(Done::Forwarded(body)) => match decode_reply(&body) {
                    Ok(r) => r.map(DataReply::Remote),
                    Err(_) => Err(RangeError::Unavailable),
                },
                Ok(_) => Err(RangeError::Unavailable),
                // Nothing was sent: the node is gone (a leader that died). The caller tries again
                // and the router another replica.
                Err(OpError::Unreachable) => Err(RangeError::NotLeader { hint: NodeId(0) }),
                Err(_) => Err(RangeError::Timeout),
            };
            self.learn(g, node, &answer);
            let _ = to.send(answer);
            return true;
        }
        let mine = self.host.on_done(core, op, r);
        self.answer(core, now);
        mine
    }

    /// The router learns from the answer of `node` for range `g`.
    fn learn(&mut self, g: GroupId, node: NodeId, answer: &Result<DataReply, RangeError>) {
        let Some(t) = self.router.route_group(g) else { return };
        let read = match answer {
            Ok(_) => Answer::Done,
            Err(RangeError::NotLeader { hint }) => {
                Answer::NotLeader { hint: Some(*hint).filter(|n| *n != NodeId(0)) }
            }
            Err(RangeError::Timeout | RangeError::Unavailable | RangeError::Overloaded) => {
                Answer::Busy
            }
            Err(_) => Answer::Failed,
        };
        let start = self.router.table().entry(g).map(|e| e.start.clone()).unwrap_or_default();
        let sent = teseladb_router::route::Target { node, ..t };
        // Only the leader cache matters here: the caller retries.
        let _ = self.router.on_answer(&start, &sent, 1, true, read);
    }

    /// RefDb's UUID of each collection, by its identifier.
    pub fn collections(&self) -> &BTreeMap<u32, [u8; 16]> {
        &self.collections
    }

    /// The core's LSM.
    pub fn lsm(&self) -> &Rc<RefCell<Lsm>> {
        &self.lsm
    }

    /// Asks this core's replica of `g`, if it leads, for a checksum now (FR-014, on demand).
    pub fn check(&mut self, core: &MultiRaftCore<CoreSm>, g: GroupId) -> Result<(), RangeError> {
        self.host.check(core, g)
    }

    /// Another replica's sum for a range of this core, sent to it as the range's leader.
    pub fn on_checksum_report(&mut self, r: &Report) {
        Checksums::on_report(&mut self.host, r);
    }

    /// The range's leader marked this node's replica of `g` for rebuild.
    pub fn rebuild(&mut self, g: GroupId) {
        self.checksums.order(g);
    }

    /// The core parked a snapshot of range `g` until its data is here.
    pub fn snapshot_data(
        &mut self,
        core: &MultiRaftCore<CoreSm>,
        g: GroupId,
        index: u64,
        now: Nanos,
    ) {
        // The range's end in the snapshot: its data reaches it.
        let end = core_end(core, g);
        self.transfers.want(g, index, end, now);
    }

    /// A request of another replica for a piece of a range's export.
    pub fn on_fetch(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        from: (NodeId, u64),
        f: Fetch,
        now: Nanos,
    ) {
        let lsm = self.lsm.clone();
        self.transfers.on_fetch(core, &mut lsm.borrow_mut(), from, f, now);
    }

    /// Next disk operation of the LSM or the transfers, with its owner.
    pub fn poll_io(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        now: Nanos,
    ) -> Option<(IoOp, IoOwner)> {
        let (op, done) = {
            let mut lsm = self.lsm.borrow_mut();
            let op = lsm.poll_io(now);
            // A job that ended inside `poll_io` (an export) is only told here.
            (op, lsm.take_completions())
        };
        self.lsm_done(core, done, now);
        if let Some(op) = op {
            return Some((op, IoOwner::Lsm));
        }
        if let Some(op) = self.transfers.poll_io() {
            return Some((op, IoOwner::Transfer));
        }
        if let Some(op) = self.backups.poll_io() {
            return Some((op, IoOwner::Backup));
        }
        self.relocations.poll_io().map(|op| (op, IoOwner::Relocate))
    }

    /// The result of an operation of [`DataCore::poll_io`].
    pub fn on_io_done(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        owner: IoOwner,
        done: IoDone,
        now: Nanos,
    ) {
        match owner {
            IoOwner::Lsm => {
                let c = self.lsm.borrow_mut().on_io_done(done, now);
                self.lsm_done(core, c, now);
            }
            IoOwner::Transfer => {
                let lsm = self.lsm.clone();
                self.transfers.on_io_done(core, &mut lsm.borrow_mut(), done, now);
            }
            IoOwner::Relocate => {
                let lsm = self.lsm.clone();
                self.relocations.on_io_done(&lsm, done);
            }
            IoOwner::Backup => {
                let lsm = self.lsm.clone();
                self.backups.on_io_done(core, &mut lsm.borrow_mut(), done);
                self.answer(core, now);
            }
        }
    }

    fn lsm_done(&mut self, core: &mut MultiRaftCore<CoreSm>, c: Completions, now: Nanos) {
        let lsm = self.lsm.clone();
        for (id, files) in c.exported {
            let Err(files) = self.relocations.on_exported(id, files) else { continue };
            if let Err(files) = self.backups.on_exported(id, files) {
                self.transfers.on_exported(id, files);
            }
        }
        for (id, r) in c.ingested {
            let Err(r) = self.relocations.on_ingested(core, &lsm, id, r) else { continue };
            let r = match self.backups.on_ingested(id, r) {
                Ok(restored) => {
                    if let Some(g) = restored {
                        // The log no longer rebuilds what was ingested: a replica behind it gets
                        // a snapshot past it.
                        let ingested = teseladb_range::sm::RangeSm::data_ingested;
                        core.data_entered(g, |sm| sm.range_mut().into_iter().for_each(ingested));
                        // That snapshot is saved once the memtable's entries are down: they go
                        // now, rather than when the core is next idle, so that a replica behind
                        // it does not wait on the load.
                        lsm.borrow_mut().flush();
                    }
                    continue;
                }
                Err(r) => r,
            };
            let ready = self.transfers.on_ingested(core, &mut lsm.borrow_mut(), id, r, now);
            match ready {
                // The range as its split left it: the replica opens again with it.
                Some(g) if core.awaited_snapshot(g).is_none() => {
                    if core.close_group(g).is_ok() {
                        self.host.remove_range(g);
                        self.replicas.hold(g);
                        self.replicas.release(g);
                    }
                }
                Some(g) => core.snapshot_data_ready(g, now),
                None => {}
            }
        }
        for id in c.cleared {
            if self.relocations.on_cleared(&lsm, id) {
                continue;
            }
            self.transfers.on_cleared(core, &mut lsm.borrow_mut(), id);
            self.checksums.on_cleared(&mut self.replicas, id);
        }
    }
}

/// The end of the range in the snapshot `g` waits for (empty: unknown).
fn core_end(core: &MultiRaftCore<CoreSm>, g: GroupId) -> Vec<u8> {
    let sm = core.state_machine(g).and_then(CoreSm::range);
    sm.zip(core.awaited_snapshot(g))
        .and_then(|(sm, snap)| sm.snapshot_end(snap))
        .unwrap_or_default()
}

/// The answer of the host to a [`RemoteReq`].
fn remote_reply(reply: Reply) -> RemoteReply {
    match reply {
        Reply::Mutated { ts, result, .. } => RemoteReply::Written { ts, result },
        Reply::Batch(rows) => {
            RemoteReply::Rows(rows.into_iter().map(|(k, (_, v))| (k, v)).collect())
        }
        Reply::Value { ts, value } => RemoteReply::Value { ts, value },
        Reply::Intents { ts } => RemoteReply::Intents { ts },
        Reply::Record(record) => RemoteReply::Record(record),
        Reply::Changes { rows, to, oldest_intent } => {
            RemoteReply::Changes { rows, to, oldest_intent }
        }
        Reply::Fenced { ts } => RemoteReply::Fenced { ts },
    }
}

/// A mutation that fails with the error it carries.
struct Refuse(MutationError);

impl Mutation for Refuse {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        Err(self.0.clone())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::ranges::Ranges;
    use std::sync::mpsc::{Receiver, channel};
    use teseladb_lsm::LsmConfig;
    use teseladb_meta::group0::{RangeDesc, ReplicaRole};
    use teseladb_multiraft::CoreConfig;
    use teseladb_raftlog::{CoreStore, MemDisk, StorageConfig};

    use crate::forward::FetchReply;
    use crate::transfer::FETCH_RETRY;

    const ME: NodeId = NodeId(1);
    const G: GroupId = GroupId(4);

    fn desc(voters: &[u64]) -> RangeDesc {
        RangeDesc {
            group: G,
            start: vec![1],
            end: vec![4],
            core: 0,
            replicas: voters.iter().map(|n| (NodeId(*n), ReplicaRole::Voter)).collect(),
            replica_epoch: 1,
            generation: 1,
            leader_hint: None,
            leader_term: 0,
            pending_split: None,
            settling: false,
        }
    }

    fn setup(voters: &[u64]) -> (MultiRaftCore<CoreSm>, DataCore) {
        let (core, d, _) = setup_disk(voters);
        (core, d)
    }

    /// [`setup`] with the disk the core's LSM is on.
    fn setup_disk(voters: &[u64]) -> (MultiRaftCore<CoreSm>, DataCore, MemDisk) {
        let mut disk = MemDisk::new();
        let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
        let core = MultiRaftCore::new(CoreConfig::new(ME, 0), store, 1);
        let lcfg = LsmConfig { dir: "core-00/lsm".into(), ..LsmConfig::default() };
        let lsm = Rc::new(RefCell::new(Lsm::recover(&lcfg, &mut disk).unwrap().0));
        let t = Transfers::new(ME, "core-00/t".into(), 1);
        let mut d = DataCore::new(ME, 0, lsm, NodeClock::for_node(ME), t, 1 << 20);
        d.set_ranges(&Ranges { descs: vec![desc(voters)], ..Ranges::default() });
        (core, d, disk)
    }

    /// Answers the forward in flight with `reply`; the request it answered was the core's.
    fn answer_forward(
        core: &mut MultiRaftCore<CoreSm>,
        d: &mut DataCore,
        reply: &[u8],
        now: Nanos,
    ) {
        let done = || Ok(Done::Forwarded(Bytes::from_vec(reply.to_vec())));
        let answered = (0..256).any(|op| d.on_done(core, OpId(op), done(), now));
        assert!(answered, "a forward in flight");
    }

    /// Ticks a lone replica until it leads its range and applied an entry, as the replica of a
    /// cluster's first range does: the time it got there.
    fn lead_alone(core: &mut MultiRaftCore<CoreSm>, d: &mut DataCore, disk: &mut MemDisk) -> Nanos {
        use teseladb_raftlog::Disk;
        let mut now = Nanos::from_secs(1);
        for _ in 0..1_000 {
            now = now + Nanos::from_millis(10);
            core.on_tick(now, 0);
            while let Some(op) = core.poll_io(now) {
                let done = disk.execute(op);
                core.on_io_done(done, now);
            }
            let _ = core.drain_actions();
            d.poll(core, (now, 0));
            run_io(core, d, disk, now);
            if core.state_machine(G).is_some_and(|sm| sm.applied_index() > 0) {
                return now;
            }
        }
        panic!("the replica never applied an entry")
    }

    /// Runs the disk operations of the core's LSM and transfers until none is left.
    fn run_io(core: &mut MultiRaftCore<CoreSm>, d: &mut DataCore, disk: &mut MemDisk, now: Nanos) {
        use teseladb_raftlog::Disk;
        while let Some((op, owner)) = d.poll_io(core, now) {
            let done = disk.execute(op);
            d.on_io_done(core, owner, done, now);
        }
    }

    #[test]
    fn group0s_history_and_load_settings_reach_the_host() {
        let (_, mut d) = setup(&[1]);
        let ranges = Ranges {
            descs: vec![desc(&[1])],
            history_ms: Some(5_000),
            load_split_ops: Some(70),
            ..Ranges::default()
        };
        // The host's setters are tested in `teseladb-range`; here the ranges still apply.
        d.set_ranges(&ranges);
        assert_eq!(d.replicas.descs().count(), 1);
    }

    #[test]
    fn a_follower_scan_without_candidates_left_goes_as_any_request() {
        let (mut core, mut d) = setup(&[2, 3]);
        let (tx, _rx) = channel();
        d.follower_scan(&mut core, G, scan(), tx, (Nanos::ZERO, 0));
        assert_eq!(d.forwarded.len(), 1, "to the range's leader");
    }

    /// A request for the replica on a node goes to that node even while the router does not
    /// count it among the voters yet (a learner that a restore sends its files to): sent to the
    /// leader instead, the files would never reach it and its ingestion would find nothing.
    #[test]
    fn a_request_for_a_replica_goes_to_its_node_even_as_a_learner() {
        let (mut core, mut d) = setup(&[2, 3]);
        let (tx, _rx) = channel();
        let piece = RemoteReq::RestorePiece { file: 0, offset: 0, data: Vec::new() };
        d.request(&mut core, DataReq::At(NodeId(9), G, piece), tx, (Nanos::ZERO, 0));
        let asked: Vec<NodeId> = d.forwarded.values().map(|(_, _, n)| *n).collect();
        assert_eq!(asked, vec![NodeId(9)]);
    }

    #[test]
    fn a_replica_no_snapshot_reaches_asks_for_its_range_and_the_stale_answers_are_rejected() {
        let (mut core, mut d, _disk) = setup_disk(&[1, 2]);
        let t0 = Nanos::from_secs(1);
        d.poll(&mut core, (t0, 0));
        assert!(core.state_machine(G).is_some_and(StateMachine::needs_snapshot));
        assert!(!d.transfers.waiting(), "not before UNSERVED_FETCH");
        let t1 = t0 + UNSERVED_FETCH;
        d.poll(&mut core, (t1, 0));
        assert!(d.transfers.waiting_for(G), "the range as its split left it");
        d.poll(&mut core, (t1, 0));
        // The other replica applied a split since: no replica serves it.
        answer_forward(&mut core, &mut d, &FetchReply::Stale.encode(), t1);
        d.poll(&mut core, (t1, 0));
        assert!(core.state_machine(G).is_some(), "not closed before UNSERVED_FETCH");
        assert_eq!(d.stale.get(&G), Some(&t1), "rejected since now");
        // The core does not wait for that snapshot: the rejection is forgotten.
        d.poll(&mut core, (t1 + FETCH_RETRY, 0));
        assert!(d.stale.is_empty());
    }

    #[test]
    fn a_restore_whose_file_the_lsm_refuses_ends_in_an_error_and_takes_no_snapshot() {
        let (mut core, mut d, mut disk) = setup_disk(&[1]);
        let now = lead_alone(&mut core, &mut d, &mut disk);
        let ask = |core: &mut MultiRaftCore<CoreSm>, d: &mut DataCore, r: RemoteReq| {
            let (tx, rx) = channel();
            d.request(core, DataReq::At(ME, G, r), tx, (now, 0));
            rx.try_recv().expect("answered at once")
        };
        let piece = RemoteReq::RestorePiece { file: 0, offset: 0, data: b"not an sst".to_vec() };
        assert!(ask(&mut core, &mut d, piece).is_ok());
        assert!(ask(&mut core, &mut d, RemoteReq::RestoreIngest { files: 1 }).is_ok());
        let before = core.snapshot_index(G);
        run_io(&mut core, &mut d, &mut disk, now);
        let r = ask(&mut core, &mut d, RemoteReq::RestoreIngest { files: 1 });
        assert!(matches!(&r, Err(RangeError::Mutation(m)) if m.0.contains("failed")), "{r:?}");
        assert_eq!(core.snapshot_index(G), before);
    }

    /// A change stream's read reaches the leader's host and comes back with its rows (014 FR-003).
    #[test]
    fn a_change_streams_read_runs_on_the_leader() {
        let (mut core, mut d, mut disk) = setup_disk(&[1]);
        let from = Hlc { physical: 1, logical: 0 };
        let changes = RemoteReq::Changes { start: vec![1], end: vec![4], from };
        let mut answer = None;
        for ms in (1_000..30_000).step_by(10) {
            let now = Nanos::from_millis(ms);
            run_io(&mut core, &mut d, &mut disk, now);
            d.poll(&mut core, (now, ms));
            let (tx, rx) = channel();
            d.request(&mut core, DataReq::Remote(G, changes.clone()), tx, (now, ms));
            d.poll(&mut core, (now, ms));
            if let Ok(Ok(DataReply::Remote(r))) = rx.try_recv() {
                answer = Some(r);
                break;
            }
        }
        match answer {
            Some(RemoteReply::Changes { rows, to, .. }) => assert!(rows.is_empty() && to >= from),
            other => panic!("no changes: {other:?}"),
        }
    }

    #[test]
    fn a_replica_waiting_for_its_first_snapshot_keeps_a_restore_s_files_until_it_has_it() {
        // 007 FR-021: the snapshot it waits for holds the range as it was before the restore,
        // and installing it after the restore's files would drop them.
        let (mut core, mut d, mut disk) = setup_disk(&[1, 2]);
        let now = Nanos::from_secs(1);
        d.poll(&mut core, (now, 0));
        assert!(core.state_machine(G).is_some_and(StateMachine::needs_snapshot));
        let ask = |core: &mut MultiRaftCore<CoreSm>, d: &mut DataCore, r: RemoteReq| {
            let (tx, rx) = channel();
            d.request(core, DataReq::At(ME, G, r), tx, (now, 0));
            rx.try_recv().expect("answered at once")
        };
        let piece = RemoteReq::RestorePiece { file: 0, offset: 0, data: b"not an sst".to_vec() };
        assert!(ask(&mut core, &mut d, piece).is_ok());
        let not_yet = Ok(DataReply::Remote(RemoteReply::Restored { done: false }));
        assert_eq!(ask(&mut core, &mut d, RemoteReq::RestoreIngest { files: 1 }), not_yet);
        run_io(&mut core, &mut d, &mut disk, now);
        // Nothing went to the LSM: the files wait, staged, for the snapshot.
        assert_eq!(ask(&mut core, &mut d, RemoteReq::RestoreIngest { files: 1 }), not_yet);
    }

    #[test]
    fn a_read_at_a_mark_runs_on_the_replica_here() {
        let (mut core, mut d) = setup(&[1]);
        d.poll(&mut core, (Nanos::from_secs(1), 0));
        assert!(d.host_has(G), "the replica is open");
        let (tx, rx) = channel();
        let at = Hlc { physical: 5, logical: 0 };
        let read = RemoteReq::Read { key: vec![1, 1], at: Some(at) };
        d.request(&mut core, DataReq::Remote(G, read), tx, (Nanos::from_secs(1), 0));
        d.poll(&mut core, (Nanos::from_secs(1), 0));
        // The replica waits for its range's data: the read waits on the host with it.
        assert!(rx.try_recv().is_err());
        assert_eq!(d.clients.len(), 1);
    }

    #[test]
    fn a_follower_scan_that_got_no_answer_goes_to_the_next_candidate() {
        let (mut core, mut d) = setup(&[2, 3]);
        let (tx, _rx) = channel();
        let r = follower(&[(2, true), (3, false)]);
        d.request(&mut core, DataReq::Remote(G, r), tx, (Nanos::ZERO, 0));
        let op = *d.follower_forwards.keys().next().unwrap();
        assert!(d.on_done(&mut core, op, Err(OpError::Timeout), Nanos::ZERO));
        let (_, (_, _, sent)) =
            d.follower_forwards.iter().next().map(|(o, v)| (*o, v.clone())).unwrap();
        assert_eq!(sent, follower(&[(3, false)]));
    }

    #[test]
    fn the_range_its_split_left_reopens_the_replica_once_ingested() {
        for close_first in [false, true] {
            let (mut core, mut d, mut disk) = setup_disk(&[1, 2]);
            let t0 = Nanos::from_secs(1);
            d.poll(&mut core, (t0, 0));
            let t1 = t0 + UNSERVED_FETCH;
            d.poll(&mut core, (t1, 0));
            d.poll(&mut core, (t1, 0));
            // An empty range: only the bundle's last file, ingested at once.
            let last = teseladb_range::snapshot::trailer(0, 0);
            let empty = teseladb_range::snapshot::Chunk {
                range: G,
                index: 0,
                file: 0,
                files: 1,
                offset: 0,
                size: last.len() as u64,
                data: last,
            };
            answer_forward(&mut core, &mut d, &FetchReply::Chunk(empty).encode(), t1);
            if close_first {
                core.close_group(G).unwrap();
            }
            run_io(&mut core, &mut d, &mut disk, t1);
            assert_eq!(d.transfers.ingested(), 1);
            assert!(core.state_machine(G).is_none(), "closed to open again with it");
        }
    }

    #[test]
    fn a_snapshot_of_a_range_merged_away_waits_for_the_merge() {
        let (mut core, mut d, _disk) = setup_disk(&[1, 2]);
        let t0 = Nanos::from_secs(1);
        d.poll(&mut core, (t0, 0));
        d.poll(&mut core, (t0 + UNSERVED_FETCH, 0));
        assert!(d.transfers.waiting_for(G));
        // Group 0 no longer has the range, and gave a higher number since.
        let gone = Ranges { last_group: G.0 + 1, ..Ranges::default() };
        d.set_ranges(&gone);
        d.poll(&mut core, (t0 + UNSERVED_FETCH, 0));
        assert!(!(0..256).any(|op| d.forwarded.contains_key(&OpId(op))));
    }

    fn scan() -> RemoteReq {
        RemoteReq::Scan { start: vec![1], end: vec![2], limit: 1, concern: ReadConcern::Local }
    }

    /// Sends a request for `G` and gives the forward it became.
    fn forward(
        core: &mut MultiRaftCore<CoreSm>,
        d: &mut DataCore,
    ) -> (OpId, NodeId, Receiver<Result<DataReply, RangeError>>) {
        let (tx, rx) = channel();
        d.request(core, DataReq::Remote(G, scan()), tx, (Nanos::ZERO, 0));
        let (op, (_, _, node)) = d.forwarded.iter().next().map(|(o, v)| (*o, v.clone())).unwrap();
        (op, node, rx)
    }

    #[test]
    fn a_node_without_a_replica_forwards_and_follows_the_answers() {
        let (mut core, mut d) = setup(&[2, 3]);
        let now = Nanos::ZERO;
        // First the first voter; a timeout moves to the next one.
        let (op, node, rx) = forward(&mut core, &mut d);
        assert_eq!(node, NodeId(2));
        assert!(d.on_done(&mut core, op, Err(OpError::Timeout), now));
        assert_eq!(rx.recv().unwrap(), Err(RangeError::Timeout));
        let (op, node, rx) = forward(&mut core, &mut d);
        assert_eq!(node, NodeId(3));
        // A `NotLeader` that names the other voter.
        let body = encode_reply(&Err(RangeError::NotLeader { hint: NodeId(2) }));
        assert!(d.on_done(&mut core, op, Ok(Done::Forwarded(Bytes::from_vec(body))), now));
        assert_eq!(rx.recv().unwrap(), Err(RangeError::NotLeader { hint: NodeId(2) }));
        let (op, node, rx) = forward(&mut core, &mut d);
        assert_eq!(node, NodeId(2));
        // An answer that does not decode, or of another kind.
        let junk = Ok(Done::Forwarded(Bytes::from_vec(vec![0xff, 0xff])));
        assert!(d.on_done(&mut core, op, junk, now));
        assert_eq!(rx.recv().unwrap(), Err(RangeError::Unavailable));
        let (op, _, rx) = forward(&mut core, &mut d);
        assert!(d.on_done(&mut core, op, Ok(Done::Read { index: 1 }), now));
        assert_eq!(rx.recv().unwrap(), Err(RangeError::Unavailable));
        let (op, _, rx) = forward(&mut core, &mut d);
        let rows = encode_reply(&Ok(RemoteReply::Rows(vec![])));
        assert!(d.on_done(&mut core, op, Ok(Done::Forwarded(Bytes::from_vec(rows))), now));
        assert_eq!(rx.recv().unwrap(), Ok(DataReply::Remote(RemoteReply::Rows(vec![]))));
        // A failure the leader decided.
        let (op, _, rx) = forward(&mut core, &mut d);
        let failed = encode_reply(&Err(RangeError::Unsatisfiable));
        assert!(d.on_done(&mut core, op, Ok(Done::Forwarded(Bytes::from_vec(failed))), now));
        assert_eq!(rx.recv().unwrap(), Err(RangeError::Unsatisfiable));
        assert!(!d.on_done(&mut core, OpId(999_999), Err(OpError::Timeout), now));
    }

    #[test]
    fn a_hint_that_is_no_voter_is_ignored_and_answers_without_a_route_teach_nothing() {
        let (mut core, mut d) = setup(&[2, 3]);
        let now = Nanos::ZERO;
        // A hint outside the range's voters goes to the leader the router knows, or a voter.
        let (tx, rx) = channel();
        d.forward(&mut core, G, scan(), tx, NodeId(9), now);
        let (op, (_, _, node)) = d.forwarded.iter().next().map(|(o, v)| (*o, v.clone())).unwrap();
        assert_eq!(node, NodeId(2));
        // The range left Group 0 while its request travelled: the answer still reaches the
        // client, and the router has no route to correct.
        d.set_ranges(&Ranges::default());
        let rows = encode_reply(&Ok(RemoteReply::Rows(vec![])));
        assert!(d.on_done(&mut core, op, Ok(Done::Forwarded(Bytes::from_vec(rows))), now));
        assert_eq!(rx.recv().unwrap(), Ok(DataReply::Remote(RemoteReply::Rows(vec![]))));
    }

    #[test]
    fn host_answers_nobody_waits_for_and_ingestions_of_no_transfer_are_dropped() {
        let (mut core, mut d) = setup(&[1]);
        let now = Nanos::ZERO;
        // A read the host answers for a range it does not hold, asked by no client of this core.
        d.host.get(&mut core, GroupId(77), &[1], ReadConcern::Local, now, 0);
        d.answer(&mut core, now);
        assert!(d.clients.is_empty() && d.host.take_replies().is_empty());
        // The end of an ingestion no transfer waits for asks for no snapshot.
        let c = Completions { ingested: vec![(42, Ok(()))], ..Completions::default() };
        d.lsm_done(&mut core, c, now);
        assert_eq!(d.transfers.ingested(), 0);
    }

    #[test]
    fn a_range_led_by_this_node_but_not_open_here_names_no_leader() {
        // The router's leader is this node, which has not opened the range: another voter.
        let (mut core, mut d) = setup(&[1, 2]);
        let (_, node, _) = forward(&mut core, &mut d);
        assert_eq!(node, NodeId(2));
        // A forwarded request for it names no leader.
        let body = encode_for(G, &scan());
        d.request_remote(&mut core, (NodeId(2), 5), &body, (Nanos::ZERO, 0));
        let (mut core, mut d) = setup(&[1]);
        let (tx, rx) = channel();
        d.request(&mut core, DataReq::Remote(G, scan()), tx, (Nanos::ZERO, 0));
        assert_eq!(rx.recv().unwrap(), Err(RangeError::NotLeader { hint: NodeId(0) }));
        d.request_remote(&mut core, (NodeId(2), 6), &body, (Nanos::ZERO, 0));
        // The status of a range this node does not hold.
        let (tx, rx) = channel();
        d.request(&mut core, DataReq::Status(G), tx, (Nanos::ZERO, 0));
        let Ok(DataReply::Status(s)) = rx.recv().unwrap() else { panic!() };
        assert_eq!((s.role, s.bounds), (None, (Vec::new(), Vec::new())));
    }

    fn follower(nodes: &[(u64, bool)]) -> RemoteReq {
        RemoteReq::FollowerScan {
            start: vec![1],
            end: vec![2],
            limit: 1,
            max_stale_ms: None,
            nodes: nodes.iter().map(|(n, l)| (NodeId(*n), *l)).collect(),
        }
    }

    #[test]
    fn a_hint_naming_this_node_goes_to_another_voter() {
        let (mut core, mut d) = setup(&[1, 2, 3]);
        let (tx, _rx) = channel();
        d.forward(&mut core, G, scan(), tx, ME, Nanos::ZERO);
        let (_, (_, _, node)) = d.forwarded.iter().next().map(|(o, v)| (*o, v.clone())).unwrap();
        assert_ne!(node, ME);
    }

    #[test]
    fn a_follower_scan_skips_this_node_when_it_has_no_replica() {
        let (mut core, mut d) = setup(&[1, 2]);
        let (tx, _rx) = channel();
        let r = follower(&[(1, false), (2, true)]);
        d.request(&mut core, DataReq::Remote(G, r), tx, (Nanos::ZERO, 0));
        let (_, (_, g, sent)) =
            d.follower_forwards.iter().next().map(|(o, v)| (*o, v.clone())).unwrap();
        assert_eq!(g, G);
        assert_eq!(sent, follower(&[(2, true)]), "forwarded without this node");
        // The candidate after the last one: none; any other request stays as it is.
        assert_eq!(DataCore::next_candidate(follower(&[])), follower(&[]));
        assert_eq!(DataCore::next_candidate(scan()), scan());
    }

    #[test]
    fn a_core_change_message_that_could_not_leave_goes_with_the_next_ones() {
        let (_, mut d) = setup(&[1]);
        d.requeue_relocate(1, RelocateMsg::Imported { group: G });
        let msgs = d.take_relocate_msgs();
        assert!(matches!(msgs[..], [(1, RelocateMsg::Imported { group: G })]));
    }
}
