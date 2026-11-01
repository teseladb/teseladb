//! Handshake of the inter-node protocol (contract `peer-frames.md` of specification 007) and the two helpers that do
//! the blocking parts away from the cores: the acceptor (accepts connections, validates the
//! peer's `Hello` and hands the stream to the core it names) and the connector (dials a node and
//! hands the established stream back to the core that asked).

use std::fmt;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use teseladb_types::NodeId;

use teseladb_tls::ClientConfig;

use crate::mailbox::MailboxSender;
use crate::peer::{
    EnrollAccepted, EnrollRequest, FrameError, Hello, JoinAccepted, PeerMsg, Versions,
    decode_frame, encode_frame,
};
use crate::secure::{Channel, PeerTls};

/// Answers a `JoinRequest` (the acceptor calls it in the connection's own thread, so it may block
/// until the cluster decides).
pub type JoinHandler = Arc<dyn Fn(JoinRequest) -> Result<JoinAccepted, String> + Send + Sync>;

/// Answers an [`EnrollRequest`] (013 FR-005), in the connection's own thread like a join.
pub type EnrollHandler = Arc<dyn Fn(EnrollRequest) -> Result<EnrollAccepted, String> + Send + Sync>;

/// What a node that joins says about itself (`JoinRequest`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinRequest {
    /// Name.
    pub name: String,
    /// Inter-node address.
    pub peer_addr: String,
    /// HTTP address.
    pub http_addr: String,
    /// Native protocol address.
    pub native_addr: String,
    /// Cores (0 = adopt the cluster's).
    pub cores: u16,
}

/// Maximum time to complete a handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Why a handshake failed (the connection is closed).
#[derive(Debug)]
pub enum HandshakeError {
    /// Socket error (including a timeout or the peer closing).
    Io(io::Error),
    /// Invalid frame.
    Frame(FrameError),
    /// The first frame was not a `Hello`.
    NotHello,
    /// The peer belongs to another cluster.
    ClusterMismatch,
    /// The peer has another number of cores (`CoreCountMismatch`).
    CoreCountMismatch {
        /// Ours.
        local: u16,
        /// The peer's.
        remote: u16,
    },
    /// The peer asked for a core that does not exist.
    BadCore(u16),
    /// The peer did not identify itself (`node_id = 0` is only for joining, not handled here).
    NoNodeId,
    /// The peer sent something else than the expected join frame.
    UnexpectedFrame,
    /// Over TLS, a peer that is not enrolling sent no certificate of the cluster's CA.
    NoCertificate,
    /// The two nodes speak no protocol version in common (007 FR-027).
    VersionMismatch {
        /// Ours.
        local: Versions,
        /// The peer's.
        remote: Versions,
    },
    /// The dialed node answered with another identity.
    UnexpectedNode {
        /// Expected.
        expected: NodeId,
        /// Received.
        got: NodeId,
    },
}

impl fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for HandshakeError {}

impl From<io::Error> for HandshakeError {
    fn from(e: io::Error) -> Self {
        HandshakeError::Io(e)
    }
}

/// Outcome of a handshake: the peer's `Hello`, the protocol version both speak and the bytes
/// already read after it.
#[derive(Debug)]
pub struct Handshaked {
    /// The peer's handshake.
    pub hello: Hello,
    /// The highest protocol version both nodes speak.
    pub version: u16,
    /// Bytes received after the `Hello` (the start of the next frames).
    pub rest: Vec<u8>,
}

fn send_hello(
    stream: &mut TcpStream,
    ch: &mut Channel,
    local: &Hello,
) -> Result<(), HandshakeError> {
    send_frame(stream, ch, &PeerMsg::Hello(local.clone()))
}

fn read_hello(stream: &mut TcpStream, ch: &mut Channel) -> Result<Handshaked, HandshakeError> {
    let (msg, rest) = read_frame(stream, ch, Vec::new())?;
    match msg {
        // The version both speak is known once the `Hello` is validated.
        PeerMsg::Hello(hello) => Ok(Handshaked { version: hello.versions.max, hello, rest }),
        _ => Err(HandshakeError::NotHello),
    }
}

