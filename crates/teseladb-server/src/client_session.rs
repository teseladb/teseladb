//! One client connection of the clients' port on the engine (023 FR-018, FR-019): the wire
//! protocol between the bytes the port reads and writes and the node's commands, as a
//! [`Session`] of `teseladb_io::engine::client_port`. It is the same whether the connection came
//! through the kernel's TCP or through AF_XDP with the port's own TCP stack (FR-021).
//!
//! Its requests run in order in an engine thread of the connection's shard, where a command
//! waits for the cores, a majority, a leader or its `maxTimeMS` without holding up the shard; a
//! request that arrives meanwhile waits in the connection, as it waited in the socket. An
//! awaitable `hello` waits until its deadline or a change of the node's state, and with
//! `exhaustAllowed` keeps answering that way; the requests behind it wait for it. TLS is the
//! port's (the engine's TLS streams): the session sees plain bytes and the client's certificates.

use std::net::IpAddr;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use teseladb_io::engine::client_port::{ConnInfo, Service, Session};
use teseladb_io::engine::tls_stream::Sniff;
use teseladb_wire::commands::{Shared, lock};
use teseladb_wire::compressed::Compressor;
use teseladb_wire::connection::{Connection, ReplyPieces, Request, RequestArena};
use teseladb_wire::dispatch::{Action, NodeInfo, dispatch_with, hello_after_wait};
use teseladb_wire::error::{CommandError, INTERNAL_ERROR, SHUTDOWN_IN_PROGRESS};
use teseladb_wire::hello::HelloRequest;
use teseladb_wire::ingress::Admission;
use teseladb_x509::Certificate;

use crate::backend::ServerBackend;
use crate::client_listener::{NodeState, next_connection_id, quiesce_reply, seed};
use crate::diag::{Diag, PortStats};
use crate::routes::{SharedRoutes, TenantStates};
use crate::tls_port::TlsPort;

/// The log line of a command no engine thread could run (no counterpart in the reference
/// server, which closes the connection when it cannot start its thread).
pub(crate) const NOT_RUN_ID: i32 = 0;

/// What a new connection of the port starts with.
#[derive(Clone)]
pub(crate) struct ClientKit {
    pub(crate) compressors: Vec<Compressor>,
    pub(crate) tls: TlsPort,
    /// Clients authenticate before running commands.
    pub(crate) auth: bool,
    /// `authenticationMechanisms`.
    pub(crate) mechanisms: Vec<teseladb_auth::sasl::SaslMechanism>,
    /// Where its connections keep the buffers of their requests (012 FR-032).
    pub(crate) arena: Option<Arc<dyn RequestArena>>,
    /// The port's counters.
    pub(crate) diag: Arc<Diag>,
}

/// What every connection of a port shares: the port's [`Service`].
pub(crate) struct PortCore {
    pub(crate) state: Arc<dyn NodeState>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) kit: ClientKit,
    /// Connections open on the port, on every shard and on AF_XDP.
    pub(crate) open: Arc<AtomicUsize>,
    /// `--maxConns`.
    pub(crate) max: usize,
    /// The number of the port's next connection.
    pub(crate) ids: Arc<AtomicI32>,
}

impl PortCore {
    /// A session for a new connection, counted among the open ones; `None` when the node
    /// drained (007 FR-015) or past `--maxConns`, closed at once as the reference server does
    /// with `net.maxIncomingConnections`.
    pub(crate) fn session(&self, info: &ConnInfo) -> Option<WireSession> {
        if self.state.closed() {
            return None;
        }
        let stats = &self.kit.diag.stats;
        if self.open.load(Ordering::Acquire) >= self.max {
            PortStats::add(&stats.rejected, 1);
            return None;
        }
        self.open.fetch_add(1, Ordering::AcqRel);
        PortStats::add(&stats.total_created, 1);
        if let Some(took) = info.handshake {
            let ms = usize::try_from(took.as_millis()).unwrap_or(usize::MAX);
            let micros = usize::try_from(took.as_micros()).unwrap_or(usize::MAX);
            PortStats::add(&stats.tls_handshakes, 1);
            PortStats::add(&stats.tls_handshake_ms, ms);
            PortStats::add(&stats.tls_handshake_micros, micros);
        }
        let id = next_connection_id(&self.ids);
        let mut conn = Connection::with_compressors(id, &self.kit.compressors);
        conn.arena.clone_from(&self.kit.arena);
        conn.peer = info.peer.map(|a| a.to_string());
        conn.auth.required = self.kit.auth;
        conn.auth.mechanisms.clone_from(&self.kit.mechanisms);
        conn.auth.client_addr = conn.peer.clone();
        conn.auth.server_addr = info.local.map(|a| a.to_string());
        let backend = self.state.handle().map(|h| {
            ServerBackend::new(h, seed(u64::try_from(id).unwrap_or(0)))
                .with_diag(Arc::clone(&self.kit.diag))
        });
        let mut s = WireSession {
            conn: Some(conn),
            backlog: Vec::new(),
            out: Vec::new(),
            framer: Framer::default(),
            wait: None,
            state: Arc::clone(&self.state),
            shared: Arc::clone(&self.shared),
            diag: Arc::clone(&self.kit.diag),
            open: Arc::clone(&self.open),
            backend,
            peer_certificates: info.peer_certificates.clone(),
            member_subject: crate::cluster_auth::member_subject(&self.kit.tls),
            peer_ip: info.peer.map(|a| a.ip()),
            admitted: false,
            counted: None,
            seen: 0,
            tenants_seen: None,
        };
        s.certify();
        Some(s)
    }
}

