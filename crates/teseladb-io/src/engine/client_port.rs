//! A clients' port on the engine's network (023 FR-018, FR-019): it takes its connections the way
//! every port of the node does ([`super::accept`]: shard 0 owns the listening socket, every shard
//! takes its part of the connections, TLS first as `--tlsMode` says), and each connection is a
//! task of the shard that took it from then on.
//!
//! The port knows bytes and time; what the bytes mean is the [`Session`] the port's [`Service`]
//! makes for each connection (the wire protocol, in the server). A connection has three tasks:
//! its reader, which reads the socket into the connection's inbox; its writer, which sends what
//! the session answered; and its driver, which hands what arrived to the session and runs the
//! session's work in an engine thread of [`super::stacks`] (a command waits for the cores without
//! holding up the shard).
//!
//! A connection whose answers wait to go out past [`PAUSE_AT`] bytes is not read until they are
//! below [`RESUME_AT`]: a client that does not read its answers stops being read.
//!
//! Every engine wait is in a non-generic function of this crate.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb::engine::core::sleep::sleep_steady;
use teseladb::engine::core::timer::Timer;
use teseladb::engine::net::api::{ConnectedSocket, ConnectedSocketInputStreamConfig};
use teseladb_x509::Certificate;

use super::accept::{AcceptConfig, Acceptor, MakeSide, Side, Taken};
use super::post::Wake;
use super::stacks;
use super::tls_stream::{Sniff, TlsSession};

/// Answers waiting to go out past which a connection is not read (023 FR-018).
pub const PAUSE_AT: usize = 4 << 20;
/// Answers waiting to go out below which a paused connection is read again.
pub const RESUME_AT: usize = 2 << 20;
/// Bytes the output stream of a connection buffers before it writes.
const OUTPUT_BUFFER: usize = 64 << 10;
/// How long a connection has to send its first bytes and end its TLS handshake.
pub const HANDSHAKE_LIMIT: Duration = Duration::from_secs(10);
/// The port's name in errors.
const NAME: &str = "clients' port";

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
    /// No engine thread could run its work ([`super::stacks::run`] failed to start it, `why`
    /// says why): it logs it and answers its next request with an error instead of running it;
    /// `false` closes the connection once that answer is sent (what a session that cannot
    /// answer does).
    fn refuse(&mut self, why: &str) -> bool {
        let _ = why;
        false
    }
    /// The connection ended; what was not run is dropped.
    fn end(self: Box<Self>);
}

/// What came of running a session's work in an engine thread.
pub(crate) enum Ran {
    /// It ran; whether the connection stays open.
    Done(bool),
    /// It panicked (the panic's message went to the standard error).
    Panicked,
    /// No engine thread could run it, and why.
    NotRun(String),
}

/// How long a connection whose work no engine thread could run waits before it tries again.
pub(crate) const RETRY_AFTER: Duration = Duration::from_millis(10);

