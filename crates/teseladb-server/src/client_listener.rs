//! The clients' port (006 FR-005) on the engine (023 FR-018, FR-019): every shard listens on the
//! port's addresses through the engine's network, the engine hands each connection to a shard,
//! and the connection is a task of that shard (`teseladb_io::engine::client_port`) with the wire
//! protocol of `teseladb-wire` as its session ([`crate::client_session`]). A connection's
//! commands run in an engine thread of its shard: a command that waits (for a majority, a leader
//! or its `maxTimeMS`) holds up only its own connection, as the reference server's thread per
//! connection (`ServiceExecutorSynchronous`) does, and no request changes shards on its way to
//! the cores but the one `smp` message there and back. What arrives while a command runs waits
//! in the connection, in order.
//!
//! Beyond `--maxConns` open connections a new one is closed at once, as the reference server does
//! with `net.maxIncomingConnections`. An awaitable `hello` waits until its deadline (or until the
//! node changes state), and with `exhaustAllowed` keeps answering that way. The port's periodic
//! work (the sessions' keeper, the resolvers of finished transactions, the ranges' statistics and
//! the reader of the tenants' data) runs as tasks of the shards ([`crate::client_tasks`]).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use teseladb_hlc::to_timestamp;
use teseladb_multiraft::NodeClock;
use teseladb_wire::commands::registry::REFRESH_INTERVAL_MS;
use teseladb_wire::commands::{NodeSessions, REAP_INTERVAL_MS, Timeouts};
use teseladb_wire::compressed::Compressor;
use teseladb_wire::dispatch::{ClusterTime, NodeInfo, Topology};
use teseladb_wire::error::{CommandError, SHUTDOWN_IN_PROGRESS};

use crate::backend::NodeHandle;
use crate::certs::Certificates;
use crate::clustertime_keys::KeySlot;
use crate::diag::ProcessConfig;
use crate::tls_port::TlsPort;

/// The reference server's default `net.maxIncomingConnections`.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1_000_000;
/// The stack a connection's commands run on: address space outside the shard's memory that the
/// kernel commits as a command reaches it (`teseladb_io::engine::stacks`), so what a command
/// takes from the shard does not grow with it. It is far smaller than the `COMMAND_STACK` of
/// server-side JavaScript: a call of JavaScript runs on a stack of its own of that size instead
/// ([`set_deep_stack`], 016 A016-3-18).
pub const ENGINE_COMMAND_STACK: usize = 16 << 20;

/// Runs each call of server-side JavaScript on a native stack of its own of `COMMAND_STACK`
/// bytes, reserved and committed as it is used (016 A016-3-18, 023 FR-018): the 51 000 nested
/// calls of the reference router fit, whatever the stack of the thread that runs the command.
pub fn set_deep_stack() {
    fn runner(call: &mut dyn FnMut(Option<usize>)) {
        teseladb_sys::deep_stack::run(teseladb_wire::commands::js::COMMAND_STACK, call);
    }
    teseladb_wire::commands::js::set_deep_stack(runner);
}

