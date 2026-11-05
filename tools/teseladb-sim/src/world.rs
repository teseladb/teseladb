//! Simulated world: event scheduler by `(time, sequence)` and nodes with their
//! cores (`CoreStore` over the node's `SimDisk`). The world runs each core's I/O with the simulated
//! disk's latency (one operation in flight per core, like the disk thread) and its deadlines
//! (`next_deadline`); workloads only request work and receive what completed.
//!
//! In cluster mode every core runs a `MultiRaftCore` with `teseladb-testkv` over its store, like the
//! node's core loop: a tick every 10 ms of its local clock, frames over the simulated network, and
//! the operations it ends and what it observes collected for the workload and the checkers.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::path::PathBuf;
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{Lsm, LsmConfig, LsmError};
use teseladb_multiraft::chunks::Chunk;
use teseladb_multiraft::{
    CoreConfig, Done, GroupDesc, GroupHeartbeat, GroupHeartbeatResp, MultiRaftCore, NodeClock,
    Observation, OpError, OpId, PeerFrame, route_stray,
};
use teseladb_range::rollback::{RollbackFile, rollback_files};
use teseladb_range::sm::RangeSm;
use teseladb_raft::{Message, Role};
use teseladb_raftlog::{
    Completions, CoreStore, Disk, FileId, IoDone, IoOp, LogEntry, SimDisk, SimDiskConfig,
    StorageConfig, StorageError,
};
use teseladb_types::{Bytes, Decoder, Encoder, GroupId, Nanos, NodeId, Rng};

use crate::checkers::Violation;
use crate::checkers::raft_safety::{AppliedEntry, RaftSafety, ReadRegistration};
use crate::clock::{ClockLimits, SimClock};
use crate::faults::FaultAction;
use crate::net::{ANY_CONNECTION, NetParams, SimNetwork};
use crate::node::{NodeOutput, NodeSm, Registry, SmKind};
use crate::trace::Trace;
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::GROUP0;

/// Kinds of frame between simulated nodes.
const FRAME_RAFT: u8 = 0;
const FRAME_HEARTBEATS: u8 = 1;
const FRAME_HEARTBEAT_RESPS: u8 = 2;
const FRAME_CHUNK: u8 = 3;
const FRAME_ACK: u8 = 4;
const FRAME_FORWARD: u8 = 5;
const FRAME_FORWARD_RESP: u8 = 6;
/// Snapshots are taken this often in the simulator, so that transfers and restarts from a
/// snapshot happen in every run.
pub const SIM_SNAPSHOT_ENTRIES: u64 = 8;

/// Encodes a frame of a core for the simulated network.
pub fn encode_frame(f: &PeerFrame) -> Bytes {
    let mut e = Encoder::new();
    match f {
        PeerFrame::Raft { wall_ms, hlc, msgs: batch } => {
            e.u8(FRAME_RAFT)
                .u64(*wall_ms)
                .u64(hlc.physical)
                .u32(hlc.logical)
                .var(batch.len() as u64);
            for (g, m) in batch {
                e.u64(g.0);
                m.encode(&mut e);
            }
        }
        PeerFrame::Heartbeats { wall_ms, hlc, beats } => {
            e.u8(FRAME_HEARTBEATS)
                .u64(*wall_ms)
                .u64(hlc.physical)
                .u32(hlc.logical)
                .var(beats.len() as u64);
            for h in beats {
                e.u64(h.group.0).u64(h.term).u64(h.commit).u64(h.lease_seq);
                let zero = teseladb_hlc::Hlc { physical: 0, logical: 0 };
                let (c, index) = h.closed.unwrap_or((zero, 0));
                e.bool(h.closed.is_some()).u64(c.physical).u32(c.logical).u64(index);
            }
        }
        PeerFrame::HeartbeatResps { wall_ms, hlc, resps: rs } => {
            e.u8(FRAME_HEARTBEAT_RESPS)
                .u64(*wall_ms)
                .u64(hlc.physical)
                .u32(hlc.logical)
                .var(rs.len() as u64);
            for r in rs {
                e.u64(r.group.0).u64(r.term).bool(r.ok).u64(r.lease_seq);
            }
        }
        PeerFrame::SnapshotChunk(c) => {
            e.u8(FRAME_CHUNK).u64(c.group.0).u64(c.index).u64(c.term).u64(c.offset);
            e.u64(c.total).bytes(&c.data);
        }
        PeerFrame::SnapshotAck { group, index, offset, ok } => {
            e.u8(FRAME_ACK).u64(group.0).u64(*index).u64(*offset).bool(*ok);
        }
        PeerFrame::Forward { req, deadline_ms, hlc, body } => {
            e.u8(FRAME_FORWARD).u64(*req).u32(*deadline_ms);
            encode_clock(&mut e, *hlc);
            e.bytes(body);
        }
        PeerFrame::ForwardResp { req, hlc, body } => {
            e.u8(FRAME_FORWARD_RESP).u64(*req);
            encode_clock(&mut e, *hlc);
            e.bytes(body);
        }
    }
    Bytes::from_vec(e.finish())
}

/// Writes the sender's HLC of a forwarded request or answer, if it has one.
fn encode_clock(e: &mut Encoder, hlc: Option<Hlc>) {
    e.bool(hlc.is_some());
    if let Some(h) = hlc {
        e.u64(h.physical).u32(h.logical);
    }
}

/// Reads what [`encode_clock`] wrote.
fn decode_clock(d: &mut Decoder<'_>) -> Option<Hlc> {
    let bad = "a frame the simulator encoded";
    d.bool()
        .expect(bad)
        .then(|| Hlc { physical: d.u64().expect(bad), logical: d.u32().expect(bad) })
}

/// The frame answers heartbeats: the leader that gets it renews its lease with it.
pub fn answers_heartbeats(b: &[u8]) -> bool {
    matches!(decode_frame(b), PeerFrame::HeartbeatResps { resps, .. } if !resps.is_empty())
}

