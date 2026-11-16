//! Connections between nodes on the engine's POSIX network stack (023 FR-015): one outbound
//! connection per node, dialed by the core that has frames for it and redialed with a backoff of
//! 10 ms to 1 s with jitter after a failure; up to [`MAX_PENDING`] bytes of frames wait for a
//! node that is not connected; the inbound connections come from the node's listening socket,
//! which the engine hands to the shard of the core that the dialer's source port names.
//!
//! The protocol does not change: the handshake of [`crate::handshake`], over TLS 1.3 with the
//! streams of [`tls_stream`] between nodes with a PKI, and the frames of [`crate::peer`]. Each connection has a reader task, which hands
//! what it reads to the core's turn, and a writer task, which sends in one write what the turn
//! queued for it. A connection that reaches a shard other than its core's (a NAT that changed its
//! source port) is relayed: its shard reads it and hands the bytes to the core's shard.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, SystemError};
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::reactor::{connect_from, listen_with};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::timer::{Clock, SteadyClockType, Timer};
use teseladb::engine::core::with_timeout::with_timeout;
use teseladb::engine::net::api::{
    ConnectedSocket, ConnectedSocketInputStreamConfig, ListenOptions, LoadBalancingAlgorithm,
    ServerSocket,
};
use teseladb::engine::net::socket_defs::{SocketAddress, Transport};
use teseladb_tls::ClientConfig;
use teseladb_types::{Nanos, NodeId, Rng};

use super::blocking;
use super::connect::{socket_address, std_addr};
use super::core_loop::{BACKOFF_MAX_MS, BACKOFF_MIN_MS, CoreLogic, MAX_PENDING};
use super::post::{CoreIo, Gate, ShardPost, Wake};
use super::tls_stream::{self, TlsSession};
use crate::file_send::FileSend;
use crate::handshake::{
    AcceptorOptions, HANDSHAKE_TIMEOUT, HandshakeError, Handshaked, JOIN_TIMEOUT, JoinRequest,
    validate,
};
use crate::peer::{Hello, PeerMsg, decode_frame, encode_frame, encode_frame_for};
use crate::secure::PeerTls;

/// Bytes an output stream of a peer connection buffers before it writes.
const OUTPUT_BUFFER: usize = 64 * 1024;
/// Source ports tried for a dial that must come from a port of its core (see [`source_port`]).
const SOURCE_PORT_TRIES: u32 = 16;
/// The ephemeral ports a dial picks its source port from.
const SOURCE_PORTS: std::ops::Range<u32> = 32_768..61_000;

/// What happens on a core's connections, handed from their tasks to the core's turn.
pub(crate) enum NetEvent {
    /// Bytes read from connection `token`.
    Data(u64, TemporaryBuffer<u8>),
    /// Connection `token` ended (read or write failed, or the peer closed).
    Closed(u64),
    /// A dial this core asked for ended.
    Dialed(NodeId, Result<Established, HandshakeError>),
    /// A connection of a node to this core passed its handshake on this shard.
    Accepted(Established),
    /// An event of a connection that another shard holds for this core.
    Relayed(Relayed),
}

/// What another shard tells a core about a connection it relays to it.
pub(crate) enum Relayed {
    /// A node's connection to the core passed its handshake on shard `.0` as relay `.1`.
    Open { from: (u32, u64), got: Handshaked },
    /// Its decrypted bytes.
    Data((u32, u64), Vec<u8>),
    /// It ended.
    Closed((u32, u64)),
}

/// An engine connection (plaintext: over TLS, the stream of [`tls_stream`]) and its two streams.
pub(crate) struct Stream {
    sock: Rc<RefCell<ConnectedSocket>>,
    input: InputStream<u8>,
    output: OutputStream<u8>,
}

impl Stream {
    fn new(mut sock: ConnectedSocket) -> Stream {
        let input = sock.input(ConnectedSocketInputStreamConfig::default());
        let output = sock.output(OUTPUT_BUFFER);
        Stream { sock: Rc::new(RefCell::new(sock)), input, output }
    }

    /// Closes the connection: what its output holds goes first (over TLS, then `close_notify`).
    async fn close(mut self) {
        let _ = self.output.close().await;
        Stream::shutdown(&self.sock);
    }

    /// Ends both directions: a pending read returns the end of the stream.
    fn shutdown(sock: &RefCell<ConnectedSocket>) {
        if let Ok(mut s) = sock.try_borrow_mut() {
            s.shutdown_input();
            s.shutdown_output();
        }
    }
}

/// A connection past its handshake.
pub(crate) struct Established {
    stream: Stream,
    got: Handshaked,
}

/// Shuts a socket down when a handshake takes longer than its limit.
struct Deadline {
    timer: Pin<Box<Timer>>,
}

