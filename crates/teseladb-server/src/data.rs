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
use std::sync::Arc;
use std::sync::mpsc::Sender;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Completions, Lsm, Polled};
use teseladb_meta::command::Group0Command;
use teseladb_multiraft::{Done, MultiRaftCore, NodeClock, OpError, OpId, StateMachine};
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
use teseladb_wire::commands::mutation::Sent;
use teseladb_raft::Role;
use teseladb_raftlog::{IoDone, IoOp};
use teseladb_types::{Bytes, GroupId, Nanos, NodeId};

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
use crate::pressure::{CorePressure, NodePressure};
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
/// A request that waits this long in its range is written to the log, once.
const LONG_WAIT: Nanos = Nanos::from_secs(10);
/// How often the LSM learns the live bytes of the core's ranges, which bound its files' space.
const LIVE_EVERY: Nanos = Nanos::from_secs(5);
/// How often a core counts its memtables and its log with the node's.
const PRESSURE_EVERY: Nanos = Nanos::from_millis(100);
/// Longest a memtable holds back a range's log before it is flushed, however busy its core:
/// RefEngine's checkpoint period (`syncPeriodSecs`), after which RefDb removes old journal files.
pub const CHECKPOINT_EVERY: Nanos = Nanos::from_secs(60);

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
    /// Where a split by size would cut this node's replica of a range: the middle key of its
    /// data. Asked apart from [`DataReq::Status`] because measuring walks the range's memtable,
    /// which periodic status checks did on the core of the data range several times a second
    /// (H-007 of the native tests).
    Cut(GroupId),
    /// A write or a scan of the CRUD commands (006 FR-014) for a range: run here if this replica
    /// leads it, or forwarded to its leader.
    Remote(GroupId, RemoteReq),
    /// A request for the replica of a range on node `.0` (a restore's, 007 FR-021): run here if
    /// it is this node, or forwarded to it.
    At(NodeId, GroupId, RemoteReq),
    /// The sizes of this core's LSM, for the metrics (004 FR-019).
    Lsm,
    /// A scan of a range this replica leads (`.1`, a [`RemoteReq::Scan`]) whose documents go
    /// through `.2`, the `$match`es and partial `$group` of an aggregation, on this core (010
    /// FR-034): the answer is the partial groups, not the documents.
    Partial(GroupId, RemoteReq, Fold),
}

/// The program of an aggregation that a core runs over the documents of its range (010 FR-034).
/// Two are equal when they are the same program.
#[derive(Clone, Debug)]
pub struct Fold(pub Arc<teseladb_exec::pipeline::Program>);

impl PartialEq for Fold {
    fn eq(&self, other: &Fold) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Fold {}

/// The answer to a [`DataReq::Partial`].
#[derive(Clone, Debug)]
pub struct Folded {
    /// The partial groups of the documents read; `None` when the core declined them.
    pub groups: Option<Arc<teseladb_exec::pipeline::PartialGroups>>,
    /// Documents read.
    pub examined: usize,
    /// Where the scan of the range goes on, when it stopped at its number of documents.
    pub next: Option<Vec<u8>>,
}

impl PartialEq for Folded {
    fn eq(&self, other: &Folded) -> bool {
        let same = match (&self.groups, &other.groups) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (a, b) => a.is_none() && b.is_none(),
        };
        same && self.examined == other.examined && self.next == other.next
    }
}

impl Eq for Folded {}

