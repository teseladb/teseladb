//! The clients' port on AF_XDP on the engine (023 FR-021): the socket of queue `q` is waited on
//! by the reactor of shard `q mod n` (a pollable descriptor of the shard instead of a thread that
//! waits in `epoll`), and a task of that shard runs the queue's rounds ([`XdpPort::round`]). Its
//! connections are served by the same sessions as those of the kernel's TCP
//! ([`super::client_port::Session`]): their work runs in an engine thread of the shard, and their
//! answers go back through the queue's TCP stack. A frame of a connection another queue has goes
//! to that queue's shard as a message.
//!
//! A connection whose answers wait to go out past [`PAUSE_AT`] bytes is paused (its stack's
//! window closes) until they are below [`RESUME_AT`].
//!
//! TLS (013 FR-007, `--tlsMode`) is taken as on the kernel's TCP ([`super::tls_stream::accept`]):
//! the sans-I/O state machine of `teseladb-tls` sits between the queue's stack and the session,
//! the first byte chooses with `allowTLS` and `preferTLS`, and the session opens once the
//! handshake is done, within [`HANDSHAKE_LIMIT`].

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::net::SocketAddr;
use std::os::fd::IntoRawFd;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::internal::pollable_fd::PollableFd;
use teseladb::engine::core::posix::FileDesc;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::timer::Timer;

use teseladb_tls::{Connection, ServerConfig};

use super::client_port::{ConnInfo, HANDSHAKE_LIMIT, PAUSE_AT, RESUME_AT, Service, Session};
use super::post::Wake;
use super::tls_stream::{Sniff, TLS_HANDSHAKE, server_conn};
use super::{answer, stacks, wait};
use crate::conn_port::{ConnPort, NetHandler};
use crate::xdp_port::XdpPort;

/// The longest a queue waits without a round, so that its stack's timers and the frames other
/// queues forward are never late by more.
const ROUND_AT_LEAST: Duration = Duration::from_millis(50);

/// The TLS of a connection of a queue.
enum Wrap {
    /// In clear.
    Plain,
    /// The first byte chooses between TLS and clear (`allowTLS`, `preferTLS`).
    Sniffing(Arc<ServerConfig>),
    /// TLS.
    Tls(Box<Connection>),
}

impl Wrap {
    /// Whether the session may open: in clear, or with the TLS handshake done.
    fn settled(&self) -> bool {
        match self {
            Wrap::Plain => true,
            Wrap::Sniffing(_) => false,
            Wrap::Tls(c) => !c.is_handshaking(),
        }
    }
}

/// A session of a connection of a queue, and where it is.
struct XConn {
    /// `None` while its work runs in an engine thread, or before it opened.
    session: Option<Box<dyn Session>>,
    /// What opens the session once the connection's TLS is settled; `None` once it opened.
    opening: Option<ConnInfo>,
    tls: Wrap,
    /// When the queue's stack accepted it.
    since: Instant,
    /// What arrived while its work ran.
    arrived: Vec<u8>,
    /// The stack closed the connection.
    closed: bool,
    /// Its reading is paused for the answers waiting to go out.
    paused: bool,
}

/// Sessions whose work ended, with their token and whether their connection stays open.
type Finished = Rc<RefCell<Vec<(u64, Box<dyn Session>, bool)>>>;

/// The sessions of one queue's connections.
struct Handler {
    service: Arc<dyn Service>,
    conns: HashMap<u64, XConn>,
    /// Sessions whose work ended, with whether their connection stays open.
    done: Finished,
    /// Connections to look at in the next round: something arrived or their work ended.
    dirty: Vec<u64>,
    /// The waits of the sessions, by deadline.
    timers: BTreeSet<(Instant, u64)>,
    /// Every session looks at whether it must close (the node's state changed).
    check_all: Rc<Cell<bool>>,
    wake: Rc<Wake>,
    scratch: Vec<u8>,
}

impl Handler {
    /// The earliest deadline of a session's wait.
    fn next_deadline(&self) -> Option<Instant> {
        self.timers.first().map(|(at, _)| *at)
    }

