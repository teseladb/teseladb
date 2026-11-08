//! The clients' port (006 FR-005): an acceptor that hands each connection to one of the node's
//! client workers in turn, one worker per core, each with its own poller; the protocol is
//! `teseladb-wire`'s, which turns bytes into requests and replies into bytes.
//!
//! A worker only reads, writes and parses: a command of a node runs on one of the worker's
//! executor threads, which waits for the cores, so a command that waits (for a majority, a
//! leader or its `maxTimeMS`) never holds up the worker's other connections. A connection has at
//! most one command running; what arrives meanwhile waits in its backlog, in order.
//!
//! Beyond `--maxConns` open connections a new one is closed at once, as `refnode` does with
//! `net.maxIncomingConnections`. An awaitable `hello` waits in its worker until its deadline (or
//! until the node changes state), and with `exhaustAllowed` keeps answering that way.

use std::collections::BTreeMap;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use teseladb_hlc::to_timestamp;
use teseladb_io::mailbox::{self, Mailbox, MailboxSender};
use teseladb_io::poller::{Interest, Poller, WAKE_TOKEN, Waker};
use teseladb_multiraft::NodeClock;
use teseladb_txn::reaper::Reaper;
use teseladb_wire::commands::diagnostic_data;
use teseladb_wire::commands::registry::{REFRESH_INTERVAL_MS, refresh};
use teseladb_wire::commands::{
    Backend, NodeSessions, REAP_INTERVAL_MS, Shared, Timeouts, lock, reap,
};
use teseladb_wire::compressed::Compressor;
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{
    Action, ClusterTime, NodeInfo, Topology, answers_without_waiting, dispatch_with,
    hello_after_wait,
};

use crate::backend::{NodeHandle, SCAN_EVERY, ServerBackend};
use crate::certs::Certificates;
use crate::clustertime_keys::KeySlot;
use crate::diag::{Diag, PortStats, ProcessConfig};
use crate::diagnostic_capture::{self, Capture};
use crate::tls_port::{TlsPort, Transport};
use teseladb_wire::error::{CommandError, SHUTDOWN_IN_PROGRESS};
use teseladb_wire::hello::HelloRequest;
use teseladb_wire::ingress::Admission;

/// RefDb's default `net.maxIncomingConnections`.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1_000_000;
/// Bytes read from a socket at a time.
const READ_CHUNK: usize = 64 * 1024;

/// What the clients' port needs to know.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientConfig {
    /// Addresses to listen on (`--bind_ip` with `--port`).
    pub addrs: Vec<SocketAddr>,
    /// Most open connections (`--maxConns`).
    pub max_connections: usize,
    /// Compressors the node takes (`--networkMessageCompressors`).
    pub compressors: Vec<Compressor>,
    /// How long cursors and sessions live without use.
    pub timeouts: Timeouts,
    /// Time between two reapings of expired cursors and sessions.
    pub reap_every: Duration,
    /// Time between two refreshes of the node's sessions in `config.system.sessions`.
    pub refresh_every: Duration,
    /// `enableTestCommands`: `configureFailPoint` answers.
    pub test_commands: bool,
    /// `--redactClientLogData` (013 FR-027): the value the node starts with.
    pub redact_client_log_data: bool,
    /// Whether server-side JavaScript runs (`--noscripting` turns it off, 016 FR-009).
    pub javascript_enabled: bool,
    /// `--setParameter auditAuthorizationSuccess` of a node without an audit log (with one, the
    /// auditor's configuration has it).
    pub audit_authorization_success: bool,
    /// `ocspEnabled` (013 FR-031).
    pub ocsp_enabled: bool,
    /// `useInternalAuthzForX509` (013 FR-033).
    pub use_internal_authz_for_x509: bool,
    /// `clusterAuthMode` at start (013 FR-034).
    pub cluster_auth_mode: teseladb_wire::commands::server_params::ClusterAuthMode,
    /// TLS on the connections the node opens (`preferTLS` or `requireTLS`), which the modes of
    /// `clusterAuthMode` past `keyFile` need.
    pub tls_outgoing: bool,
    /// `transactionLifetimeLimitSeconds` (008 FR-010).
    pub transaction_lifetime_secs: i64,
    /// `maxConcurrentMultiDocumentTransactions` (008 FR-020).
    pub max_concurrent_transactions: i64,
    /// The command line and data directory the diagnostic commands report.
    pub process: ProcessConfig,
    /// TLS on the port (`--tlsMode` and the certificate options, 013).
    pub tls: TlsPort,
    /// Clients authenticate before running commands (a node started without `--insecure`, 013).
    pub auth: bool,
    /// What `rotateCertificates` loads again (013 FR-006); `None` on a node without a PKI.
    pub certificates: Option<Arc<Certificates>>,
    /// The audit log (013 FR-016).
    pub audit: Option<crate::audit_log::AuditLog>,
    /// The LDAP server of PLAIN logins (013 FR-025).
    pub ldap: Option<crate::ldap_client::SharedLdap>,
    /// The STS of REFDB-AWS logins (013 FR-025).
    pub sts: Option<Arc<crate::sts_client::Sts>>,
    /// The identity providers of REFDB-OIDC logins (013 FR-025).
    pub oidc: Option<Arc<crate::oidc_client::Oidc>>,
    /// The keytab of GSSAPI logins (013 FR-025).
    pub kerberos: Option<Arc<Vec<teseladb_auth::kerberos::ServiceKey>>>,
    /// `authenticationMechanisms`; empty, RefDb's default.
    pub mechanisms: Vec<teseladb_auth::sasl::SaslMechanism>,
    /// The parameters of the limiter of new connections given at start (006 FR-036), in order.
    pub ingress: Vec<teseladb_wire::ingress::Setting>,
    /// The other parameters of `--setParameter` (006 FR-037), applied in order after the rest.
    pub startup: Vec<teseladb_wire::commands::server_params::Startup>,
}

impl ClientConfig {
    /// The node's empty cursor and session tables for this port.
    pub fn sessions(&self) -> NodeSessions {
        let mut s = NodeSessions::new(self.timeouts);
        s.audit = self.audit.as_ref().map(crate::audit_log::AuditLog::for_node);
        s.params.monitor_ms = i64::try_from(self.reap_every.as_millis()).unwrap_or(i64::MAX);
        s.params.redact_client_log_data = self.redact_client_log_data;
        s.params.javascript_enabled = self.javascript_enabled;
        s.params.audit_authorization_success = self.audit_authorization_success;
        s.params.ocsp_enabled = self.ocsp_enabled;
        s.params.use_internal_authz_for_x509 = self.use_internal_authz_for_x509;
        s.params.cluster_auth_mode = self.cluster_auth_mode;
        s.params.tls_outgoing = self.tls_outgoing;
        s.params.transaction_lifetime_secs = self.transaction_lifetime_secs;
        s.params.max_concurrent_transactions = self.max_concurrent_transactions;
        for setting in &self.ingress {
            s.ingress.set(setting.clone());
        }
        for parameter in &self.startup {
            teseladb_wire::commands::server_params::apply_startup(&mut s, parameter);
        }
        if self.test_commands { s.with_test_commands() } else { s }
    }

    /// The port on `addrs` with RefDb's defaults.
    pub fn new(addrs: Vec<SocketAddr>) -> ClientConfig {
        ClientConfig {
            addrs,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            compressors: crate::cli::DEFAULT_COMPRESSORS.to_vec(),
            timeouts: Timeouts::default(),
            reap_every: Duration::from_millis(REAP_INTERVAL_MS),
            refresh_every: Duration::from_millis(REFRESH_INTERVAL_MS),
            test_commands: false,
            redact_client_log_data: false,
            javascript_enabled: true,
            audit_authorization_success: false,
            ocsp_enabled: true,
            use_internal_authz_for_x509: false,
            cluster_auth_mode: teseladb_wire::commands::server_params::ClusterAuthMode::Undefined,
            tls_outgoing: true,
            transaction_lifetime_secs:
                teseladb_wire::commands::server_params::TRANSACTION_LIFETIME_DEFAULT_SECS,
            max_concurrent_transactions:
                teseladb_wire::commands::server_params::MAX_CONCURRENT_TRANSACTIONS_DEFAULT,
            process: ProcessConfig::default(),
            tls: TlsPort::default(),
            auth: false,
            certificates: None,
            audit: None,
            ldap: None,
            sts: None,
            oidc: None,
            kerberos: None,
            mechanisms: Vec::new(),
            ingress: Vec::new(),
            startup: Vec::new(),
        }
    }
}

