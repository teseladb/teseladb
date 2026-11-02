//! Start-up of a node (001 FR-018, 005 FR-015 and FR-016, 007 FR-003): the data directory's
//! `LOCK`, `node.meta`, the recovery of every core's log, Group 0 on core 0 (created by the
//! founder, received by a node that joins), every core's LSM and data ranges, one thread per core
//! running its loop, and the acceptor of the inter-node port.
//!
//! A core's state lives on its thread: the thread recovers its log and its LSM, and only then runs
//! its loop; the data ranges Group 0 places on the core open there as core 0 hands them over
//! ([`crate::ranges`]). The node talks to the ranges through [`Node::put`], [`Node::get`] and
//! [`Node::data_status`], each request sent to the core of its range (006 brings RefDb's
//! protocol).

use crate::authz_lease::AuthGate;
use crate::clustertime_keys::{KeyDriver, KeySlot};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::collections::hash_map::RandomState;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use teseladb_auth::clustertime::KeySealer;

use teseladb_hlc::Hlc;
use teseladb_io::handshake::{
    Acceptor, AcceptorOptions, EnrollHandler, JOIN_TIMEOUT, JoinHandler, JoinRequest,
    request_join_with,
};
use teseladb_io::peer::{EnrollAccepted, EnrollRequest, Hello, JoinAccepted};
use teseladb_io::runtime::{CoreInput, CoreRuntime, CoreSender, CoreSenders, RuntimeConfig};
use teseladb_io::secure::PeerTls;
use teseladb_lsm::{Lsm, LsmConfig, LsmError};
use teseladb_meta::bootstrap::{DATA_RANGE, data_range_core};
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::enroll_tokens::TOKEN_ID_LEN;
use teseladb_meta::group0::{GROUP0, Group0Error, Group0Sm};
use teseladb_multiraft::{CoreConfig, GroupDesc, MultiRaftCore, NodeClock, SNAPSHOT_ENTRIES};
use teseladb_raftlog::paths::core_dir;
use teseladb_raftlog::{
    CoreStore, Disk, FileId, IoErrno, StorageConfig, StorageError, open_node, read_node_meta,
    write_atomic, write_node_meta,
};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_sys::FileLock;
use teseladb_types::{GroupId, NodeId};
use teseladb_wire::commands::{NodeSessions, Shared};

use crate::admin::{NodeAdmin, answer};
use crate::backend::NodeHandle;
use crate::catalog_ops::{CatalogAnswer, CatalogReq};
use crate::client_listener::{ClientConfig, ClientListener, NodeClockState, NodePresence};
use crate::clock::{ClockKeeper, start_clock};
use crate::coresm::CoreSm;
use crate::data::{DataCore, DataError, DataReply, DataReq, DataStatus};
use crate::enroll::{Enroller, SECRET_LEN, Token};
use crate::hosting::Hosting;
use crate::liveness::SharedLoads;
use crate::logic::{Group0View, NodeLogic, NodeMsg};
use crate::metrics_http::{AdminListener, Handler};
use crate::ranges::SPLIT_BYTES;
use crate::routes::SharedRoutes;
use crate::transfer::{Transfers, transfer_dir};

/// The node that creates the cluster: the first node `Bootstrap` adds.
pub const FOUNDER: NodeId = NodeId(1);

/// File with the Group 0 voters a node received when it joined (one `NodeId` per line).
pub const JOIN_VOTERS: &str = "join.voters";

/// File with the format of the data the node keeps: the commands of its ranges' logs, their
/// snapshots and their local keys.
pub const DATA_FORMAT_FILE: &str = "data.format";

/// The data format this version writes and the only one it reads. 1: before D-068. 2: `Write`
/// carries `live_delta`, `Split` `right_live` and `SplitGenesis` the new range's end; a range's
/// snapshot carries its end and live bytes, which the local key `07` counts (D-067, D-068). 3:
/// merges (007 FR-009): `MergeFreeze` and `Merge`, the local keys `08` to `0A` and their state in
/// a range's snapshot, and Group 0's snapshot format 5. 4: Group 0's `$clusterTime` signing
/// keys, kinds 44 and 45 and its snapshot format 6 (013 FR-014). 5: Group 0's snapshot format 7,
/// whose catalog keeps the databases left without collections (D-077). 6: Group 0's node
/// enrollment tokens, kinds 46 and 47 and its snapshot format 8 (013 FR-005). 7: the range commands
/// of transactions (008): `PutIntents`, `EndTxn`, `ResolveIntents` and `PushTxn`, kinds 8, 9, 10
/// and 12, with intents and records in the range's data.
pub const DATA_FORMAT: u32 = 7;

/// Bytes of system randomness that seed the generator of the `$clusterTime` signing keys.
const KEY_ENTROPY: usize = 48;

/// The format of a data directory written before [`DATA_FORMAT_FILE`] existed.
const FIRST_DATA_FORMAT: u32 = 1;

/// Longest a new node keeps trying to join before giving up.
pub const JOIN_DEADLINE: Duration = Duration::from_secs(60);

/// Pause between two rounds of join attempts.
const JOIN_RETRY: Duration = Duration::from_millis(200);

/// `not_leader` hints a round follows at most.
const JOIN_HOPS: usize = 3;

/// Why a node could not start.
#[derive(Debug)]
pub enum NodeError {
    /// Another process holds the data directory.
    Locked,
    /// Invalid configuration.
    Config(String),
    /// The data on disk is corrupt or of another format.
    Storage(StorageError),
    /// System error.
    Io(io::Error),
    /// Joining an existing cluster failed.
    Join(String),
    /// The LSM of the data range's core could not be opened.
    Lsm(LsmError),
    /// The data directory holds data of another format than this version reads.
    DataFormat {
        /// The directory's format.
        found: u32,
    },
}

impl fmt::Display for NodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NodeError::Locked => f.write_str("another process is using the data directory"),
            NodeError::Config(m) | NodeError::Join(m) => f.write_str(m),
            NodeError::Storage(StorageError::CoreCountMismatch { meta, requested }) => write!(
                f,
                "the data directory was created with {meta} cores and {requested} were requested; \
                 start with --cores {meta}"
            ),
            NodeError::Storage(e) => write!(f, "storage: {e:?}"),
            NodeError::Io(e) => write!(f, "{e}"),
            NodeError::Lsm(e) => write!(f, "LSM: {e}"),
            NodeError::DataFormat { found } => write!(
                f,
                "the data directory holds data format {found} and this version of TeselaDB reads \
                 only format {DATA_FORMAT}: start the version that wrote it, or start this one on \
                 an empty directory and add the node to the cluster again"
            ),
        }
    }
}