    /// Runs the work of connection `token`'s session in an engine thread.
    fn start(&mut self, token: u64, now: Instant) {
        let Some(c) = self.conns.get_mut(&token) else { return };
        let Some(mut s) = c.session.take() else { return };
        let (done, wake) = (Rc::clone(&self.done), Rc::clone(&self.wake));
        let job = stacks::run(Box::new(move || {
            let alive = s.run(now);
            done.borrow_mut().push((token, s, alive));
            wake.wake();
        }));
        drop(job);
    }

    /// Hands what the session of `token` answered to the stack; pauses the connection when its
    /// answers wait past [`PAUSE_AT`].
    fn hand_out(&mut self, net: &mut dyn ConnPort, token: u64) {
        let Some(c) = self.conns.get_mut(&token) else { return };
        let Some(s) = c.session.as_mut() else { return };
        s.take_out(&mut self.scratch);
        if !self.scratch.is_empty() {
            match &mut c.tls {
                Wrap::Tls(conn) => {
                    let sealed = conn.write(&self.scratch).is_ok();
                    let out = conn.take_output();
                    if !out.is_empty() {
                        let _ = net.send(token, &out);
                    }
                    if !sealed {
                        net.close(token);
                    }
                }
                Wrap::Plain | Wrap::Sniffing(_) => {
                    let _ = net.send(token, &self.scratch);
                }
            }
            self.scratch.clear();
        }
        if !c.paused && net.queued(token) > PAUSE_AT {
            c.paused = true;
            net.pause(token);
        }
    }

    /// Closes connection `token`, with a TLS `close_notify` first.
    fn close(&mut self, net: &mut dyn ConnPort, token: u64) {
        if let Some(XConn { tls: Wrap::Tls(conn), .. }) = self.conns.get_mut(&token)
            && conn.close().is_ok()
        {
            let out = conn.take_output();
            if !out.is_empty() {
                let _ = net.send(token, &out);
            }
        }
        net.close(token);
    }

    /// Opens the session of `token` once its TLS is settled; `false` when the service refused
    /// it (the connection is closed).
    fn open(&mut self, net: &mut dyn ConnPort, token: u64) -> bool {
        let Some(c) = self.conns.get_mut(&token) else { return false };
        if !c.tls.settled() {
            return true;
        }
        let Some(mut info) = c.opening.take() else { return true };
        if let Wrap::Tls(conn) = &c.tls {
            info.tls = true;
            info.peer_certificates = conn.peer_certificates().to_vec();
            info.handshake = Some(c.since.elapsed());
        }
        match self.service.open(info) {
            Some(s) => {
                c.session = Some(s);
                if !c.arrived.is_empty() {
                    self.dirty.push(token);
                }
                true
            }
            None => {
                self.conns.remove(&token);
                net.close(token);
                false
            }
        }
    }

    /// Ends the session of `token` (in an engine thread: its end may wait for the cores) and
    /// forgets the connection.
    fn end(&mut self, token: u64) {
        if let Some(c) = self.conns.remove(&token)
            && let Some(s) = c.session
        {
            drop(stacks::run(Box::new(move || s.end())));
        }
    }

    /// Looks at connection `token`: runs its work if it has some, closes it if it must, and
    /// schedules its wait.
    fn look(&mut self, net: &mut dyn ConnPort, token: u64, now: Instant) {
        let closed = match self.conns.get(&token) {
            // A connection whose TLS is not settled has its handshake's time only.
            Some(c) if c.opening.is_some() => {
                let limit = c.since + HANDSHAKE_LIMIT;
                if c.closed {
                    self.end(token);
                } else if now >= limit {
                    self.conns.remove(&token);
                    net.close(token);
                } else {
                    self.timers.insert((limit, token));
                }
                return;
            }
            // A session away in an engine thread is looked at when its work comes back.
            Some(c) if c.session.is_some() => c.closed,
            _ => return,
        };
        if closed {
            self.end(token);
            return;
        }
        let (must_close, work, deadline) = {
            let Some(c) = self.conns.get_mut(&token) else { return };
            let Some(s) = c.session.as_mut() else { return };
            if !c.arrived.is_empty() {
                s.receive(&std::mem::take(&mut c.arrived));
            }
            let must_close = s.must_close();
            (must_close, !must_close && s.has_work(now), s.deadline())
        };
        if must_close {
            self.hand_out(net, token);
            self.close(net, token);
            return;
        }
        if work {
            self.start(token, now);
            return;
        }
        if let Some(at) = deadline {
            self.timers.insert((at, token));
        }
    }
}