/// Where replies get the node's state from.
pub trait NodeState: Send + Sync + 'static {
    /// The node's `topologyVersion`, cluster time and wall clock now.
    fn info(&self) -> NodeInfo;

    /// How the data commands reach the node's cores; `None` answers only the handshake and
    /// `ping` (a port without a node behind it).
    fn handle(&self) -> Option<NodeHandle> {
        None
    }

    /// The node drained and closed its clients' port (007 FR-015): new connections are dropped
    /// and every request gets `ShutdownInProgress` before its connection closes.
    fn closed(&self) -> bool {
        false
    }

    /// Wake `waker` whenever the `topologyVersion` counter changes, so the waiting `hello`s
    /// answer at once (018 FR-015); a node without one never changes it.
    fn watch_topology(&self, _waker: &Arc<Waker>) {}
}

/// What core 0 tells the clients' port of the node's state in Group 0 (007 FR-015): the
/// `topologyVersion` counter and whether the port is closed.
#[derive(Debug, Default)]
pub struct NodePresence {
    counter: AtomicI64,
    closed: AtomicBool,
    /// The replica set and the member this node is, in the replica set presentation (018
    /// FR-007).
    replica: std::sync::Mutex<Option<Arc<ReplicaMember>>>,
    /// The client workers to wake when the counter changes; a worker that stopped drops its
    /// waker and leaves the list at the next change.
    watchers: std::sync::Mutex<Vec<std::sync::Weak<Waker>>>,
    /// The replica set port (018 FR-016) and the member this node is on it.
    port: std::sync::Mutex<Option<(u16, Arc<ReplicaMember>)>>,
}

/// A replica set and the member this node is in it.
pub type ReplicaMember =
    (teseladb_topology::replset::ReplicaSet, teseladb_topology::replset::MemberId);

impl NodePresence {
    /// The counter of the node's state, and whether it drained.
    pub fn set(&self, counter: i64, closed: bool) {
        let before = self.counter.swap(counter, Ordering::AcqRel);
        self.closed.store(closed, Ordering::Release);
        if before != counter {
            let mut watchers =
                self.watchers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            watchers.retain(|w| match w.upgrade() {
                // A wake-up that fails finds the worker stopping, which needs none.
                Some(waker) => {
                    let _ = waker.wake();
                    true
                }
                None => false,
            });
        }
    }

    /// The replica set port and this node's member on it (`None`: closed).
    pub fn set_replica_port(&self, port: Option<(u16, ReplicaMember)>) {
        let mut slot = self.port.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let same = match (slot.as_ref(), port.as_ref()) {
            (Some((a, x)), Some((b, y))) => a == b && **x == *y,
            (None, None) => true,
            _ => false,
        };
        if !same {
            *slot = port.map(|(p, m)| (p, Arc::new(m)));
        }
    }