/// The sizes of a core's LSM: its files by level and what it holds in memory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LsmSizes {
    /// Bytes of the files of each level, level 0 first.
    pub level_bytes: Vec<u64>,
    /// Bytes of its history files (004 FR-031), which `level_bytes` counts too.
    pub history_bytes: u64,
    /// Files of each level, level 0 first.
    pub level_files: Vec<usize>,
    /// Bytes in its memtables, frozen ones included.
    pub memtable_bytes: u64,
    /// Bytes in its block cache.
    pub cache_bytes: u64,
    /// Compactions it ran since it opened.
    pub compactions: u64,
    /// The live bytes of the core's ranges it was last told.
    pub live_bytes: u64,
    /// Bytes of its files after its last compaction of everything that their space asked for.
    pub full_after: u64,
    /// Reads and writes of the core's ranges that waited for another write's latch.
    pub latch_waits: u64,
    /// Bytes the core's Raft log takes on disk, and those of the entries it holds in memory
    /// (012 FR-016, FR-018).
    pub log_disk_bytes: u64,
    /// Bytes of the Raft entries the core's groups hold in memory.
    pub log_memory_bytes: u64,
    /// Bytes its LSM wrote to files per thousand bytes its callers wrote.
    pub write_amp_milli: u64,
    /// Writes that came while the LSM asked to slow down, and while it rejected them.
    pub stalls: u64,
    /// Writes that came while the LSM rejected proposals.
    pub rejects: u64,
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
    /// The answer to a [`DataReq::Cut`] (`None`: no replica, or no data to cut).
    Cut(Option<Vec<u8>>),
    /// The answer to a [`DataReq::Remote`].
    Remote(RemoteReply),
    /// The answer to a [`DataReq::Lsm`].
    Lsm(LsmSizes),
    /// The answer to a [`DataReq::Partial`].
    Partial(Folded),
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
    /// The handle's scan for an aggregation's program (010 FR-034), of at most `.2` documents.
    Partial(DataSender, Fold, usize),
}