impl Deadline {
    fn new(sock: &Rc<RefCell<ConnectedSocket>>, limit: Duration) -> Deadline {
        let sock = Rc::downgrade(sock);
        let mut timer = Box::pin(Timer::with_callback(move || {
            if let Some(s) = sock.upgrade() {
                Stream::shutdown(&s);
            }
        }));
        timer.as_mut().arm_in(limit);
        Deadline { timer }
    }

    fn cancel(mut self) {
        self.timer.as_mut().cancel();
    }
}

fn io_error(e: ExceptionPtr) -> HandshakeError {
    HandshakeError::Io(std::io::Error::other(e.to_string()))
}

fn eof() -> HandshakeError {
    HandshakeError::Io(std::io::ErrorKind::UnexpectedEof.into())
}

/// Reads one frame (`buf` holds bytes already received); returns it and what followed it.
async fn read_frame(
    s: &mut Stream,
    mut buf: Vec<u8>,
) -> Result<(PeerMsg, Vec<u8>), HandshakeError> {
    loop {
        if let Some((msg, used)) = decode_frame(&buf).map_err(HandshakeError::Frame)? {
            return Ok((msg, buf.split_off(used)));
        }
        let b = s.input.read().await.map_err(io_error)?;
        if b.empty() {
            return Err(eof());
        }
        buf.extend_from_slice(b.as_slice());
    }
}

async fn send_frame(s: &mut Stream, msg: &PeerMsg) -> Result<(), HandshakeError> {
    let mut frame = Vec::new();
    encode_frame(msg, false, &mut frame).map_err(HandshakeError::Frame)?;
    s.output.write(&frame).await.map_err(io_error)?;
    s.output.flush().await.map_err(io_error)
}

async fn read_hello(s: &mut Stream) -> Result<Handshaked, HandshakeError> {
    match read_frame(s, Vec::new()).await? {
        (PeerMsg::Hello(hello), rest) => {
            Ok(Handshaked { version: hello.versions.max, hello, rest })
        }
        _ => Err(HandshakeError::NotHello),
    }
}

/// Over TLS every peer but one that enrolls shows a certificate of the cluster's CA (the TLS
/// layer checked its chain).
fn certified(tls: Option<&TlsSession>) -> Result<(), HandshakeError> {
    match tls {
        Some(t) if t.peer_certificates().is_empty() => Err(HandshakeError::NoCertificate),
        _ => Ok(()),
    }
}

/// The address of `addr`: a host name is resolved off the shard.
async fn resolve(addr: &str) -> Result<SocketAddr, HandshakeError> {
    if let Ok(a) = addr.parse::<SocketAddr>() {
        return Ok(a);
    }
    let not_found = || {
        HandshakeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "address without results",
        ))
    };
    let (host, port) = addr.rsplit_once(':').ok_or_else(not_found)?;
    let port = port.parse::<u16>().map_err(|_| not_found())?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let found = blocking::resolve(host, port, HANDSHAKE_TIMEOUT).await.map_err(io_error)?;
    found.into_iter().next().ok_or_else(not_found)
}

/// A source port for a dial of core `core` of a node with `cores` cores: the listener of a node
/// with several cores hands each connection to the shard its source port names (the port modulo
/// the cores), and a node's core `k` runs on shard `k`.
fn source_port(rng: &mut Rng, core: u16, cores: u16) -> u16 {
    let n = u32::from(cores.max(1));
    let p = rng.range(u64::from(SOURCE_PORTS.start), u64::from(SOURCE_PORTS.end)) as u32;
    let p = p - p % n + u32::from(core) % n;
    let p = if p >= SOURCE_PORTS.end { p - n } else { p };
    u16::try_from(p).unwrap_or(u16::MAX)
}

/// The wildcard address of `target`'s family with `port`.
fn wildcard(target: SocketAddr, port: u16) -> SocketAddr {
    match target {
        SocketAddr::V4(_) => SocketAddr::from(([0, 0, 0, 0], port)),
        SocketAddr::V6(_) => SocketAddr::from(([0u16; 8], port)),
    }
}

/// Connects to `target` from a source port of core `hello.core` when the node has several cores.
async fn connect(
    target: SocketAddr,
    hello: &Hello,
    rng: &mut Rng,
) -> Result<ConnectedSocket, HandshakeError> {
    let deadline = SteadyClockType::add(SteadyClockType::now(), HANDSHAKE_TIMEOUT);
    let tries = if hello.cores > 1 { SOURCE_PORT_TRIES } else { 1 };
    let mut last = eof();
    for _ in 0..tries {
        let local = match hello.cores > 1 {
            true => socket_address(wildcard(target, source_port(rng, hello.core, hello.cores))),
            false => SocketAddress::default(),
        };
        let f = connect_from(socket_address(target), local, Transport::TCP);
        match with_timeout::<SteadyClockType, _>(deadline, f).await {
            Ok(s) => return Ok(s),
            Err(e) => last = io_error(e),
        }
        if SteadyClockType::now() >= deadline {
            break;
        }
    }
    Err(last)
}