impl NetHandler for Handler {
    fn accepted(
        &mut self,
        _net: &mut dyn ConnPort,
        token: u64,
        peer: Option<SocketAddr>,
        local: Option<SocketAddr>,
    ) -> bool {
        let info =
            ConnInfo { peer, local, tls: false, peer_certificates: Vec::new(), handshake: None };
        let tls = match self.service.sniff() {
            Sniff::Plain => Wrap::Plain,
            Sniff::Tls(config) => match server_conn(config) {
                Ok(conn) => Wrap::Tls(Box::new(conn)),
                Err(_) => return false,
            },
            Sniff::Either(config) => Wrap::Sniffing(config),
        };
        let since = Instant::now();
        let (session, opening) = if tls.settled() {
            let Some(session) = self.service.open(info) else { return false };
            (Some(session), None)
        } else {
            self.timers.insert((since + HANDSHAKE_LIMIT, token));
            (None, Some(info))
        };
        let c = XConn {
            session,
            opening,
            tls,
            since,
            arrived: Vec::new(),
            closed: false,
            paused: false,
        };
        self.conns.insert(token, c);
        true
    }

    fn received(&mut self, net: &mut dyn ConnPort, token: u64, bytes: &[u8]) {
        let Some(c) = self.conns.get_mut(&token) else { return };
        if let Wrap::Sniffing(config) = &c.tls {
            c.tls = if bytes.first() == Some(&TLS_HANDSHAKE) {
                match server_conn(Arc::clone(config)) {
                    Ok(conn) => Wrap::Tls(Box::new(conn)),
                    Err(_) => return net.close(token),
                }
            } else {
                Wrap::Plain
            };
        }
        let plain = match &mut c.tls {
            Wrap::Plain | Wrap::Sniffing(_) => Cow::Borrowed(bytes),
            Wrap::Tls(conn) => {
                let fed = conn.feed(bytes);
                let out = conn.take_output();
                if !out.is_empty() {
                    let _ = net.send(token, &out);
                }
                if fed.is_err() {
                    return net.close(token);
                }
                Cow::Owned(conn.read())
            }
        };
        if !plain.is_empty() {
            match c.session.as_mut() {
                Some(s) => s.receive(&plain),
                None => c.arrived.extend_from_slice(&plain),
            }
        }
        if c.opening.is_some() && !self.open(net, token) {
            return;
        }
        self.dirty.push(token);
    }

    fn sent(&mut self, net: &mut dyn ConnPort, token: u64) {
        if let Some(c) = self.conns.get_mut(&token)
            && c.paused
            && net.queued(token) < RESUME_AT
        {
            c.paused = false;
            net.resume(token);
        }
    }

    fn closed(&mut self, _net: &mut dyn ConnPort, token: u64) {
        if let Some(c) = self.conns.get_mut(&token) {
            c.closed = true;
            // A session away in an engine thread ends when its work comes back.
            if c.session.is_some() || c.opening.is_some() {
                self.end(token);
            }
        }
    }

    fn woken(&mut self, net: &mut dyn ConnPort) {
        let now = Instant::now();
        let finished = std::mem::take(&mut *self.done.borrow_mut());
        for (token, session, alive) in finished {
            let Some(c) = self.conns.get_mut(&token) else {
                drop(stacks::run(Box::new(move || session.end())));
                continue;
            };
            c.session = Some(session);
            self.hand_out(net, token);
            if !alive {
                self.close(net, token);
                continue;
            }
            self.dirty.push(token);
        }
        while let Some(&(at, token)) = self.timers.first()
            && at <= now
        {
            self.timers.remove(&(at, token));
            self.dirty.push(token);
        }
        if self.check_all.replace(false) {
            self.dirty.extend(self.conns.keys().copied());
        }
        let mut dirty = std::mem::take(&mut self.dirty);
        dirty.sort_unstable();
        dirty.dedup();
        for token in dirty {
            self.timers.retain(|(_, t)| *t != token);
            self.look(net, token, now);
        }
    }
}