/// Decodes a frame written by [`encode_frame`] (the simulator only carries its own frames).
pub fn decode_frame(b: &[u8]) -> PeerFrame {
    let bad = "frame of this world";
    let mut d = Decoder::new(b);
    let f = match d.u8().expect(bad) {
        FRAME_RAFT => {
            let wall_ms = d.u64().expect(bad);
            let hlc =
                teseladb_hlc::Hlc { physical: d.u64().expect(bad), logical: d.u32().expect(bad) };
            let n = d.var().expect(bad);
            let batch = (0..n)
                .map(|_| (GroupId(d.u64().expect(bad)), Message::decode(&mut d).expect(bad)))
                .collect();
            PeerFrame::Raft { wall_ms, hlc, msgs: batch }
        }
        FRAME_HEARTBEATS => {
            let wall_ms = d.u64().expect(bad);
            let hlc =
                teseladb_hlc::Hlc { physical: d.u64().expect(bad), logical: d.u32().expect(bad) };
            let n = d.var().expect(bad);
            let hs = (0..n)
                .map(|_| {
                    let group = GroupId(d.u64().expect(bad));
                    let (term, commit) = (d.u64().expect(bad), d.u64().expect(bad));
                    let lease_seq = d.u64().expect(bad);
                    let some = d.bool().expect(bad);
                    let c = teseladb_hlc::Hlc {
                        physical: d.u64().expect(bad),
                        logical: d.u32().expect(bad),
                    };
                    let index = d.u64().expect(bad);
                    GroupHeartbeat {
                        group,
                        term,
                        commit,
                        lease_seq,
                        closed: some.then_some((c, index)),
                    }
                })
                .collect();
            PeerFrame::Heartbeats { wall_ms, hlc, beats: hs }
        }
        FRAME_HEARTBEAT_RESPS => {
            let wall_ms = d.u64().expect(bad);
            let hlc =
                teseladb_hlc::Hlc { physical: d.u64().expect(bad), logical: d.u32().expect(bad) };
            let n = d.var().expect(bad);
            let rs = (0..n)
                .map(|_| GroupHeartbeatResp {
                    group: GroupId(d.u64().expect(bad)),
                    term: d.u64().expect(bad),
                    ok: d.bool().expect(bad),
                    lease_seq: d.u64().expect(bad),
                })
                .collect();
            PeerFrame::HeartbeatResps { wall_ms, hlc, resps: rs }
        }
        FRAME_CHUNK => PeerFrame::SnapshotChunk(Chunk {
            group: GroupId(d.u64().expect(bad)),
            index: d.u64().expect(bad),
            term: d.u64().expect(bad),
            offset: d.u64().expect(bad),
            total: d.u64().expect(bad),
            data: Bytes::copy_from_slice(d.bytes().expect(bad)),
        }),
        FRAME_ACK => PeerFrame::SnapshotAck {
            group: GroupId(d.u64().expect(bad)),
            index: d.u64().expect(bad),
            offset: d.u64().expect(bad),
            ok: d.bool().expect(bad),
        },
        FRAME_FORWARD => PeerFrame::Forward {
            req: d.u64().expect(bad),
            deadline_ms: d.u32().expect(bad),
            hlc: decode_clock(&mut d),
            body: Bytes::copy_from_slice(d.bytes().expect(bad)),
        },
        _ => PeerFrame::ForwardResp {
            req: d.u64().expect(bad),
            hlc: decode_clock(&mut d),
            body: Bytes::copy_from_slice(d.bytes().expect(bad)),
        },
    };
    d.finish().expect(bad);
    f
}

/// Scheduler event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The disk operation in flight of a core finishes.
    Disk {
        /// Node.
        node: u32,
        /// Core.
        core: u16,
        /// Node generation when it was emitted (if the node crashed, it is dropped).
        life: u64,
    },
    /// Deadline of a core (`next_deadline`).
    Tick {
        /// Node.
        node: u32,
        /// Core.
        core: u16,
        /// Node generation.
        life: u64,
    },
    /// Turn of a workload client.
    Client {
        /// Client.
        client: u32,
    },
    /// Delivery of a message.
    Deliver {
        /// Source.
        from: u32,
        /// Destination.
        to: u32,
        /// Core.
        core: u16,
        /// Connection epoch.
        epoch: u64,
        /// Frame.
        frame: Bytes,
    },
    /// A fault starts.
    Fault(FaultAction),
    /// A fault with a duration ends.
    FaultEnd(FaultAction),
    /// A crashed node comes back.
    Restart {
        /// Node.
        node: u32,
    },
    /// End of the fault window.
    Heal,
    /// Periodic check.
    CheckPoint,
}

impl Event {
    /// Short name for the trace.
    pub fn kind(&self) -> &'static str {
        match self {
            Event::Disk { .. } => "disk",
            Event::Tick { .. } => "tick",
            Event::Client { .. } => "client",
            Event::Deliver { .. } => "deliver",
            Event::Fault(_) => "fault",
            Event::FaultEnd(_) => "fault-end",
            Event::Restart { .. } => "restart",
            Event::Heal => "heal",
            Event::CheckPoint => "check",
        }
    }
}

struct Pending {
    at: Nanos,
    seq: u64,
    ev: Event,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.seq) == (other.at, other.seq)
    }
}

impl Eq for Pending {}

impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Pending {
    /// Min-heap: the first one has the smallest `(time, sequence)`.
    fn cmp(&self, other: &Self) -> Ordering {
        (other.at, other.seq).cmp(&(self.at, self.seq))
    }
}

/// Tick period of a cluster core (production: 10 ms).
pub const CLUSTER_TICK: Nanos = Nanos::from_millis(10);
/// The longest a node waiting for its clock to pass the HLC bound sleeps before it looks again
/// (the server's `start_clock` sleeps at most 50 ms at a time).
pub const START_POLL: Nanos = Nanos::from_millis(50);