/// Reads one frame (`buf` holds bytes already received); returns it and what followed it.
fn read_frame(
    stream: &mut TcpStream,
    ch: &mut Channel,
    mut buf: Vec<u8>,
) -> Result<(PeerMsg, Vec<u8>), HandshakeError> {
    loop {
        if let Some((msg, used)) = decode_frame(&buf).map_err(HandshakeError::Frame)? {
            return Ok((msg, buf.split_off(used)));
        }
        ch.recv(stream, &mut buf)?;
    }
}

fn send_frame(
    stream: &mut TcpStream,
    ch: &mut Channel,
    msg: &PeerMsg,
) -> Result<(), HandshakeError> {
    let mut buf = Vec::new();
    encode_frame(msg, false, &mut buf).map_err(HandshakeError::Frame)?;
    ch.send(stream, &buf)?;
    Ok(())
}

fn set_timeouts(stream: &TcpStream, t: Option<Duration>) -> io::Result<()> {
    stream.set_read_timeout(t)?;
    stream.set_write_timeout(t)
}

/// Checks a peer's `Hello` against ours (contract rules).
pub fn validate(local: &Hello, remote: &Hello) -> Result<u16, HandshakeError> {
    if remote.cluster_id != local.cluster_id {
        return Err(HandshakeError::ClusterMismatch);
    }
    if remote.cores != local.cores {
        return Err(HandshakeError::CoreCountMismatch { local: local.cores, remote: remote.cores });
    }
    if remote.core >= local.cores {
        return Err(HandshakeError::BadCore(remote.core));
    }
    if remote.node_id == NodeId(0) {
        return Err(HandshakeError::NoNodeId);
    }
    common_version(local.versions, remote.versions)
}

/// The highest protocol version both ranges have (007 FR-027).
fn common_version(local: Versions, remote: Versions) -> Result<u16, HandshakeError> {
    let highest = local.max.min(remote.max);
    match highest >= local.min.max(remote.min) {
        true => Ok(highest),
        false => Err(HandshakeError::VersionMismatch { local, remote }),
    }
}

/// Dialer side: sends `local` (with the core of the connection), waits for the answer and
/// validates it. The stream is left in blocking mode with the handshake timeouts cleared.
pub fn dial(stream: &mut TcpStream, local: &Hello) -> Result<Handshaked, HandshakeError> {
    dial_on(stream, &mut Channel::Plain, local)
}

/// [`dial`] over `ch`, whose TLS handshake runs first.
pub fn dial_on(
    stream: &mut TcpStream,
    ch: &mut Channel,
    local: &Hello,
) -> Result<Handshaked, HandshakeError> {
    set_timeouts(stream, Some(HANDSHAKE_TIMEOUT))?;
    ch.handshake(stream)?;
    send_hello(stream, ch, local)?;
    let got = read_hello(stream, ch)?;
    let version = validate(local, &got.hello)?;
    set_timeouts(stream, None)?;
    Ok(Handshaked { version, ..got })
}

/// Acceptor side: waits for the peer's `Hello`, validates it and answers with `local` (whose
/// `core` is set to the one the peer asked for).
pub fn answer(stream: &mut TcpStream, local: &Hello) -> Result<Handshaked, HandshakeError> {
    match answer_or_join(stream, &mut Channel::Plain, local, &AcceptorOptions::default())? {
        Some(got) => Ok(got),
        None => Err(HandshakeError::NoNodeId),
    }
}

/// What an acceptor does besides handing connections to the cores.
#[derive(Clone, Default)]
pub struct AcceptorOptions {
    /// Answers the nodes that join (`node_id = 0` and a `JoinRequest`).
    pub join: Option<JoinHandler>,
    /// Answers the nodes that enroll (`node_id = 0` and an `EnrollRequest`, 013).
    pub enroll: Option<EnrollHandler>,
    /// TLS between nodes; `None` in clear.
    pub tls: Option<PeerTls>,
}

