//! A clients' port on the engine's network (023 FR-018, FR-019): every shard listens on the
//! port's addresses through the engine's POSIX stack, where shard 0 owns the listening socket
//! (no `SO_REUSEPORT`) and the engine hands each accepted connection to a shard; the connection
//! is a task of that shard from then on.
//!
//! The port knows bytes and time; what the bytes mean is the [`Session`] the port's [`Service`]
//! makes for each connection (the wire protocol, in the server). A connection has three tasks:
//! its reader, which reads the socket into the connection's inbox; its writer, which sends what
//! the session answered; and its driver, which hands what arrived to the session and runs the
//! session's work in an engine thread of [`super::stacks`] (a command waits for the cores without
//! holding up the shard). TLS comes first: the first bytes choose it as `--tlsMode` says
//! ([`tls_stream::accept`] with the [`Sniff`] the service gives).
//!
//! A connection whose answers wait to go out past [`PAUSE_AT`] bytes is not read until they are
//! below [`RESUME_AT`]: a client that does not read its answers stops being read.
//!
//! Every engine wait is in a non-generic function of this crate.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::reactor::listen_with;
use teseladb::engine::core::timer::Timer;
use teseladb::engine::net::api::{
    ConnectedSocket, ConnectedSocketInputStreamConfig, ListenOptions, ServerSocket,
};
use teseladb_x509::Certificate;

use super::connect::socket_address;
use super::peers::std_addr;
use super::post::Wake;
use super::stacks;
use super::tls_stream::{self, Sniff, TlsSession};

/// Answers waiting to go out past which a connection is not read (023 FR-018).
pub const PAUSE_AT: usize = 4 << 20;
/// Answers waiting to go out below which a paused connection is read again.
pub const RESUME_AT: usize = 2 << 20;
/// Bytes the output stream of a connection buffers before it writes.
const OUTPUT_BUFFER: usize = 64 << 10;
/// How long a connection has to send its first bytes and end its TLS handshake.
pub const HANDSHAKE_LIMIT: Duration = Duration::from_secs(10);

/// What the port knows of a connection when it opens it.
#[derive(Clone, Debug)]
pub struct ConnInfo {
    /// The client's address.
    pub peer: Option<SocketAddr>,
    /// The address it connected to.
    pub local: Option<SocketAddr>,
    /// Whether it speaks TLS.
    pub tls: bool,
    /// The certificates the client sent in its TLS handshake, leaf first.
    pub peer_certificates: Vec<Certificate>,
    /// How long its TLS handshake took, from the accept.
    pub handshake: Option<Duration>,
}

/// What serves one connection: the protocol between the bytes and the node. It lives on the
/// connection's shard; [`Session::run`] runs in an engine thread and may wait there.
pub trait Session {
    /// Bytes that arrived, in order.
    fn receive(&mut self, bytes: &[u8]);
    /// Whether it has work to run now: a whole request that nothing holds back, or a wait due
    /// at `now`.
    fn has_work(&self, now: Instant) -> bool;
    /// Runs its work (in an engine thread); `false` closes the connection once what it
    /// answered is sent.
    fn run(&mut self, now: Instant) -> bool;
    /// Moves what it answered to `out`.
    fn take_out(&mut self, out: &mut Vec<u8>);
    /// When its next wait is due, if it has one.
    fn deadline(&self) -> Option<Instant>;
    /// Whether it must close now, without running anything more (a drained node, a suspended
    /// tenant); looked at after every wake-up.
    fn must_close(&mut self) -> bool;
    /// The connection ended; what was not run is dropped.
    fn end(self: Box<Self>);
}

/// What makes the sessions of a port; one for the whole port, shared by its shards.
pub trait Service: Send + Sync {
    /// How the port takes the first bytes of a connection (`--tlsMode`, with the certificates
    /// in use now).
    fn sniff(&self) -> Sniff;
    /// A session for a new connection; `None` closes it (a drained node, past `--maxConns`).
    fn open(&self, info: ConnInfo) -> Option<Box<dyn Session>>;
}

/// What the tasks of one connection share.
struct Conn {
    sock: Rc<RefCell<ConnectedSocket>>,
    /// What the reader read and the driver has not taken.
    inbox: RefCell<Vec<u8>>,
    /// The reader met the end of the stream or an error.
    eof: Cell<bool>,
    /// Wakes the driver: bytes arrived, the peer closed, a write failed, the port stops, a
    /// wait is due, or the node's state changed.
    wake: Wake,
    /// What the writer sends next.
    out: RefCell<Vec<u8>>,
    /// Bytes handed to the writer and not yet written.
    unsent: Cell<usize>,
    out_wake: Wake,
    /// Wakes a reader paused by [`PAUSE_AT`].
    resume: Wake,
    /// The writer ends once it sent what it holds.
    closing: Cell<bool>,
    write_failed: Cell<bool>,
    /// The port stops: the driver closes the connection.
    stopping: Cell<bool>,
}