/// Runs the work of `session` in an engine thread of this shard; the session comes back with
/// what came of it, also when its work panicked or did not run (the job owns the session only
/// while it runs, so it always comes back).
pub(crate) async fn run_work(
    session: Box<dyn Session>,
    now: Instant,
) -> (Option<Box<dyn Session>>, Ran) {
    let slot = Rc::new(RefCell::new(Some(session)));
    let outcome = Rc::new(RefCell::new(None));
    let (back, out) = (Rc::clone(&slot), Rc::clone(&outcome));
    let ran = stacks::run(Box::new(move || {
        let Some(mut s) = back.borrow_mut().take() else { return };
        let r = catch_unwind(AssertUnwindSafe(|| s.run(now)));
        *out.borrow_mut() = Some(r.map_or(Ran::Panicked, Ran::Done));
        *back.borrow_mut() = Some(s);
    }))
    .await;
    let how = outcome.borrow_mut().take().unwrap_or_else(|| {
        Ran::NotRun(ran.err().map_or_else(|| "it did not start".to_owned(), |e| e.to_string()))
    });
    let session = slot.borrow_mut().take();
    (session, how)
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

/// The port on one shard: its service and the connections open here.
struct ClientSide {
    id: u64,
    service: Arc<dyn Service>,
    next: Cell<u64>,
    conns: RefCell<HashMap<u64, Rc<Conn>>>,
}

thread_local! {
    /// The ports of this shard, by the number of their [`Acceptor`].
    static SIDES: RefCell<HashMap<u64, Rc<ClientSide>>> = RefCell::new(HashMap::new());
}

impl ClientSide {
    /// The side of port `id` on this shard, registered so that [`wake_here`] finds it.
    fn here(id: u64, service: Arc<dyn Service>) -> Rc<ClientSide> {
        let side = Rc::new(ClientSide {
            id,
            service,
            next: Cell::new(0),
            conns: RefCell::new(HashMap::new()),
        });
        SIDES.with(|s| s.borrow_mut().insert(id, Rc::clone(&side)));
        side
    }

    fn conns(&self) -> Vec<Rc<Conn>> {
        self.conns.borrow().values().cloned().collect()
    }
}

impl Side for ClientSide {
    fn sniff(&self) -> Sniff {
        self.service.sniff()
    }

    fn serve(self: Rc<Self>, conn: Taken) -> impl std::future::Future<Output = ()> + 'static {
        serve(self, conn)
    }

    /// Its connections close: what they answered goes out first, and a command that runs ends
    /// before its connection closes.
    fn stop(&self) {
        SIDES.with(|s| s.borrow_mut().remove(&self.id));
        for c in self.conns() {
            c.stopping.set(true);
            c.wake.wake();
        }
    }
}

fn side_here(port: u64) -> Option<Rc<ClientSide>> {
    SIDES.with(|s| s.borrow().get(&port).cloned())
}

/// Wakes every connection of port `port` on this shard: the node's state changed, so each
/// session looks at its waits and whether it must close.
pub fn wake_here(port: u64) {
    for c in side_here(port).map(|s| s.conns()).unwrap_or_default() {
        c.wake.wake();
    }
}

/// Connections of port `port` open on this shard.
pub fn open_here(port: u64) -> usize {
    side_here(port).map_or(0, |s| s.conns.borrow().len())
}

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

/// Runs the future `f` makes on `shard` and waits until it resolves, from any thread (an engine
/// thread parks); a failure keeps the kind of an [`io::Error`].
fn on_shard_until<R: Send + 'static>(
    shard: u32,
    f: Box<dyn FnOnce() -> Future<R> + Send>,
) -> io::Result<R> {
    let (tx, rx) = super::answer::channel();
    let posted = super::wait::post(
        shard,
        Box::new(move || {
            run_in_background(f().then_wrapped(move |r: Future<R>| {
                let _ = tx.send(r.get().map_err(|e| io_error(&e)));
                make_ready_future(())
            }));
        }),
    );
    if !posted {
        return Err(io::Error::other("no engine runs in this process"));
    }
    rx.recv().map_err(|_| io::Error::other("the engine stopped"))?
}

fn io_error(e: &ExceptionPtr) -> io::Error {
    match e.downcast_ref::<io::Error>() {
        Some(io) => io::Error::new(io.kind(), io.to_string()),
        None => io::Error::other(e.to_string()),
    }
}

/// A running port, usable from any thread: what [`start_port`] started on every shard.
#[derive(Debug)]
pub struct PortRef {
    acceptor: Acceptor,
    shards: u32,
}