/// One queue on its shard.
struct Queue {
    port: RefCell<XdpPort>,
    handler: RefCell<Handler>,
    /// Frames other queues got for this queue's connections.
    forwarded: RefCell<Vec<Vec<u8>>>,
    wake: Rc<Wake>,
    /// Arms the wait on the socket again, once a round took what arrived.
    arm: Wake,
    stopping: Cell<bool>,
}

thread_local! {
    /// The queues of this shard, by run and queue.
    static QUEUES: RefCell<HashMap<(u64, u32), Rc<Queue>>> = RefCell::new(HashMap::new());
}

/// Numbers the runs of the process.
static NEXT_RUN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// The shard of queue `q` among `shards`.
fn shard_of(q: u32, shards: u32) -> u32 {
    q % shards.max(1)
}

/// Hands `frame` to queue `owner` of run `run`, on its shard.
fn forward(run: u64, owner: u32, shards: u32, frame: Vec<u8>) {
    let _ = wait::post(
        shard_of(owner, shards),
        Box::new(move || {
            if let Some(q) = QUEUES.with(|m| m.borrow().get(&(run, owner)).cloned()) {
                q.forwarded.borrow_mut().push(frame);
                q.wake.wake();
            }
        }),
    );
}

/// Installs `port` on this shard and starts its tasks.
fn install(run: u64, shards: u32, port: XdpPort, service: Arc<dyn Service>) -> io::Result<()> {
    let fd = port.socket()?;
    let queue = port.queue();
    let wake = Rc::new(Wake::default());
    let handler = Handler {
        service,
        conns: HashMap::new(),
        done: Rc::new(RefCell::new(Vec::new())),
        dirty: Vec::new(),
        timers: BTreeSet::new(),
        check_all: Rc::new(Cell::new(false)),
        wake: Rc::clone(&wake),
        scratch: Vec::new(),
    };
    let q = Rc::new(Queue {
        port: RefCell::new(port),
        handler: RefCell::new(handler),
        forwarded: RefCell::new(Vec::new()),
        wake,
        arm: Wake::default(),
        stopping: Cell::new(false),
    });
    QUEUES.with(|m| m.borrow_mut().insert((run, queue), Rc::clone(&q)));
    let pfd = PollableFd::from_file_desc(FileDesc::from_fd(fd.into_raw_fd()));
    q.arm.wake();
    drop(make_coroutine(watch(Rc::clone(&q), pfd)));
    drop(make_coroutine(rounds(run, shards, q)));
    Ok(())
}

/// Waits on the queue's socket each time a round asks, and wakes the queue when frames came.
async fn watch(q: Rc<Queue>, mut fd: PollableFd) -> Result<(), ExceptionPtr> {
    loop {
        q.arm.wait().await;
        if q.stopping.get() {
            break;
        }
        if fd.readable().await.is_err() {
            break;
        }
        q.wake.wake();
    }
    fd.close();
    Ok(())
}