/// Dials `node` at `addr` as `hello` and runs the handshake, over TLS with `client`.
async fn dial(
    node: NodeId,
    addr: String,
    hello: Hello,
    client: Option<Arc<ClientConfig>>,
    seed: u64,
) -> Result<Established, HandshakeError> {
    let target = resolve(&addr).await?;
    let mut rng = Rng::new(seed);
    let mut sock = connect(target, &hello, &mut rng).await?;
    // Frames are small and latency-bound; Nagle would hold them (on the TCP socket, under TLS).
    let _ = sock.set_nodelay(true);
    if let Some(c) = client {
        // No name is checked: the peer is whoever holds a certificate of the cluster's CA.
        let t = tls_stream::client(sock, c, None, None, Some(HANDSHAKE_TIMEOUT)).await;
        sock = t.map_err(io_error)?.socket;
    }
    let mut s = Stream::new(sock);
    let deadline = Deadline::new(&s.sock, HANDSHAKE_TIMEOUT);
    let got = async {
        send_frame(&mut s, &PeerMsg::Hello(hello.clone())).await?;
        let got = read_hello(&mut s).await?;
        let version = validate(&hello, &got.hello)?;
        if got.hello.node_id != node {
            return Err(HandshakeError::UnexpectedNode { expected: node, got: got.hello.node_id });
        }
        Ok::<_, HandshakeError>(Handshaked { version, ..got })
    }
    .await;
    deadline.cancel();
    match got {
        Ok(got) => Ok(Established { stream: s, got }),
        Err(e) => {
            s.close().await;
            Err(e)
        }
    }
}

/// What the acceptor of one shard needs.
pub(crate) struct AcceptCtx {
    /// The node's `Hello` (its `core` is set to the one each peer asks for).
    pub(crate) local: Hello,
    pub(crate) opts: AcceptorOptions,
    /// The shard of the node's core 0: core `k` runs on shard `base + k`.
    pub(crate) base: u32,
    pub(crate) gate: Arc<Gate>,
    /// This shard.
    pub(crate) shard: u32,
}

thread_local! {
    /// The listening sockets of this shard, to stop them.
    static LISTENERS: RefCell<Vec<(SocketAddr, Rc<RefCell<ServerSocket>>)>> = const { RefCell::new(Vec::new()) };
    /// The connections this shard relays to other shards, by relay number.
    static RELAYS: RefCell<HashMap<u64, Rc<RefCell<ConnectedSocket>>>> = RefCell::new(HashMap::new());
    static NEXT_RELAY: Cell<u64> = const { Cell::new(0) };
}

thread_local! {
    /// The peer port this shard reserved before the node's cores ran, by the address it took.
    static RESERVED: RefCell<Vec<(SocketAddr, ServerSocket)>> = const { RefCell::new(Vec::new()) };
}

/// How the node's peer port listens: spread by the dialer's source port over `cores` shards from
/// `base`, or all on `base` with one core.
fn peer_listen_options(cores: u16, base: u32) -> ListenOptions {
    let mut opts = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    if cores > 1 {
        opts.lba = LoadBalancingAlgorithm::Port;
    } else {
        opts.set_fixed_cpu(base);
    }
    opts
}

/// The [`std::io::Error`] of a listen that failed, with the system's kind when it has one (an
/// address in use reads as [`std::io::ErrorKind::AddrInUse`]).
fn listen_error(e: &ExceptionPtr) -> std::io::Error {
    match e.downcast_ref::<SystemError>() {
        Some(s) => {
            let kind = std::io::Error::from_raw_os_error(s.code()).kind();
            std::io::Error::new(kind, s.to_string())
        }
        None => std::io::Error::other(e.to_string()),
    }
}

/// Reserves the node's peer port at `at` on this shard (shard 0, which owns the listening
/// socket) before the node's cores run (023 FR-015, T055): the socket listens, so its address
/// (a port 0 picks one) names the node when it joins, but nothing accepts until
/// [`listen_here`] takes it; a peer that calls meanwhile waits in the backlog. Returns the
/// address it took.
pub(crate) fn reserve_here(at: SocketAddr, cores: u16, base: u32) -> std::io::Result<SocketAddr> {
    let ss = listen_with(socket_address(at), peer_listen_options(cores, base))
        .map_err(|e| listen_error(&e))?;
    let addr = ss
        .local_address()
        .ok()
        .and_then(|a| std_addr(&a))
        .ok_or_else(|| std::io::Error::other("the listening socket has no address"))?;
    RESERVED.with(|r| r.borrow_mut().push((addr, ss)));
    Ok(addr)
}