impl Service for PortCore {
    fn sniff(&self) -> Sniff {
        self.kit.tls.sniff()
    }

    fn open(&self, info: ConnInfo) -> Option<Box<dyn Session>> {
        self.session(&info).map(|s| Box::new(s) as Box<dyn Session>)
    }
}

/// Counts the whole messages in the bytes of a connection, by the length each one starts with,
/// so that a connection runs its requests only when one arrived whole. A length no message can
/// have counts as a message: the connection reads it and closes.
#[derive(Debug, Default)]
struct Framer {
    /// The length bytes of the message being read.
    head: [u8; 4],
    have: usize,
    /// Bytes of the message's body still to come; `None` while its length is read.
    left: Option<usize>,
    /// Whole messages not yet taken.
    complete: usize,
    /// A length no message can have came: the stream is not read any further.
    broken: bool,
}

impl Framer {
    fn feed(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() && !self.broken {
            match self.left {
                None => {
                    let take = (4 - self.have).min(bytes.len());
                    self.head[self.have..self.have + take].copy_from_slice(&bytes[..take]);
                    self.have += take;
                    bytes = &bytes[take..];
                    if self.have < 4 {
                        return;
                    }
                    self.have = 0;
                    let len = i32::from_le_bytes(self.head);
                    match usize::try_from(len).ok().filter(|n| {
                        (teseladb_wire::msg::HEADER_LEN..=teseladb_wire::MAX_MESSAGE_SIZE)
                            .contains(n)
                    }) {
                        Some(n) => self.left = Some(n - 4),
                        None => {
                            self.broken = true;
                            self.complete += 1;
                        }
                    }
                }
                Some(left) => {
                    let take = left.min(bytes.len());
                    bytes = &bytes[take..];
                    if take == left {
                        self.left = None;
                        self.complete += 1;
                    } else {
                        self.left = Some(left - take);
                    }
                }
            }
        }
    }

    /// One whole message was taken.
    fn taken(&mut self) {
        self.complete = self.complete.saturating_sub(1);
    }

    /// The connection found no whole message: none is counted.
    fn none_left(&mut self) {
        if !self.broken {
            self.complete = 0;
        }
    }
}

/// What holds a connection's next requests until its deadline.
pub(crate) enum Pending {
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
    pub(crate) fn deadline(&self) -> Instant {
        match self {
            Pending::Hello(h) => h.deadline,
            Pending::Held { deadline, .. } | Pending::Queued { deadline, .. } => *deadline,
        }
    }
}

/// An awaitable `hello` waiting for its answer.
pub(crate) struct HelloWait {
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

/// The connection of a wait whose client closed: a held one's close is recorded by the caller, and
/// one in the limiter's queue leaves it as interrupted (006 FR-036).
fn abandon(p: Pending, shared: &Shared) -> Option<Connection> {
    match p {
        Pending::Held { conn, .. } => Some(*conn),
        Pending::Queued { conn, .. } => {
            lock(shared).ingress.abandon();
            Some(*conn)
        }
        Pending::Hello(_) => None,
    }
}

/// Microseconds since the first connection's first request: the limiter's clock.
fn limiter_now_us() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    let micros = START.get_or_init(Instant::now).elapsed().as_micros();
    u64::try_from(micros).unwrap_or(u64::MAX)
}

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
fn count_connection(routes: &SharedRoutes, conn: &Connection) -> Counted {
    let Some(user) = conn.auth.user.as_ref().filter(|u| u.tenant != 0) else { return Counted::No };
    let states = routes.tenants();
    let limits = states.get(&user.tenant).map(|s| s.limits.clone()).unwrap_or_default();
    if crate::tenant_limits::is_service_user(&limits, &user.name) {
        return Counted::No;
    }
    if routes.limiter().connect(user.tenant, limits.max_connections, crate::clock::wall_ms()) {
        Counted::Yes(user.tenant)
    } else {
        Counted::Over
    }
}