impl fmt::Debug for AcceptorOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcceptorOptions")
            .field("join", &self.join.is_some())
            .field("enroll", &self.enroll.is_some())
            .field("tls", &self.tls.is_some())
            .finish()
    }
}

/// Over TLS every peer but one that enrolls shows a certificate of the cluster's CA (the TLS
/// layer checked its chain).
fn certified(ch: &Channel) -> Result<(), HandshakeError> {
    match ch {
        Channel::Tls(_) if ch.peer_certificates().is_empty() => Err(HandshakeError::NoCertificate),
        _ => Ok(()),
    }
}

/// Acceptor side with joins and enrollments: a `Hello` with `node_id = 0` is answered without
/// checking the cluster or the cores (the node does not know them yet), then its `JoinRequest`
/// goes to the join handler or its `EnrollRequest` to the enrollment handler, and the answer is
/// written back (`Ok(None)`). Any other `Hello` is validated and answered.
fn answer_or_join(
    stream: &mut TcpStream,
    ch: &mut Channel,
    local: &Hello,
    opts: &AcceptorOptions,
) -> Result<Option<Handshaked>, HandshakeError> {
    set_timeouts(stream, Some(HANDSHAKE_TIMEOUT))?;
    ch.handshake(stream)?;
    let got = read_hello(stream, ch)?;
    let newcomer = opts.join.is_some() || opts.enroll.is_some();
    if got.hello.node_id != NodeId(0) || !newcomer {
        let version = validate(local, &got.hello)?;
        certified(ch)?;
        send_hello(stream, ch, &Hello { core: got.hello.core, ..local.clone() })?;
        set_timeouts(stream, None)?;
        return Ok(Some(Handshaked { version, ..got }));
    }
    send_hello(stream, ch, &Hello { core: 0, ..local.clone() })?;
    // The cluster may take a while (a proposal and a configuration change).
    stream.set_write_timeout(Some(JOIN_TIMEOUT))?;
    match (read_frame(stream, ch, got.rest)?.0, &opts.join, &opts.enroll) {
        (PeerMsg::JoinRequest { name, peer_addr, http_addr, native_addr, cores }, Some(j), _) => {
            certified(ch)?;
            let req = JoinRequest { name, peer_addr, http_addr, native_addr, cores };
            send_frame(stream, ch, &PeerMsg::JoinResponse(j(req)))?;
        }
        (PeerMsg::EnrollRequest(req), _, Some(e)) => {
            send_frame(stream, ch, &PeerMsg::EnrollResponse(e(req)))?;
        }
        _ => return Err(HandshakeError::UnexpectedFrame),
    }
    Ok(None)
}

/// Longest a join may take on the cluster's side.
pub const JOIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Joining side: dials `addr` as `node_id = 0`, sends `req` and waits for the `JoinResponse`.
pub fn request_join(
    addr: &str,
    req: &JoinRequest,
) -> Result<Result<JoinAccepted, String>, HandshakeError> {
    request_join_with(addr, req, None)
}

/// [`request_join`] over TLS with `client` (`None` in clear).
pub fn request_join_with(
    addr: &str,
    req: &JoinRequest,
    client: Option<&Arc<ClientConfig>>,
) -> Result<Result<JoinAccepted, String>, HandshakeError> {
    let msg = PeerMsg::JoinRequest {
        name: req.name.clone(),
        peer_addr: req.peer_addr.clone(),
        http_addr: req.http_addr.clone(),
        native_addr: req.native_addr.clone(),
        cores: req.cores,
    };
    let hello = Hello {
        cluster_id: [0; 16],
        node_id: NodeId(0),
        core: 0,
        cores: req.cores,
        peer_addr: req.peer_addr.clone(),
        http_addr: req.http_addr.clone(),
        native_addr: req.native_addr.clone(),
        versions: Versions::OWN,
    };
    match newcomer(addr, &hello, &msg, client)? {
        PeerMsg::JoinResponse(r) => Ok(r),
        _ => Err(HandshakeError::UnexpectedFrame),
    }
}