/// Core of a node, with its disk (each core has its own I/O executor, like the production disk
/// thread; their files are in different directories).
#[derive(Default)]
pub struct SimCore {
    /// Engine of the core (`None` while the node is down, or in cluster mode).
    pub store: Option<CoreStore>,
    /// Multi-Raft core over the store with `teseladb-testkv` (cluster mode, `None` while the
    /// node is down).
    pub cluster: Option<Box<MultiRaftCore<NodeSm>>>,
    /// Next cluster tick (local time of the node).
    next_tick: Nanos,
    /// The core's LSM (004), beside its log on the same disk; the workload that uses it opens it
    /// and drops it with the rest of the RAM in a crash.
    pub lsm: Option<Rc<RefCell<Lsm>>>,
    /// Disk of the core.
    pub disk: SimDisk,
    inflight: Option<IoOp>,
    /// The operation in flight is the LSM's.
    lsm_inflight: bool,
    /// Rollback files the disk refused, written again on the next flush (lost in a crash).
    rollback: Vec<RollbackFile>,
    /// What the LSM finished (exports and ingestions of range snapshots), for the workload.
    pub lsm_done: teseladb_lsm::Completions,
    tick_at: Option<Nanos>,
    /// Frozen until this instant.
    pub frozen_until: Nanos,
}

/// Node.
pub struct SimNode {
    /// Identifier.
    pub id: u32,
    /// Powered on.
    pub alive: bool,
    /// Generation: it increases on every crash to drop the events of the previous life.
    pub life: u64,
    /// Clock.
    pub clock: SimClock,
    /// Cores.
    pub cores: Vec<SimCore>,
    /// Disk slowness factor (1 = normal).
    pub disk_slow: u64,
    /// A core's WAL was damaged while it was down and its restart has not checked it yet.
    pub corrupted: bool,
    /// Out for good: its recovery found the damage (002 research R3c).
    pub lost: bool,
    /// HLC of the node, shared by its cores (005 FR-003); a new one on every start.
    pub hlc: NodeClock,
    /// The HLC's upper bound in the node's `node.meta` (005 FR-003): it survives crashes, and a
    /// start waits until the wall clock passes it. Written at once and always (the server's
    /// thread retries a failed write, and marks wait for it meanwhile).
    pub hlc_bound: u64,
}

/// An LSM error as the storage reports it when a node starts.
fn lsm_error(e: LsmError) -> StorageError {
    match e {
        LsmError::Corrupt { file, offset } => {
            StorageError::Corrupt { file: format!("lsm/{file:06}.sst").into(), offset }
        }
        LsmError::Stopped(errno) => StorageError::Io { op: "lsm", errno },
        LsmError::ReadOnly | LsmError::Overloaded | LsmError::IngestOverlap => {
            StorageError::DiskFull
        }
    }
}

/// The state machine of a new group of `kind` on core `c`: a range goes over the core's LSM, which
/// opens here the first time.
fn make_sm(
    registry: &Registry,
    range_lsm: Option<&LsmConfig>,
    c: &mut SimCore,
    desc: &GroupDesc,
    kind: SmKind,
) -> Result<NodeSm, StorageError> {
    let SmKind::Range(start, end) = kind else {
        return Ok(registry.make(kind).expect("a registered state machine"));
    };
    if c.lsm.is_none() {
        let cfg = range_lsm.expect("a world with ranges has their LSM's configuration");
        let lsm = Lsm::recover(cfg, &mut c.disk).map_err(lsm_error)?.0;
        c.lsm = Some(Rc::new(RefCell::new(lsm)));
    }
    let lsm = c.lsm.clone().expect("open");
    let sm = RangeSm::open(desc.group, start.to_vec(), end.to_vec(), lsm, &mut c.disk);
    sm.map(NodeSm::Range).map_err(lsm_error)
}

/// The world.
pub struct World {
    /// Seed.
    pub seed: u64,
    /// Single source of randomness.
    pub rng: Rng,
    now: Nanos,
    seq: u64,
    queue: BinaryHeap<Pending>,
    /// Nodes.
    pub nodes: Vec<SimNode>,
    /// Network.
    pub net: SimNetwork,
    /// Trace.
    pub trace: Trace,
    /// Engine configuration.
    pub storage: StorageConfig,
    /// Cores run `MultiRaftCore` with `teseladb-testkv`.
    pub cluster_mode: bool,
    /// Groups of the cluster and what runs them: every node holds each one on core
    /// `group % cores` (Group 0 on core 0).
    pub groups: Vec<(GroupDesc, SmKind)>,
    /// State machines that later specifications register (research R1b).
    pub registry: Registry,
    /// Configuration of the LSM a core opens for its ranges (005).
    pub range_lsm: Option<LsmConfig>,
    /// The fault window ended: NTP resynchronized the clocks, and no clock changes after that.
    pub healed: bool,
    /// A node at the edge of the clock margin that leads (with its distance from the median, ns), to
    /// be cut off for this long as soon as a follower answers its heartbeats, which renews its
    /// lease (D-044).
    pub edge_cut: Option<(u32, i64, Nanos)>,
    /// The last node at the edge of the clock margin cut off while it led, and when: its lease
    /// reads go on until the lease ends, and workloads may read through it meanwhile.
    pub edge_cut_at: Option<(u32, Nanos)>,
    /// Snapshots `(node, core, group, index)` waiting for their data (005 FR-012).
    pub snapshot_waits: Vec<(u32, u16, GroupId, u64)>,
    /// The pending `Bootstrap` of Group 0: node, core and operation.
    bootstrap_op: Option<(u32, u16, OpId)>,
    /// No more `Bootstrap` proposals: the cluster already existed (the final check).
    bootstrap_closed: bool,
    /// Operations the cluster cores ended, for the workload: (node, core, operation, result).
    pub events: Vec<(u32, u16, OpId, Result<Done<NodeOutput>, OpError>)>,
    /// Raft safety checker of the cluster cores.
    pub raft_safety: RaftSafety,
    /// First violation found by the world's checkers.
    pub violation: Option<Violation>,
}