/// Whether connection `conn` belongs to a tenant that `states` has suspended.
fn suspended(conn: &Connection, states: &TenantStates) -> bool {
    conn.auth
        .user
        .as_ref()
        .is_some_and(|u| u.tenant != 0 && states.get(&u.tenant).is_some_and(|s| !s.admits_users()))
}

/// Kills the cursors and change streams of the tenants `states` suspends and `seen` (the states
/// the caller last looked at) did not; a second time kills nothing.
fn kill_suspended_cursors(shared: &Shared, seen: Option<&TenantStates>, states: &TenantStates) {
    let before = |id: &u32| seen.and_then(|seen| seen.get(id)).is_some_and(|s| !s.admits_users());
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
        lock(shared).cursors.kill_namespaces(|ns| held.contains(&tenant_of(ns)));
    }
}

/// One client connection's wire protocol.
pub(crate) struct WireSession {
    /// The connection; `None` while the failpoint or the limiter holds one of its requests.
    conn: Option<Connection>,
    /// What arrived while the failpoint or the limiter held the connection's request.
    backlog: Vec<u8>,
    /// What it answered and the port has not taken.
    out: Vec<u8>,
    framer: Framer,
    /// What holds its next requests, if anything.
    wait: Option<Pending>,
    state: Arc<dyn NodeState>,
    shared: Arc<Shared>,
    diag: Arc<Diag>,
    open: Arc<AtomicUsize>,
    /// The backend of its commands, if a node is behind the port.
    backend: Option<ServerBackend>,
    /// The client's TLS certificates, leaf first.
    peer_certificates: Vec<Certificate>,
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
    /// The `topologyVersion` counter its waiting `hello` last saw: a change answers it.
    seen: i64,
    /// The tenants' states it last closed or counted for (007 FR-034).
    tenants_seen: Option<Arc<TenantStates>>,
}

impl WireSession {
    /// Records on the connection the X.509 authentication user of the client certificate, if any
    /// and not yet recorded; a member's when its subject has the organization, unit or domain
    /// components of the port's (013 FR-034).
    fn certify(&mut self) {
        let Some(conn) = self.conn.as_mut() else { return };
        if conn.auth.peer_user.is_none()
            && let Some(leaf) = self.peer_certificates.first()
        {
            conn.auth.peer_user = Some(leaf.subject.x509_user_name());
            conn.auth.peer_member = self
                .member_subject
                .as_deref()
                .is_some_and(|node| crate::cluster_auth::is_member(&leaf.subject, node));
        }
    }

    /// Queues `bytes` to go out after what waits.
    fn push_out(&mut self, bytes: Vec<u8>) {
        if self.out.is_empty() {
            self.out = bytes;
        } else {
            self.out.extend_from_slice(&bytes);
        }
    }

    /// Runs the requests that arrived, in order, until one waits; `false` if the connection
    /// must close.
    fn serve(&mut self, info: &NodeInfo) -> bool {
        while self.wait.is_none() {
            let Some(mut conn) = self.conn.take() else { return true };
            let req = match conn.next_request() {
                Ok(Some(r)) => r,
                next => {
                    self.framer.none_left();
                    self.conn = Some(conn);
                    return next.is_ok();
                }
            };
            self.framer.taken();
            if !self.admitted {
                // The first request takes a token of the limiter of new connections, waits for
                // one, or closes the connection without a reply (006 FR-036).
                match lock(&self.shared).ingress.admit(self.peer_ip, limiter_now_us()) {
                    Admission::Go => self.admitted = true,
                    Admission::Reject => {
                        self.conn = Some(conn);
                        return false;
                    }
                    Admission::Wait { micros } => {
                        self.admitted = true;
                        let deadline = Instant::now() + Duration::from_micros(micros);
                        self.wait = Some(Pending::Queued { conn: Box::new(conn), req, deadline });
                        return true;
                    }
                }
            }
            if !self.handle(conn, req, info) {
                return false;
            }
        }
        true
    }