/// Bounds what each call of server-side JavaScript takes by the free memory of the shard that
/// runs it (023 FR-014): the shard's memory is a pool of its own and an allocation it cannot
/// serve ends the node, where a thread with the system's allocator had the whole machine. A
/// thread outside the engine's pools (none set up, or the allocator that counts nothing when
/// the engine's is not built) keeps the interpreter's limit.
pub fn set_js_memory_room() {
    fn room() -> Option<usize> {
        let s = teseladb::engine::core::memory::stats();
        (s.total_memory() > 0 && s.mallocs() > 0).then(|| s.free_memory())
    }
    teseladb_wire::commands::js::set_memory_room(room);
}

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
    /// The interface whose IPv4 clients the port takes with AF_XDP and its own TCP stack
    /// (`--net-xdp`, 012 FR-033), besides the kernel's listener of `addrs`.
    pub xdp: Option<String>,
    /// Clients authenticate before running commands (a node started without `--insecure`, 013).
    pub auth: bool,
    /// What `rotateCertificates` loads again (013 FR-006); `None` on a node without a PKI.
    pub certificates: Option<Arc<Certificates>>,
    /// The audit log (013 FR-016).
    pub audit: Option<crate::audit_log::AuditLog>,
    /// The LDAP server of PLAIN logins (013 FR-025).
    pub ldap: Option<crate::ldap_client::SharedLdap>,
    /// The STS of AWS IAM authentication logins (013 FR-025).
    pub sts: Option<Arc<crate::sts_client::Sts>>,
    /// The identity providers of OIDC authentication logins (013 FR-025).
    pub oidc: Option<Arc<crate::oidc_client::Oidc>>,
    /// The keytab of GSSAPI logins (013 FR-025).
    pub kerberos: Option<Arc<Vec<teseladb_auth::kerberos::ServiceKey>>>,
    /// `authenticationMechanisms`; empty, the reference server's default.
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
        s.hot.redact_client_log_data.store(self.redact_client_log_data, Ordering::Relaxed);
        s.hot
            .javascript_enabled
            .store(self.javascript_enabled, std::sync::atomic::Ordering::Relaxed);
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

    /// The port on `addrs` with the reference server's defaults.
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
            xdp: None,
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

    /// The node's routing table, without a copy of the whole handle: the workers read it on
    /// every request (H-013: copying the handle moved a dozen counters every thread shares).
    fn routes(&self) -> Option<&crate::routes::SharedRoutes> {
        None
    }

    /// The node drained and closed its clients' port (007 FR-015): new connections are dropped
    /// and every request gets `ShutdownInProgress` before its connection closes.
    fn closed(&self) -> bool {
        false
    }

    /// The node is in its quiesce period ([`NodePresence::quiesce`]): every `hello` answers
    /// `ShutdownInProgress`.
    fn quiescing(&self) -> bool {
        false
    }

    /// A client's command other than the handshake starts.
    fn note_command(&self) {}

    /// Calls `wake` whenever the `topologyVersion` counter or the tenants' states change, so
    /// the waiting `hello`s answer at once (018 FR-015) and the connections of a suspended
    /// tenant close; a node without one never changes it. The state keeps `wake` weakly.
    fn watch_topology(&self, _wake: &Arc<dyn Fn() + Send + Sync>) {}
}