/// Handle of a rollback file while it is written (beside the log's and the LSM's).
const ROLLBACK_FILE: FileId = FileId(1 << 21);

/// Writes `f` durably unless a file is already at its path; `false` if the disk refused it, after
/// removing what it wrote.
fn write_file_once(disk: &mut SimDisk, f: &RollbackFile) -> bool {
    let path = PathBuf::from(&f.path);
    let dir = path.parent().map(PathBuf::from).unwrap_or_default();
    if disk.list(&dir).unwrap_or_default().contains(&path) {
        return true;
    }
    let ops = [
        IoOp::Create { file: ROLLBACK_FILE, path: path.clone(), prealloc: f.bytes.len() as u64 },
        IoOp::WriteAt {
            file: ROLLBACK_FILE,
            offset: 0,
            data: Bytes::from_vec(f.bytes.clone()),
            batch: 0,
        },
        IoOp::Sync { file: ROLLBACK_FILE, batch: 0 },
        IoOp::Close { file: ROLLBACK_FILE },
        IoOp::SyncDir { path: dir.clone() },
    ];
    for op in ops {
        if let IoDone::Err { .. } = disk.execute(op) {
            disk.execute(IoOp::Close { file: ROLLBACK_FILE });
            disk.execute(IoOp::Remove { path });
            disk.execute(IoOp::SyncDir { path: dir });
            return false;
        }
    }
    true
}

/// Storage configuration of the simulator: small files so that they rotate often.
pub fn sim_storage_config() -> StorageConfig {
    StorageConfig {
        wal_segment_size: 1 << 20,
        max_batch_bytes: 64 << 10,
        ..StorageConfig::default()
    }
}

impl World {
    /// World with `nodes` freshly created nodes of `cores` cores.
    pub fn new(seed: u64, nodes: u32, cores: u16, trace: bool) -> World {
        World::with_clocks(seed, nodes, cores, trace, false, ClockLimits::default())
    }

    /// The same with Multi-Raft cores (node `i` is `NodeId(i + 1)`); the workload adds the groups
    /// ([`World::add_group`]).
    pub fn cluster(seed: u64, nodes: u32, cores: u16, trace: bool) -> World {
        World::with_clocks(seed, nodes, cores, trace, true, ClockLimits::default())
    }

    /// A world (with cluster cores if `cluster_mode`) whose nodes draw their clocks within
    /// `clocks`.
    pub fn with_clocks(
        seed: u64,
        nodes: u32,
        cores: u16,
        trace: bool,
        cluster_mode: bool,
        clocks: ClockLimits,
    ) -> World {
        let mut rng = Rng::new(seed);
        let storage = sim_storage_config();
        let mut list = Vec::new();
        for id in 0..nodes {
            let clock = SimClock::within(&mut rng, 1_800_000_000_000, clocks);
            let cores = (0..cores)
                .map(|_| SimCore {
                    disk: SimDisk::new(SimDiskConfig {
                        torn_write_probability: 0.5,
                        ..Default::default()
                    }),
                    ..SimCore::default()
                })
                .collect();
            list.push(SimNode {
                id,
                alive: false,
                life: 0,
                clock,
                cores,
                disk_slow: 1,
                corrupted: false,
                lost: false,
                hlc: NodeClock::for_node(NodeId(u64::from(id) + 1)),
                hlc_bound: 0,
            });
        }
        let mut w = World {
            seed,
            rng,
            now: Nanos::ZERO,
            seq: 0,
            queue: BinaryHeap::new(),
            nodes: list,
            net: SimNetwork::new(nodes as usize, NetParams::default()),
            trace: Trace::new(trace),
            storage,
            cluster_mode,
            groups: Vec::new(),
            registry: Registry::default(),
            range_lsm: None,
            healed: false,
            edge_cut: None,
            edge_cut_at: None,
            snapshot_waits: Vec::new(),
            bootstrap_op: None,
            bootstrap_closed: false,
            events: Vec::new(),
            raft_safety: RaftSafety::default(),
            violation: None,
        };
        if cluster_mode {
            // Group 0 on core 0 of every node, all of them voters (research R1b).
            let voters = (1..=u64::from(nodes)).map(NodeId).collect();
            let desc = GroupDesc { group: GROUP0, voters, learners: Vec::new() };
            w.groups.push((desc, SmKind::Group0));
        }
        for n in 0..nodes {
            w.start_node(n).expect("new disk");
        }
        w
    }

    /// From now on nobody proposes `Bootstrap`, as a server whose cluster already exists: a lost
    /// Group 0 stays lost instead of being created again.
    pub fn close_bootstrap(&mut self) {
        self.bootstrap_closed = true;
    }

    /// Nodes powered on whose Group 0 replica has the cluster created.
    pub fn group0_bootstrapped(&self) -> u32 {
        let created = |n: &SimNode| {
            n.alive
                && n.cores[0].cluster.as_ref().is_some_and(|c| {
                    c.state_machine(GROUP0)
                        .and_then(NodeSm::group0)
                        .is_some_and(|g| g.bootstrapped())
                })
        };
        self.nodes.iter().filter(|n| created(n)).count() as u32
    }

    /// The replica of `node` that leads Group 0 proposes `Bootstrap` while the cluster is not
    /// created, one at a time (all nodes are voters from the start, so the founder may not lead;
    /// a second `Bootstrap` only answers `AlreadyBootstrapped`).
    fn bootstrap_group0(&mut self, node: u32) {
        if self.bootstrap_op.is_some() || self.bootstrap_closed {
            return;
        }
        let local = self.local(node);
        let c = self.nodes[node as usize].cores[0].cluster.as_mut().expect("a powered-on core");
        let created =
            c.state_machine(GROUP0).and_then(NodeSm::group0).is_some_and(|g| g.bootstrapped());
        let leads = c.status(GROUP0).is_some_and(|s| s.role == Role::Leader);
        if created || !leads {
            return;
        }
        let first_node = NodeAddrs {
            name: b"n1".to_vec(),
            peer_addr: b"n1".to_vec(),
            cores: 1,
            ..NodeAddrs::default()
        };
        let cmd =
            Group0Command::Bootstrap { cluster_id: [1; 16], first_node, cert_fingerprint: [0; 32] };
        // A refused proposal is tried again on the next tick.
        self.bootstrap_op = c.propose(GROUP0, &cmd.encode(), local).ok().map(|op| (node, 0, op));
    }