/// The answer to a [`DataReq::Partial`] from the answer `r` of its scan of at most `limit`
/// documents: the program run over the rows, on this core. A leader that moved, or an answer
/// without rows, goes back as an error and the caller reads the range itself.
fn folded(
    r: Result<RemoteReply, RangeError>,
    fold: &Fold,
    limit: usize,
) -> Result<DataReply, RangeError> {
    match r? {
        RemoteReply::Rows(rows) => {
            let next = match (rows.len() >= limit, rows.last()) {
                (true, Some((k, _))) => Some([k.as_slice(), &[0]].concat()),
                _ => None,
            };
            let groups = fold.0.run(&rows).map(Arc::new);
            Ok(DataReply::Partial(Folded { groups, examined: rows.len(), next }))
        }
        _ => Err(RangeError::Unavailable),
    }
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
    /// The failure of the core's LSM was written to the log.
    failure_told: bool,
    /// When the LSM learns the live bytes of the core's ranges next.
    live_at: Nanos,
    /// The compactions `compact` asked for (DV-017): the bytes of the files when each was asked,
    /// and who waits for the bytes it freed.
    compactions: BTreeMap<u64, (u64, std::sync::mpsc::Sender<u64>)>,
    /// Its part in what the node's cores hold together (012 FR-018, FR-019), and when it counts
    /// it next.
    pressure: Option<(CorePressure, Nanos)>,
    /// The LSM left background work that waits for no disk operation (004 FR-030).
    lsm_more: bool,
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
            failure_told: false,
            live_at: Nanos::ZERO,
            compactions: BTreeMap::new(),
            pressure: None,
            lsm_more: false,
        }
    }

    /// Whether the LSM has background work that waits for no disk operation (004 FR-030): the
    /// core's loop comes back to [`DataCore::poll_io`] without sleeping, and serves what
    /// arrived meanwhile between two steps of that work.
    pub fn has_more(&self) -> bool {
        self.lsm_more
    }

    /// The core counts its memtables and its log with the node's other cores, and flushes for
    /// the node when `node` asks (012 FR-018, FR-019).
    pub fn with_pressure(mut self, node: Arc<NodePressure>) -> DataCore {
        self.pressure = Some((CorePressure::new(node), Nanos::ZERO));
        self
    }

    /// Compacts the core's LSM for `compact` (DV-017, H-054): `reply` gets the bytes of files it
    /// freed once the memtable is in files and every file was compacted.
    pub fn compact(&mut self, reply: std::sync::mpsc::Sender<u64>) {
        let mut lsm = self.lsm.borrow_mut();
        let before = lsm.metrics().level_bytes.iter().sum::<u64>();
        let id = lsm.compact_now();
        self.compactions.insert(id, (before, reply));
    }

    /// Core changes with their files under `dir` and the node's table of which cores hold each
    /// range (007 FR-011); with `link`, the node's cores share one file system and a range that
    /// changes core hands over its whole files as hard links (004 FR-033).
    pub fn with_relocations(mut self, dir: PathBuf, hosting: Hosting, link: bool) -> DataCore {
        let core = self.replicas.core();
        self.replicas.set_hosting(hosting.clone());
        let relocations = Relocations::new(core, dir, hosting);
        self.relocations = if link { relocations.linking() } else { relocations };
        self
    }

    /// The node's cores, among which the new half of a split by load moves (007 FR-040).
    pub fn with_cores(mut self, cores: u16) -> DataCore {
        self.replicas.set_cores(cores);
        self
    }

    /// Publishes the views of the ranges this core leads in `views`, which the node's client
    /// workers read (005 FR-028).
    pub fn with_views(mut self, views: teseladb_range::view::RangeViews) -> DataCore {
        self.host.set_views(views);
        self
    }

    /// Reads a client worker served of range `g` without this core: `ops` of them, sampled by
    /// `keys` (005 FR-028). They count for the range's split by load (007 FR-006) only when it has
    /// replicas on other nodes, whose leaders a split can spread: on one node they would split
    /// the range and move its half to another core (FR-040) for reads that use no core, and
    /// each move leaves its keys unserved for a moment (H-051 of the native tests).
    pub fn count_outside(&mut self, g: GroupId, ops: u64, keys: &[Vec<u8>], now: Nanos) {
        let spread = self.replicas.nodes(g).len() > 1;
        self.host.count_outside(g, (ops, keys), spread, now);
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
        of: (GroupId, u64),
        now: Nanos,
    ) {
        let answer = crate::merges::probe_answer((core, &self.lsm.borrow(), &self.host), of, now);
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
        self.host.set_moving(self.relocations.leaving());
        self.host.set_counted_writes(ranges.counted_writes);
        self.host.set_reaching_records(ranges.reaching_records);
        self.host.set_counted_resolutions(ranges.counted_resolutions);
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
            DataReq::Cut(g) => {
                let sm = core.state_machine(g).and_then(CoreSm::range);
                let cut = sm
                    .and_then(|s| self.lsm.borrow().interval_size(s.bounds().0, s.bounds().1).cut);
                let _ = reply.send(Ok(DataReply::Cut(cut)));
                return;
            }
            DataReq::Status(g) => {
                let _ = reply.send(Ok(DataReply::Status(self.status(core, g))));
                return;
            }
            DataReq::Lsm => {
                let latch_waits = self
                    .replicas
                    .open_ranges()
                    .filter_map(|g| self.host.metrics(core, g, now))
                    .map(|m| m.latch_waits)
                    .sum();
                let m = self.lsm.borrow().metrics();
                let sizes = LsmSizes {
                    level_bytes: m.level_bytes,
                    history_bytes: m.history_bytes,
                    level_files: m.level_files,
                    memtable_bytes: m.memtable_bytes,
                    cache_bytes: m.cache_bytes,
                    compactions: m.compactions_total,
                    live_bytes: m.live_bytes,
                    full_after: m.full_after,
                    latch_waits,
                    log_disk_bytes: core.store().wal_disk_bytes(),
                    log_memory_bytes: core.store().entries_bytes(),
                    write_amp_milli: (m.write_amp * 1000.0) as u64,
                    stalls: m.stalls_total,
                    rejects: m.rejects_total,
                };
                let _ = reply.send(Ok(DataReply::Lsm(sizes)));
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
            DataReq::Partial(g, r, fold) => {
                // Only where this replica holds the range: the program does not go to another
                // node, and the caller reads the range itself instead.
                let limit = match &r {
                    RemoteReq::Scan { limit, .. } => usize::try_from(*limit).unwrap_or(usize::MAX),
                    _ => 0,
                };
                if !self.host_has(g) || limit == 0 {
                    let _ = reply.send(Err(RangeError::NotLeader { hint: NodeId(0) }));
                    return;
                }
                self.run(core, (g, r), Client::Partial(reply, fold, limit), (now, wall_ms));
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
        // This node is skipped without a replica, and when it leads the range and the request
        // does not let the leader serve it (`secondary`, which never reads the primary).
        let leads = core.status(g).is_some_and(|st| st.role == Role::Leader);
        while nodes.first().is_some_and(|(n, leader_ok)| {
            *n == self.me && (!self.host_has(g) || (leads && !*leader_ok))
        }) {
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
            RemoteReq::FollowerScan { start, end, limit, max_stale_ms, mut nodes, backward } => {
                if !nodes.is_empty() {
                    nodes.remove(0);
                }
                RemoteReq::FollowerScan { start, end, limit, max_stale_ms, nodes, backward }
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
            RemoteReq::Write { keys, mutation, dedup, wc, deadline, prepared } => {
                // Decoded only if the host has to evaluate it (005 FR-035); a mutation that
                // does not decode fails inside the host as a refused one.
                let m: Box<dyn Mutation> = Box::new(Sent::new(mutation, prepared.0.map(|p| *p)));
                let w = WriteReq { g, keys, m, dedup, wc };
                match deadline {
                    Some(d) => self.host.mutate_until(core, w, d, (now, wall_ms)),
                    None => self.host.mutate(core, w, now, wall_ms),
                }
            }
            RemoteReq::Scan { start, end, limit, concern, backward } => {
                let limit = usize::try_from(limit).unwrap_or(usize::MAX);
                let req = ScanReq { g, start, end, concern, limit, backward };
                self.host.scan(core, req, now, wall_ms)
            }
            // The leader scans as for `primary`; a follower at its closed mark. The replica that
            // leads serves it as the leader even when the router did not know it led yet, if the
            // request lets the leader serve it (each candidate's flag); `follower_scan` skipped it
            // otherwise.
            RemoteReq::FollowerScan { start, end, limit, max_stale_ms, backward, .. } => {
                let limit = usize::try_from(limit).unwrap_or(usize::MAX);
                let leads = core.status(g).is_some_and(|st| st.role == Role::Leader);
                if leads {
                    let concern = ReadConcern::Majority;
                    let req = ScanReq { g, start, end, concern, limit, backward };
                    self.host.scan(core, req, now, wall_ms)
                } else {
                    let view = teseladb_range::push::TxnView::default();
                    let scan = Some((end, limit));
                    let concern = ReadConcern::Local;
                    let spec = ReadSpec {
                        g,
                        key: start,
                        scan,
                        backward,
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
            RemoteReq::ReadWith { key, scan, backward, at, concern, view, uncertain_to } => {
                let scan =
                    scan.map(|(end, limit)| (end, usize::try_from(limit).unwrap_or(usize::MAX)));
                let spec = ReadSpec { g, key, scan, backward, at, concern, view, uncertain_to };
                self.host.read_with(core, spec, now, wall_ms)
            }
            // Every request of a transaction waits for the range's majority (008), but its intents,
            // which answer once in the leader's log: the coordinator checks they were applied
            // before it commits (FR-021, D-160).
            RemoteReq::Txn { op, deadline } => {
                let wc = match op {
                    teseladb_range::host::TxnOp::PutIntents { .. } => {
                        WriteConcern { w: W::Nodes(1), j: false }
                    }
                    _ => WriteConcern { w: W::Majority, j: true },
                };
                let req = TxnReq { g, op, wc };
                match deadline {
                    Some(d) => self.host.txn_until(core, req, d, (now, wall_ms)),
                    None => self.host.txn(core, req, now, wall_ms),
                }
            }
            // A change stream's read, on the leader (014 FR-003).
            RemoteReq::Changes { start, end, from, at } => {
                self.host.changes(core, ChangesReq { g, start, end, from, at }, now)
            }
            // A resharding's write fence, on the leader (018 FR-002).
            RemoteReq::Fence { start, end, on } => self.host.fence(core, (g, start, end), on, now),
            // The live bytes, from the state machine at once (018 FR-017).
            RemoteReq::Live => {
                let live = core.state_machine(g).and_then(CoreSm::range).map(|sm| sm.live_bytes());
                let hint = core.status(g).map_or(NodeId(0), |s| s.leader);
                let r = live.map(|bytes| RemoteReply::Live { bytes });
                self.send_answer(core, client, r.ok_or(RangeError::NotLeader { hint }), now);
                return;
            }
            // A collection's live documents, from the leader's counts (007 FR-039): only the
            // leader has applied every write it acknowledged.
            RemoteReq::DocCount { tenant, collection } => {
                // The next counts read the view, which a quiescent group does not publish.
                self.host.note_reads(core, g, now);
                let st = core.status(g);
                let leads =
                    st.as_ref().is_some_and(|s| s.role == Role::Leader) && core.leader_ready(g);
                let sm = core.state_machine(g).and_then(CoreSm::range).filter(|_| leads);
                let hint = st.map_or(NodeId(0), |s| s.leader);
                let r = sm.map(|sm| RemoteReply::DocCount {
                    n: sm.doc_counts().map(|c| c.get(&(tenant, collection)).copied().unwrap_or(0)),
                });
                self.send_answer(core, client, r.ok_or(RangeError::NotLeader { hint }), now);
                return;
            }
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
            Client::Partial(to, fold, limit) => drop(to.send(folded(r, &fold, limit))),
            Client::Remote(node, req) => {
                let body = encode_reply(&r);
                core.answer_forward(node, req, Bytes::from_vec(body));
            }
        }
    }

    /// Whether no request waits in the host (H-037).
    pub fn idle(&self) -> bool {
        self.host.idle()
    }

    /// Moves the host's requests, the replicas, the leaders' duties and the transfers.
    pub fn poll(&mut self, core: &mut MultiRaftCore<CoreSm>, (now, wall_ms): (Nanos, u64)) {
        self.wall_ms = wall_ms;
        self.replicas.open(core, &mut self.host, &self.lsm);
        // Each replica follows what its state machine applied: a split's cut, the GC threshold
        // handed to the LSM.
        self.host.follow_state(core);
        self.host.poll(core, now, wall_ms);
        for (g, why) in core.take_stops() {
            crate::log::defer_at(
                crate::log::Level::Error,
                crate::log::Pending {
                    id: 0,
                    msg: "a range stopped: it applies, reads and writes nothing more".to_owned(),
                    attr: format!("{{\"range\":{},\"why\":{why:?}}}", g.0),
                },
            );
        }
        if now >= self.live_at {
            self.live_at = Nanos(now.0.saturating_add(LIVE_EVERY.0));
            let live: u64 = self
                .replicas
                .open_ranges()
                .filter_map(|g| core.state_machine(g).and_then(CoreSm::range))
                .map(teseladb_range::sm::RangeSm::live_bytes)
                .sum();
            self.lsm.borrow_mut().set_live_bytes(live);
        }
        for what in self.host.long_waits(now, LONG_WAIT) {
            crate::log::defer(crate::log::Pending {
                id: 0,
                msg: "a request waited in its range longer than 10 s".to_owned(),
                attr: format!("{{\"core\":{},\"request\":{what:?}}}", self.replicas.core()),
            });
        }
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
        // A range whose leader is about to propose `Relocate` proposes no more writes (H-025).
        self.host.set_moving(self.relocations.leaving());
        self.group0.extend(self.relocations.take_group0());
        self.flush_idle(core, now);
        self.relieve(core, now);
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
    /// them holds its log back, or at once, once per mark, for a snapshot someone waits for
    /// ([`MultiRaftCore::snapshot_flush_mark`]).
    fn flush_idle(&mut self, core: &MultiRaftCore<CoreSm>, now: Nanos) {
        let groups = core.group_ids();
        let ranges = groups.iter().filter(|g| self.host_has(**g));
        let applied = ranges
            .clone()
            .filter_map(|g| core.state_machine(*g))
            .map(StateMachine::applied_index)
            .fold(0u64, u64::wrapping_add);
        let held = ranges.clone().any(|g| self.lsm.borrow().release_floor(*g).is_some());
        let (idle, flush) = self.idle.step(applied, held, core.snapshot_flush_mark(), now);
        self.idle = idle;
        if flush {
            self.lsm.borrow_mut().flush();
        }
    }

    /// Counts the core's memtables and its log with the node's, and does what the node asks of
    /// it: its memtable flushed, and the snapshots of the groups that hold its log's oldest
    /// segment (a snapshot the release floor holds back gets its flush from `flush_idle`).
    fn relieve(&mut self, core: &mut MultiRaftCore<CoreSm>, now: Nanos) {
        let Some((pressure, at)) = self.pressure.as_mut() else { return };
        if now < *at {
            return;
        }
        *at = Nanos(now.0.saturating_add(PRESSURE_EVERY.0));
        let memtables = self.lsm.borrow().memtables_bytes();
        let relief = pressure.step(memtables, core.store().wal_disk_bytes(), now);
        if relief.flush {
            self.lsm.borrow_mut().flush();
        }
        if relief.snapshots {
            for holder in core.store().wal_holders() {
                core.snapshot_now(holder.group);
            }
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
        let (polled, done) = {
            let mut lsm = self.lsm.borrow_mut();
            // One step of its background work a turn (004 FR-030).
            let polled = lsm.poll_io_step(now);
            // A job that ended inside `poll_io_step` (an export) is only told here.
            (polled, lsm.take_completions())
        };
        self.lsm_done(core, done, now);
        self.tell_failure();
        self.lsm_more = matches!(polled, Polled::More);
        if let Polled::Op(op) = polled {
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
                self.tell_failure();
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

    /// Writes the failure of the core's LSM to the log the first time it appears: from then on
    /// the core reads and flushes nothing, and its ranges answer that they are unavailable.
    fn tell_failure(&mut self) {
        if self.failure_told {
            return;
        }
        let Some(e) = self.lsm.borrow().failure().cloned() else { return };
        self.failure_told = true;
        crate::log::defer_at(
            crate::log::Level::Error,
            crate::log::Pending {
                id: 0,
                msg: "the storage of a core failed: it reads and flushes nothing more".to_owned(),
                attr: format!(
                    "{{\"core\":{},\"error\":{:?}}}",
                    self.replicas.core(),
                    e.to_string()
                ),
            },
        );
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
            let restore = self.backups.on_ingested(&mut lsm.borrow_mut(), id, r);
            let r = match restore {
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
        for id in c.compacted {
            if let Some((before, reply)) = self.compactions.remove(&id) {
                let after = lsm.borrow().metrics().level_bytes.iter().sum::<u64>();
                let _ = reply.send(before.saturating_sub(after));
            }
        }
        for id in c.cleared {
            if self.relocations.on_cleared(&lsm, id) {
                continue;
            }
            if self.backups.on_cleared(&mut lsm.borrow_mut(), id) {
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
            moved_ms: 0,
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

    /// Runs the disk operations of the core's LSM and transfers until none is left, and the
    /// LSM's background work until it waits for a disk operation.
    fn run_io(core: &mut MultiRaftCore<CoreSm>, d: &mut DataCore, disk: &mut MemDisk, now: Nanos) {
        use teseladb_raftlog::Disk;
        loop {
            match d.poll_io(core, now) {
                Some((op, owner)) => {
                    let done = disk.execute(op);
                    d.on_io_done(core, owner, done, now);
                }
                None if d.has_more() => {}
                None => return,
            }
        }
    }

    #[test]
    fn the_lsm_does_a_step_of_background_work_a_turn_and_the_core_is_asked_again() {
        // 004 FR-030: a flush of 300 KiB yields after each 64 KiB. The core gets no operation
        // for its disk then, and is told that there is more to do without one.
        use teseladb_raftlog::Disk;
        let (mut core, mut d, mut disk) = setup_disk(&[1]);
        {
            let mut lsm = d.lsm.borrow_mut();
            for i in 1..=300u64 {
                let at = teseladb_hlc::Hlc { physical: i, logical: 0 };
                lsm.put(GroupId(900), i, format!("k{i:04}").as_bytes(), at, &[7; 1024]).unwrap();
            }
            lsm.flush();
        }
        let now = Nanos::from_secs(1);
        let (mut yields, mut ops) = (0, 0);
        loop {
            match d.poll_io(&mut core, now) {
                Some((op, owner)) => {
                    assert!(!d.has_more(), "an operation for the disk comes first");
                    ops += 1;
                    let done = disk.execute(op);
                    d.on_io_done(&mut core, owner, done, now);
                }
                None if d.has_more() => yields += 1,
                None => break,
            }
        }
        assert!(yields >= 3, "{yields} yields");
        assert!(ops >= 3, "{ops} operations: the file, its bytes and its sync");
        assert!(!d.has_more());
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

    /// A scan with a read preference that lets the leader serve it (`secondaryPreferred`,
    /// `nearest`) and names this node, while its replica leads (the router had no hint of the
    /// leader yet): the leader takes it as the leader, with what it has applied. As a follower it
    /// had no closed mark yet and refused it at once (a follower's closed mark runs seconds
    /// behind the writes). A `secondary` read never reads the primary: the leader passes it on,
    /// and with no other candidate it is refused.
    #[test]
    fn a_follower_scan_on_the_leader_reads_as_the_leader_when_the_preference_lets_it() {
        let (mut core, mut d, mut disk) = setup_disk(&[1]);
        let now = lead_alone(&mut core, &mut d, &mut disk);
        let (tx, rx) = channel();
        let req = DataReq::Remote(G, follower(&[(1, true)]));
        d.request(&mut core, req, tx, (now, 0));
        d.poll(&mut core, (now, 0));
        let refused = matches!(rx.try_recv(), Ok(Err(RangeError::NotLeader { .. })));
        assert!(!refused, "refused as a follower");
        let (tx, rx) = channel();
        let req = DataReq::Remote(G, follower(&[(1, false)]));
        d.request(&mut core, req, tx, (now, 0));
        d.poll(&mut core, (now, 0));
        let refused = matches!(rx.try_recv(), Ok(Err(RangeError::NotLeader { .. })));
        assert!(refused, "a secondary read served by the primary");
    }

    /// A change stream's read reaches the leader's host and comes back with its rows (014 FR-003).
    #[test]
    fn a_change_streams_read_runs_on_the_leader() {
        let (mut core, mut d, mut disk) = setup_disk(&[1]);
        let from = Hlc { physical: 1, logical: 0 };
        let changes = RemoteReq::Changes { start: vec![1], end: vec![4], from, at: None };
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
        let concern = ReadConcern::Local;
        RemoteReq::Scan { start: vec![1], end: vec![2], limit: 1, concern, backward: false }
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
            backward: false,
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

    #[test]
    fn a_core_flushes_its_memtable_when_the_node_asks() {
        let (mut core, d, mut disk) = setup_disk(&[1]);
        let put = |d: &DataCore, k: u8| {
            let at = Hlc { physical: 1, logical: 0 };
            d.lsm.borrow_mut().put(G, 1, &[1, k], at, &[k; 64]).unwrap();
        };
        // Alone, a core keeps what its memtable holds.
        let mut d = d;
        let now = lead_alone(&mut core, &mut d, &mut disk);
        put(&d, 1);
        d.poll(&mut core, (now, 0));
        assert!(d.lsm.borrow().memtables_bytes().1 > 0);
        // A node with a byte for its memtables is over it: the core, above its share, flushes.
        let node = Arc::new(NodePressure::new(1, 8 << 20, 1));
        let mut d = d.with_pressure(node.clone());
        d.poll(&mut core, (now, 0));
        assert_eq!(d.lsm.borrow().memtables_bytes().1, 0, "the active memtable was frozen");
        assert!(node.memtables() > 0, "the frozen one counts until it is in a file");
        run_io(&mut core, &mut d, &mut disk, now);
        // A log past its cap flushes too, with the node well under its memory: a segment of
        // this core's log is larger than the cap of a node of many cores without segments kept.
        let node = Arc::new(NodePressure::new(u16::MAX, 0, u64::MAX));
        assert!(core.store().wal_disk_bytes() > node.log_cap());
        let mut d = d.with_pressure(node);
        put(&d, 2);
        d.poll(&mut core, (now + Nanos::from_secs(2), 0));
        assert_eq!(d.lsm.borrow().memtables_bytes().1, 0);
    }
}