impl std::error::Error for NodeError {}

impl From<io::Error> for NodeError {
    fn from(e: io::Error) -> Self {
        NodeError::Io(e)
    }
}

impl From<StorageError> for NodeError {
    fn from(e: StorageError) -> Self {
        NodeError::Storage(e)
    }
}

/// Parameters of a node.
#[derive(Clone, Debug)]
pub struct NodeConfig {
    /// Cores (`None` = the data directory's, or the machine's for a new one).
    pub cores: Option<u16>,
    /// Inter-node address to listen on (port 0 = any).
    pub listen_peer: SocketAddr,
    /// Host the other nodes dial (`None` = the listening address).
    pub advertise: Option<String>,
    /// Administration address announced to the cluster.
    pub admin_addr: String,
    /// Client address announced to the cluster.
    pub native_addr: String,
    /// Inter-node addresses of nodes of an existing cluster.
    pub join: Vec<String>,
    /// How long a new node keeps trying to join ([`JOIN_DEADLINE`]).
    pub join_deadline: Duration,
    /// Configuration of the log.
    pub storage: StorageConfig,
    /// Tick of the cores.
    pub tick: Duration,
    /// Seed of the elections (`None` = random).
    pub seed: Option<u64>,
    /// Configuration of each core's LSM (its directory is the core's `lsm`).
    pub lsm: LsmConfig,
    /// Live bytes at which a range splits; on disk, eight times as many (007 FR-005, D-068).
    pub split_bytes: u64,
    /// Entries applied past a group's latest snapshot that trigger a new one.
    pub snapshot_entries: u64,
    /// The clients' port (006); `None`: the node does not open it.
    pub client: Option<ClientConfig>,
    /// The `root` user document to create while the cluster has no user (013 FR-004).
    pub root_user: Option<Vec<u8>>,
    /// Seals the `$clusterTime` signing keys with the master key; `None` signs with zeros and
    /// takes any signature, as RefDb without authentication (013 FR-014).
    pub clustertime_sealer: Option<KeySealer>,
    /// TLS 1.3 between nodes with the node's certificate (013 FR-006); `None` in clear.
    pub peer_tls: Option<PeerTls>,
    /// Accepts node enrollments with the CA and the master key (013 FR-005).
    pub enroller: Option<Enroller>,
    /// Where the administration port listens (`--admin-listen`, 007 FR-020); `None`: the node
    /// does not open it.
    pub admin_listen: Option<SocketAddr>,
    /// TLS on the administration port, on a node with a PKI (013 FR-017).
    pub admin_tls: Option<crate::metrics_http::AdminTls>,
    /// The data directory, whose disk space the heartbeats carry (007 FR-010); `None`: unknown.
    pub data_dir: Option<std::path::PathBuf>,
    /// The node's tags (`--tags`), sorted by name (007 FR-013).
    pub tags: Vec<(String, String)>,
    /// Bytes of memory the node may use (`--memory-limit`, 012 FR-013); `None`: three quarters of
    /// the memory the process has.
    pub memory_limit: Option<u64>,
    /// Where the cores run (`--cpu-list`, 012 FR-006).
    pub affinity: crate::affinity::Affinity,
    /// Whether the files the node writes outside its cores' disks (the search segments) use
    /// `O_DIRECT` (012 FR-003): what `--direct-io` decided for the cores' disks.
    pub direct_io: bool,
    /// How the clients' port starts its executors and runs the TTL monitor; a test changes them.
    #[doc(hidden)]
    pub client_hooks: crate::client_listener::Hooks,
}

impl Default for NodeConfig {
    fn default() -> Self {
        NodeConfig {
            cores: None,
            listen_peer: SocketAddr::from(([127, 0, 0, 1], 0)),
            advertise: None,
            admin_addr: String::new(),
            native_addr: String::new(),
            join: Vec::new(),
            join_deadline: JOIN_DEADLINE,
            storage: StorageConfig::default(),
            tick: Duration::from_millis(10),
            seed: None,
            lsm: LsmConfig::default(),
            split_bytes: SPLIT_BYTES,
            snapshot_entries: SNAPSHOT_ENTRIES,
            client: None,
            root_user: None,
            clustertime_sealer: None,
            peer_tls: None,
            enroller: None,
            admin_listen: None,
            admin_tls: None,
            data_dir: None,
            tags: Vec::new(),
            memory_limit: None,
            affinity: crate::affinity::Affinity::Off,
            direct_io: false,
            client_hooks: crate::client_listener::Hooks::default(),
        }
    }
}

/// Asks the nodes of `seeds` to join, following `not_leader` hints, until one accepts or
/// `deadline` passes.
pub fn join_cluster(
    seeds: &[String],
    req: &JoinRequest,
    deadline: Duration,
) -> Result<JoinAccepted, NodeError> {
    join_cluster_with(seeds, req, deadline, None)
}

/// [`join_cluster`] over TLS with `tls` (`None` in clear).
pub fn join_cluster_with(
    seeds: &[String],
    req: &JoinRequest,
    deadline: Duration,
    tls: Option<&PeerTls>,
) -> Result<JoinAccepted, NodeError> {
    let end = Instant::now() + deadline;
    let mut last = String::from("no node answered");
    loop {
        let mut queue: VecDeque<String> = seeds.iter().cloned().collect();
        let mut hops = 0;
        while let Some(addr) = queue.pop_front() {
            let client = tls.map(PeerTls::client);
            match request_join_with(&addr, req, client.as_ref()) {
                Ok(Ok(a)) => return Ok(a),
                Ok(Err(reason)) => {
                    // The leader's address goes first; a bounded number of hints per round.
                    if let Some(leader) = reason.strip_prefix("not_leader ")
                        && hops < JOIN_HOPS
                    {
                        hops += 1;
                        queue.push_front(leader.to_owned());
                    }
                    last = format!("{addr}: {reason}");
                }
                Err(e) => last = format!("{addr}: {e}"),
            }
        }
        if Instant::now() >= end {
            return Err(NodeError::Join(format!("could not join the cluster: {last}")));
        }
        thread::sleep(JOIN_RETRY);
    }
}