impl Conn {
    fn shutdown(&self) {
        if let Ok(mut s) = self.sock.try_borrow_mut() {
            s.shutdown_input();
            s.shutdown_output();
        }
    }
}

/// A port's listeners and connections on one shard.
#[derive(Default)]
struct PortHere {
    listeners: Vec<Rc<RefCell<ServerSocket>>>,
    next: u64,
    conns: HashMap<u64, Rc<Conn>>,
}

thread_local! {
    /// The ports of this shard, by number.
    static PORTS: RefCell<HashMap<u64, PortHere>> = RefCell::new(HashMap::new());
}

fn with_port<R>(port: u64, f: impl FnOnce(&mut PortHere) -> R) -> R {
    PORTS.with(|p| f(p.borrow_mut().entry(port).or_default()))
}

/// Listens for port `port` at `at` on this shard and accepts its connections here until
/// [`stop_here`]; the address listened on. The first shard to listen at an address binds it;
/// the others take their part of its connections, so they listen at the address the first one
/// got (with the port the system chose for port 0).
///
/// # Errors
///
/// When the address cannot be listened on.
pub fn listen_here(port: u64, at: SocketAddr, service: Arc<dyn Service>) -> io::Result<SocketAddr> {
    let opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    let ss = listen_with(socket_address(at), opts).map_err(|e| io::Error::other(e.to_string()))?;
    let bound = ss.local_address().ok().and_then(|a| std_addr(&a)).unwrap_or(at);
    let ss = Rc::new(RefCell::new(ss));
    with_port(port, |p| p.listeners.push(Rc::clone(&ss)));
    drop(make_coroutine(accept_loop(port, ss, service)));
    Ok(bound)
}

async fn accept_loop(
    port: u64,
    ss: Rc<RefCell<ServerSocket>>,
    service: Arc<dyn Service>,
) -> Result<(), ExceptionPtr> {
    loop {
        let f = ss.borrow_mut().accept();
        match f.await {
            Ok(a) => {
                let remote = std_addr(&a.remote_address);
                drop(make_coroutine(serve(port, a.connection, remote, Arc::clone(&service))));
            }
            // An aborted listener ends the loop; a failed accept loses only that connection.
            Err(_) if !listening(port, &ss) => return Ok(()),
            Err(_) => {}
        }
    }
}

fn listening(port: u64, ss: &Rc<RefCell<ServerSocket>>) -> bool {
    PORTS.with(|p| {
        p.borrow().get(&port).is_some_and(|h| h.listeners.iter().any(|l| Rc::ptr_eq(l, ss)))
    })
}

/// Stops port `port` on this shard: its listeners stop accepting and its connections close
/// (what they answered goes out first; a command that runs ends before its connection closes).
pub fn stop_here(port: u64) {
    let Some(here) = PORTS.with(|p| p.borrow_mut().remove(&port)) else { return };
    for l in here.listeners {
        l.borrow_mut().abort_accept();
    }
    for c in here.conns.into_values() {
        c.stopping.set(true);
        c.wake.wake();
    }
}

/// Wakes every connection of port `port` on this shard: the node's state changed, so each
/// session looks at its waits and whether it must close.
pub fn wake_here(port: u64) {
    let conns: Vec<Rc<Conn>> = PORTS
        .with(|p| p.borrow().get(&port).map(|h| h.conns.values().cloned().collect()))
        .unwrap_or_default();
    for c in conns {
        c.wake.wake();
    }
}

/// Connections of port `port` open on this shard.
pub fn open_here(port: u64) -> usize {
    PORTS.with(|p| p.borrow().get(&port).map_or(0, |h| h.conns.len()))
}

/// Numbers the ports of the process.
static NEXT_PORT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// Runs `f` on `shard` and waits for its result, from any thread (an engine thread parks).
fn on_shard<R: Send + 'static>(shard: u32, f: Box<dyn FnOnce() -> R + Send>) -> io::Result<R> {
    let (tx, rx) = super::answer::channel();
    let posted = super::wait::post(
        shard,
        Box::new(move || {
            let _ = tx.send(f());
        }),
    );
    if !posted {
        return Err(io::Error::other("no engine runs in this process"));
    }
    rx.recv().map_err(|_| io::Error::other("the engine stopped"))
}

/// A running port, usable from any thread: what [`start_port`] started on every shard.
#[derive(Debug)]
pub struct PortRef {
    id: u64,
    shards: u32,
    addrs: Vec<SocketAddr>,
}