    /// The replica set port and this node's member on it.
    pub fn replica_port(&self) -> Option<(u16, Arc<ReplicaMember>)> {
        self.port.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// Wakes `waker` at every change of the counter.
    pub fn watch(&self, waker: &Arc<Waker>) {
        let weak = Arc::downgrade(waker);
        self.watchers.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(weak);
    }

    /// How many client workers it wakes.
    pub fn watchers(&self) -> usize {
        let watchers = self.watchers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        watchers.iter().filter(|w| w.strong_count() > 0).count()
    }

    /// The replica set presentation this node answers in (`None`: as a `refrouter`).
    pub fn set_replica(&self, replica: Option<ReplicaMember>) {
        let mut slot = self.replica.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.as_deref() != replica.as_ref() {
            *slot = replica.map(Arc::new);
        }
    }

    /// The replica set presentation this node answers in.
    pub fn replica(&self) -> Option<Arc<ReplicaMember>> {
        self.replica.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
    }

    /// The `topologyVersion` counter.
    pub fn counter(&self) -> i64 {
        self.counter.load(Ordering::Acquire)
    }

    /// Whether the port is closed.
    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// [`NodeState`] of a running node: its process id and its HLC.
#[derive(Clone, Debug)]
pub struct NodeClockState {
    process_id: [u8; 12],
    clock: NodeClock,
    handle: Option<NodeHandle>,
    /// The `$clusterTime` signing keys, on a node with authentication (013 FR-014).
    keys: Option<Arc<KeySlot>>,
    /// The node's state in Group 0 as core 0 publishes it (007 FR-015).
    presence: Arc<NodePresence>,
    /// The state of the replica set port (018 FR-016): `hello` and the commands see the replica
    /// set of that port, whatever the main port shows.
    port_view: bool,
}

fn wall_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

impl NodeClockState {
    /// The state of a node whose HLC is `clock`; `random` makes its `processId`.
    pub fn new(clock: NodeClock, random: u64) -> NodeClockState {
        // An ObjectId: seconds since 1970, then random bytes.
        let mut process_id = [0u8; 12];
        process_id[..4].copy_from_slice(&((wall_ms() / 1000) as u32).to_be_bytes());
        process_id[4..].copy_from_slice(&random.to_be_bytes());
        NodeClockState {
            process_id,
            clock,
            handle: None,
            keys: None,
            presence: Arc::new(NodePresence::default()),
            port_view: false,
        }
    }

    /// The state of a node that signs and checks `$clusterTime` with the keys of `keys`.
    pub fn with_keys(self, keys: Arc<KeySlot>) -> NodeClockState {
        NodeClockState { keys: Some(keys), ..self }
    }

    /// The state with the node's cores behind it, for the data commands.
    pub fn with_handle(self, handle: NodeHandle) -> NodeClockState {
        NodeClockState { handle: Some(handle), ..self }
    }

    /// Where core 0 publishes the node's state in Group 0.
    pub fn with_presence(self, presence: Arc<NodePresence>) -> NodeClockState {
        NodeClockState { presence, ..self }
    }

    /// The state of the replica set port (018 FR-016).
    pub fn on_replica_set_port(self) -> NodeClockState {
        NodeClockState { port_view: true, ..self }
    }
}

impl NodeState for NodeClockState {
    fn info(&self) -> NodeInfo {
        let (time, increment) = to_timestamp(self.clock.0.peek());
        let member = match self.port_view {
            true => self.presence.replica_port().map(|(_, m)| m),
            false => self.presence.replica(),
        };
        let replica_set = member.map(|r| {
            let (set, me) = (*r).clone();
            // `lastWrite`: the node's mark, in the presentation's term.
            let at = teseladb_topology::replset::OpTime {
                ts: teseladb_topology::replset::Timestamp { time, increment },
                term: set.term(),
            };
            let last = teseladb_topology::replset::LastWrite { op_time: at, majority_op_time: at };
            Arc::new(teseladb_wire::hello::ReplicaView { set, me, last })
        });
        NodeInfo {
            topology: Topology { process_id: self.process_id, counter: self.presence.counter() },
            cluster_time: ClusterTime { time, increment },
            now_ms: wall_ms(),
            keys: self.keys.as_ref().map(|k| k.ring()),
            replica_set,
        }
    }

    fn handle(&self) -> Option<NodeHandle> {
        self.handle.clone()
    }

    fn watch_topology(&self, waker: &Arc<Waker>) {
        self.presence.watch(waker);
    }

    fn closed(&self) -> bool {
        self.presence.closed()
    }
}

/// A request of connection `token` for an executor, with the connection it runs on.
struct Job {
    token: u64,
    conn: Connection,
    req: Request,
}

/// What an executor gives back: the connection and the request's action.
struct Finished {
    token: u64,
    conn: Connection,
    req: Request,
    action: Action,
}

/// Starts a thread named after its first argument that runs its second.
pub type SpawnFn = fn(String, Box<dyn FnOnce() + Send>) -> io::Result<()>;

/// How a client listener starts its executors and how often it runs the TTL monitor: in
/// production [`Hooks::default`]; a test gives a spawn that fails or starts late, and a short TTL
/// period, to reach what a running node meets only by chance.
#[derive(Clone, Copy, Debug)]
pub struct Hooks {
    /// Starts an executor's thread.
    pub spawn_executor: SpawnFn,
    /// Period of the TTL monitor.
    pub ttl_every: Duration,
}

impl Default for Hooks {
    fn default() -> Hooks {
        Hooks {
            spawn_executor: spawn_thread,
            ttl_every: Duration::from_secs(teseladb_wire::commands::ttl::TTL_MONITOR_SECS),
        }
    }
}

/// A named thread that is not joined.
fn spawn_thread(name: String, run: Box<dyn FnOnce() + Send>) -> io::Result<()> {
    command_thread(name).spawn(with_command_stack(run)).map(drop)
}

/// A thread that runs commands: with the stack the calls of server-side JavaScript need (016
/// A016-3-18), which the system reserves and only commits as it is used.
pub(crate) fn command_thread(name: String) -> thread::Builder {
    thread::Builder::new().name(name).stack_size(teseladb_wire::commands::js::COMMAND_STACK)
}

/// `run`, after telling the calls of JavaScript the stack of [`command_thread`].
pub(crate) fn with_command_stack<T>(run: impl FnOnce() -> T) -> impl FnOnce() -> T {
    move || {
        teseladb_wire::commands::js::set_stack(teseladb_wire::commands::js::COMMAND_STACK);
        run()
    }
}

/// A worker's executors: threads that run the commands of its connections against the node,
/// started when none is idle and kept for the next ones.
struct Pool {
    spawn: SpawnFn,
    handle: NodeHandle,
    shared: Arc<Shared>,
    state: Arc<dyn NodeState>,
    diag: Arc<Diag>,
    done: MailboxSender<WorkerMsg>,
    /// Name of the worker, for its executors' threads.
    worker: u16,
    executors: Vec<mpsc::Sender<Job>>,
    idle: Vec<usize>,
    /// Executors that finished a command give their number back here.
    returned: mpsc::Receiver<usize>,
    give_back: mpsc::Sender<usize>,
}

impl Pool {
    /// Hands `job` to an idle executor, starting one if none is; the job back if no thread
    /// could start.
    fn run(&mut self, job: Job) -> Result<(), Box<Job>> {
        self.idle.extend(self.returned.try_iter());
        let k = match self.idle.pop() {
            Some(k) => k,
            None => match self.start() {
                Some(k) => k,
                None => return Err(Box::new(job)),
            },
        };
        self.executors[k].send(job).map_err(|mpsc::SendError(job)| Box::new(job))
    }

    fn start(&mut self) -> Option<usize> {
        let k = self.executors.len();
        let (tx, rx) = mpsc::channel::<Job>();
        let seed = seed(self.worker) ^ ((k as u64) << 32);
        let mut backend =
            ServerBackend::new(self.handle.clone(), seed).with_diag(self.diag.clone());
        let (shared, state, diag, done) =
            (self.shared.clone(), self.state.clone(), self.diag.clone(), self.done.clone());
        let give_back = self.give_back.clone();
        let name = format!("client-{}-run-{k}", self.worker);
        let run = move || {
            while let Some(Job { token, mut conn, req }) = next_job(&rx) {
                let info = state.info();
                diag.stats.active.fetch_add(1, Ordering::Relaxed);
                let action = dispatch_with(&mut conn, &req, &info, Some((&mut backend, &*shared)));
                diag.stats.active.fetch_sub(1, Ordering::Relaxed);
                let finished = Finished { token, conn, req, action };
                // Idle again: the pool reads it before it picks an executor.
                let _ = give_back.send(k);
                if done.send_waiting(WorkerMsg::Done(Box::new(finished))).is_err() {
                    return;
                }
                // With the answer on its way, the intents of a transaction it ended (008 FR-005).
                teseladb_wire::commands::txn::resolve_queued(&mut backend, &shared);
            }
        };
        // The thread ends when the worker drops its sender; it is not joined, since a command
        // may still be waiting for its deadline when the port stops.
        (self.spawn)(name, Box::new(run)).ok()?;
        self.executors.push(tx);
        Some(k)
    }
}

/// The next job of an executor, or `None` once its worker dropped the sender. A job that arrives
/// within a short spin is taken without the executor sleeping: the worker hands each command over
/// and the wake-up of the executor was one of the waits of every request (H-013).
fn next_job(rx: &mpsc::Receiver<Job>) -> Option<Job> {
    let mut got = None;
    teseladb_io::spin::spin_until(teseladb_io::spin::SPIN_BEFORE_SLEEP, || match rx.try_recv() {
        Ok(job) => {
            got = Some(Some(job));
            true
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            got = Some(None);
            true
        }
        Err(mpsc::TryRecvError::Empty) => false,
    });
    got.unwrap_or_else(|| rx.recv().ok())
}

/// What a worker's connections run the commands with: its executors when a node is behind the
/// port, except for the commands that never wait, which the worker runs with its own backend.
struct Cmds<'a> {
    pool: Option<&'a mut Pool>,
    inline: Option<&'a mut ServerBackend>,
    shared: &'a Shared,
    state: &'a dyn NodeState,
}

/// The seed of worker `k`'s random numbers: the clock and the worker.
fn seed(k: u16) -> u64 {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    nanos ^ (u64::from(k) << 48) ^ 0x9e37_79b9_7f4a_7c15
}

/// A connection for a worker, or a command an executor finished.
enum WorkerMsg {
    Conn(TcpStream, i32),
    Done(Box<Finished>),
}

/// The running clients' port.
pub struct ClientListener {
    addrs: Vec<SocketAddr>,
    stop: Arc<AtomicBool>,
    acceptor: (Arc<Waker>, JoinHandle<()>),
    workers: Vec<(Arc<Waker>, JoinHandle<()>)>,
    /// The thread that reaps and refreshes the node's sessions, with a node behind the port.
    keeper: Option<JoinHandle<()>>,
    /// The thread that builds the node's search indexes, with a node behind the port.
    builder: Option<JoinHandle<()>>,
    /// The thread that reads the data each tenant uses (007 FR-031), with a node behind the port.
    usage: Option<JoinHandle<()>>,
    open: Arc<AtomicUsize>,
}

impl std::fmt::Debug for ClientListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientListener").field("addrs", &self.addrs).finish()
    }
}

impl ClientListener {
    /// Listens on `cfg.addrs` with `workers` workers.
    pub fn spawn(
        cfg: &ClientConfig,
        workers: u16,
        state: Arc<dyn NodeState>,
    ) -> io::Result<ClientListener> {
        ClientListener::spawn_with(cfg, workers, state, Hooks::default())
    }