/// The voters of [`JOIN_VOTERS`], if the node joined a cluster.
/// Checks the data format of the directory, which a new one takes: one with `node.meta` and no
/// [`DATA_FORMAT_FILE`] was written before the file existed. A node never reads a format it does
/// not know: it would stop its ranges at the first entry it cannot decode and hang its writes.
fn check_data_format(disk: &mut dyn Disk, existing: bool) -> Result<(), NodeError> {
    let found = match disk.read_all(Path::new(DATA_FORMAT_FILE)) {
        Ok(b) => String::from_utf8_lossy(&b).trim().parse::<u32>().map_err(|_| {
            NodeError::Config(format!("{DATA_FORMAT_FILE}: not a data format number"))
        })?,
        Err(IoErrno::NotFound) if existing => FIRST_DATA_FORMAT,
        Err(IoErrno::NotFound) => {
            let text = DATA_FORMAT.to_string().into_bytes();
            write_atomic(disk, Path::new(DATA_FORMAT_FILE), text, FileId(u32::MAX - 2))?;
            DATA_FORMAT
        }
        Err(e) => {
            return Err(NodeError::Io(io::Error::other(format!("{DATA_FORMAT_FILE}: {e:?}"))));
        }
    };
    match found == DATA_FORMAT {
        true => Ok(()),
        false => Err(NodeError::DataFormat { found }),
    }
}

fn read_join_voters(disk: &mut dyn Disk) -> Result<Option<Vec<NodeId>>, NodeError> {
    let text = match disk.read_all(Path::new(JOIN_VOTERS)) {
        Ok(b) => b,
        Err(IoErrno::NotFound) => return Ok(None),
        Err(e) => return Err(NodeError::Io(io::Error::other(format!("{JOIN_VOTERS}: {e:?}")))),
    };
    let voters = String::from_utf8_lossy(&text)
        .lines()
        .map(|l| l.parse::<u64>().map(NodeId))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| NodeError::Config(format!("{JOIN_VOTERS}: not a list of node ids")))?;
    Ok(Some(voters))
}

/// Random bytes from the hasher keys the standard library seeds from the system (enough for an
/// identifier; secrets come from 013's generator).
fn random_u64() -> u64 {
    let mut h = RandomState::new().build_hasher();
    h.write_u128(SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    h.finish()
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Takes the data directory's lock (created if missing).
pub fn lock_dir(dir: &Path) -> Result<FileLock, NodeError> {
    fs::create_dir_all(dir)?;
    let file =
        OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("LOCK"))?;
    FileLock::try_lock(file).map_err(|e| match e.kind() {
        io::ErrorKind::WouldBlock => NodeError::Locked,
        _ => NodeError::Io(e),
    })
}

type CoreThread<D> = JoinHandle<io::Result<Option<D>>>;

/// What core 0 adds: Group 0.
struct Group0Spec {
    /// Group 0's description (only used for a new replica).
    desc: GroupDesc,
    /// The founder's `Bootstrap`: the cluster and the first node.
    founder: Option<([u8; 16], NodeAddrs)>,
    /// This node's name and addresses.
    addrs: NodeAddrs,
    /// This start's `boot_epoch`.
    boot_epoch: u32,
    /// This node's tags (`--tags`, 007 FR-013).
    tags: Vec<(String, String)>,
}

/// What a core's thread needs to build its state.
struct CoreSpec {
    core: u16,
    node_id: NodeId,
    clock: NodeClock,
    storage: StorageConfig,
    lsm: LsmConfig,
    snapshot_entries: u64,
    seed: u64,
    /// Core 0's part.
    group0: Option<Group0Spec>,
    /// Size at which a range splits.
    split_bytes: u64,
    /// The routing table (core 0 writes it).
    routes: Option<SharedRoutes>,
    hello: Hello,
    tick: Duration,
    /// The node's cursors and sessions (core 0, for the kills of other nodes).
    sessions: Option<Arc<Shared>>,
    /// The node's search indexes, whose segment files travel with the snapshots of the core's
    /// ranges (011 FR-013).
    search: Arc<Shared>,
    /// Where core 0 publishes the node's authorization lease (013).
    auth: Option<Arc<AuthGate>>,
    /// The `root` user core 0 creates while the cluster has none (013).
    root_user: Option<Vec<u8>>,
    /// Core 0's `$clusterTime` signing keys: their sealer and where it publishes them (013).
    clustertime_keys: Option<(KeySealer, Arc<KeySlot>)>,
    /// TLS for the connections the core dials (013).
    peer_tls: Option<PeerTls>,
    /// Where every core publishes its load for core 0's heartbeat (007 FR-010).
    loads: Arc<SharedLoads>,
    /// Where core 0 tells the clients' port the node's state in Group 0 (007 FR-015).
    presence: Option<Arc<NodePresence>>,
    /// Which cores of the node hold each range (007 FR-011).
    hosting: Hosting,
    /// The CPU the core's thread is pinned to (012 FR-006).
    cpu: Option<usize>,
}

/// The data ranges of a core: its LSM recovered. Its ranges open as Group 0 gives them, each from
/// the LSM's flushed index (the log replays the rest, FR-015).
fn open_data<D: Disk>(spec: &CoreSpec, disk: &mut D) -> Result<DataCore, NodeError> {
    let dir = core_dir(spec.core);
    let cfg = LsmConfig { dir: dir.join("lsm"), ..spec.lsm.clone() };
    let (lsm, _) = Lsm::recover(&cfg, disk).map_err(NodeError::Lsm)?;
    let lsm = Rc::new(RefCell::new(lsm));
    let transfers = Transfers::new(spec.node_id, transfer_dir(&dir), random_u64())
        .with_segments(spec.search.clone());
    let clock = spec.clock.clone();
    let data = DataCore::new(spec.node_id, spec.core, lsm, clock, transfers, spec.split_bytes);
    let data = data.with_backups(dir.join("backup"), random_u64());
    Ok(data.with_relocations(dir.join("relocate"), spec.hosting.clone()))
}

/// A core's state, built on its thread.
fn build_core<D: Disk>(spec: &CoreSpec, disk: &mut D) -> Result<NodeLogic, NodeError> {
    let (store, _) = CoreStore::recover(&spec.storage, spec.core, disk)?;
    // The ranges of its log, before core 0 hands out Group 0's (007 FR-011).
    for g in store.group_ids().into_iter().filter(|g| *g != GROUP0) {
        spec.hosting.add(g, spec.core);
    }
    // A new instance on every start, even with a fixed seed (`CoreConfig::instance`).
    let ccfg = CoreConfig {
        instance: random_u64(),
        clock: spec.clock.clone(),
        snapshot_entries: spec.snapshot_entries,
        ..CoreConfig::new(spec.node_id, spec.core)
    };
    let mut core = MultiRaftCore::new(ccfg, store, spec.seed);
    let mut node0 = None;
    if let Some(g) = &spec.group0 {
        // A restarted node recovers Group 0 from its log; `desc` only describes a new one.
        core.create_group(g.desc.clone(), CoreSm::Group0(Group0Sm::default()))
            .map_err(|e| NodeError::Config(format!("Group 0: {e:?}")))?;
        node0 = Some((g.founder.clone(), (spec.node_id, g.addrs.clone()), g.boot_epoch, &g.tags));
    }
    let data = open_data(spec, disk)?;
    let mut logic = match node0 {
        Some((founder, me, boot_epoch, tags)) => NodeLogic::new(core, founder, Some(me))
            .with_boot_epoch(boot_epoch)
            .with_tags(tags.clone())
            .with_zone(crate::local_zone::local_zone()),
        None => NodeLogic::new(core, None, None),
    };
    logic = logic.with_data(data);
    if let Some(r) = &spec.routes {
        logic = logic.with_routes(r.clone());
    }
    if let Some(g) = &spec.auth {
        logic = logic.with_auth_gate(g.clone());
    }
    if let Some(r) = &spec.root_user {
        logic = logic.with_root_user(r.clone());
    }
    if let Some((sealer, slot)) = &spec.clustertime_keys {
        let mut entropy = [0u8; KEY_ENTROPY];
        teseladb_sys::fill_random(&mut entropy).map_err(NodeError::Io)?;
        let driver = KeyDriver::new(sealer.clone(), slot.clone(), &entropy);
        entropy = [0; KEY_ENTROPY];
        std::hint::black_box(&entropy);
        logic = logic.with_clustertime_keys(driver);
    }
    if let Some(s) = &spec.sessions {
        logic = logic.with_sessions(s.clone());
    }
    logic = logic.with_loads(spec.core, spec.loads.clone());
    if let Some(p) = &spec.presence {
        logic = logic.with_presence(p.clone());
    }
    Ok(logic)
}

/// A core's thread: builds its state, hands its mailbox to `ready` and runs its loop. Returns its
/// disk when it stops (`None` if its state could not be built).
fn run_core<D: Disk + Send + 'static>(
    spec: CoreSpec,
    mut disk: D,
    ready: mpsc::Sender<Result<CoreSender<NodeMsg>, NodeError>>,
) -> io::Result<Option<D>> {
    // Pinned before it allocates anything, so that its memory is on its CPU's node.
    if let Some(cpu) = spec.cpu
        && let Err(e) = crate::affinity::place_current(cpu)
    {
        let _ = ready.send(Err(NodeError::Config(format!("core {}: CPU {cpu}: {e}", spec.core))));
        return Ok(None);
    }
    let logic = match build_core(&spec, &mut disk) {
        Ok(l) => l,
        Err(e) => {
            let _ = ready.send(Err(e));
            return Ok(None);
        }
    };
    let rcfg =
        RuntimeConfig { tick: spec.tick, seed: spec.seed ^ 0x5eed, tls: spec.peer_tls.clone() };
    match CoreRuntime::new(logic, disk, spec.hello, rcfg) {
        Ok((rt, tx)) => {
            let _ = ready.send(Ok(tx));
            rt.run().map(|(_, d)| d)
        }
        Err(e) => {
            let _ = ready.send(Err(NodeError::Io(e)));
            Ok(None)
        }
    }
}