/// Starts a port at `addrs` on every shard of the engine, with `service` for its connections.
/// Called from a thread that is not a shard, or from an engine thread.
///
/// # Errors
///
/// When no engine runs, or an address cannot be listened on (the port is then stopped where
/// it started).
pub fn start_port(addrs: &[SocketAddr], service: Arc<dyn Service>) -> io::Result<PortRef> {
    let id = NEXT_PORT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let shards = on_shard(0, Box::new(teseladb::engine::core::smp::Smp::count))?;
    let mut port = PortRef { id, shards, addrs: Vec::with_capacity(addrs.len()) };
    for at in addrs {
        // Shard 0 binds the address; the others take their part of its connections.
        let svc = Arc::clone(&service);
        let at = *at;
        let bound = match on_shard(0, Box::new(move || listen_here(id, at, svc))) {
            Ok(Ok(b)) => b,
            Ok(Err(e)) | Err(e) => {
                port.stop();
                return Err(e);
            }
        };
        for shard in 1..shards {
            let svc = Arc::clone(&service);
            if let Err(e) =
                on_shard(shard, Box::new(move || listen_here(id, bound, svc))).and_then(|r| r)
            {
                port.stop();
                return Err(e);
            }
        }
        port.addrs.push(bound);
    }
    Ok(port)
}

impl PortRef {
    /// The addresses it listens on, with the ports the system chose for port 0.
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// Its number in the process.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The shards it runs on.
    pub fn shards(&self) -> u32 {
        self.shards
    }

    /// Wakes every connection of the port on every shard (the node's state changed); does not
    /// wait.
    pub fn wake(&self) {
        wake_port(self.id, self.shards);
    }

    /// A function that wakes every connection of the port, for whoever publishes the node's
    /// state.
    pub fn waker(&self) -> Arc<dyn Fn() + Send + Sync> {
        let (id, shards) = (self.id, self.shards);
        Arc::new(move || wake_port(id, shards))
    }

    /// Connections open on each shard, in shard order.
    pub fn open_by_shard(&self) -> Vec<usize> {
        let id = self.id;
        (0..self.shards)
            .map(|s| on_shard(s, Box::new(move || open_here(id))).unwrap_or(0))
            .collect()
    }

    /// Stops the port on every shard and waits until each shard stopped accepting: its
    /// connections close once what they answered went out.
    pub fn stop(&self) {
        let id = self.id;
        for shard in 0..self.shards {
            let _ = on_shard(shard, Box::new(move || stop_here(id)));
        }
    }
}

fn wake_port(id: u64, shards: u32) {
    for shard in 0..shards {
        let _ = super::wait::post(shard, Box::new(move || wake_here(id)));
    }
}

/// A connection the engine handed to this shard: TLS as the service says, then its tasks.
async fn serve(
    port: u64,
    mut sock: ConnectedSocket,
    remote: Option<SocketAddr>,
    service: Arc<dyn Service>,
) -> Result<(), ExceptionPtr> {
    let _ = sock.set_nodelay(true);
    let local = std_addr(&sock.local_address());
    let from = Instant::now();
    let sniff = service.sniff();
    let plain = matches!(sniff, Sniff::Plain);
    let accepted = match tls_stream::accept(sock, sniff, (!plain).then_some(HANDSHAKE_LIMIT)).await
    {
        Ok(a) => a,
        // A client that sent nothing in time or failed its handshake is closed.
        Err(_) => return Ok(()),
    };
    let tls: Option<TlsSession> = accepted.tls;
    let info = ConnInfo {
        peer: remote,
        local,
        tls: tls.is_some(),
        peer_certificates: tls.as_ref().map(TlsSession::peer_certificates).unwrap_or_default(),
        handshake: tls.as_ref().map(|_| from.elapsed()),
    };
    let mut sock = accepted.socket;
    let Some(session) = service.open(info) else {
        sock.shutdown_input();
        sock.shutdown_output();
        return Ok(());
    };
    let input = sock.input(ConnectedSocketInputStreamConfig::default());
    let output = sock.output(OUTPUT_BUFFER);
    let conn = Rc::new(Conn {
        sock: Rc::new(RefCell::new(sock)),
        inbox: RefCell::new(Vec::new()),
        eof: Cell::new(false),
        wake: Wake::default(),
        out: RefCell::new(Vec::new()),
        unsent: Cell::new(0),
        out_wake: Wake::default(),
        resume: Wake::default(),
        closing: Cell::new(false),
        write_failed: Cell::new(false),
        stopping: Cell::new(false),
    });
    let id = with_port(port, |p| {
        p.next += 1;
        p.conns.insert(p.next, Rc::clone(&conn));
        p.next
    });
    drop(make_coroutine(reader(input, Rc::clone(&conn))));
    drop(make_coroutine(writer(output, Rc::clone(&conn))));
    drive(&conn, session).await;
    PORTS.with(|p| {
        if let Some(h) = p.borrow_mut().get_mut(&port) {
            h.conns.remove(&id);
        }
    });
    // The writer sends what is left, then closes.
    conn.closing.set(true);
    conn.out_wake.wake();
    Ok(())
}