/// What core 0 tells the clients' port of the node's state in Group 0 (007 FR-015): the
/// `topologyVersion` counter and whether the port is closed.
#[derive(Debug, Default)]
pub struct NodePresence {
    counter: AtomicI64,
    closed: AtomicBool,
    /// The node is stopping and in its quiesce period ([`NodePresence::quiesce`]).
    quiescing: AtomicBool,
    /// When a client's last command other than the handshake started, in milliseconds of the
    /// wall clock, while the node quiesces.
    last_command_ms: AtomicU64,
    /// The replica set and the member this node is, in the replica set presentation (018
    /// FR-007).
    replica: std::sync::Mutex<Option<Arc<ReplicaMember>>>,
    /// The ports to wake when the counter changes (each wakes its connections on every shard);
    /// a port that stopped drops its waker and leaves the list at the next change.
    watchers: std::sync::Mutex<Vec<std::sync::Weak<dyn Fn() + Send + Sync>>>,
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
            self.wake();
        }
    }

    /// Wakes every connection of every port: the node's state or the tenants' states changed
    /// (007 FR-034), and an idle connection waits until something wakes it. Each port's waker
    /// posts the wake-up to its shards and returns.
    pub fn wake(&self) {
        let mut watchers = self.watchers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        watchers.retain(|w| match w.upgrade() {
            Some(wake) => {
                wake();
                true
            }
            None => false,
        });
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

    /// Calls `wake` at every change of the counter or of the tenants' states. The ports that
    /// stopped leave the list here.
    pub fn watch(&self, wake: &Arc<dyn Fn() + Send + Sync>) {
        let weak = Arc::downgrade(wake);
        let mut watchers = self.watchers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        watchers.retain(|w| w.strong_count() > 0);
        watchers.push(weak);
    }

    /// How many ports it wakes.
    pub fn watchers(&self) -> usize {
        let watchers = self.watchers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        watchers.iter().filter(|w| w.strong_count() > 0).count()
    }

    /// The replica set presentation this node answers in (`None`: as a reference router).
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

    /// The `topologyVersion` counter: Group 0's, and past every value of it once the node
    /// quiesces, so that the waiting `hello`s answer.
    pub fn counter(&self) -> i64 {
        let raw = self.counter.load(Ordering::Acquire);
        if self.quiescing() { raw.saturating_add(QUIESCE_COUNTER_STEP) } else { raw }
    }

    /// The node stops (a signal or `shutdown`): it enters its quiesce period, as a reference
    /// router does. Its `topologyVersion` changes, every `hello` answers `ShutdownInProgress`
    /// (the waiting ones at once), and the other commands still run, so that the drivers stop
    /// sending to it before its connections close (007 FR-027).
    pub fn quiesce(&self) {
        self.last_command_ms.store(wall_ms() as u64, Ordering::Release);
        if !self.quiescing.swap(true, Ordering::AcqRel) {
            self.wake();
        }
    }

    /// Whether the node is in its quiesce period.
    pub fn quiescing(&self) -> bool {
        self.quiescing.load(Ordering::Acquire)
    }

    /// A client's command other than the handshake starts while the node quiesces.
    pub fn note_command(&self) {
        self.last_command_ms.store(wall_ms() as u64, Ordering::Release);
    }

    /// How long no client has started a command other than the handshake, while the node
    /// quiesces.
    pub fn quiet_for(&self) -> Duration {
        let last = self.last_command_ms.load(Ordering::Acquire);
        Duration::from_millis((wall_ms() as u64).saturating_sub(last))
    }

    /// Whether the port is closed.
    pub fn closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

/// How much the `topologyVersion` counter grows when the node enters its quiesce period: past
/// any counter Group 0 gives.
const QUIESCE_COUNTER_STEP: i64 = 1 << 48;

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

    fn routes(&self) -> Option<&crate::routes::SharedRoutes> {
        self.handle.as_ref().map(|h| &h.routes)
    }

    fn watch_topology(&self, wake: &Arc<dyn Fn() + Send + Sync>) {
        self.presence.watch(wake);
    }

    fn closed(&self) -> bool {
        self.presence.closed()
    }

    fn quiescing(&self) -> bool {
        self.presence.quiescing()
    }

    fn note_command(&self) {
        if self.presence.quiescing() {
            self.presence.note_command();
        }
    }
}

/// How the clients' port runs its periodic tasks: in production [`Hooks::default`]; a test gives
/// short periods to reach what a running node meets only by chance.
#[derive(Clone, Copy, Debug)]
pub struct Hooks {
    /// Period of the TTL monitor.
    pub ttl_every: Duration,
    /// Period of the statistics of the ranges compactions rewrote (010 FR-029).
    pub stats_every: Duration,
}

impl Default for Hooks {
    fn default() -> Hooks {
        Hooks {
            ttl_every: Duration::from_secs(teseladb_wire::commands::ttl::TTL_MONITOR_SECS),
            stats_every: Duration::from_secs(
                teseladb_wire::commands::range_stats::STATS_EVERY_SECS,
            ),
        }
    }
}

/// The number of the port's next connection: one counter for every way the port takes them
/// (the kernel's TCP and AF_XDP), so that no two connections share a number.
pub(crate) fn next_connection_id(ids: &std::sync::atomic::AtomicI32) -> i32 {
    let id =
        ids.fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| Some(id.wrapping_add(1).max(1)));
    id.unwrap_or(1)
}

/// The seed of the random numbers of backend `k`: the clock and the number.
pub(crate) fn seed(k: u64) -> u64 {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64);
    nanos ^ (k << 40) ^ 0x9e37_79b9_7f4a_7c15
}

/// The state the commands of a command's thread keep, saved while its engine thread waits (023
/// FR-018): the hooks every process that runs commands on the engine sets once.
fn set_switch_hooks() {
    fn save() -> Box<dyn std::any::Any> {
        Box::new(teseladb_wire::commands::context::take())
    }
    fn restore(state: Box<dyn std::any::Any>) {
        if let Ok(c) = state.downcast::<teseladb_wire::commands::context::CommandContext>() {
            teseladb_wire::commands::context::put(*c);
        }
    }
    let _ = teseladb_io::engine::wait::set_switch_hooks(teseladb_io::engine::wait::SwitchHooks {
        save,
        restore,
    });
}