    /// The node whose `Bootstrap` of Group 0 is in flight.
    pub fn group0_pending(&self) -> Option<u32> {
        self.bootstrap_op.map(|(node, _, _)| node)
    }

    /// Current instant.
    pub fn now(&self) -> Nanos {
        self.now
    }

    /// Schedules an event.
    pub fn schedule(&mut self, at: Nanos, ev: Event) {
        self.seq += 1;
        self.queue.push(Pending { at: at.max(self.now), seq: self.seq, ev });
    }

    /// When the next event happens, without taking it.
    #[cfg(test)]
    pub(crate) fn next_at(&self) -> Option<Nanos> {
        self.queue.peek().map(|p| p.at)
    }

    /// How many events are due.
    #[cfg(test)]
    pub(crate) fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Next event (advances the clock), with its sequence number.
    pub fn pop(&mut self) -> Option<(Nanos, u64, Event)> {
        let p = self.queue.pop()?;
        self.now = p.at;
        Some((p.at, p.seq, p.ev))
    }

    /// Local monotonic time of a node.
    pub fn local(&self, node: u32) -> Nanos {
        self.nodes[node as usize].clock.mono(self.now)
    }

    /// Wall time (ms) of a node.
    pub fn wall_ms(&self, node: u32) -> u64 {
        self.nodes[node as usize].clock.wall_ms(self.now)
    }

    /// Engine of a core, if the node is powered on.
    pub fn store(&mut self, node: u32, core: u16) -> Option<&mut CoreStore> {
        self.nodes[node as usize].cores[core as usize].store.as_mut()
    }

    /// Multi-Raft core, if the node is powered on (cluster mode) and the core is not frozen: a
    /// frozen core's process is paused, so no workload hands it a request or moves its host until
    /// it thaws, as frames and disk results wait for it (005: a paused leader must not serve a read
    /// with a lease that ran out during the pause).
    pub fn cluster_core(&mut self, node: u32, core: u16) -> Option<&mut MultiRaftCore<NodeSm>> {
        if self.frozen(node, core) {
            return None;
        }
        self.nodes[node as usize].cores[core as usize].cluster.as_deref_mut()
    }

    /// Whether the core of `node` is frozen now.
    pub fn frozen(&self, node: u32, core: u16) -> bool {
        self.nodes[node as usize].cores[core as usize].frozen_until > self.now
    }

    /// The core whether or not it is frozen, for the simulator's own checks.
    pub fn paused_core(&mut self, node: u32, core: u16) -> Option<&mut MultiRaftCore<NodeSm>> {
        self.nodes[node as usize].cores[core as usize].cluster.as_deref_mut()
    }

    /// Core that holds group `g` on every node.
    pub fn core_of(&self, g: GroupId) -> u16 {
        (g.0 % self.nodes[0].cores.len() as u64) as u16
    }

    /// Adds a group of the cluster run by `kind` and creates it on every powered-on node.
    pub fn add_group(&mut self, desc: GroupDesc, kind: SmKind) {
        let core = self.core_of(desc.group);
        self.groups.push((desc.clone(), kind));
        for node in 0..self.nodes.len() as u32 {
            let c = &mut self.nodes[node as usize].cores[core as usize];
            if c.cluster.is_none() {
                continue;
            }
            let sm = make_sm(&self.registry, self.range_lsm.as_ref(), c, &desc, kind)
                .expect("a new LSM opens");
            let cluster = c.cluster.as_mut().expect("checked");
            cluster.create_group(desc.clone(), sm).expect("a new group");
            self.settle(node, core);
        }
    }

    /// `node` is up and leads a group of the workload's data, any but Group 0.
    pub fn leads_data(&self, node: u32) -> bool {
        let n = &self.nodes[node as usize];
        n.alive
            && n.cores.iter().filter_map(|c| c.cluster.as_deref()).any(|c| {
                c.group_ids()
                    .into_iter()
                    .any(|g| g != GROUP0 && c.status(g).is_some_and(|s| s.role == Role::Leader))
            })
    }

    /// Groups that `node` leads.
    fn led(&mut self, node: u32) -> usize {
        self.nodes[node as usize]
            .cores
            .iter()
            .filter_map(|c| c.cluster.as_deref())
            .map(|c| {
                c.group_ids()
                    .into_iter()
                    .filter(|&g| c.status(g).is_some_and(|s| s.role == Role::Leader))
                    .count()
            })
            .sum()
    }

    /// The live node that leads the most groups (ties: the lowest); a random live node when none
    /// leads or the world has no cluster.
    pub fn busiest_leader(&mut self) -> u32 {
        let mut best: Option<(usize, u32)> = None;
        for i in 0..self.nodes.len() as u32 {
            if !self.nodes[i as usize].alive {
                continue;
            }
            let led = self.led(i);
            if led > 0 && best.is_none_or(|(b, _)| led > b) {
                best = Some((led, i));
            }
        }
        match best {
            Some((_, node)) => node,
            None => self.rng.range(0, self.nodes.len() as u64) as u32,
        }
    }