async fn reader(mut input: InputStream<u8>, conn: Rc<Conn>) -> Result<(), ExceptionPtr> {
    loop {
        while conn.unsent.get() > PAUSE_AT && !conn.closing.get() && !conn.stopping.get() {
            conn.resume.wait().await;
        }
        if conn.closing.get() {
            break;
        }
        match input.read().await {
            Ok(b) if !b.empty() => {
                conn.inbox.borrow_mut().extend_from_slice(b.as_slice());
                conn.wake.wake();
            }
            _ => break,
        }
    }
    conn.eof.set(true);
    conn.wake.wake();
    Ok(())
}

async fn writer(mut out: OutputStream<u8>, conn: Rc<Conn>) -> Result<(), ExceptionPtr> {
    let mut batch = Vec::new();
    loop {
        if conn.out.borrow().is_empty() {
            if conn.closing.get() || conn.write_failed.get() {
                break;
            }
            conn.out_wake.wait().await;
            continue;
        }
        std::mem::swap(&mut *conn.out.borrow_mut(), &mut batch);
        let n = batch.len();
        let sent = async {
            out.write(&batch).await?;
            out.flush().await
        }
        .await;
        batch.clear();
        conn.unsent.set(conn.unsent.get().saturating_sub(n));
        if conn.unsent.get() < RESUME_AT {
            conn.resume.wake();
        }
        if sent.is_err() {
            conn.write_failed.set(true);
            conn.wake.wake();
            break;
        }
    }
    let _ = out.close().await;
    conn.shutdown();
    // A reader paused on a full output sees the end.
    conn.resume.wake();
    Ok(())
}

/// Arms `timer` to wake `conn` at `at`, or disarms it.
fn arm(timer: &mut Pin<Box<Timer>>, at: Option<Instant>) {
    timer.as_mut().cancel();
    if let Some(at) = at {
        timer.as_mut().arm_in(at.saturating_duration_since(Instant::now()));
    }
}

/// Hands what `session` answered to the writer.
fn hand_out(conn: &Conn, session: &mut dyn Session, scratch: &mut Vec<u8>) {
    session.take_out(scratch);
    if scratch.is_empty() {
        return;
    }
    conn.unsent.set(conn.unsent.get() + scratch.len());
    let mut out = conn.out.borrow_mut();
    if out.is_empty() {
        std::mem::swap(&mut *out, scratch);
    } else {
        out.append(scratch);
    }
    conn.out_wake.wake();
}

/// Ends `session` in an engine thread: its end may wait for the cores (the audit of a logout).
async fn finish(session: Box<dyn Session>) {
    let _ = stacks::run(Box::new(move || session.end())).await;
}

/// The driver of a connection: what arrives goes to the session, its work runs in an engine
/// thread, its answers go to the writer, until it or the peer closes or the port stops.
async fn drive(conn: &Rc<Conn>, session: Box<dyn Session>) {
    let weak = Rc::downgrade(conn);
    let mut timer = Box::pin(Timer::with_callback(move || {
        if let Some(c) = weak.upgrade() {
            c.wake.wake();
        }
    }));
    let slot: Rc<RefCell<Option<Box<dyn Session>>>> = Rc::new(RefCell::new(Some(session)));
    let mut scratch = Vec::new();
    loop {
        let arrived = std::mem::take(&mut *conn.inbox.borrow_mut());
        let mut s = match slot.borrow_mut().take() {
            Some(s) => s,
            None => return,
        };
        if !arrived.is_empty() {
            s.receive(&arrived);
        }
        if conn.stopping.get() || conn.write_failed.get() || s.must_close() {
            hand_out(conn, s.as_mut(), &mut scratch);
            finish(s).await;
            return;
        }
        let now = Instant::now();
        if s.has_work(now) {
            let alive = Rc::new(Cell::new(true));
            let (back, flag) = (Rc::clone(&slot), Rc::clone(&alive));
            let ran = stacks::run(Box::new(move || {
                let mut s = s;
                flag.set(s.run(now));
                *back.borrow_mut() = Some(s);
            }))
            .await;
            let Some(mut s) = slot.borrow_mut().take() else { return };
            hand_out(conn, s.as_mut(), &mut scratch);
            if ran.is_err() || !alive.get() {
                finish(s).await;
                return;
            }
            *slot.borrow_mut() = Some(s);
            // More may have arrived, or be due, while it ran.
            continue;
        }
        hand_out(conn, s.as_mut(), &mut scratch);
        if conn.eof.get() && conn.inbox.borrow().is_empty() {
            finish(s).await;
            return;
        }
        arm(&mut timer, s.deadline());
        *slot.borrow_mut() = Some(s);
        if conn.inbox.borrow().is_empty() {
            conn.wake.wait().await;
        }
    }
}