    /// [`ClientListener::spawn`] with `hooks`, for tests.
    #[doc(hidden)]
    pub fn spawn_with(
        cfg: &ClientConfig,
        workers: u16,
        state: Arc<dyn NodeState>,
        hooks: Hooks,
    ) -> io::Result<ClientListener> {
        let listeners = cfg
            .addrs
            .iter()
            .map(|a| crate::listen::bind(crate::listen::CLIENTS, *a))
            .collect::<io::Result<Vec<_>>>()?;
        let addrs =
            listeners.iter().map(TcpListener::local_addr).collect::<io::Result<Vec<_>>>()?;
        let stop = Arc::new(AtomicBool::new(false));
        let open = Arc::new(AtomicUsize::new(0));
        let port = addrs.first().map_or(0, SocketAddr::port);
        let diag = Arc::new(
            Diag::new(cfg.process.clone(), port, open.clone(), cfg.max_connections)
                .with_certificates(cfg.certificates.clone())
                .with_tls(cfg.tls.clone())
                .with_ldap(cfg.ldap.clone())
                .with_sts(cfg.sts.clone())
                .with_oidc(cfg.oidc.clone())
                .with_kerberos(cfg.kerberos.clone()),
        );
        let shared: Arc<Shared> = match state.handle() {
            Some(h) => h.sessions,
            None => Arc::new(Mutex::new(cfg.sessions())),
        };
        let mut handles = Vec::new();
        let mut senders = Vec::new();
        for k in 0..workers.max(1) {
            let poller = Poller::new()?;
            let waker = Arc::new(poller.waker());
            state.watch_topology(&waker);
            let (tx, rx) = mailbox::channel(waker.clone());
            let pool = state.handle().map(|handle| {
                let (give_back, returned) = mpsc::channel();
                Pool {
                    spawn: hooks.spawn_executor,
                    handle,
                    shared: shared.clone(),
                    state: state.clone(),
                    diag: diag.clone(),
                    done: tx.clone(),
                    worker: k,
                    executors: Vec::new(),
                    idle: Vec::new(),
                    returned,
                    give_back,
                }
            });
            let w = Worker {
                poller,
                mailbox: rx,
                conns: BTreeMap::new(),
                awaits: BTreeMap::new(),
                seen: state.info().topology.counter,
                next_token: 1,
                state: state.clone(),
                pool,
                inline: state
                    .handle()
                    .map(|h| ServerBackend::new(h, seed(k)).with_diag(diag.clone())),
                shared: shared.clone(),
                diag: diag.clone(),
                compressors: cfg.compressors.clone(),
                tls: cfg.tls.clone(),
                auth: cfg.auth,
                mechanisms: cfg.mechanisms.clone(),
                open: open.clone(),
                stop: stop.clone(),
                tenants_seen: None,
            };
            let t =
                command_thread(format!("client-{k}")).spawn(with_command_stack(move || w.run()))?;
            handles.push((waker, t));
            senders.push(tx);
        }
        let poller = Poller::new()?;
        for (i, l) in listeners.iter().enumerate() {
            l.set_nonblocking(true)?;
            poller.register(l, i as u64, Interest::READ)?;
        }
        let waker = Arc::new(poller.waker());
        let acceptor = Acceptor {
            poller,
            listeners,
            workers: senders,
            max: cfg.max_connections,
            open: open.clone(),
            stop: stop.clone(),
            next_id: 1,
            diag: diag.clone(),
            state: state.clone(),
        };
        let t =
            thread::Builder::new().name("client-accept".into()).spawn(move || acceptor.run())?;
        let builder = match state.handle() {
            Some(h) => {
                let backend =
                    ServerBackend::new(h, seed(workers.saturating_add(1))).with_diag(diag.clone());
                Some(crate::search_builder::spawn(backend, shared.clone(), stop.clone())?)
            }
            None => None,
        };
        let usage = match state.handle() {
            Some(h) => {
                let backend = ServerBackend::new(h, seed(workers.saturating_add(2)));
                Some(crate::tenant_limits::spawn_reader(backend, stop.clone())?)
            }
            None => None,
        };
        let keeper = match state.handle() {
            Some(h) => {
                let k = Keeper {
                    backend: ServerBackend::new(h, seed(workers)).with_diag(diag.clone()),
                    capture: cfg.process.dbpath.as_deref().map(Capture::new),
                    shared,
                    refresh_every: cfg.refresh_every,
                    ttl_every: hooks.ttl_every,
                    stop: stop.clone(),
                    reapers: BTreeMap::new(),
                };
                Some(thread::Builder::new().name("client-sessions".into()).spawn(move || k.run())?)
            }
            None => None,
        };
        Ok(ClientListener {
            addrs,
            stop,
            acceptor: (waker, t),
            workers: handles,
            keeper,
            builder,
            usage,
            open,
        })
    }

    /// The addresses it listens on (with the ports the system chose for port 0).
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// Connections open now.
    pub fn open_connections(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }

    /// Stops accepting, closes every connection and waits for the threads.
    pub fn stop(self) {
        self.stop.store(true, Ordering::Release);
        let threads = std::iter::once(self.acceptor).chain(self.workers);
        for (waker, t) in threads {
            // A thread that already ended needs no wake-up, and its end is all `join` waits for.
            let _ = waker.wake();
            let _ = t.join();
        }
        for k in self.keeper.into_iter().chain(self.builder).chain(self.usage) {
            k.thread().unpark();
            let _ = k.join();
        }
    }
}

/// Reaps the node's expired cursors and sessions, writes the sessions it used to
/// `config.system.sessions`, runs the TTL monitor and the reapers of abandoned transactions, each
/// on its own period, at the node's cluster time.
struct Keeper {
    backend: ServerBackend,
    shared: Arc<Shared>,
    refresh_every: Duration,
    /// Period of the TTL monitor (009 FR-010): `ttlMonitorSleepSecs`.
    ttl_every: Duration,
    stop: Arc<AtomicBool>,
    /// The reapers of abandoned transactions, one per core (008 FR-007).
    reapers: BTreeMap<u16, Reaper>,
    /// The periodic diagnostic capture of a node with a data directory (010 FR-024).
    capture: Option<Capture>,
}

impl Keeper {
    /// Writes the node's diagnostic sample (010 FR-024). A sample that cannot be taken or
    /// written is logged and the next one comes a period later.
    fn sample(&mut self) {
        let Some(capture) = self.capture.as_mut() else { return };
        let at = i64::try_from(self.backend.wall_ms()).unwrap_or(i64::MAX);
        let taken = diagnostic_data::capture(&mut self.backend, &self.shared);
        self.backend.end_operation();
        let written = match taken {
            Ok(sample) => capture.write(at, sample).map_err(|e| e.to_string()),
            Err(e) => Err(e.errmsg),
        };
        if let Err(e) = written {
            let attr = format!("{{\"error\":{e:?}}}");
            let msg = "Diagnostic capture failed".to_owned();
            crate::log::defer(crate::log::Pending { id: diagnostic_capture::FAILED_ID, msg, attr });
        }
    }

    /// The reaping period: `clientCursorMonitorFrequencySecs` as `setParameter` left it, at
    /// least a millisecond.
    fn reap_period(&self) -> Duration {
        let ms = lock(&self.shared).params.monitor_ms;
        Duration::from_millis(u64::try_from(ms).unwrap_or(0).max(1))
    }

    fn run(mut self) {
        let start = Instant::now();
        let (mut next_reap, mut next_refresh) =
            (start + self.reap_period(), start + self.refresh_every);
        // The TTL monitor (009 FR-010): every `ttlMonitorSleepSecs` at the cluster time.
        let ttl_every = self.ttl_every;
        let mut next_ttl = start + ttl_every;
        let mut next_txn = start + SCAN_EVERY;
        let mut next_sample = start + diagnostic_capture::PERIOD;
        // Sessions used from this cluster time on go in the next refresh.
        let mut since = 0;
        while !self.stop.load(Ordering::Acquire) {
            let now = Instant::now();
            if now >= next_reap {
                reap(&self.shared, self.backend.now_ms());
                next_reap = now + self.reap_period();
            }
            if now >= next_refresh {
                let at = self.backend.now_ms();
                // A refresh that fails (no leader for the range) goes again at the next one.
                if refresh(&mut self.backend, &self.shared, since, at).is_ok() {
                    since = at;
                }
                self.backend.end_operation();
                next_refresh = now + self.refresh_every;
            }
            if now >= next_ttl {
                let at = i64::try_from(self.backend.now_ms()).unwrap_or(i64::MAX);
                // A pass that fails (no leader for a range) goes again at the next one.
                let _ = teseladb_wire::commands::ttl::pass(&mut self.backend, &self.shared, at);
                // Its reads of the indexes no longer hold back their changes (009 FR-006).
                self.backend.end_operation();
                next_ttl = now + ttl_every;
            }
            if now >= next_txn {
                self.backend.reap_transactions(&mut self.reapers);
                // The heartbeats of the client transactions between their statements.
                teseladb_wire::commands::txn::tick_idle(&mut self.backend, &self.shared);
                self.backend.end_operation();
                next_txn = now + SCAN_EVERY;
            }
            if now >= next_sample {
                self.sample();
                next_sample = now + diagnostic_capture::PERIOD;
            }
            let next = next_reap.min(next_refresh).min(next_ttl).min(next_txn).min(next_sample);
            thread::park_timeout(next.saturating_duration_since(Instant::now()));
        }
    }
}

