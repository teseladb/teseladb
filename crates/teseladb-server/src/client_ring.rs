//! The clients' port on io_uring rings (012 FR-024): with the io_uring executor and a port without
//! TLS, one ring per core accepts the connections with a multishot `accept`, reads them with a
//! multishot `recv` into its provided buffers and writes their replies with `sendmsg`, with no
//! thread per connection.
//!
//! A command can wait (for a majority, a leader, its `maxTimeMS` or an `awaitData` cursor), so the
//! ring thread never runs one: a connection with a request goes to a thread of the port's command
//! pool, which runs what arrived and hands the connection back with its replies. The pool has a
//! thread per connection running a command, not per open connection: an idle connection costs a
//! socket and its state, and its thread goes back to the pool. A connection away on the pool
//! keeps what arrives meanwhile, and stops being read past [`PAUSE_AT`] bytes; one whose replies
//! the client does not read stops being read too (backpressure).
//!
//! TLS connections keep the thread per connection of [`crate::client_listener`].

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use teseladb_io::net_ring::{NetHandler, NetRing};
use teseladb_io::poller::Waker;
use teseladb_sys::uring::Part;
use teseladb_wire::commands::Shared;

use crate::backend::ServerBackend;
use crate::client_listener::{
    Client, ClientKit, Cmds, NodeState, Pending, abandon, command_thread, kill_suspended_cursors,
    run_pending, seed, uncount_service_users, with_command_stack,
};
use crate::diag::{Diag, PortStats};

/// Bytes a connection may have waiting (arrived while away on the pool, or replies not sent)
/// before the ring stops reading it.
pub const PAUSE_AT: usize = 4 << 20;
/// How long a pool thread waits for work before it ends.
const POOL_IDLE: Duration = Duration::from_secs(10);
/// Longest wait of a ring between two looks at the port's stop.
const RING_TICK: Duration = Duration::from_millis(500);
/// How long a stopping ring waits for its connections away on the pool.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// What the rings and the pool share.
pub(crate) struct RingParts {
    pub(crate) state: Arc<dyn NodeState>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) kit: ClientKit,
    pub(crate) diag: Arc<Diag>,
    pub(crate) open: Arc<AtomicUsize>,
    pub(crate) stop: Arc<AtomicBool>,
    pub(crate) max: usize,
    /// The number of the next connection, across the rings.
    pub(crate) next_id: AtomicI32,
}

/// The running rings of a port and its command pool.
pub(crate) struct RingPort {
    pub(crate) rings: Vec<(Waker, JoinHandle<()>)>,
    pub(crate) pool: Arc<Pool>,
}

impl RingPort {
    /// Wakes the rings and the pool to stop, and waits for the rings.
    pub(crate) fn stop(self) {
        self.pool.stop();
        for (waker, t) in self.rings {
            let _ = waker.wake();
            let _ = t.join();
        }
    }
}

/// Starts `rings` rings that share `listeners`.
pub(crate) fn spawn(
    parts: RingParts,
    listeners: &[TcpListener],
    rings: usize,
) -> io::Result<RingPort> {
    let parts = Arc::new(parts);
    let listeners: Vec<Arc<TcpListener>> =
        listeners.iter().map(|l| l.try_clone().map(Arc::new)).collect::<io::Result<_>>()?;
    let pool = Arc::new(Pool::new(parts.clone()));
    let mut started = Vec::with_capacity(rings);
    for i in 0..rings.max(1) {
        let net = NetRing::new(listeners.clone())?;
        let waker = net.waker();
        let mut ring = RingClients::new(parts.clone(), pool.clone(), waker.clone(), i);
        let t = thread::Builder::new().name(format!("client-ring-{i}")).spawn(move || {
            let mut net = net;
            ring.run(&mut net);
        })?;
        started.push((waker, t));
    }
    Ok(RingPort { rings: started, pool })
}

/// What a ring hands the pool: a connection and what to run on it.
struct Job {
    token: u64,
    client: Box<Client>,
    /// The wait that is due, or `None` to serve what arrived.
    due: Option<Pending>,
    to: Arc<Mailbox>,
}

/// A connection back from the pool, and whether it stays open.
struct Done {
    token: u64,
    client: Box<Client>,
    alive: bool,
}

/// Where the pool leaves a ring's connections.
struct Mailbox {
    done: Mutex<Vec<Done>>,
    waker: Waker,
}

impl Mailbox {
    fn post(&self, done: Done) {
        self.done.lock().unwrap_or_else(PoisonError::into_inner).push(done);
        // A ring that already ended reads no more mail.
        let _ = self.waker.wake();
    }