/// The running clients' port.
pub struct ClientListener {
    addrs: Vec<SocketAddr>,
    /// The port on the engine's shards.
    port: teseladb_io::engine::client_port::PortRef,
    /// Wakes the port's connections; the node's presence keeps a weak reference to it.
    wake: Arc<dyn Fn() + Send + Sync>,
    /// Connections open on the port, on every shard and on AF_XDP.
    open: Arc<std::sync::atomic::AtomicUsize>,
    /// The port on AF_XDP (012 FR-033), or why it is not there.
    xdp: crate::client_xdp::XdpState,
    /// The tasks of the port on the shards (023 FR-019), the builder of the search indexes among
    /// them.
    tasks: Vec<crate::client_tasks::Task>,
}

impl std::fmt::Debug for ClientListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientListener").field("addrs", &self.addrs).finish()
    }
}

/// How many tasks resolve the intents of the finished transactions: a resolution waits for a
/// write of the log, so a few run side by side.
const RESOLVERS: usize = 4;
/// How long an idle resolver pauses before it looks at the queue again; a transaction that ends
/// wakes it at once.
const RESOLVER_IDLE: Duration = Duration::from_millis(100);

impl ClientListener {
    /// Listens on `cfg.addrs` on the shards of the process's engine.
    ///
    /// # Errors
    ///
    /// When no engine runs or an address cannot be listened on.
    pub fn spawn(cfg: &ClientConfig, state: Arc<dyn NodeState>) -> io::Result<ClientListener> {
        ClientListener::spawn_with(cfg, state, Hooks::default())
    }

    /// [`ClientListener::spawn`] with `hooks`, for tests.
    ///
    /// # Errors
    ///
    /// As [`ClientListener::spawn`].
    #[doc(hidden)]
    pub fn spawn_with(
        cfg: &ClientConfig,
        state: Arc<dyn NodeState>,
        hooks: Hooks,
    ) -> io::Result<ClientListener> {
        use crate::client_session::{ClientKit, PortCore};
        use crate::diag::Diag;

        set_switch_hooks();
        set_deep_stack();
        set_js_memory_room();
        teseladb_io::engine::stacks::set_stack_size(ENGINE_COMMAND_STACK);
        let open = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let port_no = cfg.addrs.first().map_or(0, SocketAddr::port);
        let diag = Arc::new(
            Diag::new(cfg.process.clone(), port_no, open.clone(), cfg.max_connections)
                .with_certificates(cfg.certificates.clone())
                .with_tls(cfg.tls.clone())
                .with_ldap(cfg.ldap.clone())
                .with_sts(cfg.sts.clone())
                .with_oidc(cfg.oidc.clone())
                .with_kerberos(cfg.kerberos.clone()),
        );
        let shared: Arc<teseladb_wire::commands::Shared> = match state.handle() {
            Some(h) => h.sessions,
            None => Arc::new(std::sync::Mutex::new(cfg.sessions())),
        };
        let core = Arc::new(PortCore {
            state: state.clone(),
            shared: shared.clone(),
            kit: ClientKit {
                compressors: cfg.compressors.clone(),
                tls: cfg.tls.clone(),
                auth: cfg.auth,
                mechanisms: cfg.mechanisms.clone(),
                // Requests are read into the engine's memory, on huge pages where the system has
                // them (023 FR-014).
                arena: None,
                diag: diag.clone(),
            },
            open: open.clone(),
            max: cfg.max_connections,
            ids: Arc::new(std::sync::atomic::AtomicI32::new(1)),
        });
        let service: Arc<dyn teseladb_io::engine::client_port::Service> = core.clone();
        let port = teseladb_io::engine::client_port::start_port(&cfg.addrs, service)?;
        let addrs = port.addrs().to_vec();
        let xdp = crate::client_xdp::start(cfg, &addrs, core);
        let wake = Arc::new({
            let port_wake = port.waker();
            let xdp_wake = xdp.waker();
            move || {
                port_wake();
                if let Some(w) = xdp_wake.as_ref() {
                    w();
                }
            }
        }) as Arc<dyn Fn() + Send + Sync>;
        state.watch_topology(&wake);
        let shards = port.shards();
        let mut l = ClientListener { addrs, port, wake, open, xdp, tasks: Vec::new() };
        if let Some(h) = state.handle()
            && let Err(e) = l.start_tasks(&h, cfg, &shared, &diag, hooks, shards)
        {
            l.stop();
            return Err(e);
        }
        Ok(l)
    }