    /// Runs request `req` of `conn` with the connection's backend, or without one when no node
    /// is behind the port, and queues its action. A connection that just signed in counts for
    /// its tenant, and past its limit closes without its answer (007 FR-032); one whose tenant is
    /// suspended closes once answered. `false` if the connection must close.
    fn handle(&mut self, mut conn: Connection, req: Request, info: &NodeInfo) -> bool {
        let stats = &self.diag.stats;
        PortStats::add(&stats.requests, 1);
        PortStats::add(&stats.bytes_in, usize::try_from(req.header.length).unwrap_or(0));
        // A drained node answers every request with `ShutdownInProgress` and closes the
        // connection after the reply (007 FR-015), labelled so that the driver retries a
        // retryable write on another node (D-148).
        if self.state.closed() {
            let e = CommandError::new(SHUTDOWN_IN_PROGRESS, "the node is drained");
            let e = teseladb_wire::error::with_retryable_labels(e, &req.command);
            self.apply(conn, req, Action::Reply(e.to_document()), info);
            return false;
        }
        if self.state.quiescing() && teseladb_wire::hello::is_hello(req.name()) {
            let doc = quiesce_reply(&self.state.info());
            return self.apply(conn, req, Action::Reply(doc), info);
        }
        self.state.note_command();
        stats.active.fetch_add(1, Ordering::Relaxed);
        let fresh;
        let (now_info, commands) = match self.backend.as_mut() {
            Some(b) => {
                // A write moves the cluster time: each reply reads it after its command.
                fresh = self.state.info();
                (&fresh, Some((b, &*self.shared)))
            }
            None => (info, None),
        };
        let action = dispatch_with(&mut conn, &req, now_info, commands);
        self.diag.stats.active.fetch_sub(1, Ordering::Relaxed);
        self.conn = Some(conn);
        self.certify();
        let Some(conn) = self.conn.take() else { return false };
        let (held, over) = match self.state.routes() {
            None => (false, false),
            Some(routes) => {
                // A command that ran while its tenant was suspended closes its connection once
                // answered.
                let held = suspended(&conn, &routes.tenants());
                let mut over = false;
                if self.counted.is_none() {
                    match count_connection(routes, &conn) {
                        Counted::Yes(tenant) => self.counted = Some(tenant),
                        Counted::Over => over = true,
                        Counted::No => {}
                    }
                }
                (held, over)
            }
        };
        if over {
            self.conn = Some(conn);
            return false;
        }
        self.apply(conn, req, action, info) && !held
    }

    /// Queues the reply of `conn`'s request `req`, or the wait it asks for, and keeps `conn`;
    /// `false` if the connection must close.
    fn apply(
        &mut self,
        mut conn: Connection,
        req: Request,
        action: Action,
        info: &NodeInfo,
    ) -> bool {
        let alive = match action {
            Action::Reply(doc) if req.more_to_come() => {
                drop(doc);
                true
            }
            Action::Reply(doc) => {
                let pieces = conn.reply_pieces(&req, &doc, false);
                PortStats::add(&self.diag.stats.bytes_out, pieces.len());
                match pieces {
                    ReplyPieces::Pieces(f) if f.borrowed() > 0 => {
                        for p in f.slices() {
                            self.out.extend_from_slice(p);
                        }
                    }
                    ReplyPieces::Pieces(f) => self.push_out(f.into_vec()),
                    ReplyPieces::Bytes(bytes) => self.push_out(bytes),
                }
                true
            }
            Action::Await { max_await_ms, exhaust, hello } => {
                let every = Duration::from_millis(max_await_ms);
                let response_to = req.header.request_id;
                let deadline = Instant::now() + every;
                // A change of the node's state from now on answers it.
                self.seen = info.topology.counter;
                let wait =
                    HelloWait { req, hello, deadline, every, exhaust, response_to, held: None };
                self.wait = Some(Pending::Hello(wait));
                true
            }
            Action::Delay { ms } => {
                // What arrives meanwhile goes to the backlog, until the held request runs.
                let deadline = Instant::now() + Duration::from_millis(ms);
                self.wait = Some(Pending::Held { conn: Box::new(conn), req, deadline });
                return true;
            }
            Action::Close => false,
        };
        self.conn = Some(conn);
        alive
    }