struct Acceptor {
    poller: Poller,
    listeners: Vec<TcpListener>,
    workers: Vec<MailboxSender<WorkerMsg>>,
    max: usize,
    open: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    next_id: i32,
    diag: Arc<Diag>,
    state: Arc<dyn NodeState>,
}

impl Acceptor {
    fn run(mut self) {
        let mut ready = Vec::new();
        while !self.stop.load(Ordering::Acquire) {
            match self.poller.poll(None) {
                Ok(events) => ready.extend(events.iter().map(|e| e.token)),
                Err(_) => return,
            }
            for token in ready.drain(..).filter(|&t| t != WAKE_TOKEN) {
                self.accept(token as usize);
            }
        }
    }

    /// Accepts every pending connection of listener `i` (non-blocking: it stops when none is
    /// left).
    fn accept(&mut self, i: usize) {
        while let Ok((stream, _)) = self.listeners[i].accept() {
            // A drained node's port takes no connection (007 FR-015).
            if self.state.closed() {
                continue;
            }
            // Over the limit the connection is closed at once, as `refnode` does.
            if self.open.load(Ordering::Acquire) >= self.max {
                PortStats::add(&self.diag.stats.rejected, 1);
                continue;
            }
            self.open.fetch_add(1, Ordering::AcqRel);
            PortStats::add(&self.diag.stats.total_created, 1);
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1).max(1);
            let worker = &self.workers[id as usize % self.workers.len()];
            // A worker only goes away when the listener stops, after this thread.
            let _ = worker.send_waiting(WorkerMsg::Conn(stream, id));
        }
    }
}

/// What waits in a worker until its deadline.
enum Pending {
    /// An awaitable `hello` waiting for its answer.
    Hello(HelloWait),
    /// A request the failpoint holds (`blockConnection`), dispatched again at the deadline with
    /// its connection, which waits with it.
    Held { conn: Box<Connection>, req: Request, deadline: Instant },
    /// The first request of a connection waiting in the queue of the limiter of new connections
    /// for its token (006 FR-036), dispatched at the deadline as a held one.
    Queued { conn: Box<Connection>, req: Request, deadline: Instant },
}

impl Pending {
    fn deadline(&self) -> Instant {
        match self {
            Pending::Hello(h) => h.deadline,
            Pending::Held { deadline, .. } | Pending::Queued { deadline, .. } => *deadline,
        }
    }
}

/// An awaitable `hello` waiting for its answer.
struct HelloWait {
    req: Request,
    hello: HelloRequest,
    deadline: Instant,
    every: Duration,
    exhaust: bool,
    /// The message the next reply answers (the request, then the previous reply of the stream).
    response_to: i32,
    /// The error the failpoint holds the next reply of the stream for (`blockConnection`).
    held: Option<teseladb_bson::Document>,
}

/// One client connection of a worker. Its socket is non-blocking: reads and writes end with
/// `WouldBlock`, never with `Interrupted`.
struct Client {
    stream: TcpStream,
    /// Plain or TLS between the socket and `conn`.
    transport: Transport,
    /// The connection; `None` while an executor runs one of its commands.
    conn: Option<Connection>,
    /// What arrived while the connection was with an executor.
    backlog: Vec<u8>,
    /// Its token in the worker.
    token: u64,
    /// When it was accepted, until its TLS handshake is counted (`None` after, or in clear).
    handshake_from: Option<Instant>,
    out: Vec<u8>,
    pos: usize,
    /// Whether the poller watches it for writing.
    watching_write: bool,
    /// An awaitable `hello` is waiting: later requests wait behind it.
    waiting: bool,
    /// The awaitable `hello` [`Client::serve`] just found, for the worker to schedule.
    new_wait: Option<Pending>,
    /// The port's counters.
    diag: Arc<Diag>,
    /// The subject of the port's certificate, when it names an organization, unit or domain
    /// component: a client certificate with the same ones is a member's (013 FR-034).
    member_subject: Option<Arc<teseladb_x509::Name>>,
    /// The client's address, which the bypass list of the limiter of new connections names.
    peer_ip: Option<IpAddr>,
    /// Whether the limiter of new connections let its first request through (006 FR-036).
    admitted: bool,
    /// The tenant whose connections on this node count this one, once its user signed in (007
    /// FR-032).
    counted: Option<u32>,
}

/// Microseconds since the first connection's first request: the limiter's clock.
fn limiter_now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let micros = START.get_or_init(Instant::now).elapsed().as_micros();
    u64::try_from(micros).unwrap_or(u64::MAX)
}