    /// A random live node that leads no group; any live node if all lead (or none is alive: any).
    pub fn follower(&mut self) -> u32 {
        let mut idle = Vec::new();
        let mut alive = Vec::new();
        for i in 0..self.nodes.len() as u32 {
            if !self.nodes[i as usize].alive {
                continue;
            }
            alive.push(i);
            if self.led(i) == 0 {
                idle.push(i);
            }
        }
        let pool = if !idle.is_empty() {
            idle
        } else if !alive.is_empty() {
            alive
        } else {
            (0..self.nodes.len() as u32).collect()
        };
        pool[self.rng.range(0, pool.len() as u64) as usize]
    }

    /// Starts a node recovering each core from its disk.
    pub fn start_node(&mut self, node: u32) -> Result<(), StorageError> {
        let local = self.local(node);
        let seed = self.rng.next_u64();
        let n = &mut self.nodes[node as usize];
        let cores = n.cores.len() as u16;
        // One HLC per node, shared by its cores, with the bound on disk (005 FR-003).
        let clock = NodeClock::with_bound(NodeId(u64::from(node) + 1), n.hlc_bound);
        n.hlc = clock.clone();
        for (i, c) in n.cores.iter_mut().enumerate() {
            let (s, _) = CoreStore::recover(&self.storage, i as u16, &mut c.disk)?;
            if self.cluster_mode {
                let id = NodeId(u64::from(node) + 1);
                let cfg = CoreConfig {
                    clock: clock.clone(),
                    observe: true,
                    snapshot_entries: SIM_SNAPSHOT_ENTRIES,
                    instance: n.life,
                    ..CoreConfig::new(id, i as u16)
                };
                let mut core = MultiRaftCore::new(cfg, s, seed ^ i as u64);
                // Each group of this core: recovered from its log, or created if the node crashed
                // before its creation was durable.
                for (desc, kind) in self
                    .groups
                    .iter()
                    .filter(|(d, _)| (d.group.0 % u64::from(cores)) as u16 == i as u16)
                {
                    let sm = make_sm(&self.registry, self.range_lsm.as_ref(), c, desc, *kind)?;
                    core.create_group(desc.clone(), sm).expect("each group once");
                }
                c.cluster = Some(Box::new(core));
                c.next_tick = local + CLUSTER_TICK;
            } else {
                c.store = Some(s);
            }
            c.inflight = None;
            c.lsm_inflight = false;
            c.tick_at = None;
            c.frozen_until = Nanos::ZERO;
        }
        n.alive = true;
        self.renew_bound(node);
        for core in 0..self.nodes[node as usize].cores.len() as u16 {
            self.pump(node, core);
        }
        Ok(())
    }

    /// When `node` may start, if its wall clock has not passed the HLC bound on its disk yet
    /// (005 FR-003): the real instant its wall clock reaches the millisecond after the bound, as
    /// it reads now. A clock that jumps before then is looked at again at that instant, as the
    /// server checks its clock again after each sleep.
    pub fn start_wait(&self, node: u32) -> Option<Nanos> {
        let n = &self.nodes[node as usize];
        let until = teseladb_hlc::wait_on_start(n.hlc_bound, self.wall_ms(node))?;
        let at = n.clock.wall_reaches(self.now, until);
        Some(at)
    }

    /// When `node`, waiting for its wall clock to pass the HLC bound on its disk, looks at its
    /// clock again: at [`World::start_wait`] or after [`START_POLL`], whichever comes first, as
    /// the server sleeps at most that long between looks (a clock NTP sets right meanwhile lets
    /// it start then).
    pub fn start_recheck(&self, node: u32) -> Option<Nanos> {
        self.start_wait(node).map(|at| at.min(self.now + START_POLL))
    }

    /// Writes a new HLC bound to the node's `node.meta` if one is due, as the server's clock
    /// thread does (005 FR-003).
    fn renew_bound(&mut self, node: u32) {
        let n = &mut self.nodes[node as usize];
        if let Some(bound) = n.hlc.0.renewal() {
            n.hlc_bound = bound;
            n.hlc.0.raise_bound(bound);
        }
    }

    /// The node crashes: it loses its RAM and what was not synced; the pending events of this life
    /// are dropped when they arrive.
    pub fn crash_node(&mut self, node: u32) {
        // What it had in flight is gone with it.
        if self.bootstrap_op.is_some_and(|(n, _, _)| n == node) {
            self.bootstrap_op = None;
        }
        let n = &mut self.nodes[node as usize];
        n.alive = false;
        n.life += 1;
        for c in &mut n.cores {
            c.store = None;
            c.cluster = None;
            c.lsm = None;
            c.lsm_inflight = false;
            c.lsm_done = teseladb_lsm::Completions::default();
            c.rollback.clear();
            c.inflight = None;
            c.tick_at = None;
            c.frozen_until = Nanos::ZERO;
            c.disk.crash(&mut self.rng);
        }
        // Its connections close: the other nodes see them drop (what was in flight to it, a
        // snapshot transfer say, starts again), as a node does when its TCP connection resets.
        let gone = NodeId(u64::from(node) + 1);
        for other in 0..self.nodes.len() as u32 {
            if other == node || !self.nodes[other as usize].alive {
                continue;
            }
            for core in 0..self.nodes[other as usize].cores.len() as u16 {
                if let Some(c) = self.paused_core(other, core) {
                    c.report_unreachable(gone);
                    self.settle(other, core);
                }
            }
        }
    }