/// Starts a port at `addrs` on every shard of the engine, with `service` for its connections.
/// Called from a thread that is not a shard, or from an engine thread.
///
/// # Errors
///
/// When no engine runs, or an address cannot be listened on (nothing keeps listening then).
pub fn start_port(addrs: &[SocketAddr], service: Arc<dyn Service>) -> io::Result<PortRef> {
    let shards = on_shard(0, Box::new(teseladb::engine::core::smp::Smp::count))?;
    let cfg = AcceptConfig {
        name: NAME,
        addrs: addrs.to_vec(),
        handshake: HANDSHAKE_LIMIT,
        // The service bounds the connections (`--maxConns`).
        max_connections: None,
        local_address: true,
    };
    let make: MakeSide<ClientSide> = Arc::new(move |id| ClientSide::here(id, Arc::clone(&service)));
    let acceptor = on_shard_until(0, Box::new(move || Acceptor::start(cfg, make)))?;
    Ok(PortRef { acceptor, shards })
}

impl PortRef {
    /// The addresses it listens on, with the ports the system chose for port 0.
    pub fn addrs(&self) -> &[SocketAddr] {
        self.acceptor.addrs()
    }

    /// Its number in the process.
    pub fn id(&self) -> u64 {
        self.acceptor.id()
    }

    /// The shards it runs on.
    pub fn shards(&self) -> u32 {
        self.shards
    }

    /// Wakes every connection of the port on every shard (the node's state changed); does not
    /// wait.
    pub fn wake(&self) {
        wake_port(self.id(), self.shards);
    }

    /// A function that wakes every connection of the port, for whoever publishes the node's
    /// state.
    pub fn waker(&self) -> Arc<dyn Fn() + Send + Sync> {
        let (id, shards) = (self.id(), self.shards);
        Arc::new(move || wake_port(id, shards))
    }

    /// Connections open on each shard, in shard order.
    pub fn open_by_shard(&self) -> Vec<usize> {
        let id = self.id();
        (0..self.shards)
            .map(|s| on_shard(s, Box::new(move || open_here(id))).unwrap_or(0))
            .collect()
    }

    /// Stops the port on every shard and waits until its connections closed: what they
    /// answered goes out first, and a command that runs ends before its connection closes.
    pub fn stop(&self) {
        let acceptor = self.acceptor.clone();
        let _ = on_shard_until(0, Box::new(move || acceptor.stop()));
    }
}

fn wake_port(id: u64, shards: u32) {
    for shard in 0..shards {
        let _ = super::wait::post(shard, Box::new(move || wake_here(id)));
    }
}

/// A connection the port took on this shard: its session, then its tasks.
async fn serve(side: Rc<ClientSide>, taken: Taken) {
    let tls: Option<TlsSession> = taken.tls;
    let info = ConnInfo {
        peer: taken.peer,
        local: taken.local,
        tls: tls.is_some(),
        peer_certificates: tls.as_ref().map(TlsSession::peer_certificates).unwrap_or_default(),
        handshake: taken.handshake,
    };
    let mut sock = taken.socket;
    let Some(session) = side.service.open(info) else {
        sock.shutdown_input();
        sock.shutdown_output();
        return;
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
    let id = side.next.get() + 1;
    side.next.set(id);
    side.conns.borrow_mut().insert(id, Rc::clone(&conn));
    drop(make_coroutine(reader(input, Rc::clone(&conn))));
    drop(make_coroutine(writer(output, Rc::clone(&conn))));
    drive(&conn, session).await;
    side.conns.borrow_mut().remove(&id);
    // The writer sends what is left, then closes.
    conn.closing.set(true);
    conn.out_wake.wake();
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
            let (back, how) = run_work(s, now).await;
            let Some(mut s) = back else { return };
            let (alive, retry) = match how {
                Ran::Done(alive) => (alive, false),
                // Its state is not known after a panic: it ends, and the connection with it.
                Ran::Panicked => (false, false),
                // Answered with an error and logged; it waits before it tries again.
                Ran::NotRun(why) => (s.refuse(&why), true),
            };
            hand_out(conn, s.as_mut(), &mut scratch);
            if !alive {
                finish(s).await;
                return;
            }
            if retry {
                *slot.borrow_mut() = Some(s);
                let _ = sleep_steady(RETRY_AFTER).await;
                continue;
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