/// A running node.
pub struct Node<D> {
    node_id: NodeId,
    cluster_id: [u8; 16],
    cores: u16,
    peer_addr: SocketAddr,
    /// What makes senders of the cores' mailboxes: the node is shared between threads, and each
    /// call makes its own sender (012 FR-005).
    senders: Vec<CoreSenders<NodeMsg>>,
    threads: Vec<CoreThread<D>>,
    acceptor: Acceptor,
    client: Option<ClientListener>,
    /// The administration port (007 FR-020).
    admin: Option<AdminListener>,
    clock: ClockKeeper,
    routes: SharedRoutes,
    /// Issues enrollment tokens, on a node with a PKI.
    enroller: Option<Enroller>,
    /// Its memory and the split between its cores (012 FR-013).
    memory: Option<crate::memory::MemoryBudget>,
    /// The CPU of each core (012 FR-006).
    cpus: Vec<Option<usize>>,
    /// Each core's LSM configuration.
    lsm: LsmConfig,
    /// Set by a client's `shutdown` (007 FR-018).
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    _lock: Option<FileLock>,
}

impl<D> fmt::Debug for Node<D> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Node")
            .field("node_id", &self.node_id)
            .field("cores", &self.cores)
            .field("peer_addr", &self.peer_addr)
            .finish()
    }
}

/// How long a member waits for Group 0 to take an enrollment token.
pub(crate) const ENROLL_WAIT: Duration = Duration::from_secs(10);

/// Issues an enrollment token valid for `ttl` with `enroller`'s CA (013 FR-005): Group 0, through
/// `core0`, records it, and the returned text starts a node with
/// `--join <addr> --enroll-token <text>`.
pub(crate) fn enroll_token(
    enroller: &Enroller,
    core0: &CoreSender<NodeMsg>,
    ttl: Duration,
) -> Result<String, String> {
    let mut raw = [0u8; TOKEN_ID_LEN + SECRET_LEN];
    teseladb_sys::fill_random(&mut raw).map_err(|e| e.to_string())?;
    let mut token = Token {
        fingerprint: enroller.fingerprint(),
        id: [0; TOKEN_ID_LEN],
        secret: [0; SECRET_LEN],
    };
    token.id.copy_from_slice(&raw[..TOKEN_ID_LEN]);
    token.secret.copy_from_slice(&raw[TOKEN_ID_LEN..]);
    raw.fill(0);
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis());
    let expires = u64::try_from(now_ms + ttl.as_millis()).unwrap_or(u64::MAX);
    group0_change(core0, Group0Command::CreateEnrollToken(token.record(expires)))?;
    Ok(token.to_text())
}