    /// Starts the port's tasks on the shards, spread over them, and the search builder.
    fn start_tasks(
        &mut self,
        h: &NodeHandle,
        cfg: &ClientConfig,
        shared: &Arc<teseladb_wire::commands::Shared>,
        diag: &Arc<crate::diag::Diag>,
        hooks: Hooks,
        shards: u32,
    ) -> io::Result<()> {
        use crate::backend::ServerBackend;
        use crate::client_tasks::spawn;

        let shard = |k: usize| u32::try_from(k).unwrap_or(0) % shards.max(1);
        let gone = || io::Error::other("the engine stopped before the port's tasks started");
        // The search builder, in the background's group of its shard (023 FR-025).
        let backend = ServerBackend::new(h.clone(), seed(u64::MAX - 1)).with_diag(diag.clone());
        let builder = crate::search_builder::spawn(backend, shared.clone(), shard(3 + RESOLVERS))
            .ok_or_else(gone)?;
        self.tasks.push(builder);
        // The sessions' keeper.
        let keeper = Keeper {
            handle: h.clone(),
            diag: diag.clone(),
            // What the directory holds is read now, before the keeper runs on its shard.
            capture: cfg.process.dbpath.as_deref().map(crate::diagnostic_capture::Capture::new),
            shared: shared.clone(),
            refresh_every: cfg.refresh_every,
            ttl_every: hooks.ttl_every,
        };
        self.tasks.push(spawn(shard(0), Box::new(move |stop| keeper.run(stop))).ok_or_else(gone)?);
        // The reader of the tenants' data (007 FR-031).
        let hu = h.clone();
        let usage = Box::new(move |stop: &crate::client_tasks::TaskStop| {
            let backend = ServerBackend::new(hu, seed(u64::MAX - 2));
            crate::tenant_limits::run_reader(backend, stop);
        });
        self.tasks.push(spawn(shard(1), usage).ok_or_else(gone)?);
        // The statistics of the ranges compactions rewrote (010 FR-029).
        let hs = h.clone();
        let every = hooks.stats_every;
        let statistics = Box::new(move |stop: &crate::client_tasks::TaskStop| {
            let mut backend = ServerBackend::new(hs, seed(u64::MAX - 3 - RESOLVERS as u64));
            while !stop.stopped() {
                stop.pause(every);
                if stop.stopped() {
                    break;
                }
                // A range whose statistics could not be read waits for the next pass.
                let _ = teseladb_wire::commands::range_stats::pass(&mut backend);
                teseladb_wire::commands::Backend::end_operation(&mut backend);
            }
        });
        self.tasks.push(spawn(shard(2), statistics).ok_or_else(gone)?);
        // The resolvers of the finished transactions (008 FR-005), apart from the connections:
        // run after the answer on the connection, a resolution held that client's next command
        // for a write of the log (H-050 of the native tests).
        let mut resolvers = Vec::with_capacity(RESOLVERS);
        for i in 0..RESOLVERS {
            let (hr, sh) = (h.clone(), shared.clone());
            let body = Box::new(move |stop: &crate::client_tasks::TaskStop| {
                let mut backend = ServerBackend::new(hr, seed(u64::MAX - 3 - i as u64));
                while !stop.stopped() {
                    while teseladb_wire::commands::txn::resolve_one(&mut backend, &sh) {
                        teseladb_wire::commands::Backend::end_operation(&mut backend);
                    }
                    stop.pause(RESOLVER_IDLE);
                }
            });
            let task = spawn(shard(3 + i), body).ok_or_else(gone)?;
            resolvers.push(task.handle());
            self.tasks.push(task);
        }
        let wake = move || resolvers.iter().for_each(|r| r.wake());
        teseladb_wire::commands::hot::of(shared).set_resolvers(Box::new(wake));
        Ok(())
    }