/// Enrolling side (013 FR-005): dials `addr` over TLS with `client`, whose trust pins the
/// cluster's CA, sends `req` and waits for the `EnrollResponse`.
pub fn request_enroll(
    addr: &str,
    req: &EnrollRequest,
    client: &Arc<ClientConfig>,
) -> Result<Result<EnrollAccepted, String>, HandshakeError> {
    let hello = Hello {
        cluster_id: [0; 16],
        node_id: NodeId(0),
        core: 0,
        cores: 0,
        peer_addr: String::new(),
        http_addr: String::new(),
        native_addr: String::new(),
        versions: Versions::OWN,
    };
    match newcomer(addr, &hello, &PeerMsg::EnrollRequest(req.clone()), Some(client))? {
        PeerMsg::EnrollResponse(r) => Ok(r),
        _ => Err(HandshakeError::UnexpectedFrame),
    }
}

/// A node that is not a member yet: its `Hello`, then `msg`; returns the answer frame.
fn newcomer(
    addr: &str,
    hello: &Hello,
    msg: &PeerMsg,
    client: Option<&Arc<ClientConfig>>,
) -> Result<PeerMsg, HandshakeError> {
    let mut stream = connect_addr(addr)?;
    let mut ch = Channel::dialed(client)?;
    set_timeouts(&stream, Some(HANDSHAKE_TIMEOUT))?;
    ch.handshake(&mut stream)?;
    send_hello(&mut stream, &mut ch, hello)?;
    let got = read_hello(&mut stream, &mut ch)?;
    send_frame(&mut stream, &mut ch, msg)?;
    stream.set_read_timeout(Some(JOIN_TIMEOUT + HANDSHAKE_TIMEOUT))?;
    Ok(read_frame(&mut stream, &mut ch, got.rest)?.0)
}

/// A connection established by the acceptor, for the core named in the peer's `Hello`.
#[derive(Debug)]
pub struct Inbound {
    /// Stream (blocking mode; the core switches it to non-blocking).
    pub stream: TcpStream,
    /// Its channel, past the TLS handshake.
    pub channel: Channel,
    /// Handshake.
    pub got: Handshaked,
}

/// Running acceptor.
#[derive(Debug)]
pub struct Acceptor {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Acceptor {
    /// Accepts on `listener` in a thread. Every connection is handshaked in its own short-lived
    /// thread (a slow peer does not hold up the others) and handed to `cores[hello.core]`, wrapped
    /// with `wrap`. Failed handshakes are closed, and so are joins (`node_id = 0`).
    pub fn spawn<M: Send + 'static>(
        listener: TcpListener,
        local: Hello,
        cores: Vec<MailboxSender<M>>,
        wrap: fn(Inbound) -> M,
    ) -> io::Result<Acceptor> {
        Acceptor::spawn_with_join(listener, local, cores, wrap, None)
    }