/// Runs `cmd` on Group 0 through core 0 and waits for the outcome.
pub(crate) fn group0_change(core0: &CoreSender<NodeMsg>, cmd: Group0Command) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    core0
        .send_waiting(CoreInput::App(NodeMsg::Catalog(CatalogReq::Security(cmd), tx)))
        .map_err(|_| "the node is stopping".to_owned())?;
    match rx.recv_timeout(ENROLL_WAIT) {
        Ok(CatalogAnswer::Done(_)) => Ok(()),
        Ok(CatalogAnswer::Refused(Group0Error::EnrollTokenUsed)) => {
            Err("the enrollment token was already used".to_owned())
        }
        Ok(CatalogAnswer::Refused(Group0Error::EnrollTokenExpired)) => {
            Err("the enrollment token expired".to_owned())
        }
        Ok(CatalogAnswer::Refused(Group0Error::EnrollTokenUnknown)) => {
            Err("the enrollment token is not known to the cluster".to_owned())
        }
        Ok(CatalogAnswer::Refused(e)) => Err(format!("Group 0 refused it: {e:?}")),
        Ok(_) | Err(_) => Err("the cluster did not answer".to_owned()),
    }
}

/// A member's answer to an enrollment: the key is checked, Group 0 takes the token, and the node
/// gets its certificate and the sealed keys.
fn enroll_one(
    e: &Enroller,
    core0: &CoreSender<NodeMsg>,
    req: &EnrollRequest,
) -> Result<EnrollAccepted, String> {
    Enroller::public_key(req)?;
    let secret_hash = crate::enroll::secret_hash(&req.secret);
    group0_change(core0, Group0Command::ConsumeEnrollToken { id: req.token_id, secret_hash })?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let now = i64::try_from(now).unwrap_or(i64::MAX);
    e.accept(req, now, &mut |b| teseladb_sys::fill_random(b))
}

/// What a new node learns before it creates anything: the cluster it joined, if any.
fn join_first(
    cfg: &NodeConfig,
    meta_disk: &mut dyn Disk,
    advertised_peer: &str,
) -> Result<Option<JoinAccepted>, NodeError> {
    let req = JoinRequest {
        name: advertised_peer.to_owned(),
        peer_addr: advertised_peer.to_owned(),
        http_addr: cfg.admin_addr.clone(),
        native_addr: cfg.native_addr.clone(),
        cores: cfg.cores.unwrap_or(0),
    };
    let a = join_cluster_with(&cfg.join, &req, cfg.join_deadline, cfg.peer_tls.as_ref())?;
    if cfg.cores.is_some_and(|c| c != a.cores) {
        return Err(NodeError::Join(format!(
            "CoreCountMismatch: the cluster has {} cores and --cores is {}",
            a.cores, req.cores
        )));
    }
    // The voters go to disk before `node.meta`: a restart before Group 0 arrives creates the same
    // Group 0 replica.
    let text: Vec<String> = a.voters.iter().map(|(n, _)| n.0.to_string()).collect();
    write_atomic(
        meta_disk,
        Path::new(JOIN_VOTERS),
        text.join("\n").into_bytes(),
        FileId(u32::MAX - 1),
    )?;
    Ok(Some(a))
}