impl Client {
    /// Reads what arrived; `false` if the peer closed or the socket failed.
    fn read(&mut self) -> bool {
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            match self.stream.read(&mut chunk) {
                Ok(0) => return false,
                Ok(n) => {
                    PortStats::add(&self.diag.stats.physical_in, n);
                    let mut plain = Vec::new();
                    if !self.transport.incoming(&chunk[..n], &mut plain, &mut self.out) {
                        // The TLS alert goes out before the connection closes.
                        let _ = self.write();
                        return false;
                    }
                    match self.conn.as_mut() {
                        Some(conn) => conn.receive(&plain),
                        None => self.backlog.extend_from_slice(&plain),
                    }
                    self.note_handshake();
                    self.note_certificate();
                }
                Err(e) => return e.kind() == ErrorKind::WouldBlock,
            }
        }
    }

    /// Counts the TLS handshake once it has finished (`transportSecurity`).
    fn note_handshake(&mut self) {
        if let Some(from) = self.handshake_from
            && self.transport.tls_established()
        {
            self.handshake_from = None;
            let took = from.elapsed();
            let ms = usize::try_from(took.as_millis()).unwrap_or(usize::MAX);
            let micros = usize::try_from(took.as_micros()).unwrap_or(usize::MAX);
            PortStats::add(&self.diag.stats.tls_handshakes, 1);
            PortStats::add(&self.diag.stats.tls_handshake_ms, ms);
            PortStats::add(&self.diag.stats.tls_handshake_micros, micros);
        }
    }

    /// Records the REFDB-X509 user of the client certificate once the TLS handshake has it.
    fn note_certificate(&mut self) {
        if let Some(conn) = self.conn.as_mut() {
            certify(conn, &self.transport, self.member_subject.as_deref());
        }
    }

    /// Runs the requests that arrived, in order, until one waits; `false` if the connection
    /// must close. The data commands run on `cmds`, waiting for the cores.
    fn serve(&mut self, info: &NodeInfo, cmds: &mut Cmds<'_>) -> bool {
        while !self.waiting {
            let Some(mut conn) = self.conn.take() else { return true };
            let req = match conn.next_request() {
                Ok(Some(r)) => r,
                next => {
                    self.conn = Some(conn);
                    return next.is_ok();
                }
            };
            if !self.admitted {
                // The first request takes a token of the limiter of new connections, waits for
                // one, or closes the connection without a reply (006 FR-036).
                let peer = self.peer_ip;
                match lock(cmds.shared).ingress.admit(peer, limiter_now_us()) {
                    Admission::Go => self.admitted = true,
                    Admission::Reject => {
                        self.conn = Some(conn);
                        return false;
                    }
                    Admission::Wait { micros } => {
                        self.admitted = true;
                        self.waiting = true;
                        let deadline = Instant::now() + Duration::from_micros(micros);
                        self.new_wait =
                            Some(Pending::Queued { conn: Box::new(conn), req, deadline });
                        return true;
                    }
                }
            }
            if !self.handle(conn, req, info, cmds) {
                return false;
            }
        }
        true
    }

    /// Runs what arrived and writes the replies; the replies of the requests before a fault still
    /// go out before the connection closes, as `refnode` answers each message before reading the
    /// next. `false` if the connection must close.
    fn flush_after(&mut self, info: &NodeInfo, cmds: &mut Cmds<'_>) -> bool {
        let served = self.serve(info, cmds);
        self.write() && served
    }

    /// Runs request `req` of `conn`: on an executor, whose action comes back to
    /// [`Client::finish`], or here when no node is behind the port. `false` if the connection must
    /// close.
    fn handle(
        &mut self,
        mut conn: Connection,
        req: Request,
        info: &NodeInfo,
        cmds: &mut Cmds<'_>,
    ) -> bool {
        let stats = &self.diag.stats;
        PortStats::add(&stats.requests, 1);
        PortStats::add(&stats.bytes_in, usize::try_from(req.header.length).unwrap_or(0));
        // A drained node answers every request with `ShutdownInProgress` and closes the
        // connection after the reply (007 FR-015).
        if cmds.state.closed() {
            let e = CommandError::new(SHUTDOWN_IN_PROGRESS, "the node is drained");
            self.apply(conn, req, Action::Reply(e.to_document()));
            return false;
        }
        // With `enableTestCommands` every request goes where the failpoint sees it, the
        // monitors' `hello` too (006 FR-034).
        let tests = lock(cmds.shared).failpoint.is_some();
        let pool = cmds.pool.as_deref_mut().filter(|_| tests || !answers_without_waiting(&req));
        match pool {
            Some(pool) => match pool.run(Job { token: self.token, conn, req }) {
                Ok(()) => true,
                // No thread could start: the connection closes, as when it cannot be served.
                Err(job) => {
                    self.conn = Some(job.conn);
                    false
                }
            },
            None => {
                stats.active.fetch_add(1, Ordering::Relaxed);
                let fresh;
                let (info, commands) = match cmds.inline.as_deref_mut() {
                    Some(b) => {
                        // A write moves the cluster time: each reply reads it after its command.
                        fresh = cmds.state.info();
                        (&fresh, Some((b, cmds.shared)))
                    }
                    None => (info, None),
                };
                let action = dispatch_with(&mut conn, &req, info, commands);
                stats.active.fetch_sub(1, Ordering::Relaxed);
                self.apply(conn, req, action)
            }
        }
    }

    /// Takes back the connection an executor ran `req` on, with what arrived meanwhile, and
    /// queues the request's action; `false` if the connection must close.
    fn finish(&mut self, mut conn: Connection, req: Request, action: Action) -> bool {
        if !self.backlog.is_empty() {
            conn.receive(&std::mem::take(&mut self.backlog));
        }
        certify(&mut conn, &self.transport, self.member_subject.as_deref());
        self.apply(conn, req, action)
    }

    /// Queues the reply of `conn`'s request `req`, or the wait it asks for, and keeps `conn`;
    /// `false` if the connection must close.
    fn apply(&mut self, mut conn: Connection, req: Request, action: Action) -> bool {
        let alive = match action {
            Action::Reply(doc) if req.more_to_come() => {
                drop(doc);
                true
            }
            Action::Reply(doc) => {
                let bytes = conn.reply(&req, &doc, false);
                PortStats::add(&self.diag.stats.bytes_out, bytes.len());
                self.transport.outgoing(&bytes, &mut self.out)
            }
            Action::Await { max_await_ms, exhaust, hello } => {
                let every = Duration::from_millis(max_await_ms);
                let response_to = req.header.request_id;
                let deadline = Instant::now() + every;
                self.waiting = true;
                let wait =
                    HelloWait { req, hello, deadline, every, exhaust, response_to, held: None };
                self.new_wait = Some(Pending::Hello(wait));
                true
            }
            Action::Delay { ms } => {
                // What arrives meanwhile goes to the backlog, as while an executor runs.
                let deadline = Instant::now() + Duration::from_millis(ms);
                self.waiting = true;
                self.new_wait = Some(Pending::Held { conn: Box::new(conn), req, deadline });
                return true;
            }
            Action::Close => false,
        };
        self.conn = Some(conn);
        alive
    }

    /// Answers its awaitable `hello` `p`, due at `p.deadline`; with exhaust, the next one of the
    /// stream, and whether the connection stays open. Each reply of a stream after the first is a
    /// `hello` the failpoint acts on, as on a `refrouter` 9.0.4 with `enableTestCommands`: it holds
    /// it, answers its error and ends the stream, or closes the connection.
    fn fire(
        &mut self,
        mut p: HelloWait,
        info: &NodeInfo,
        shared: &Shared,
    ) -> (Option<Pending>, bool) {
        // A connection waiting for a `hello` is never with an executor.
        let Some(conn) = self.conn.as_mut() else { return (None, true) };
        let streamed = p.response_to != p.req.header.request_id;
        let fault = match (&p.held, streamed) {
            (None, true) => {
                let app = conn.hello.app_name.clone();
                lock(shared).failpoint.as_mut().and_then(|fp| {
                    fp.check(teseladb_wire::concerns::canonical(p.req.name()), app.as_deref())
                })
            }
            _ => None,
        };
        if let Some(f) = fault {
            if f.close {
                return (None, false);
            }
            match (f.block_ms, f.error) {
                (0, None) => {}
                (0, Some(e)) => p.held = Some(e),
                (ms, error) => {
                    p.deadline = Instant::now() + Duration::from_millis(ms);
                    p.held = error;
                    return (Some(Pending::Hello(p)), true);
                }
            }
        }
        if let Some(e) = p.held.take() {
            let doc = teseladb_wire::hello::with_cluster_time(e, info);
            let (bytes, _) = conn.reply_to(p.response_to, &p.req, &doc, false);
            let _ = self.transport.outgoing(&bytes, &mut self.out);
            self.waiting = false;
            return (None, true);
        }
        let doc = hello_after_wait(conn, &p.hello, info);
        let (bytes, id) = conn.reply_to(p.response_to, &p.req, &doc, p.exhaust);
        // A TLS failure here shows on the next read, which closes the connection.
        let _ = self.transport.outgoing(&bytes, &mut self.out);
        self.waiting = p.exhaust;
        p.response_to = id;
        p.deadline += p.every;
        (p.exhaust.then_some(Pending::Hello(p)), true)
    }

    /// Runs the request the failpoint held, on its connection with what arrived meanwhile;
    /// `false` if the connection must close.
    fn resume(
        &mut self,
        mut conn: Connection,
        req: Request,
        info: &NodeInfo,
        cmds: &mut Cmds<'_>,
    ) -> bool {
        if !self.backlog.is_empty() {
            conn.receive(&std::mem::take(&mut self.backlog));
        }
        self.waiting = false;
        self.handle(conn, req, info, cmds)
    }

    /// Writes what it can; `false` if the socket failed.
    fn write(&mut self) -> bool {
        while self.pos < self.out.len() {
            match self.stream.write(&self.out[self.pos..]) {
                Ok(n) => {
                    PortStats::add(&self.diag.stats.physical_out, n);
                    self.pos += n;
                }
                Err(e) => return e.kind() == ErrorKind::WouldBlock,
            }
        }
        self.out.clear();
        self.pos = 0;
        true
    }

    /// Watches the socket for writing while replies wait to go out; `false` if the poller
    /// refused.
    fn watch(&mut self, poller: &Poller, token: u64) -> bool {
        let want = self.pos < self.out.len();
        if want == self.watching_write {
            return true;
        }
        self.watching_write = want;
        let interest = if want { Interest::BOTH } else { Interest::READ };
        poller.reregister(&self.stream, token, interest).is_ok()
    }
}