    /// Asks the core for its next disk operation if it has none in flight, and schedules its deadline.
    pub fn pump(&mut self, node: u32, core: u16) {
        let now = self.now;
        let local = self.local(node);
        let n = &mut self.nodes[node as usize];
        if !n.alive {
            return;
        }
        let life = n.life;
        let c = &mut n.cores[core as usize];
        if c.frozen_until > now {
            // Frozen: it resumes when it thaws.
            if c.tick_at.is_none_or(|t| t > c.frozen_until) {
                c.tick_at = Some(c.frozen_until);
                let at = c.frozen_until;
                self.schedule(at, Event::Tick { node, core, life });
            }
            return;
        }
        if c.inflight.is_none() {
            let next = match (&mut c.store, &mut c.cluster) {
                (Some(store), _) => store.poll_io(local),
                (None, Some(cluster)) => cluster.poll_io(local),
                (None, None) => unreachable!("a powered-on core has an engine"),
            };
            // The log first, then the LSM: one operation in flight per core, as on its disk thread.
            let next = match (next, &mut c.lsm) {
                (None, Some(lsm)) => {
                    let mut l = lsm.borrow_mut();
                    let op = l.poll_io(local);
                    // A job that ended inside `poll_io` (an export) is only told here.
                    let d = l.take_completions();
                    c.lsm_done.flushed.extend(d.flushed);
                    c.lsm_done.exported.extend(d.exported);
                    c.lsm_done.ingested.extend(d.ingested);
                    c.lsm_done.cleared.extend(d.cleared);
                    c.lsm_inflight = op.is_some();
                    op
                }
                (op, _) => op,
            };
            if let Some(op) = next {
                let lat = c.disk.latency(&op, &mut self.rng);
                let lat = Nanos(lat.0 * n.disk_slow);
                c.inflight = Some(op);
                self.schedule(now + lat, Event::Disk { node, core, life });
            }
        }
        let n = &mut self.nodes[node as usize];
        let c = &mut n.cores[core as usize];
        let deadline = match (&c.store, &c.cluster) {
            // With an operation in flight the store is pumped again when it completes: a deadline
            // that is already due would only schedule ticks at this same instant.
            (Some(_), _) if c.inflight.is_some() => None,
            (Some(store), _) => store.next_deadline(),
            // The next tick, or the store's deadline if it comes first.
            (None, Some(_)) if c.inflight.is_some() => Some(c.next_tick),
            (None, Some(cluster)) => {
                Some(cluster.next_deadline().map_or(c.next_tick, |d| d.min(c.next_tick)))
            }
            (None, None) => unreachable!("a powered-on core has an engine"),
        };
        if let Some(d) = deadline {
            // Deadline in local time → the world instant at which the node's clock reaches it.
            let at = n.clock.reaches(now, d);
            if c.tick_at.is_none_or(|t| t > at) {
                c.tick_at = Some(at);
                self.schedule(at, Event::Tick { node, core, life });
            }
        }
    }

    /// Finishes a core's operation in flight and returns what completed (`None` if the event is
    /// from a previous life of the node).
    pub fn on_disk(&mut self, node: u32, core: u16, life: u64) -> Option<Completions> {
        let local = self.local(node);
        let n = &mut self.nodes[node as usize];
        if !n.alive || n.life != life {
            return None;
        }
        let c = &mut n.cores[core as usize];
        let op = c.inflight.take().expect("operation in flight");
        let done = c.disk.execute(op);
        if std::mem::take(&mut c.lsm_inflight) {
            let d = c.lsm.as_ref().expect("its LSM is open").borrow_mut().on_io_done(done, local);
            c.lsm_done.flushed.extend(d.flushed);
            c.lsm_done.exported.extend(d.exported);
            c.lsm_done.ingested.extend(d.ingested);
            c.lsm_done.cleared.extend(d.cleared);
            self.pump(node, core);
            return Some(Completions::default());
        }
        let out = match (&mut c.store, &mut c.cluster) {
            (Some(store), _) => store.on_io_done(done, local),
            (None, Some(cluster)) => {
                cluster.on_io_done(done, local);
                self.flush(node, core);
                Completions::default()
            }
            (None, None) => unreachable!("a powered-on core has an engine"),
        };
        self.pump(node, core);
        Some(out)
    }

    /// Deadline of a core.
    pub fn on_tick(&mut self, node: u32, core: u16, life: u64) -> Option<Completions> {
        let local = self.local(node);
        let now = self.now;
        let n = &mut self.nodes[node as usize];
        if !n.alive || n.life != life {
            return None;
        }
        let c = &mut n.cores[core as usize];
        if c.tick_at.is_some_and(|t| t <= now) {
            c.tick_at = None;
        }
        if let Some(store) = c.store.as_mut() {
            store.on_tick(local);
            let out = store.take_completions();
            self.pump(node, core);
            return Some(out);
        }
        // Cluster core: its 10 ms tick (the store's deadlines are served by `poll_io`).
        if local >= c.next_tick {
            c.next_tick = local + CLUSTER_TICK;
            let wall = self.wall_ms(node);
            let c = &mut self.nodes[node as usize].cores[core as usize];
            let cluster = c.cluster.as_mut().expect("powered on");
            cluster.on_tick(local, wall);
            if core == 0 {
                self.renew_bound(node);
                self.bootstrap_group0(node);
            }
            self.flush(node, core);
        }
        self.pump(node, core);
        Some(Completions::default())
    }

    /// After the workload called a cluster core: hands out what it produced and starts its I/O.
    pub fn settle(&mut self, node: u32, core: u16) {
        self.flush(node, core);
        self.pump(node, core);
    }

    /// Keeps the first violation a checker reports.
    fn note(&mut self, r: Result<(), Violation>) {
        self.violation = self.violation.take().or(r.err());
    }

    /// Writes on the core's disk the rollback files of the entries of its ranges a later leader's
    /// log replaced (005 FR-010, D-034), as the embedder does under `<data>/rollback/`. A file
    /// already there is kept: its name carries the time, term and index of the rollback. A file
    /// the disk refuses (full, or a failed sync) waits in RAM and is written again on the core's
    /// next flush.
    fn write_rollback(&mut self, node: u32, core: u16, rolled: &[(GroupId, LogEntry)]) {
        let ranges: Vec<(GroupId, LogEntry)> = rolled
            .iter()
            .filter(|(g, _)| {
                self.groups.iter().any(|(d, k)| d.group == *g && matches!(k, SmKind::Range(..)))
            })
            .cloned()
            .collect();
        let wall = self.wall_ms(node);
        let c = &mut self.nodes[node as usize].cores[core as usize];
        if !ranges.is_empty() {
            c.rollback.extend(rollback_files(&ranges, wall));
        }
        let pending = std::mem::take(&mut c.rollback);
        for f in pending {
            if !write_file_once(&mut c.disk, &f) {
                c.rollback.push(f);
            }
        }
    }