    /// Like [`Acceptor::spawn`]; a peer whose `Hello` has `node_id = 0` is a node that joins: its
    /// `JoinRequest` goes to `join` and the answer is sent back on the same connection, which is
    /// then closed.
    pub fn spawn_with_join<M: Send + 'static>(
        listener: TcpListener,
        local: Hello,
        cores: Vec<MailboxSender<M>>,
        wrap: fn(Inbound) -> M,
        join: Option<JoinHandler>,
    ) -> io::Result<Acceptor> {
        let opts = AcceptorOptions { join, ..AcceptorOptions::default() };
        Acceptor::spawn_with(listener, local, cores, wrap, opts)
    }

    /// Like [`Acceptor::spawn`], with joins, enrollments and TLS as `opts` says.
    pub fn spawn_with<M: Send + 'static>(
        listener: TcpListener,
        local: Hello,
        cores: Vec<MailboxSender<M>>,
        wrap: fn(Inbound) -> M,
        opts: AcceptorOptions,
    ) -> io::Result<Acceptor> {
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let handle = thread::Builder::new().name("peer-accept".into()).spawn(move || {
            // A failed accept (for example, the peer reset before it was taken) only loses that
            // connection.
            for mut stream in listener.incoming().flatten() {
                if flag.load(Ordering::Acquire) {
                    break;
                }
                let _ = stream.set_nodelay(true);
                let local = local.clone();
                // Each handshake thread sends through senders of its own (one ring each).
                let cores = cores.clone();
                let opts = opts.clone();
                let _ = thread::Builder::new().name("peer-handshake".into()).spawn(move || {
                    // A join or enrollment that was answered, or a failed handshake, closes the
                    // connection.
                    let Ok(mut channel) = Channel::accepted(opts.tls.as_ref()) else { return };
                    if let Ok(Some(got)) = answer_or_join(&mut stream, &mut channel, &local, &opts)
                    {
                        let core = usize::from(got.hello.core);
                        let _ = cores[core].send_waiting(wrap(Inbound { stream, channel, got }));
                    }
                });
            }
        })?;
        Ok(Acceptor { addr, stop, handle: Some(handle) })
    }

    /// Address it listens on.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stops accepting (waits for the thread).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        if let Some(h) = self.handle.take() {
            self.stop.store(true, Ordering::Release);
            // Unblocks the pending `accept`.
            let _ = TcpStream::connect(self.addr);
            let _ = h.join();
        }
    }
}

impl Drop for Acceptor {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Result of a dial, delivered to the core that asked for it.
#[derive(Debug)]
pub struct Outbound {
    /// Dialed node.
    pub node: NodeId,
    /// Established stream, its channel and the handshake, or the error.
    pub result: Result<(TcpStream, Channel, Handshaked), HandshakeError>,
}

/// Dials `node` at `addr` in a short-lived thread and sends the outcome to `reply`, wrapped with
/// `wrap`.
pub fn connect<M: Send + 'static>(
    node: NodeId,
    addr: String,
    local: Hello,
    reply: MailboxSender<M>,
    wrap: fn(Outbound) -> M,
) {
    connect_with(node, addr, local, None, reply, wrap);
}

/// [`connect`] over TLS with `client` (`None` in clear).
pub fn connect_with<M: Send + 'static>(
    node: NodeId,
    addr: String,
    local: Hello,
    client: Option<Arc<ClientConfig>>,
    reply: MailboxSender<M>,
    wrap: fn(Outbound) -> M,
) {
    let spawned = thread::Builder::new().name("peer-dial".into()).spawn(move || {
        let result = dial_addr(&addr, &local, client.as_ref()).and_then(|(stream, ch, got)| {
            if got.hello.node_id == node {
                Ok((stream, ch, got))
            } else {
                Err(HandshakeError::UnexpectedNode { expected: node, got: got.hello.node_id })
            }
        });
        let _ = reply.send_waiting(wrap(Outbound { node, result }));
    });
    // Without a thread there is no dial: the core sees nothing and retries after its backoff.
    drop(spawned);
}

fn dial_addr(
    addr: &str,
    local: &Hello,
    client: Option<&Arc<ClientConfig>>,
) -> Result<(TcpStream, Channel, Handshaked), HandshakeError> {
    let mut stream = connect_addr(addr)?;
    let mut ch = Channel::dialed(client)?;
    let got = dial_on(&mut stream, &mut ch, local)?;
    Ok((stream, ch, got))
}

/// Resolves `addr` and connects to its first address within the handshake timeout.
fn connect_addr(addr: &str) -> io::Result<TcpStream> {
    let no_address = io::Error::new(io::ErrorKind::NotFound, "address without results");
    let target = addr.to_socket_addrs()?.next().ok_or(no_address)?;
    let stream = TcpStream::connect_timeout(&target, HANDSHAKE_TIMEOUT)?;
    stream.set_nodelay(true)?;
    Ok(stream)
}