/// Records on `conn` the REFDB-X509 user of the client certificate `transport` received, if
/// any and not yet recorded; a member's when its subject has the organization, unit or domain
/// components of `member` (013 FR-034).
fn certify(conn: &mut Connection, transport: &Transport, member: Option<&teseladb_x509::Name>) {
    if conn.auth.peer_user.is_none()
        && let Some(leaf) = transport.peer_certificates().first()
    {
        conn.auth.peer_user = Some(leaf.subject.x509_user_name());
        conn.auth.peer_member =
            member.is_some_and(|node| crate::cluster_auth::is_member(&leaf.subject, node));
    }
}

struct Worker {
    poller: Poller,
    mailbox: Mailbox<WorkerMsg>,
    conns: BTreeMap<u64, Client>,
    /// Deadlines of the awaitable `hello`s with their connection; an entry whose connection
    /// closed or already answered is skipped.
    awaits: BTreeMap<(Instant, u64), Pending>,
    /// The `topologyVersion` counter the waiting `hello`s last saw: a change answers them.
    seen: i64,
    next_token: u64,
    state: Arc<dyn NodeState>,
    /// The executors of the commands, if a node is behind the port.
    pool: Option<Pool>,
    /// The backend of the commands the worker runs itself ([`answers_without_waiting`]).
    inline: Option<ServerBackend>,
    /// The node's cursors and sessions, shared by its workers.
    shared: Arc<Shared>,
    compressors: Vec<Compressor>,
    tls: TlsPort,
    /// Clients authenticate before running commands.
    auth: bool,
    /// `authenticationMechanisms`.
    mechanisms: Vec<teseladb_auth::sasl::SaslMechanism>,
    open: Arc<AtomicUsize>,
    /// The port's counters.
    diag: Arc<Diag>,
    stop: Arc<AtomicBool>,
    /// The tenants' states the worker last closed connections for (007 FR-034).
    tenants_seen: Option<Arc<crate::routes::TenantStates>>,
}

/// How long a worker waits at most before it looks at the tenants' states again: a suspended
/// tenant's connections close within the second after its entry applies (007 FR-034).
pub const TENANT_CHECK: Duration = Duration::from_millis(250);

/// What signing in did to the count of a tenant's connections on this node.
enum Counted {
    /// Nothing: not signed in, of the default tenant or its service user.
    No,
    /// Counted for this tenant.
    Yes(u32),
    /// Past the tenant's `maxConnections`: the connection closes.
    Over,
}

/// Counts connection `conn` for its tenant if its user just signed in (007 FR-032).
fn count_connection(routes: &crate::routes::SharedRoutes, conn: &Connection) -> Counted {
    let Some(user) = conn.auth.user.as_ref().filter(|u| u.tenant != 0) else { return Counted::No };
    let states = routes.tenants();
    let limits = states.get(&user.tenant).map(|s| s.limits.clone()).unwrap_or_default();
    if crate::tenant_limits::is_service_user(&limits, &user.name) {
        return Counted::No;
    }
    let wall_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    if routes.limiter().connect(user.tenant, limits.max_connections, wall_ms) {
        Counted::Yes(user.tenant)
    } else {
        Counted::Over
    }
}

/// Whether connection `conn` belongs to a tenant that `states` has suspended.
fn suspended(conn: &Connection, states: &crate::routes::TenantStates) -> bool {
    conn.auth
        .user
        .as_ref()
        .is_some_and(|u| u.tenant != 0 && states.get(&u.tenant).is_some_and(|s| !s.admits_users()))
}

impl Worker {
    /// Stops counting the connections of a user that is now its tenant's service user (007 FR-032): a connection
    /// counts when it signs in, by this core's view of the tenants, which can name the service user later than
    /// Group 0 does, and a service user's connection counts for nothing.
    fn uncount_service_users(
        &mut self,
        routes: &crate::routes::SharedRoutes,
        states: &crate::routes::TenantStates,
    ) {
        for c in self.conns.values_mut() {
            let Some(tenant) = c.counted else { continue };
            let service =
                c.conn.as_ref().and_then(|conn| conn.auth.user.as_ref()).is_some_and(|u| {
                    states
                        .get(&tenant)
                        .is_some_and(|s| crate::tenant_limits::is_service_user(&s.limits, &u.name))
                });
            if service {
                routes.limiter().disconnect(tenant);
                c.counted = None;
            }
        }
    }

    /// Closes the connections of the tenants suspended since the last look.
    fn close_suspended(&mut self) {
        let Some(h) = self.state.handle() else { return };
        let states = h.routes.tenants();
        if self.tenants_seen.as_ref().is_some_and(|seen| Arc::ptr_eq(seen, &states)) {
            return;
        }
        let gone: Vec<u64> = self
            .conns
            .iter()
            .filter(|(_, c)| c.conn.as_ref().is_some_and(|conn| suspended(conn, &states)))
            .map(|(t, _)| *t)
            .collect();
        for t in gone {
            self.close(t);
        }
        self.uncount_service_users(&h.routes, &states);
        // The cursors and change streams of the tenants suspended since the last look go too;
        // each worker does it, and a second time kills nothing.
        let before = |id: &u32| {
            self.tenants_seen
                .as_ref()
                .and_then(|seen| seen.get(id))
                .is_some_and(|s| !s.admits_users())
        };
        let held: Vec<u32> = states
            .iter()
            .filter(|(id, s)| !s.admits_users() && !before(id))
            .map(|(id, _)| *id)
            .collect();
        if !held.is_empty() {
            let tenant_of = |ns: &str| {
                let db = ns.split_once('.').map_or(ns, |(db, _)| db);
                teseladb_meta::tenants::split_db(db).0
            };
            lock(&self.shared).cursors.kill_namespaces(|ns| held.contains(&tenant_of(ns)));
        }
        self.tenants_seen = Some(states);
    }

    fn run(mut self) {
        let mut inbox = Vec::new();
        let mut events = Vec::new();
        while !self.stop.load(Ordering::Acquire) {
            let next = self.awaits.first_key_value().map(|(&(d, _), _)| d);
            let timeout = next
                .map_or(TENANT_CHECK, |d| d.saturating_duration_since(Instant::now()))
                .min(TENANT_CHECK);
            // A reply or a request that arrives within a short spin is taken without sleeping
            // (H-013): the spin looks at the mailbox and, without waiting, at the sockets; the
            // events it takes are not asked for again.
            let mut failed = false;
            let mut seen = false;
            let spun = teseladb_io::spin::spin_until(teseladb_io::spin::SPIN_BEFORE_SLEEP, || {
                if !self.mailbox.is_empty() {
                    return true;
                }
                match self.poller.poll(Some(Duration::ZERO)) {
                    Ok(ev) => {
                        seen |= !ev.is_empty();
                        events.extend(ev.iter().filter(|e| e.token != WAKE_TOKEN).copied());
                        seen
                    }
                    Err(_) => {
                        failed = true;
                        true
                    }
                }
            });
            if failed {
                break;
            }
            if !seen {
                let timeout = Some(if spun { Duration::ZERO } else { timeout });
                match self.poller.poll(timeout) {
                    Ok(ev) => events.extend(ev.iter().filter(|e| e.token != WAKE_TOKEN).copied()),
                    Err(_) => break,
                }
            }
            let info = self.state.info();
            if info.topology.counter != self.seen {
                self.seen = info.topology.counter;
                self.hasten_hellos();
            }
            for e in events.drain(..) {
                self.on_event(e.token, e.readable || e.hangup, &info);
            }
            self.mailbox.drain_into(&mut inbox);
            for msg in inbox.drain(..) {
                match msg {
                    WorkerMsg::Conn(stream, id) => self.add(stream, id),
                    WorkerMsg::Done(f) => self.done(*f, &info),
                }
            }
            self.fire_awaits(&info);
            self.close_suspended();
        }
        let tokens: Vec<u64> = self.conns.keys().copied().collect();
        for t in tokens {
            self.close(t);
        }
        for p in std::mem::take(&mut self.awaits).into_values() {
            if let Pending::Held { conn, .. } | Pending::Queued { conn, .. } = p {
                self.gone(*conn);
            }
        }
    }