impl<D: Disk + Send + 'static> Node<D> {
    /// Starts a node. `open_disk` gives a disk rooted at the data directory (one per core and
    /// one for `node.meta`); `lock` is the data directory's lock, held while the node runs.
    pub fn start(
        cfg: NodeConfig,
        mut open_disk: impl FnMut() -> io::Result<D>,
        lock: Option<FileLock>,
    ) -> Result<Node<D>, NodeError> {
        let listener = TcpListener::bind(cfg.listen_peer)?;
        let peer_addr = listener.local_addr()?;
        let advertise = cfg.advertise.clone().unwrap_or_else(|| peer_addr.ip().to_string());
        let advertised_peer = format!("{advertise}:{}", peer_addr.port());

        let mut meta_disk = open_disk()?;
        let existing = read_node_meta(&mut meta_disk)?;
        check_data_format(&mut meta_disk, existing.is_some())?;
        // A node without an identifier (a new directory, or one that stopped between `node.meta`
        // and the write of its identifier) joins with `--join` before creating anything: it adopts
        // the cluster's identifier and cores. One that had started joining never founds a cluster.
        let unassigned = existing.as_ref().is_none_or(|m| m.node_id == 0);
        let started_joining = read_join_voters(&mut meta_disk)?.is_some();
        let joined = match unassigned && !cfg.join.is_empty() {
            true => join_first(&cfg, &mut meta_disk, &advertised_peer)?,
            false => None,
        };
        if unassigned && joined.is_none() && started_joining {
            return Err(NodeError::Config(
                "the node stopped while joining a cluster, before it got its identifier; start it \
                 again with --join"
                    .into(),
            ));
        }
        let cluster_id = match &joined {
            Some(a) => a.cluster_id,
            None => {
                let mut id = [0u8; 16];
                id[..8].copy_from_slice(&random_u64().to_le_bytes());
                id[8..].copy_from_slice(&random_u64().to_le_bytes());
                id
            }
        };
        let machine = || thread::available_parallelism().map_or(1, |n| n.get().min(256) as u16);
        let cores = match (cfg.cores, &existing, &joined) {
            (Some(c), _, _) => c,
            (None, Some(m), _) => m.cores as u16,
            (None, None, Some(a)) => a.cores,
            (None, None, None) => machine(),
        };
        // Its memory, split between its cores (012 FR-013).
        let memory = cfg
            .memory_limit
            .or_else(teseladb_sys::memory_limit::host_default)
            .map(|total| crate::memory::MemoryBudget::new(total, cores));
        let lsm = memory.map_or_else(|| cfg.lsm.clone(), |m| m.lsm(&cfg.lsm));
        // Where each core runs (012 FR-006).
        let allowed = teseladb_sys::numa::allowed_cpus()?;
        let cpus = crate::affinity::core_cpus(&cfg.affinity, cores, &allowed)
            .map_err(NodeError::Config)?;
        let mut meta = open_node(&mut meta_disk, u32::from(cores), cluster_id, now_ms())?;
        // A node that joins gets its identifier and cluster from the cluster; the node that creates
        // the cluster is node 1 (Group 0 `Bootstrap`).
        if let Some(a) = &joined {
            meta.node_id = a.node_id.0;
            meta.cluster_id = a.cluster_id;
            write_node_meta(&mut meta_disk, &meta)?;
        } else if meta.node_id == 0 {
            meta.node_id = FOUNDER.0;
            write_node_meta(&mut meta_disk, &meta)?;
        }
        let node_id = NodeId(meta.node_id);
        let group0 = match read_join_voters(&mut meta_disk)? {
            Some(voters) => GroupDesc { group: GROUP0, voters, learners: vec![node_id] },
            None if node_id == FOUNDER => {
                GroupDesc { group: GROUP0, voters: vec![FOUNDER], learners: Vec::new() }
            }
            None => {
                return Err(NodeError::Config(format!(
                    "node {node_id:?} joined a cluster but {JOIN_VOTERS} is missing"
                )));
            }
        };
        // One clock for the node's cores, under the bound in `node.meta` (005 FR-003).
        let (clock, clock_keeper) = start_clock(meta_disk, meta.clone())?;
        let addrs = NodeAddrs {
            // The name identifies the node in the cluster: its advertised inter-node address.
            name: advertised_peer.clone().into_bytes(),
            peer_addr: advertised_peer.clone().into_bytes(),
            http_addr: cfg.admin_addr.clone().into_bytes(),
            native_addr: cfg.native_addr.clone().into_bytes(),
            cores,
        };
        let hello = Hello {
            cluster_id: meta.cluster_id,
            node_id,
            core: 0,
            cores,
            peer_addr: advertised_peer,
            http_addr: cfg.admin_addr.clone(),
            native_addr: cfg.native_addr.clone(),
            versions: teseladb_io::peer::Versions::OWN,
        };
        let seed = cfg.seed.unwrap_or_else(random_u64);

        let hosting = Hosting::default();
        let routes = SharedRoutes::default().with_hosting(hosting.clone());
        let mut tables =
            cfg.client.as_ref().map_or_else(NodeSessions::default, ClientConfig::sessions);
        if let Some(dir) = &cfg.data_dir {
            // The segments of the search indexes, one file per index (011 FR-012).
            crate::search_segments::open(dir, &mut tables.search, cfg.direct_io)?;
            // What aggregation stages write past their memory (010 FR-010).
            let disk = teseladb_io::FileDisk::open(dir.join(crate::spill::SPILL_DIR))?;
            let spill =
                crate::spill::DiskSpill::open(disk).map_err(|e| io::Error::other(e.message))?;
            tables.spill.store = Some(Arc::new(spill));
        }
        // The search segments in memory stay within the cores' parts for them (D-105): past
        // them, indexes go to their files until a query needs them again.
        tables.search.set_memory_limit(
            memory.map(|m| m.part(crate::memory::Component::SearchSegments) * u64::from(cores)),
        );
        // The open cursors' computed results stay within the cores' parts for cursors (D-105).
        tables.spill.cursor_bytes = memory.map(|m| {
            let part = m.part(crate::memory::Component::Cursors) * u64::from(cores);
            usize::try_from(part).unwrap_or(usize::MAX)
        });
        // A stage holds at most the core's part for aggregations (D-105).
        tables.spill.stage_bytes = memory.map(|m| {
            usize::try_from(m.part(crate::memory::Component::Aggregations)).unwrap_or(usize::MAX)
        });
        let sessions: Arc<Shared> = Arc::new(Mutex::new(tables));
        // Created before the cores' runtimes start: see `AuthGate`.
        let auth = Arc::new(AuthGate::default());
        let keys =
            cfg.clustertime_sealer.as_ref().map(|s| (s.clone(), Arc::new(KeySlot::default())));
        let loads = Arc::new(SharedLoads::new(usize::from(cores), cfg.data_dir.clone()));
        let presence = Arc::new(NodePresence::default());
        let mut senders = Vec::new();
        let mut threads = Vec::new();
        for k in 0..cores {
            let disk = match open_disk() {
                Ok(d) => d,
                Err(e) => {
                    stop_cores(&senders, threads);
                    return Err(NodeError::Io(e));
                }
            };
            let group0 = (k == 0).then(|| Group0Spec {
                desc: group0.clone(),
                founder: (node_id == FOUNDER).then(|| (meta.cluster_id, addrs.clone())),
                addrs: addrs.clone(),
                boot_epoch: meta.boot_epoch,
                tags: cfg.tags.clone(),
            });
            let spec = CoreSpec {
                core: k,
                node_id,
                clock: clock.clone(),
                storage: cfg.storage.clone(),
                lsm: lsm.clone(),
                snapshot_entries: cfg.snapshot_entries,
                seed: seed.wrapping_add(u64::from(k)),
                group0,
                split_bytes: cfg.split_bytes,
                routes: (k == 0).then(|| routes.clone()),
                hello: Hello { core: k, ..hello.clone() },
                tick: cfg.tick,
                sessions: (k == 0).then(|| sessions.clone()),
                search: sessions.clone(),
                auth: (k == 0).then(|| auth.clone()),
                root_user: if k == 0 { cfg.root_user.clone() } else { None },
                clustertime_keys: if k == 0 { keys.clone() } else { None },
                peer_tls: cfg.peer_tls.clone(),
                loads: loads.clone(),
                presence: (k == 0).then(|| presence.clone()),
                hosting: hosting.clone(),
                cpu: cpus[usize::from(k)],
            };
            let (tx, rx) = mpsc::channel();
            let spawned = thread::Builder::new()
                .name(format!("core-{k}"))
                .spawn(move || run_core(spec, disk, tx));
            let built = match spawned {
                Ok(t) => {
                    threads.push(t);
                    rx.recv().unwrap_or_else(|_| Err(NodeError::Config(format!("core {k} failed"))))
                }
                Err(e) => Err(NodeError::Io(e)),
            };
            match built {
                Ok(sender) => senders.push(sender),
                Err(e) => {
                    stop_cores(&senders, threads);
                    return Err(e);
                }
            }
        }
        for s in &senders {
            // A core that already stopped is noticed by whoever talks to it.
            let _ = s.send_waiting(CoreInput::App(NodeMsg::Cores(senders.clone())));
        }
        // The handshake threads share these handlers: each call makes its own sender.
        let core0 = senders[0].senders();
        let join: JoinHandler = Arc::new(move |req| {
            let (tx, rx) = mpsc::channel();
            core0
                .sender()
                .send_waiting(CoreInput::App(NodeMsg::Join(req, tx)))
                .map_err(|_| "the node is stopping".to_owned())?;
            rx.recv_timeout(JOIN_TIMEOUT).map_err(|_| "timeout".to_owned())?
        });
        let enroll = cfg.enroller.clone().map(|e| {
            let core0 = senders[0].senders();
            let handler: EnrollHandler = Arc::new(move |req| enroll_one(&e, &core0.sender(), &req));
            handler
        });
        let opts = AcceptorOptions { join: Some(join), enroll, tls: cfg.peer_tls.clone() };
        let acceptor = match Acceptor::spawn_with(
            listener,
            hello,
            senders.clone(),
            CoreInput::Inbound,
            opts,
        ) {
            Ok(a) => a,
            Err(e) => {
                stop_cores(&senders, threads);
                return Err(NodeError::Io(e));
            }
        };
        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let client = match &cfg.client {
            None => None,
            Some(c) => {
                let handle = NodeHandle {
                    node: node_id,
                    senders: senders.iter().map(CoreSender::senders).collect(),
                    clock: clock.clone(),
                    sessions: sessions.clone(),
                    routes: routes.clone(),
                    auth: auth.clone(),
                    shutdown: shutdown.clone(),
                };
                let mut state = NodeClockState::new(clock.clone(), random_u64())
                    .with_handle(handle)
                    .with_presence(presence.clone());
                if let Some((_, slot)) = &keys {
                    state = state.with_keys(slot.clone());
                }
                let state = Arc::new(state);
                match ClientListener::spawn_with(c, cores, state, cfg.client_hooks) {
                    Ok(l) => Some(l),
                    Err(e) => {
                        acceptor.stop();
                        stop_cores(&senders, threads);
                        return Err(NodeError::Io(e));
                    }
                }
            }
        };
        let admin = match cfg.admin_listen {
            None => None,
            Some(at) => {
                let backend = NodeAdmin {
                    handle: NodeHandle {
                        node: node_id,
                        senders: senders.iter().map(CoreSender::senders).collect(),
                        clock: clock.clone(),
                        sessions: sessions.clone(),
                        routes: routes.clone(),
                        auth: auth.clone(),
                        shutdown: shutdown.clone(),
                    },
                    enroller: cfg.enroller.clone(),
                    certificates: cfg.client.as_ref().and_then(|c| c.certificates.clone()),
                    memory,
                };
                let handler: Handler = Arc::new(move |req, caller| answer(req, caller, &backend));
                match AdminListener::spawn(at, handler, cfg.admin_tls.clone()) {
                    Ok(l) => Some(l),
                    Err(e) => {
                        if let Some(c) = client {
                            c.stop();
                        }
                        acceptor.stop();
                        stop_cores(&senders, threads);
                        return Err(NodeError::Io(e));
                    }
                }
            }
        };
        Ok(Node {
            node_id,
            cluster_id: meta.cluster_id,
            cores,
            peer_addr,
            senders: senders.iter().map(CoreSender::senders).collect(),
            threads,
            acceptor,
            client,
            admin,
            clock: clock_keeper,
            routes,
            enroller: cfg.enroller.clone(),
            memory,
            cpus,
            lsm,
            shutdown,
            _lock: lock,
        })
    }

    /// The node's memory and the split between its cores (012 FR-013); `None` when none was given
    /// and the system did not say how much there is.
    pub fn memory(&self) -> Option<crate::memory::MemoryBudget> {
        self.memory
    }

    /// The CPU each core is pinned to, by core (012 FR-006); `None` for a core left unpinned.
    pub fn cpus(&self) -> &[Option<usize>] {
        &self.cpus
    }

    /// The LSM configuration of each core: the node's with the memtables and the block cache in
    /// their parts of the node's memory.
    pub fn core_lsm(&self) -> &LsmConfig {
        &self.lsm
    }

    /// Issues an enrollment token valid for `ttl` (013 FR-005): Group 0 records it and the
    /// returned text starts a node with `--join <addr> --enroll-token <text>`.
    pub fn create_enroll_token(&self, ttl: Duration) -> Result<String, String> {
        let e = self.enroller.as_ref().ok_or("this node has no PKI (--insecure)")?;
        enroll_token(e, &self.senders[0].sender(), ttl)
    }

    /// Whether a client asked the node to stop (`shutdown`, 007 FR-018).
    pub fn shutdown_requested(&self) -> bool {
        self.shutdown.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Where the administration port listens, if the node opened it.
    pub fn admin_addr(&self) -> Option<SocketAddr> {
        self.admin.as_ref().map(AdminListener::local_addr)
    }
    /// This node.
    pub fn node_id(&self) -> NodeId {
        self.node_id
    }

    /// Cluster.
    pub fn cluster_id(&self) -> [u8; 16] {
        self.cluster_id
    }

    /// Cores.
    pub fn cores(&self) -> u16 {
        self.cores
    }

    /// Address the inter-node port listens on.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Addresses the clients' port listens on (none if the node does not open it).
    pub fn client_addrs(&self) -> &[SocketAddr] {
        self.client.as_ref().map_or(&[], |c| c.addrs())
    }

    /// Core 0's view of Group 0; `None` if it does not answer within `timeout`.
    pub fn view(&self, timeout: Duration) -> Option<Group0View> {
        let (tx, rx) = mpsc::channel();
        self.senders[0].sender().send(CoreInput::App(NodeMsg::View(tx))).ok()?;
        rx.recv_timeout(timeout).ok()
    }

    /// Waits until Group 0 has the cluster created, this node's current addresses and the `root`
    /// user this node creates, if any ([`Group0View::ready`]; `false` if `timeout` passes first).
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.view(timeout).is_some_and(|v| v.ready()) {
                return true;
            }
            thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// The last failed write of the clock bound, while writing it keeps failing.
    pub fn clock_error(&self) -> Option<StorageError> {
        self.clock.error()
    }

    /// A request for a data range, answered within `timeout` by the core that holds the range:
    /// the range of `key` for a put or a get, range `g` otherwise.
    pub fn data(&self, req: DataReq, timeout: Duration) -> Result<DataReply, DataError> {
        let g = match &req {
            DataReq::Put { key, .. } | DataReq::Get { key } => {
                self.routes.lookup(key).map(|e| e.group)
            }
            DataReq::Status(g) | DataReq::Remote(g, _) | DataReq::At(_, g, _) => Some(*g),
        };
        let (tx, rx) = mpsc::channel();
        let core = usize::from(self.core_of(g.unwrap_or(DATA_RANGE)));
        self.senders[core]
            .sender()
            .send(CoreInput::App(NodeMsg::Data(req, tx)))
            .map_err(|_| DataError::NoAnswer)?;
        match rx.recv_timeout(timeout) {
            Ok(r) => r.map_err(DataError::Range),
            Err(_) => Err(DataError::NoAnswer),
        }
    }

    /// The core that holds range `g` on every node; for a range the table does not know yet,
    /// where Group 0 places the first data range.
    fn core_of(&self, g: GroupId) -> u16 {
        self.routes.entry(g).map_or_else(|| data_range_core(self.cores), |e| e.core)
    }

    /// The ranges the node's routing table knows, in key order.
    pub fn ranges(&self) -> Vec<teseladb_router::table::RouteEntry> {
        self.routes.entries()
    }

    /// Writes `value` at `key` in the data range with `w: "majority"` and `j: true`; the mark of
    /// the write once confirmed. Only the range's leader takes it (`NotLeader` names the leader it
    /// knows).
    pub fn put(&self, key: &[u8], value: &[u8], timeout: Duration) -> Result<Hlc, DataError> {
        self.put_with(key, value, WriteConcern { w: W::Majority, j: true }, timeout)
    }

    /// [`Node::put`] with write concern `wc` (FR-010: `w: 1` is confirmed once the entry is in the
    /// leader's log, and lost if that leader falls before the majority has it).
    pub fn put_with(
        &self,
        key: &[u8],
        value: &[u8],
        wc: WriteConcern,
        timeout: Duration,
    ) -> Result<Hlc, DataError> {
        let req = DataReq::Put { key: key.to_vec(), value: value.to_vec(), wc };
        match self.data(req, timeout)? {
            DataReply::Put(ts) => Ok(ts),
            other => Err(DataError::Unexpected(format!("{other:?}"))),
        }
    }

    /// Reads `key` in the data range on its leader (`linearizable`).
    pub fn get(&self, key: &[u8], timeout: Duration) -> Result<Option<Vec<u8>>, DataError> {
        match self.data(DataReq::Get { key: key.to_vec() }, timeout)? {
            DataReply::Get(v) => Ok(v),
            other => Err(DataError::Unexpected(format!("{other:?}"))),
        }
    }

    /// This node's replica of the first data range; `None` if its core does not answer in
    /// `timeout`.
    pub fn data_status(&self, timeout: Duration) -> Option<DataStatus> {
        self.range_status(DATA_RANGE, timeout)
    }

    /// This node's replica of range `g`; `None` if its core does not answer in `timeout`.
    pub fn range_status(&self, g: GroupId, timeout: Duration) -> Option<DataStatus> {
        match self.data(DataReq::Status(g), timeout) {
            Ok(DataReply::Status(s)) => Some(s),
            _ => None,
        }
    }

    /// Stops the cores and the acceptor. Returns each core's disk (`None` for a core whose thread
    /// failed).
    pub fn stop(self) -> Vec<Option<D>> {
        if let Some(c) = self.client {
            c.stop();
        }
        if let Some(a) = self.admin {
            a.stop();
        }
        self.acceptor.stop();
        self.clock.stop();
        let senders: Vec<_> = self.senders.iter().map(CoreSenders::sender).collect();
        stop_cores(&senders, self.threads)
    }
}