/// Lets go of the ports this shard reserved and no listener took.
pub(crate) fn release_here() {
    let gone = RESERVED.with(|r| std::mem::take(&mut *r.borrow_mut()));
    for (_, mut ss) in gone {
        ss.abort_accept();
    }
}

/// Listens for the node's peers on this shard: the shard that owns the listening socket (the
/// first, `owner`) accepts and the engine hands each connection to a shard of the node's cores;
/// every one of them accepts its own. The owner takes the socket it reserved at `at`
/// ([`reserve_here`]) if it did. Returns the address listened on (the owner's).
pub(crate) fn listen_here(
    at: SocketAddr,
    owner: bool,
    cores: u16,
    ctx: AcceptCtx,
) -> std::io::Result<Option<SocketAddr>> {
    let reserved = owner
        .then(|| {
            RESERVED.with(|r| {
                let mut r = r.borrow_mut();
                let i = r.iter().position(|(a, _)| *a == at)?;
                Some(r.swap_remove(i).1)
            })
        })
        .flatten();
    let ss = match reserved {
        Some(ss) => ss,
        None => listen_with(socket_address(at), peer_listen_options(cores, ctx.base))
            .map_err(|e| listen_error(&e))?,
    };
    let addr = owner.then(|| ss.local_address().ok().and_then(|a| std_addr(&a))).flatten();
    let ss = Rc::new(RefCell::new(ss));
    LISTENERS.with(|l| l.borrow_mut().push((at, ss.clone())));
    drop(make_coroutine(accept_loop(ss, Rc::new(ctx))));
    Ok(addr)
}

/// Stops this shard's listeners on `at` (all of them with `None`): their accept loops end.
pub(crate) fn stop_listening_here(at: Option<SocketAddr>) {
    let gone: Vec<_> = LISTENERS.with(|l| {
        let mut l = l.borrow_mut();
        let (gone, kept) =
            std::mem::take(&mut *l).into_iter().partition(|(a, _)| at.is_none_or(|x| x == *a));
        *l = kept;
        gone
    });
    for (_, ss) in gone {
        ss.borrow_mut().abort_accept();
    }
}

async fn accept_loop(
    ss: Rc<RefCell<ServerSocket>>,
    ctx: Rc<AcceptCtx>,
) -> Result<(), ExceptionPtr> {
    loop {
        let f = ss.borrow_mut().accept();
        // An aborted listener ends the loop; a failed accept only loses that connection.
        match f.await {
            Ok(a) => drop(make_coroutine(answer(a.connection, ctx.clone()))),
            Err(_) if LISTENERS.with(|l| !l.borrow().iter().any(|(_, x)| Rc::ptr_eq(x, &ss))) => {
                return Ok(());
            }
            Err(_) => {}
        }
    }
}

/// The acceptor's side of a connection: validates the peer's `Hello` and hands the connection to
/// the core it names, or answers a node that joins or enrolls.
async fn answer(mut sock: ConnectedSocket, ctx: Rc<AcceptCtx>) -> Result<(), ExceptionPtr> {
    let _ = sock.set_nodelay(true);
    let (sock, tls) = match ctx.opts.tls.as_ref() {
        None => (sock, None),
        Some(t) => match tls_stream::server(sock, t.server(), Some(HANDSHAKE_TIMEOUT)).await {
            Ok(t) => (t.socket, Some(t.session)),
            // A failed TLS handshake closes the connection.
            Err(_) => return Ok(()),
        },
    };
    let mut s = Stream::new(sock);
    let deadline = Deadline::new(&s.sock, HANDSHAKE_TIMEOUT);
    let r = handshake_inbound(&mut s, tls.as_ref(), &ctx).await;
    deadline.cancel();
    match r {
        Ok(Some(got)) => route(Established { stream: s, got }, &ctx),
        // A join or enrollment that was answered, or a failed handshake, closes the connection.
        _ => s.close().await,
    }
    Ok(())
}

async fn handshake_inbound(
    s: &mut Stream,
    tls: Option<&TlsSession>,
    ctx: &AcceptCtx,
) -> Result<Option<Handshaked>, HandshakeError> {
    let got = read_hello(s).await?;
    let newcomer = ctx.opts.join.is_some() || ctx.opts.enroll.is_some();
    let local = &ctx.local;
    if got.hello.node_id != NodeId(0) || !newcomer {
        let version = validate(local, &got.hello)?;
        certified(tls)?;
        send_frame(s, &PeerMsg::Hello(Hello { core: got.hello.core, ..local.clone() })).await?;
        return Ok(Some(Handshaked { version, ..got }));
    }
    send_frame(s, &PeerMsg::Hello(Hello { core: 0, ..local.clone() })).await?;
    // The cluster may take a while (a proposal and a configuration change).
    let deadline = Deadline::new(&s.sock, JOIN_TIMEOUT + HANDSHAKE_TIMEOUT);
    let r = newcomer_request(s, tls, got.rest, ctx).await;
    deadline.cancel();
    r.map(|()| None)
}