    fn add(&mut self, stream: TcpStream, id: i32) {
        let token = self.next_token;
        self.next_token += 1;
        let transport = Transport::new(&self.tls);
        let ready = stream
            .set_nonblocking(true)
            .and_then(|()| stream.set_nodelay(true))
            .and_then(|()| self.poller.register(&stream, token, Interest::READ));
        match (ready, transport) {
            (Ok(()), Some(transport)) => {
                let mut conn = Connection::with_compressors(id, &self.compressors);
                conn.peer = stream.peer_addr().ok().map(|a| a.to_string());
                conn.auth.required = self.auth;
                conn.auth.mechanisms.clone_from(&self.mechanisms);
                conn.auth.client_addr = conn.peer.clone();
                conn.auth.server_addr = stream.local_addr().ok().map(|a| a.to_string());
                let peer_ip = stream.peer_addr().ok().map(|a| a.ip());
                let c = Client {
                    stream,
                    transport,
                    conn: Some(conn),
                    backlog: Vec::new(),
                    token,
                    handshake_from: Some(Instant::now()),
                    out: Vec::new(),
                    pos: 0,
                    watching_write: false,
                    waiting: false,
                    new_wait: None,
                    diag: self.diag.clone(),
                    member_subject: crate::cluster_auth::member_subject(&self.tls),
                    peer_ip,
                    admitted: false,
                    counted: None,
                };
                self.conns.insert(token, c);
            }
            (Ok(()), None) => {
                let _ = self.poller.deregister(&stream);
                self.open.fetch_sub(1, Ordering::AcqRel);
            }
            (Err(_), _) => {
                self.open.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    /// Closes connection `token`; one with an executor, or held by the failpoint, records its
    /// close when its connection comes back ([`Worker::gone`]).
    fn close(&mut self, token: u64) {
        if let Some(c) = self.conns.remove(&token) {
            if let (Some(tenant), Some(h)) = (c.counted, self.state.handle()) {
                h.routes.limiter().disconnect(tenant);
            }
            if let Some(conn) = c.conn {
                self.gone(conn);
            }
            let _ = self.poller.deregister(&c.stream);
            self.open.fetch_sub(1, Ordering::AcqRel);
        }
    }

    /// Records the close of a client's connection `conn` in the audit log (a logout, 013
    /// FR-016); without a node behind the port there is none.
    fn gone(&mut self, mut conn: Connection) {
        if let Some(backend) = self.inline.as_mut() {
            teseladb_wire::commands::audit::on_close(&self.shared, &mut conn, backend);
        }
    }

    /// An event of connection `token` (one per token and wait): reads if `readable`, runs what
    /// arrived and writes.
    fn on_event(&mut self, token: u64, readable: bool, info: &NodeInfo) {
        let poller = &self.poller;
        let alive = self.conns.get_mut(&token).is_some_and(|c| {
            let mut cmds = Cmds {
                pool: self.pool.as_mut(),
                inline: self.inline.as_mut(),
                shared: &self.shared,
                state: &*self.state,
            };
            (!readable || c.read()) && c.flush_after(info, &mut cmds) && c.watch(poller, token)
        });
        self.settle(token, alive);
    }

    /// A command an executor finished: its connection, if still open, gets the reply and runs
    /// what arrived meanwhile.
    fn done(&mut self, f: Finished, info: &NodeInfo) {
        let Finished { token, conn, req, action } = f;
        let poller = &self.poller;
        let Some(c) = self.conns.get_mut(&token) else {
            // The client left while the command ran: its close is recorded now.
            self.gone(conn);
            return;
        };
        let mut cmds = Cmds {
            pool: self.pool.as_mut(),
            inline: self.inline.as_mut(),
            shared: &self.shared,
            state: &*self.state,
        };
        // A command that ran while its tenant was suspended closes its connection once answered.
        let states = self.state.handle().map(|h| h.routes.tenants());
        let held = states.as_ref().is_some_and(|s| suspended(&conn, s));
        // A connection that just signed in counts for its tenant; past its limit it closes
        // without its answer (007 FR-032).
        if c.counted.is_none()
            && let Some(over) = self.state.handle().map(|h| count_connection(&h.routes, &conn))
        {
            match over {
                Counted::Yes(tenant) => c.counted = Some(tenant),
                Counted::Over => {
                    self.gone(conn);
                    self.close(token);
                    return;
                }
                Counted::No => {}
            }
        }
        let alive = c.finish(conn, req, action)
            && c.flush_after(info, &mut cmds)
            && !held
            && c.watch(poller, token);
        self.settle(token, alive);
    }

    /// Closes connection `token` if it is not `alive`; otherwise schedules the awaitable `hello`
    /// it just received, if any.
    fn settle(&mut self, token: u64, alive: bool) {
        match self.conns.get_mut(&token).filter(|_| alive) {
            Some(c) => {
                if let Some(p) = c.new_wait.take() {
                    self.awaits.insert((p.deadline(), token), p);
                }
            }
            None => self.close(token),
        }
    }

    /// Makes every waiting `hello` due now: the node's state changed (018 FR-015, as a `refnode`
    /// answers them at a change of its `topologyVersion`).
    fn hasten_hellos(&mut self) {
        let now = Instant::now();
        let due: Vec<(Instant, u64)> = self
            .awaits
            .iter()
            .filter(|(_, p)| matches!(p, Pending::Hello(_)))
            .map(|(k, _)| *k)
            .collect();
        for key in due {
            if let Some(Pending::Hello(mut h)) = self.awaits.remove(&key) {
                h.deadline = now;
                self.awaits.insert((now, key.1), Pending::Hello(h));
            }
        }
    }

    /// Answers the awaitable `hello`s whose deadline passed (those of closed connections are
    /// dropped).
    fn fire_awaits(&mut self, info: &NodeInfo) {
        let now = Instant::now();
        while let Some(entry) = self.awaits.first_entry()
            && entry.key().0 <= now
        {
            let ((_, token), p) = entry.remove_entry();
            let poller = &self.poller;
            let Some(c) = self.conns.get_mut(&token) else {
                // Closed while it waited: a held connection's close is recorded now, and one in
                // the limiter's queue leaves it as interrupted.
                match p {
                    Pending::Held { conn, .. } => self.gone(*conn),
                    Pending::Queued { conn, .. } => {
                        lock(&self.shared).ingress.abandon();
                        self.gone(*conn);
                    }
                    Pending::Hello(_) => {}
                }
                continue;
            };
            let mut cmds = Cmds {
                pool: self.pool.as_mut(),
                inline: self.inline.as_mut(),
                shared: &self.shared,
                state: &*self.state,
            };
            let resumed = match p {
                Pending::Hello(p) => {
                    let (next, open) = c.fire(p, info, &self.shared);
                    c.new_wait = next;
                    open
                }
                // The held request runs now, with what the failpoint does next.
                Pending::Held { conn, req, .. } => c.resume(*conn, req, info, &mut cmds),
                // Its turn in the limiter's queue came.
                Pending::Queued { conn, req, .. } => {
                    lock(&self.shared).ingress.dequeue();
                    c.resume(*conn, req, info, &mut cmds)
                }
            };
            let alive = c.flush_after(info, &mut cmds) && resumed && c.watch(poller, token);
            self.settle(token, alive);
        }
    }
}