    /// Answers its awaitable `hello` `p`; with exhaust, the next one of the stream, and whether
    /// the connection stays open. Each reply of a stream after the first is a `hello` the
    /// failpoint acts on, as on a reference router 9.0.4 with `enableTestCommands`: it holds it,
    /// answers its error and ends the stream, or closes the connection.
    fn fire(&mut self, mut p: HelloWait, info: &NodeInfo) -> bool {
        let quiescing = self.state.quiescing();
        // A connection waiting for a `hello` has its connection.
        let Some(conn) = self.conn.as_mut() else { return true };
        let streamed = p.response_to != p.req.header.request_id;
        let fault = match (&p.held, streamed) {
            (None, true) => {
                let app = conn.hello.app_name.clone();
                lock(&self.shared).failpoint.as_mut().and_then(|fp| {
                    fp.check(teseladb_wire::concerns::canonical(p.req.name()), app.as_deref())
                })
            }
            _ => None,
        };
        if let Some(f) = fault {
            if f.close {
                return false;
            }
            match (f.block_ms, f.error) {
                (0, None) => {}
                (0, Some(e)) => p.held = Some(e),
                (ms, error) => {
                    p.deadline = Instant::now() + Duration::from_millis(ms);
                    p.held = error;
                    self.wait = Some(Pending::Hello(p));
                    return true;
                }
            }
        }
        if let Some(e) = p.held.take() {
            let doc = teseladb_wire::hello::with_cluster_time(e, info);
            let (bytes, _) = conn.reply_to(p.response_to, &p.req, &doc, false);
            self.push_out(bytes);
            return true;
        }
        if quiescing {
            // The stream ends with the error: the driver marks the node unknown.
            let doc = quiesce_reply(info);
            let (bytes, _) = conn.reply_to(p.response_to, &p.req, &doc, false);
            self.push_out(bytes);
            return true;
        }
        let doc = hello_after_wait(conn, &p.hello, info);
        let (bytes, id) = conn.reply_to(p.response_to, &p.req, &doc, p.exhaust);
        self.push_out(bytes);
        if p.exhaust {
            p.response_to = id;
            p.deadline += p.every;
            self.seen = info.topology.counter;
            self.wait = Some(Pending::Hello(p));
        }
        true
    }

    /// Runs the request the failpoint or the limiter held, on its connection with what arrived
    /// meanwhile; `false` if the connection must close.
    fn resume(&mut self, mut conn: Connection, req: Request, info: &NodeInfo) -> bool {
        if !self.backlog.is_empty() {
            conn.receive(&std::mem::take(&mut self.backlog));
        }
        self.handle(conn, req, info)
    }

    /// Runs its wait `p`, due now; `false` if the connection must close.
    fn run_pending(&mut self, p: Pending, info: &NodeInfo) -> bool {
        match p {
            Pending::Hello(h) => self.fire(h, info),
            // The held request runs now, with what the failpoint does next.
            Pending::Held { conn, req, .. } => self.resume(*conn, req, info),
            // Its turn in the limiter's queue came.
            Pending::Queued { conn, req, .. } => {
                lock(&self.shared).ingress.dequeue();
                self.resume(*conn, req, info)
            }
        }
    }

    /// Records the close of the connection `conn` in the audit log (a logout, 013 FR-016);
    /// without a node behind the port there is none.
    fn gone(&mut self, mut conn: Connection) {
        if let Some(backend) = self.backend.as_mut() {
            teseladb_wire::commands::audit::on_close(&self.shared, &mut conn, backend);
        }
    }
}

impl Session for WireSession {
    fn receive(&mut self, bytes: &[u8]) {
        PortStats::add(&self.diag.stats.physical_in, bytes.len());
        self.framer.feed(bytes);
        match self.conn.as_mut() {
            Some(conn) => conn.receive(bytes),
            None => self.backlog.extend_from_slice(bytes),
        }
    }

    fn has_work(&self, now: Instant) -> bool {
        match &self.wait {
            Some(p) => p.deadline() <= now,
            None => self.framer.complete > 0,
        }
    }

    fn run(&mut self, now: Instant) -> bool {
        // The calls of JavaScript see the stack of this engine thread.
        teseladb_wire::commands::js::set_stack(teseladb_io::engine::stacks::stack_size());
        let info = self.state.info();
        if let Some(p) = self.wait.take_if(|p| p.deadline() <= now)
            && !self.run_pending(p, &info)
        {
            return false;
        }
        self.serve(&info)
    }