/// Stops the cores of `senders` and waits for their threads; each core's disk.
fn stop_cores<D>(senders: &[CoreSender<NodeMsg>], threads: Vec<CoreThread<D>>) -> Vec<Option<D>> {
    for s in senders {
        let _ = s.send_waiting(CoreInput::Stop);
    }
    threads.into_iter().map(|t| t.join().ok().and_then(Result::ok).flatten()).collect()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::sync::Arc;

    use teseladb_io::mailbox::{Notify, channel};

    use super::*;

    struct Quiet;

    impl Notify for Quiet {
        fn notify(&self) {}
    }

    /// A core 0 that answers the first catalog request it gets with `answer`.
    fn answering(answer: fn() -> CatalogAnswer) -> CoreSender<NodeMsg> {
        let (tx, mut mailbox) = channel::<CoreInput<NodeMsg>>(Arc::new(Quiet));
        thread::spawn(move || {
            let mut got = Vec::new();
            let deadline = Instant::now() + ENROLL_WAIT;
            while Instant::now() < deadline {
                mailbox.drain_into(&mut got);
                if let Some(CoreInput::App(NodeMsg::Catalog(_, reply))) = got.pop() {
                    let _ = reply.send(answer());
                    return;
                }
                thread::sleep(Duration::from_millis(1));
            }
        });
        tx
    }

    #[test]
    fn group0s_refusals_of_a_token_say_why() {
        let tick = || Group0Command::ClusterTick { cluster_time_ms: 1 };
        let cases: [(fn() -> CatalogAnswer, &str); 6] = [
            (|| CatalogAnswer::Done(None), ""),
            (|| CatalogAnswer::Refused(Group0Error::EnrollTokenUsed), "already used"),
            (|| CatalogAnswer::Refused(Group0Error::EnrollTokenExpired), "expired"),
            (|| CatalogAnswer::Refused(Group0Error::EnrollTokenUnknown), "not known"),
            (|| CatalogAnswer::Refused(Group0Error::NoSuchNode), "Group 0 refused it"),
            (|| CatalogAnswer::Unavailable, "did not answer"),
        ];
        for (answer, says) in cases {
            let core0 = answering(answer);
            match group0_change(&core0, tick()) {
                Ok(()) => assert!(says.is_empty()),
                Err(e) => assert!(!says.is_empty() && e.contains(says), "{e}"),
            }
        }
    }

    #[test]
    fn a_group0_change_to_a_core_that_stopped_says_so() {
        let (tx, mailbox) = channel::<CoreInput<NodeMsg>>(Arc::new(Quiet));
        drop(mailbox);
        let cmd = Group0Command::ClusterTick { cluster_time_ms: 1 };
        assert_eq!(group0_change(&tx, cmd), Err("the node is stopping".to_owned()));
    }
}