    /// The port on AF_XDP (012 FR-033): `None` without `--net-xdp`, its description, or why the
    /// interface could not take it (the kernel's TCP then serves the clients alone).
    pub fn xdp(&self) -> Option<Result<String, String>> {
        self.xdp.status()
    }

    /// The addresses it listens on (with the ports the system chose for port 0).
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// Connections open now.
    pub fn open_connections(&self) -> usize {
        self.open.load(Ordering::Acquire)
    }

    /// The shards it runs on.
    pub fn shards(&self) -> u32 {
        self.port.shards()
    }

    /// Connections of the kernel's TCP open on each shard, in shard order (the engine hands
    /// each to a shard, 023 FR-018).
    pub fn open_by_shard(&self) -> Vec<usize> {
        self.port.open_by_shard()
    }

    /// Wakes every connection: the node's state changed.
    pub fn wake(&self) {
        (self.wake)();
    }

    /// Stops accepting on every shard, closes every connection once what it answered went out
    /// (a connection running a command closes it when the command ends), and stops the port's
    /// tasks, waiting for each.
    pub fn stop(self) {
        self.port.stop();
        self.xdp.stop();
        for t in self.tasks {
            let _ = t.stop();
        }
    }
}

/// Reaps the node's expired cursors and sessions, writes the sessions it used to
/// `config.system.sessions`, runs the TTL monitor and the reapers of abandoned transactions, each
/// on its own period, at the node's cluster time.
struct Keeper {
    handle: NodeHandle,
    diag: Arc<crate::diag::Diag>,
    shared: Arc<teseladb_wire::commands::Shared>,
    refresh_every: Duration,
    /// Period of the TTL monitor (009 FR-010): `ttlMonitorSleepSecs`.
    ttl_every: Duration,
    /// The periodic diagnostic capture of a node with a data directory (010 FR-024).
    capture: Option<crate::diagnostic_capture::Capture>,
}

impl Keeper {
    /// Writes the node's diagnostic sample (010 FR-024). A sample that cannot be taken or
    /// written is logged and the next one comes a period later.
    fn sample(
        backend: &mut crate::backend::ServerBackend,
        shared: &teseladb_wire::commands::Shared,
        capture: &mut crate::diagnostic_capture::Capture,
        disk: &mut Option<Box<dyn teseladb_raftlog::Disk>>,
    ) {
        use teseladb_wire::commands::Backend;

        let at = i64::try_from(backend.wall_ms()).unwrap_or(i64::MAX);
        let taken = teseladb_wire::commands::diagnostic_data::capture(backend, shared);
        teseladb_wire::commands::Backend::end_operation(backend);
        let written = match taken {
            Ok(sample) => Keeper::disk(capture, disk)
                .and_then(|d| capture.write(d, at, sample))
                .map_err(|e| e.to_string()),
            Err(e) => Err(e.errmsg),
        };
        if let Err(e) = written {
            let attr = format!("{{\"error\":{e:?}}}");
            let msg = "Diagnostic capture failed".to_owned();
            let id = crate::diagnostic_capture::FAILED_ID;
            crate::log::defer(crate::log::Pending { id, msg, attr });
        }
    }