    fn take(&self) -> Vec<Done> {
        std::mem::take(&mut *self.done.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

#[derive(Default)]
struct PoolState {
    jobs: VecDeque<Job>,
    idle: usize,
    threads: usize,
    stopped: bool,
}

/// The threads that run the commands of the rings' connections: one per connection with a
/// command running, made when none is free and ended after [`POOL_IDLE`] without work.
pub(crate) struct Pool {
    parts: Arc<RingParts>,
    state: Mutex<PoolState>,
    ready: Condvar,
    made: AtomicUsize,
}

impl Pool {
    fn new(parts: Arc<RingParts>) -> Pool {
        Pool {
            parts,
            state: Mutex::new(PoolState::default()),
            ready: Condvar::new(),
            made: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PoolState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stop(&self) {
        self.lock().stopped = true;
        self.ready.notify_all();
    }

    /// Runs `job` on a free thread, or on a new one.
    fn submit(self: &Arc<Self>, job: Job) {
        let mut st = self.lock();
        st.jobs.push_back(job);
        if st.idle > 0 {
            drop(st);
            self.ready.notify_one();
            return;
        }
        st.threads += 1;
        drop(st);
        let n = self.made.fetch_add(1, Ordering::Relaxed);
        let pool = self.clone();
        let run = with_command_stack(move || pool.work());
        if command_thread(format!("conn-pool-{n}")).spawn(run).is_err() {
            // Without a thread the job goes back unrun and its connection closes.
            let mut st = self.lock();
            st.threads -= 1;
            if st.threads == 0
                && let Some(job) = st.jobs.pop_back()
            {
                drop(st);
                job.to.post(Done { token: job.token, client: job.client, alive: false });
            }
        }
    }

    /// The next job, or `None` once idle too long or stopped.
    fn next(&self) -> Option<Job> {
        let mut st = self.lock();
        loop {
            if let Some(job) = st.jobs.pop_front() {
                return Some(job);
            }
            if st.stopped {
                st.threads -= 1;
                return None;
            }
            st.idle += 1;
            let (guard, waited) =
                self.ready.wait_timeout(st, POOL_IDLE).unwrap_or_else(PoisonError::into_inner);
            st = guard;
            st.idle -= 1;
            if waited.timed_out() && st.jobs.is_empty() {
                st.threads -= 1;
                return None;
            }
        }
    }

    /// A pool thread: runs jobs with its own backend until idle too long.
    fn work(self: Arc<Self>) {
        // Commands run on the fast cores (H-049 of the native tests).
        let _ = teseladb_sys::favor_current_thread();
        let parts = &self.parts;
        let n = self.made.load(Ordering::Relaxed) as u64;
        let mut backend = parts
            .state
            .handle()
            .map(|h| ServerBackend::new(h, seed(n << 20)).with_diag(parts.diag.clone()));
        while let Some(job) = self.next() {
            let Job { token, mut client, due, to } = job;
            let info = parts.state.info();
            let mut cmds =
                Cmds { inline: backend.as_mut(), shared: &parts.shared, state: &*parts.state };
            let resumed = due.is_none_or(|p| run_pending(&mut client, p, &info, &mut cmds));
            let alive = client.flush_after(&info, &mut cmds) && resumed;
            // With the answer on its way, the intents of a transaction the command ended (008
            // FR-005), unless resolvers of their own do it.
            if !teseladb_wire::commands::hot::of(&parts.shared).has_resolvers()
                && let Some(b) = backend.as_mut()
            {
                teseladb_wire::commands::txn::resolve_queued(b, &parts.shared);
            }
            to.post(Done { token, client, alive });
        }
    }
}

/// Where a ring's connection is.
enum Slot {
    /// On the ring.
    Here(Box<Client>),
    /// On the pool, with what arrived meanwhile; `closed` if its socket ended meanwhile.
    Away { backlog: Vec<u8>, closed: bool },
}

struct Entry {
    slot: Slot,
    /// The ring stopped reading it.
    paused: bool,
}

/// The connections of one ring.
struct RingClients {
    parts: Arc<RingParts>,
    pool: Arc<Pool>,
    mailbox: Arc<Mailbox>,
    /// The ring's waker, which the node's presence holds weakly to answer the waiting `hello`s.
    waker: Arc<Waker>,
    clients: HashMap<u64, Entry>,
    /// Deadlines of the awaitable `hello`s and held requests, with their connection.
    awaits: BTreeMap<(Instant, u64), Pending>,
    /// The `topologyVersion` counter the waiting `hello`s last saw.
    seen: i64,
    /// The backend of the audit of closes, if a node is behind the port.
    backend: Option<ServerBackend>,
    /// The tenants' states the ring last closed connections for (007 FR-034).
    tenants_seen: Option<Arc<crate::routes::TenantStates>>,
}

impl RingClients {
    fn new(parts: Arc<RingParts>, pool: Arc<Pool>, waker: Waker, i: usize) -> RingClients {
        let backend = parts.state.handle().map(|h| {
            ServerBackend::new(h, seed(u64::MAX - 64 - i as u64)).with_diag(parts.diag.clone())
        });
        RingClients {
            seen: parts.state.info().topology.counter,
            mailbox: Arc::new(Mailbox { done: Mutex::new(Vec::new()), waker: waker.clone() }),
            waker: Arc::new(waker),
            parts,
            pool,
            clients: HashMap::new(),
            awaits: BTreeMap::new(),
            backend,
            tenants_seen: None,
        }
    }

    fn run(&mut self, net: &mut NetRing) {
        // A change of the node's state wakes the ring, to answer the waiting `hello`s.
        self.parts.state.watch_topology(&self.waker);
        while !self.parts.stop.load(Ordering::Acquire) {
            let next = self.awaits.first_key_value().map(|(&(d, _), _)| d);
            let wait = next
                .map_or(RING_TICK, |d| d.saturating_duration_since(Instant::now()).min(RING_TICK));
            if net.run_once(self, wait).is_err() {
                break;
            }
        }
        // The port stops: the connections close, and those on the pool when they come back.
        net.stop_accepting();
        let tokens: Vec<u64> = self.clients.keys().copied().collect();
        for t in tokens {
            net.close(t);
        }
        let until = Instant::now() + STOP_GRACE;
        while (net.connections() > 0 || !self.clients.is_empty()) && Instant::now() < until {
            if net.run_once(self, Duration::from_millis(50)).is_err() {
                break;
            }
        }
        for p in std::mem::take(&mut self.awaits).into_values() {
            if let Some(conn) = abandon(p, &self.parts.shared) {
                self.audit_close(conn);
            }
        }
    }

    /// Records the close of a client's connection in the audit log (013 FR-016).
    fn audit_close(&mut self, mut conn: teseladb_wire::connection::Connection) {
        if let Some(backend) = self.backend.as_mut() {
            teseladb_wire::commands::audit::on_close(&self.parts.shared, &mut conn, backend);
        }
    }

    /// A connection that ended: its tenant's count, its audit and the port's count.
    fn finish(&mut self, client: Box<Client>) {
        if let Some(conn) = client.end(&*self.parts.state) {
            self.audit_close(conn);
        }
        self.parts.open.fetch_sub(1, Ordering::AcqRel);
    }

    /// Sends connection `token` to the pool to run what arrived, or wait `due`.
    fn dispatch(&mut self, token: u64, due: Option<Pending>) {
        let Some(e) = self.clients.get_mut(&token) else { return };
        let Slot::Here(_) = e.slot else { return };
        let Slot::Here(client) =
            std::mem::replace(&mut e.slot, Slot::Away { backlog: Vec::new(), closed: false })
        else {
            return;
        };
        self.pool.submit(Job { token, client, due, to: self.mailbox.clone() });
    }

    /// Takes back the connections the pool ran: sends their replies, schedules their waits and
    /// runs what arrived meanwhile.
    fn take_back(&mut self, net: &mut NetRing) {
        for Done { token, mut client, alive } in self.mailbox.take() {
            let backlog = match self.clients.get_mut(&token).map(|e| &mut e.slot) {
                Some(Slot::Away { closed: false, backlog }) => std::mem::take(backlog),
                _ => {
                    // Its socket ended while it was away.
                    self.clients.remove(&token);
                    self.finish(client);
                    continue;
                }
            };
            let out = client.take_out();
            PortStats::add(&self.parts.diag.stats.physical_out, out.len());
            if !out.is_empty() {
                net.send(token, Part::new(out));
            }
            if let Some(p) = client.take_wait() {
                self.awaits.insert((p.deadline(), token), p);
            }
            let alive = alive && (backlog.is_empty() || client.take_in(&backlog));
            let ready = !client.is_waiting() && !backlog.is_empty();
            if let Some(e) = self.clients.get_mut(&token) {
                e.slot = Slot::Here(client);
            }
            if !alive {
                net.close(token);
            } else if ready {
                self.dispatch(token, None);
            }
            self.settle(net, token);
        }
    }

    /// Reads connection `token` again once what waits for it is small.
    fn settle(&mut self, net: &mut NetRing, token: u64) {
        let Some(e) = self.clients.get_mut(&token) else { return };
        let waiting = match &e.slot {
            Slot::Away { backlog, .. } => backlog.len(),
            Slot::Here(c) => c.pending_in(),
        } + net.queued(token);
        if e.paused && waiting < PAUSE_AT / 2 {
            e.paused = false;
            net.resume(token);
        } else if !e.paused && waiting >= PAUSE_AT {
            e.paused = true;
            net.pause(token);
        }
    }

    /// Makes every waiting `hello` due now: the node's state changed (018 FR-015).
    fn hasten_hellos(&mut self) {
        let now = Instant::now();
        let due: Vec<(Instant, u64)> = self
            .awaits
            .iter()
            .filter(|(_, p)| matches!(p, Pending::Hello(_)))
            .map(|(k, _)| *k)
            .collect();
        for key in due {
            if let Some(p) = self.awaits.remove(&key) {
                self.awaits.insert((now, key.1), p);
            }
        }
    }

    /// Sends the waits whose deadline passed to the pool with their connection.
    fn fire_awaits(&mut self) {
        let now = Instant::now();
        let mut later = Vec::new();
        while let Some(entry) = self.awaits.first_entry()
            && entry.key().0 <= now
        {
            let ((at, token), p) = entry.remove_entry();
            match self.clients.get(&token).map(|e| &e.slot) {
                Some(Slot::Here(_)) => self.dispatch(token, Some(p)),
                // Back from the pool soon: it waits for the next round.
                Some(Slot::Away { closed: false, .. }) => later.push(((at, token), p)),
                _ => {
                    if let Some(conn) = abandon(p, &self.parts.shared) {
                        self.audit_close(conn);
                    }
                }
            }
        }
        self.awaits.extend(later);
    }

    /// Closes the connections of the tenants suspended since the last look (007 FR-034).
    fn close_suspended(&mut self, net: &mut NetRing) {
        let Some(routes) = self.parts.state.routes() else { return };
        let states = routes.tenants();
        if self.tenants_seen.as_ref().is_some_and(|seen| Arc::ptr_eq(seen, &states)) {
            return;
        }
        let routes = routes.clone();
        let here = self.clients.iter_mut().filter_map(|(t, e)| match &mut e.slot {
            Slot::Here(c) => Some((*t, c)),
            Slot::Away { .. } => None,
        });
        let mut gone = Vec::new();
        let mut kept = Vec::new();
        for (t, c) in here {
            if c.is_suspended(&states) { gone.push(t) } else { kept.push(&mut **c) }
        }
        uncount_service_users(kept.into_iter(), &routes, &states);
        for t in gone {
            net.close(t);
        }
        kill_suspended_cursors(&self.parts.shared, self.tenants_seen.as_deref(), &states);
        self.tenants_seen = Some(states);
    }
}

impl NetHandler for RingClients {
    fn accepted(
        &mut self,
        _net: &mut NetRing,
        token: u64,
        peer: Option<SocketAddr>,
        local: Option<SocketAddr>,
    ) -> bool {
        let p = &self.parts;
        // A drained node's port takes no connection (007 FR-015), nor a stopping one.
        if p.state.closed() || p.stop.load(Ordering::Acquire) {
            return false;
        }
        // Over the limit the connection is closed at once, as `refnode` does.
        if p.open.load(Ordering::Acquire) >= p.max {
            PortStats::add(&p.diag.stats.rejected, 1);
            return false;
        }
        let id = p.next_id.fetch_update(Ordering::AcqRel, Ordering::Acquire, |id| {
            Some(id.wrapping_add(1).max(1))
        });
        let id = id.unwrap_or(1);
        let Some(client) = p.kit.client(id, None, peer, local) else { return false };
        p.open.fetch_add(1, Ordering::AcqRel);
        PortStats::add(&p.diag.stats.total_created, 1);
        self.clients.insert(token, Entry { slot: Slot::Here(Box::new(client)), paused: false });
        true
    }

    fn received(&mut self, net: &mut NetRing, token: u64, bytes: &[u8]) {
        let Some(e) = self.clients.get_mut(&token) else { return };
        match &mut e.slot {
            Slot::Here(client) => {
                if !client.take_in(bytes) {
                    net.close(token);
                    return;
                }
                if !client.is_waiting() {
                    self.dispatch(token, None);
                }
            }
            Slot::Away { backlog, .. } => backlog.extend_from_slice(bytes),
        }
        self.settle(net, token);
    }

    fn sent(&mut self, net: &mut NetRing, token: u64) {
        self.settle(net, token);
    }

    fn closed(&mut self, _net: &mut NetRing, token: u64) {
        let Some(e) = self.clients.get_mut(&token) else { return };
        match &mut e.slot {
            Slot::Away { closed, .. } => *closed = true,
            Slot::Here(_) => {
                if let Some(Entry { slot: Slot::Here(client), .. }) = self.clients.remove(&token) {
                    self.finish(client);
                }
            }
        }
    }

    fn woken(&mut self, net: &mut NetRing) {
        self.take_back(net);
        let info = self.parts.state.info();
        if info.topology.counter != self.seen {
            self.seen = info.topology.counter;
            self.hasten_hellos();
        }
        self.fire_awaits();
        self.close_suspended(net);
    }
}