/// The rounds of a queue until its run stops.
async fn rounds(run: u64, shards: u32, q: Rc<Queue>) -> Result<(), ExceptionPtr> {
    let weak = Rc::downgrade(&q);
    let mut timer: Pin<Box<Timer>> = Box::pin(Timer::with_callback(move || {
        if let Some(q) = weak.upgrade() {
            q.wake.wake();
        }
    }));
    while !q.stopping.get() {
        let forwarded = std::mem::take(&mut *q.forwarded.borrow_mut());
        let ok = {
            let mut port = q.port.borrow_mut();
            let mut handler = q.handler.borrow_mut();
            let mut fwd = |owner: u32, frame: Vec<u8>| forward(run, owner, shards, frame);
            port.round(&mut *handler, forwarded, &mut fwd).is_ok()
        };
        if !ok {
            break;
        }
        q.arm.wake();
        let deadline = q.handler.borrow().next_deadline();
        let mut wait = q.port.borrow().wait_at_most(ROUND_AT_LEAST);
        if let Some(at) = deadline {
            wait = wait.min(at.saturating_duration_since(Instant::now()));
        }
        timer.as_mut().cancel();
        timer.as_mut().arm_in(wait);
        q.wake.wait().await;
    }
    timer.as_mut().cancel();
    // The port stops: its connections' sessions end and the socket's wait ends.
    q.stopping.set(true);
    q.arm.wake();
    let tokens: Vec<u64> = q.handler.borrow().conns.keys().copied().collect();
    let mut handler = q.handler.borrow_mut();
    for t in tokens {
        handler.end(t);
    }
    drop(handler);
    QUEUES.with(|m| m.borrow_mut().remove(&(run, q.port.borrow().queue())));
    Ok(())
}

/// The queues of a port on AF_XDP running on the engine's shards.
#[derive(Clone, Debug)]
pub struct XdpRun {
    id: u64,
    shards: u32,
    queues: Vec<u32>,
}

/// Starts each of `ports` on the shard of its queue, with `service` for its connections.
///
/// # Errors
///
/// When no engine runs, or a queue's socket cannot be waited on (the queues started are
/// stopped).
pub fn start_xdp(ports: Vec<XdpPort>, service: Arc<dyn Service>) -> io::Result<XdpRun> {
    let id = NEXT_RUN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let shards = run_on(0, Box::new(Smp::count))?;
    let mut run = XdpRun { id, shards, queues: Vec::with_capacity(ports.len()) };
    for port in ports {
        let q = port.queue();
        let svc = Arc::clone(&service);
        match run_on(shard_of(q, shards), Box::new(move || install(id, shards, port, svc))) {
            Ok(Ok(())) => run.queues.push(q),
            Ok(Err(e)) | Err(e) => {
                run.stop();
                return Err(e);
            }
        }
    }
    Ok(run)
}

/// Runs `f` on `shard` and waits for its result.
fn run_on<R: Send + 'static>(shard: u32, f: Box<dyn FnOnce() -> R + Send>) -> io::Result<R> {
    let (tx, rx) = answer::channel();
    if !wait::post(
        shard,
        Box::new(move || {
            let _ = tx.send(f());
        }),
    ) {
        return Err(io::Error::other("no engine runs in this process"));
    }
    rx.recv().map_err(|_| io::Error::other("the engine stopped"))
}

impl XdpRun {
    /// The queues it runs.
    pub fn queues(&self) -> &[u32] {
        &self.queues
    }

    /// Every session of every queue looks at whether it must close (the node's state changed).
    pub fn wake(&self) {
        let id = self.id;
        for &q in &self.queues {
            let _ = wait::post(
                shard_of(q, self.shards),
                Box::new(move || {
                    if let Some(queue) = QUEUES.with(|m| m.borrow().get(&(id, q)).cloned()) {
                        queue.handler.borrow().check_all.set(true);
                        queue.wake.wake();
                    }
                }),
            );
        }
    }

    /// Stops its queues: each stops accepting, its sessions end and its socket is no longer
    /// waited on. Waits until each shard took the stop.
    pub fn stop(&self) {
        let id = self.id;
        for &q in &self.queues {
            let _ = run_on(
                shard_of(q, self.shards),
                Box::new(move || {
                    if let Some(queue) = QUEUES.with(|m| m.borrow().get(&(id, q)).cloned()) {
                        queue.port.borrow_mut().stop_accepting();
                        queue.stopping.set(true);
                        queue.wake.wake();
                        queue.arm.wake();
                    }
                }),
            );
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn each_queue_has_the_shard_of_its_number() {
        assert_eq!(shard_of(0, 4), 0);
        assert_eq!(shard_of(5, 4), 1);
        assert_eq!(shard_of(3, 0), 0, "no shards: the first");
    }
}