    /// The disk the capture writes to: this shard's executor of its directory (023 FR-010).
    fn disk<'a>(
        capture: &crate::diagnostic_capture::Capture,
        disk: &'a mut Option<Box<dyn teseladb_raftlog::Disk>>,
    ) -> io::Result<&'a mut dyn teseladb_raftlog::Disk> {
        if disk.is_none() {
            let place = teseladb_io::engine::disk::DiskPlace::Files(capture.dir().to_path_buf());
            *disk = Some(Box::new(place.on_shard()?));
        }
        match disk.as_deref_mut() {
            Some(d) => Ok(d),
            None => Err(io::Error::other("no disk for the diagnostic capture")),
        }
    }

    /// The reaping period: `clientCursorMonitorFrequencySecs` as `setParameter` left it, at
    /// least a millisecond.
    fn reap_period(shared: &teseladb_wire::commands::Shared) -> Duration {
        let ms = teseladb_wire::commands::lock(shared).params.monitor_ms;
        Duration::from_millis(u64::try_from(ms).unwrap_or(0).max(1))
    }

    fn run(self, stop: &crate::client_tasks::TaskStop) {
        use teseladb_wire::commands::Backend;

        let mut backend =
            crate::backend::ServerBackend::new(self.handle, seed(u64::MAX)).with_diag(self.diag);
        let mut capture = self.capture;
        // The shard's disk executor of the capture's directory, opened on the first sample.
        let mut capture_disk = None;
        let mut reapers = crate::backend::Reapers::default();
        let shared = &*self.shared;
        let start = std::time::Instant::now();
        let (mut next_reap, mut next_refresh) =
            (start + Keeper::reap_period(shared), start + self.refresh_every);
        let mut next_ttl = start + self.ttl_every;
        let mut next_txn = start + crate::backend::SCAN_EVERY;
        let mut next_sample = start + crate::diagnostic_capture::PERIOD;
        // Sessions used from this cluster time on go in the next refresh.
        let mut since = 0;
        // The reapers look at a flag of their own to stop.
        let stopped = AtomicBool::new(false);
        while !stop.stopped() {
            let now = std::time::Instant::now();
            if now >= next_reap {
                teseladb_wire::commands::reap(shared, backend.now_ms());
                next_reap = now + Keeper::reap_period(shared);
            }
            if now >= next_refresh {
                let at = backend.now_ms();
                // A refresh that fails (no leader for the range) goes again at the next one.
                if teseladb_wire::commands::registry::refresh(&mut backend, shared, since, at)
                    .is_ok()
                {
                    since = at;
                }
                backend.end_operation();
                next_refresh = now + self.refresh_every;
            }
            if now >= next_ttl {
                let at = i64::try_from(backend.now_ms()).unwrap_or(i64::MAX);
                // A pass that fails (no leader for a range) goes again at the next one.
                let _ = teseladb_wire::commands::ttl::pass(&mut backend, shared, at);
                // Its reads of the indexes no longer hold back their changes (009 FR-006).
                backend.end_operation();
                next_ttl = now + self.ttl_every;
            }
            if now >= next_txn {
                stopped.store(stop.stopped(), Ordering::Release);
                backend.reap_transactions(&mut reapers, &stopped);
                // The heartbeats of the client transactions between their statements.
                teseladb_wire::commands::txn::tick_idle(&mut backend, shared);
                backend.end_operation();
                next_txn = now + crate::backend::SCAN_EVERY;
            }
            if now >= next_sample {
                if let Some(c) = capture.as_mut() {
                    Keeper::sample(&mut backend, shared, c, &mut capture_disk);
                }
                next_sample = now + crate::diagnostic_capture::PERIOD;
            }
            let next = next_reap.min(next_refresh).min(next_ttl).min(next_txn).min(next_sample);
            stop.pause(next.saturating_duration_since(std::time::Instant::now()));
        }
    }
}

/// The answer of a `hello` while the node quiesces, as a reference router's: `ShutdownInProgress`
/// with the node's `topologyVersion`.
pub fn quiesce_reply(info: &NodeInfo) -> teseladb_bson::Document {
    let e = CommandError::new(SHUTDOWN_IN_PROGRESS, QUIESCE_MESSAGE);
    let version = teseladb_bson::Document::new()
        .with("processId", teseladb_bson::Value::ObjectId(info.topology.process_id))
        .with("counter", teseladb_bson::Value::Int64(info.topology.counter));
    let doc = teseladb_bson::Document::new().with("topologyVersion", version);
    let mut doc = doc;
    for (k, v) in e.to_document().iter() {
        doc.push(k, v.clone());
    }
    teseladb_wire::hello::with_cluster_time(doc, info)
}

/// The message of [`quiesce_reply`], the reference router's.
pub const QUIESCE_MESSAGE: &str = "The server is in quiesce mode and will shut down";