async fn newcomer_request(
    s: &mut Stream,
    tls: Option<&TlsSession>,
    rest: Vec<u8>,
    ctx: &AcceptCtx,
) -> Result<(), HandshakeError> {
    let answer = match (read_frame(s, rest).await?.0, &ctx.opts.join, &ctx.opts.enroll) {
        (PeerMsg::JoinRequest { name, peer_addr, http_addr, native_addr, cores }, Some(j), _) => {
            certified(tls)?;
            let req = JoinRequest { name, peer_addr, http_addr, native_addr, cores };
            let j = j.clone();
            // The handler waits for Group 0: in an engine thread, which parks meanwhile.
            let r = in_engine_thread(move || j(req)).await;
            PeerMsg::JoinResponse(r.unwrap_or_else(|| Err("the node is stopping".into())))
        }
        (PeerMsg::EnrollRequest(req), _, Some(e)) => {
            let e = e.clone();
            let r = in_engine_thread(move || e(req)).await;
            PeerMsg::EnrollResponse(r.unwrap_or_else(|| Err("the node is stopping".into())))
        }
        _ => return Err(HandshakeError::UnexpectedFrame),
    };
    send_frame(s, &answer).await
}

/// Runs `f`, which waits on the cores (a join, an enrollment), in an engine thread of this shard
/// (023 FR-026): its waits park the thread and the shard goes on. `None` when no engine thread
/// could be made or `f` panicked.
async fn in_engine_thread<T: 'static>(f: impl FnOnce() -> T + 'static) -> Option<T> {
    let out = Rc::new(RefCell::new(None));
    let slot = Rc::clone(&out);
    let ran = super::stacks::run(Box::new(move || *slot.borrow_mut() = Some(f()))).await;
    ran.ok().and_then(|()| out.borrow_mut().take())
}

/// Hands an accepted connection to the core it names: on this shard, or relayed to the core's.
fn route(e: Established, ctx: &AcceptCtx) {
    let target = ctx.base + u32::from(e.got.hello.core);
    if target == ctx.shard {
        super::post::with_io(|io| io.push_net(NetEvent::Accepted(e)));
        return;
    }
    let id = NEXT_RELAY.with(|n| {
        n.set(n.get() + 1);
        n.get()
    });
    let Established { mut stream, got } = e;
    RELAYS.with(|r| r.borrow_mut().insert(id, stream.sock.clone()));
    let to = ShardPost::new(target, ctx.gate.clone());
    let from = (ctx.shard, id);
    to.post_relay(Relayed::Open { from, got });
    drop(make_coroutine(async move {
        loop {
            match stream.input.read().await {
                Ok(b) if !b.empty() => {
                    if !to.post_relay(Relayed::Data(from, b.as_slice().to_vec())) {
                        break;
                    }
                }
                _ => break,
            }
        }
        RELAYS.with(|r| r.borrow_mut().remove(&id));
        to.post_relay(Relayed::Closed(from));
        stream.close().await;
        Ok::<(), ExceptionPtr>(())
    }));
}

/// Closes relay `id` of this shard (its core closed it).
fn close_relay(id: u64) {
    if let Some(s) = RELAYS.with(|r| r.borrow_mut().remove(&id)) {
        Stream::shutdown(&s);
    }
}

/// What a connection's writer task sends.
#[derive(Default)]
struct OutQueue {
    buf: RefCell<Vec<u8>>,
    wake: Wake,
    closed: Cell<bool>,
}

async fn reader(
    mut input: InputStream<u8>,
    token: u64,
    io: Rc<CoreIo>,
) -> Result<(), ExceptionPtr> {
    loop {
        match input.read().await {
            Ok(b) if !b.empty() => io.push_net(NetEvent::Data(token, b)),
            _ => break,
        }
    }
    io.push_net(NetEvent::Closed(token));
    Ok(())
}

async fn writer(
    q: Rc<OutQueue>,
    mut out: OutputStream<u8>,
    token: u64,
    io: Rc<CoreIo>,
) -> Result<(), ExceptionPtr> {
    let mut batch = Vec::new();
    while !q.closed.get() {
        if q.buf.borrow().is_empty() {
            q.wake.wait().await;
            continue;
        }
        std::mem::swap(&mut *q.buf.borrow_mut(), &mut batch);
        let sent = async {
            out.write(&batch).await?;
            out.flush().await
        }
        .await;
        batch.clear();
        if sent.is_err() {
            io.push_net(NetEvent::Closed(token));
            break;
        }
    }
    let _ = out.close().await;
    Ok(())
}