    fn take_out(&mut self, out: &mut Vec<u8>) {
        if self.out.is_empty() {
            return;
        }
        PortStats::add(&self.diag.stats.physical_out, self.out.len());
        if out.is_empty() {
            std::mem::swap(out, &mut self.out);
        } else {
            out.append(&mut self.out);
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.wait.as_ref().map(Pending::deadline)
    }

    fn must_close(&mut self) -> bool {
        // A change of the node's state answers a waiting `hello` now (018 FR-015, as a reference
        // replica set member answers them at a change of its `topologyVersion`).
        if let Some(Pending::Hello(h)) = self.wait.as_mut() {
            let counter = self.state.info().topology.counter;
            if counter != self.seen {
                self.seen = counter;
                h.deadline = Instant::now();
            }
        }
        let Some(routes) = self.state.routes() else { return false };
        let states = routes.tenants();
        if self.tenants_seen.as_ref().is_some_and(|seen| Arc::ptr_eq(seen, &states)) {
            return false;
        }
        // The tenants changed (rare): a suspended tenant's connection closes (007 FR-034), a
        // service user's stops counting (FR-036), and the suspended tenants' cursors die.
        if self.conn.as_ref().is_some_and(|c| suspended(c, &states)) {
            return true;
        }
        if let Some(tenant) = self.counted {
            let service = self.conn.as_ref().and_then(|c| c.auth.user.as_ref()).is_some_and(|u| {
                states
                    .get(&tenant)
                    .is_some_and(|s| crate::tenant_limits::is_service_user(&s.limits, &u.name))
            });
            if service {
                routes.limiter().disconnect(tenant);
                self.counted = None;
            }
        }
        kill_suspended_cursors(&self.shared, self.tenants_seen.as_deref(), &states);
        self.tenants_seen = Some(states);
        false
    }

    fn refuse(&mut self, why: &str) -> bool {
        let attr = format!("{{\"error\":{why:?}}}");
        let msg = "A command could not run: no engine thread took it".to_owned();
        crate::log::defer_at(
            crate::log::Level::Error,
            crate::log::Pending { id: NOT_RUN_ID, msg, attr },
        );
        // A wait that is due runs at the next try.
        if self.wait.is_some() {
            return true;
        }
        let Some(mut conn) = self.conn.take() else { return false };
        let req = match conn.next_request() {
            Ok(Some(r)) => r,
            next => {
                self.framer.none_left();
                self.conn = Some(conn);
                return next.is_ok();
            }
        };
        self.framer.taken();
        let e = CommandError::new(INTERNAL_ERROR, format!("the command could not run: {why}"));
        let info = self.state.info();
        self.apply(conn, req, Action::Reply(e.to_document()), &info)
    }

    fn end(mut self: Box<Self>) {
        if let (Some(tenant), Some(routes)) = (self.counted.take(), self.state.routes()) {
            routes.limiter().disconnect(tenant);
        }
        // A held connection's close is recorded now, and one in the limiter's queue leaves it
        // as interrupted.
        if let Some(p) = self.wait.take()
            && let Some(conn) = abandon(p, &self.shared)
        {
            self.gone(conn);
        }
        if let Some(conn) = self.conn.take() {
            self.gone(conn);
        }
        self.open.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn message(len: i32) -> Vec<u8> {
        let mut m = len.to_le_bytes().to_vec();
        m.resize(usize::try_from(len).unwrap_or(4).max(4), 0);
        m
    }

    #[test]
    fn whole_messages_are_counted_however_their_bytes_arrive() {
        let mut f = Framer::default();
        let mut stream = message(16);
        stream.extend(message(40));
        // One byte at a time, then the rest at once.
        for b in &stream[..10] {
            f.feed(std::slice::from_ref(b));
        }
        assert_eq!(f.complete, 0);
        f.feed(&stream[10..16]);
        assert_eq!(f.complete, 1);
        f.feed(&stream[16..]);
        assert_eq!(f.complete, 2);
        f.taken();
        f.taken();
        f.taken();
        assert_eq!(f.complete, 0);
        // Half a message, which the connection did not find whole.
        f.feed(&message(20)[..8]);
        f.none_left();
        assert_eq!(f.complete, 0);
        f.feed(&[0; 12]);
        assert_eq!(f.complete, 1);
    }

    #[test]
    fn a_length_no_message_has_counts_as_one_and_stops_the_count() {
        for len in [0, 15, -1, i32::MAX] {
            let mut f = Framer::default();
            f.feed(&len.to_le_bytes());
            assert_eq!(f.complete, 1, "{len}");
            f.feed(&message(16));
            f.none_left();
            assert_eq!(f.complete, 1, "the connection reads it and closes");
        }
    }
}