    /// Moves what a cluster core produced: frames to the network, ended operations to the
    /// workload and observations to the Raft checker.
    fn flush(&mut self, node: u32, core: u16) {
        let now = self.now;
        let life = self.nodes[node as usize].life;
        let c =
            self.nodes[node as usize].cores[core as usize].cluster.as_mut().expect("powered on");
        let observations = c.take_observations();
        let actions = c.drain_actions();
        let rolled = c.take_rolled_back();
        self.write_rollback(node, core, &rolled);
        // The log drops those entries only once their files are on disk (005 FR-010).
        let c = &mut self.nodes[node as usize].cores[core as usize];
        if c.rollback.is_empty() {
            c.cluster.as_mut().expect("powered on").rollback_durable();
        }
        for (g, index) in actions.snapshot_data {
            self.snapshot_waits.push((node, core, g, index));
        }
        for (op, r) in actions.done {
            // The `Bootstrap` of Group 0 is the world's; it is proposed again if it did not take.
            if self.bootstrap_op == Some((node, core, op)) {
                self.bootstrap_op = None;
                continue;
            }
            self.events.push((node, core, op, r));
        }
        // Reads first: they were registered before this flush's commits of the same core.
        for o in &observations {
            let r = match *o {
                Observation::ReadRegistered { group, ctx, index, term, index_term, lease } => {
                    let r = ReadRegistration { group, index, term, index_term, lease };
                    self.raft_safety.read_registered((node, core, life, ctx), r)
                }
                Observation::ReadServed { ctx, .. } => {
                    self.raft_safety.read_served((node, core, life, ctx))
                }
                _ => Ok(()),
            };
            self.note(r);
        }
        for o in observations {
            let r = match o {
                Observation::Applied { group, index, term, crc } => {
                    self.raft_safety.applied(AppliedEntry { group, index, term, crc }, node)
                }
                Observation::Elected { group, term } => self.raft_safety.leader(group, term, node),
                Observation::Committed { group, commit } => {
                    self.raft_safety.committed(group, commit);
                    Ok(())
                }
                Observation::ReadRegistered { .. } | Observation::ReadServed { .. } => Ok(()),
            };
            self.note(r);
        }
        for (to, frame) in actions.send {
            // Groups only have the world's nodes as members.
            let to = (to.0 - 1) as u32;
            let frame = encode_frame(&frame);
            if let Some(f) = self.net.send(node, to, core, now, &mut self.rng) {
                if let Some(at) = f.copy {
                    let (epoch, frame) = (ANY_CONNECTION, frame.clone());
                    self.schedule(at, Event::Deliver { from: node, to, core, epoch, frame });
                }
                self.schedule(f.at, Event::Deliver { from: node, to, core, epoch: f.epoch, frame });
            }
        }
    }

    /// A frame arrives (dropped if the node is down or the connection was cut meanwhile).
    pub fn on_deliver(&mut self, from: u32, to: u32, core: u16, epoch: u64, frame: &Bytes) {
        if !self.nodes[to as usize].alive || !self.net.accept(from, to, core, epoch) {
            return;
        }
        let now = self.local(to);
        let sender = NodeId(u64::from(from) + 1);
        let c = self.nodes[to as usize].cores[core as usize].cluster.as_deref_mut();
        let c = c.expect("powered on");
        c.on_peer_message(sender, decode_frame(frame), now);
        let strays = c.take_strays();
        self.flush(to, core);
        self.pump(to, core);
        for (sender, f) in strays {
            self.hand_stray(to, core, sender, f);
        }
    }

    /// Hands a frame that core `core` of `to` received for groups it does not run to the cores
    /// of the node that run them, as the server does (007 FR-011: a range between cores).
    fn hand_stray(&mut self, to: u32, core: u16, sender: NodeId, frame: PeerFrame) {
        let n = &self.nodes[to as usize];
        let holder = |g: GroupId| {
            (0..n.cores.len() as u16).filter(|x| *x != core).find(|x| {
                n.cores[*x as usize].cluster.as_ref().is_some_and(|c| c.status(g).is_some())
            })
        };
        let parts = route_stray(frame, holder);
        let now = self.local(to);
        for (x, f) in parts {
            if let Some(c) = self.nodes[to as usize].cores[x as usize].cluster.as_deref_mut() {
                c.on_stray(sender, f, now);
            }
            self.flush(to, x);
            self.pump(to, x);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::path::Path;

    use super::*;

    fn file() -> RollbackFile {
        RollbackFile { path: "rollback/range-1/r-1".into(), bytes: b"doc".to_vec() }
    }

    #[test]
    fn a_rollback_file_is_written_once_and_one_the_disk_refuses_leaves_nothing() {
        let mut disk = SimDisk::default();
        assert!(write_file_once(&mut disk, &file()));
        assert!(write_file_once(&mut disk, &file()), "already there");
        let mut full = SimDisk::default();
        full.set_capacity(0);
        assert!(!write_file_once(&mut full, &file()));
        assert!(full.list(Path::new("rollback/range-1")).unwrap_or_default().is_empty());
    }

    #[test]
    fn rollback_files_the_disk_refuses_wait_and_keep_the_log_entries() {
        let mut w = World::cluster(1, 1, 1, false);
        w.nodes[0].cores[0].disk.set_capacity(0);
        w.nodes[0].cores[0].rollback.push(file());
        w.flush(0, 0);
        assert_eq!(w.nodes[0].cores[0].rollback.len(), 1, "written on a later flush");
        w.nodes[0].cores[0].disk.set_capacity(u64::MAX);
        w.flush(0, 0);
        assert!(w.nodes[0].cores[0].rollback.is_empty());
    }
}