/// An open connection of the core.
struct Conn {
    peer: NodeId,
    outbound: bool,
    rbuf: Vec<u8>,
    /// What its writer sends; none for a relayed connection.
    out: Option<Rc<OutQueue>>,
    sock: Option<Rc<RefCell<ConnectedSocket>>>,
    /// The shard and number of the relay that holds it.
    relay: Option<(u32, u64)>,
}

enum LinkState {
    /// Not connected; the next frame dials.
    Idle,
    /// Waiting for the backoff.
    Backoff,
    /// Dial in progress.
    Dialing,
    /// Connected through this token.
    Up(u64),
}

struct Link {
    state: LinkState,
    pending: Vec<u8>,
    backoff_ms: u64,
    /// The protocol version both nodes spoke on the last connection (0: none yet).
    version: u16,
}

impl Link {
    fn new() -> Link {
        Link { state: LinkState::Idle, pending: Vec::new(), backoff_ms: 0, version: 0 }
    }
}

/// A core's connections to the other nodes.
pub(crate) struct Net {
    hello: Hello,
    tls: Option<PeerTls>,
    rng: Rng,
    io: Rc<CoreIo>,
    gate: Arc<Gate>,
    origin: Instant,
    conns: BTreeMap<u64, Conn>,
    links: BTreeMap<NodeId, Link>,
    /// Inter-node addresses announced in the `Hello` of incoming connections: used when the logic
    /// does not know a node's address yet (a node that just joined answers whoever reached it).
    learned: BTreeMap<NodeId, String>,
    /// The end of each link's backoff, in milliseconds of the core's clock.
    backoffs: BTreeSet<(u64, NodeId)>,
    relays: HashMap<(u32, u64), u64>,
    next_token: u64,
    events: VecDeque<NetEvent>,
    frame: Vec<u8>,
}

impl Net {
    pub(crate) fn new(
        hello: Hello,
        tls: Option<PeerTls>,
        seed: u64,
        io: Rc<CoreIo>,
        gate: Arc<Gate>,
        origin: Instant,
    ) -> Net {
        Net {
            hello,
            tls,
            rng: Rng::new(seed),
            io,
            gate,
            origin,
            conns: BTreeMap::new(),
            links: BTreeMap::new(),
            learned: BTreeMap::new(),
            backoffs: BTreeSet::new(),
            relays: HashMap::new(),
            next_token: 1,
            events: VecDeque::new(),
            frame: Vec::new(),
        }
    }

    fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }

    /// Takes the events that arrived. Returns whether any did.
    pub(crate) fn collect(&mut self) -> bool {
        self.io.take_net(&mut self.events);
        !self.events.is_empty()
    }

    /// Delivers what was read on open connections (before the turn's clock, as the reads of a
    /// poller came first) and closes those that ended. Returns whether there was any.
    pub(crate) fn read<L: CoreLogic>(&mut self, logic: &mut L, now: Nanos) -> bool {
        let mut worked = false;
        let mut rest = VecDeque::new();
        while let Some(ev) = self.events.pop_front() {
            match ev {
                NetEvent::Data(token, buf) => {
                    worked = true;
                    self.on_data(token, buf.as_slice(), logic, now);
                }
                NetEvent::Closed(token) => {
                    worked = true;
                    if self.conns.contains_key(&token) {
                        self.close(token, logic);
                    }
                }
                other => rest.push_back(other),
            }
        }
        self.events = rest;
        worked
    }

    /// Whether events that come after the turn's clock wait.
    pub(crate) fn has_inputs(&self) -> bool {
        !self.events.is_empty()
    }

    /// Takes the dials, accepted and relayed connections that arrived, in order.
    pub(crate) fn inputs<L: CoreLogic>(&mut self, logic: &mut L, now: Nanos) {
        while let Some(ev) = self.events.pop_front() {
            match ev {
                NetEvent::Data(token, buf) => self.on_data(token, buf.as_slice(), logic, now),
                NetEvent::Closed(token) => {
                    if self.conns.contains_key(&token) {
                        self.close(token, logic);
                    }
                }
                NetEvent::Accepted(e) => {
                    self.learned.insert(e.got.hello.node_id, e.got.hello.peer_addr.clone());
                    let peer = e.got.hello.node_id;
                    let token = self.add_conn(e.stream, peer, false, e.got.rest);
                    self.deliver(token, logic, now);
                }
                NetEvent::Dialed(node, Ok(e)) => {
                    let version = e.got.version;
                    let token = self.add_conn(e.stream, node, true, e.got.rest);
                    let link = self.links.entry(node).or_insert_with(Link::new);
                    link.state = LinkState::Up(token);
                    link.backoff_ms = 0;
                    link.version = version;
                    let pending = std::mem::take(&mut link.pending);
                    // Frames that came with the handshake go first; garbage there closes it.
                    if self.deliver(token, logic, now) {
                        self.queue_on(token, &pending);
                    }
                }
                NetEvent::Dialed(node, Err(_)) => self.link_failed(node, logic),
                NetEvent::Relayed(r) => self.on_relayed(r, logic, now),
            }
        }
    }

    fn on_relayed<L: CoreLogic>(&mut self, r: Relayed, logic: &mut L, now: Nanos) {
        match r {
            Relayed::Open { from, got } => {
                self.learned.insert(got.hello.node_id, got.hello.peer_addr.clone());
                let token = self.next_token;
                self.next_token += 1;
                let conn = Conn {
                    peer: got.hello.node_id,
                    outbound: false,
                    rbuf: got.rest,
                    out: None,
                    sock: None,
                    relay: Some(from),
                };
                self.conns.insert(token, conn);
                self.relays.insert(from, token);
                self.deliver(token, logic, now);
            }
            Relayed::Data(from, bytes) => {
                if let Some(&token) = self.relays.get(&from) {
                    self.on_data(token, &bytes, logic, now);
                }
            }
            Relayed::Closed(from) => {
                if let Some(&token) = self.relays.get(&from) {
                    self.close(token, logic);
                }
            }
        }
    }

    fn add_conn(&mut self, stream: Stream, peer: NodeId, outbound: bool, rest: Vec<u8>) -> u64 {
        let token = self.next_token;
        self.next_token += 1;
        let Stream { sock, input, output } = stream;
        let out = Rc::new(OutQueue::default());
        drop(make_coroutine(reader(input, token, self.io.clone())));
        drop(make_coroutine(writer(out.clone(), output, token, self.io.clone())));
        let conn =
            Conn { peer, outbound, rbuf: rest, out: Some(out), sock: Some(sock), relay: None };
        self.conns.insert(token, conn);
        token
    }

    fn on_data<L: CoreLogic>(&mut self, token: u64, raw: &[u8], logic: &mut L, now: Nanos) {
        let Some(conn) = self.conns.get_mut(&token) else { return };
        conn.rbuf.extend_from_slice(raw);
        self.deliver(token, logic, now);
    }

    /// Delivers what is buffered on `token` and closes it if a frame is invalid. Returns whether
    /// it is still open.
    fn deliver<L: CoreLogic>(&mut self, token: u64, logic: &mut L, now: Nanos) -> bool {
        let valid = self.read_frames(token, logic, now);
        if !valid {
            self.close(token, logic);
        }
        valid
    }

    /// Delivers the complete frames buffered on `token`. Returns `false` at an invalid one
    /// (nothing after it is delivered; the contract closes the connection).
    fn read_frames<L: CoreLogic>(&mut self, token: u64, logic: &mut L, now: Nanos) -> bool {
        let conn = self.conns.get_mut(&token).expect("open connection");
        let mut pos = 0;
        let valid = loop {
            match decode_frame(&conn.rbuf[pos..]) {
                Ok(Some((msg, used))) => {
                    pos += used;
                    logic.on_peer(conn.peer, msg, now);
                }
                Ok(None) => break true,
                Err(_) => break false,
            }
        };
        conn.rbuf.drain(..pos);
        valid
    }

    /// Closes connection `token`; an outbound one takes its link down.
    fn close<L: CoreLogic>(&mut self, token: u64, logic: &mut L) {
        let conn = self.conns.remove(&token).expect("open connection");
        self.drop_conn(&conn);
        if conn.outbound {
            self.link_failed(conn.peer, logic);
        }
    }

    fn drop_conn(&mut self, conn: &Conn) {
        if let Some(q) = &conn.out {
            q.closed.set(true);
            q.wake.wake();
        }
        if let Some(s) = &conn.sock {
            Stream::shutdown(s);
        }
        if let Some(from) = conn.relay {
            self.relays.remove(&from);
            ShardPost::new(from.0, self.gate.clone()).post(move || close_relay(from.1));
        }
    }

    /// Closes every connection (a stopping core).
    pub(crate) fn close_all(&mut self) {
        for (_, conn) in std::mem::take(&mut self.conns) {
            self.drop_conn(&conn);
        }
    }

    /// The outbound link to `node` is down: drop what was queued, tell the logic and redial later.
    fn link_failed<L: CoreLogic>(&mut self, node: NodeId, logic: &mut L) {
        crate::counters::record_peer_link_failed();
        let link = self.links.entry(node).or_insert_with(Link::new);
        let backoff = (link.backoff_ms * 2).clamp(BACKOFF_MIN_MS, BACKOFF_MAX_MS);
        link.backoff_ms = backoff;
        link.state = LinkState::Backoff;
        link.pending.clear();
        // An announced address that fails is forgotten: the next dial uses the logic's, which may
        // be newer (the node moved again without dialing us).
        self.learned.remove(&node);
        // Jitter of ±25 % so that the nodes of a restarted cluster do not dial in lockstep.
        let jitter = self.rng.range(0, backoff / 2 + 1);
        let at = self.now_ms() + backoff - backoff / 4 + jitter;
        self.backoffs.insert((at, node));
        logic.peer_unreachable(node);
    }

    /// When the next backoff ends, in nanoseconds of the core's clock.
    pub(crate) fn next_deadline(&self) -> Option<Nanos> {
        self.backoffs.first().map(|(ms, _)| Nanos::from_millis(*ms))
    }

    /// Ends the backoffs that are due: a link with frames waiting dials again.
    pub(crate) fn backoffs_done<L: CoreLogic>(&mut self, logic: &L) {
        let now = self.now_ms();
        while let Some(&(at, node)) = self.backoffs.first() {
            if at > now {
                break;
            }
            self.backoffs.pop_first();
            let link = self.links.get_mut(&node).expect("link of a backoff");
            link.state = LinkState::Idle;
            if !link.pending.is_empty() {
                self.dial(node, logic);
            }
        }
    }

    fn dial<L: CoreLogic>(&mut self, node: NodeId, logic: &L) {
        // The address the node itself announced in its last `Hello` is newer than the logic's
        // (a node that came back on another port); the logic's serves nodes that never dialed us.
        let Some(addr) = self.learned.get(&node).cloned().or_else(|| logic.peer_addr(node)) else {
            // Unknown address: nothing to dial; frames are dropped (Raft resends).
            self.links.get_mut(&node).expect("link").pending.clear();
            return;
        };
        self.links.get_mut(&node).expect("link").state = LinkState::Dialing;
        let hello = self.hello.clone();
        let client = self.tls.as_ref().map(PeerTls::client);
        let io = self.io.clone();
        let seed = self.rng.next_u64();
        drop(make_coroutine(async move {
            let r = dial(node, addr, hello, client, seed).await;
            io.push_net(NetEvent::Dialed(node, r));
            Ok::<(), ExceptionPtr>(())
        }));
    }

    /// Queues `msg` for `node`: on its connection, or waiting for one (dialed if idle).
    pub(crate) fn send_to<L: CoreLogic>(&mut self, node: NodeId, msg: &PeerMsg, logic: &mut L) {
        let mut frame = std::mem::take(&mut self.frame);
        frame.clear();
        // A forwarded request or answer larger than a frame goes in pieces to a node that reads
        // them (protocol 6). A frame that cannot be encoded even so is the logic's bug; it is
        // dropped like a lost message rather than taking the core down.
        let version = self.links.get(&node).map_or(0, |l| l.version);
        if encode_frame_for(msg, false, version, &mut frame).is_ok() {
            self.send_bytes(node, &frame, logic);
        }
        self.frame = frame;
    }

    fn send_bytes<L: CoreLogic>(&mut self, node: NodeId, frame: &[u8], logic: &mut L) {
        let link = self.links.entry(node).or_insert_with(Link::new);
        match link.state {
            LinkState::Up(token) => self.queue_on(token, frame),
            LinkState::Idle | LinkState::Backoff | LinkState::Dialing => {
                if link.pending.len() + frame.len() <= MAX_PENDING {
                    link.pending.extend_from_slice(frame);
                }
                if matches!(link.state, LinkState::Idle) {
                    self.dial(node, logic);
                }
            }
        }
    }

    /// Whether the links to other nodes are plaintext: only then does the task send the bytes of
    /// files itself (023 FR-017).
    pub(crate) fn plaintext(&self) -> bool {
        self.tls.is_none()
    }

    /// An answer that ends with `data`, the bytes of a file the disk executor read (023 FR-017):
    /// its head and the bytes go as one frame; one that would pass the frame's limit goes as its
    /// fallback.
    pub(crate) fn send_file_bytes<L: CoreLogic>(
        &mut self,
        node: NodeId,
        send: &FileSend,
        data: &[u8],
        logic: &mut L,
    ) {
        let version = self.links.get(&node).map_or(0, |l| l.version);
        match send.frame_head(version) {
            Some(head) if data.len() == send.len as usize => {
                let mut frame = head;
                frame.extend_from_slice(data);
                self.send_bytes(node, &frame, logic);
            }
            _ => self.send_to(node, &send.fallback(), logic),
        }
    }

    /// Queues `bytes` on connection `token`: its writer sends what a turn queued in one write.
    fn queue_on(&mut self, token: u64, bytes: &[u8]) {
        let conn = self.conns.get(&token).expect("connection of an up link");
        let Some(q) = &conn.out else { return };
        q.buf.borrow_mut().extend_from_slice(bytes);
        q.wake.wake();
    }
}
