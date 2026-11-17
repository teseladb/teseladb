//! TLS over the engine's connected sockets (023 FR-016): the sans-I/O state machine of
//! `teseladb-tls` between the socket's raw source and sink, given back as a [`ConnectedSocket`]
//! whose input and output streams carry plaintext. The connections between nodes (013 FR-006),
//! the clients' port (013 FR-007, FR-034), the HTTP and administration ports and the outbound
//! clients use it.
//!
//! [`server`] and [`client`] run the handshake (TLS 1.2 and 1.3, as the configurations allow:
//! ALPN, SNI, client certificates, OCSP stapling, session tickets) and resolve once it is done, so
//! the caller sees the peer's certificates and the agreed protocol before the first byte;
//! [`accept`] adds the first-byte choice of `allowTLS` and `preferTLS`. The [`TlsSession`] of a
//! connection answers what the handshake agreed and hands the client its session tickets.
//!
//! Bytes are copied only where `teseladb-tls` needs them: the raw bytes read go into its record
//! buffer, and the plaintext it decrypts and the records it seals are handed to the streams in
//! the vectors it made (no further copy). A packet of several fragments that fits one record is
//! joined first, so a small header and its body leave in one record and one send. Records leave in
//! the order they were made: every send takes what the connection holds under one semaphore.
//!
//! A peer that closes the socket without `close_notify` ends the input like one that sent it, as
//! the blocking implementation did.
// The engine's buffers and fragments are raw (a buffer handed over with its deleter, a fragment
// read in place); every `unsafe` block states why it is sound.
#![allow(unsafe_code)]

use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::deleter::make_object_deleter;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, make_exception_future, make_ready_future,
};
use teseladb::engine::core::iostream::{DataSink, DataSinkImpl, DataSource, DataSourceImpl};
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb::engine::core::semaphore::Semaphore;
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::net::api::{
    ConnectedSocket, ConnectedSocketInputStreamConfig, KeepaliveParams,
};
use teseladb::engine::net::packet::Packet;
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb::engine::net::stack::ConnectedSocketImpl;
use teseladb_tls::{ClientConfig, Connection, ENTROPY_LEN, ServerConfig, Ticket, Version};
use teseladb_x509::Certificate;

use super::within;

/// The first byte of a TLS handshake record.
pub const TLS_HANDSHAKE: u8 = 22;

/// The most plaintext one TLS record carries (RFC 8446, 5.1).
const MAX_RECORD: usize = 16 << 10;

/// What the raw source of a TLS socket reads at a time: room for a few whole records, adapting
/// down to the minimum for connections that only trade small messages.
const RAW_INPUT: ConnectedSocketInputStreamConfig = ConnectedSocketInputStreamConfig {
    buffer_size: 4 * ((16 << 10) + 512),
    min_buffer_size: 512,
    max_buffer_size: 128 << 10,
};

/// The raw sink's buffer: unused, the sink is detached before any write.
const RAW_OUTPUT: usize = 8 << 10;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn entropy() -> Result<[u8; ENTROPY_LEN], ExceptionPtr> {
    let mut e = [0u8; ENTROPY_LEN];
    teseladb_sys::fill_random(&mut e)?;
    Ok(e)
}

fn tls_error(e: teseladb_tls::Error) -> ExceptionPtr {
    ExceptionPtr::from(io::Error::new(io::ErrorKind::InvalidData, format!("TLS: {e:?}")))
}

fn closed_error(what: &str) -> ExceptionPtr {
    ExceptionPtr::from(io::Error::new(io::ErrorKind::BrokenPipe, what.to_string()))
}

/// The bytes of `v` as an engine buffer, without copying them: the buffer's deleter owns the
/// vector.
fn owned(mut v: Vec<u8>) -> TemporaryBuffer<u8> {
    let (ptr, len) = (v.as_mut_ptr(), v.len());
    // SAFETY: the deleter owns the vector, whose heap block does not move when the vector moves
    // into it; the block holds `len` initialized bytes and lives as long as the deleter.
    unsafe { TemporaryBuffer::with_deleter(ptr, len, make_object_deleter(v)) }
}

/// The state of one TLS connection, shared by its socket, its source, its sink and its
/// [`TlsSession`].
struct Shared {
    tls: RefCell<Tls>,
    /// The socket under TLS: its options, addresses and shutdowns.
    sock: Rc<RefCell<ConnectedSocket>>,
}

struct Tls {
    conn: Connection,
    raw_in: DataSource,
    raw_out: DataSink,
    /// One send at a time, in the order the records were made.
    out_sem: Semaphore,
    /// The socket's input ended.
    raw_eof: bool,
    /// The first error; every later operation fails with it.
    error: Option<ExceptionPtr>,
}

/// The raw streams of a socket under TLS.
struct Raw {
    sock: Rc<RefCell<ConnectedSocket>>,
    input: DataSource,
    output: DataSink,
}

impl Raw {
    fn of(mut sock: ConnectedSocket) -> Result<Raw, ExceptionPtr> {
        let input = sock.input(RAW_INPUT).detach()?;
        let output = sock.output(RAW_OUTPUT).detach()?;
        Ok(Raw { sock: Rc::new(RefCell::new(sock)), input, output })
    }
}

impl Shared {
    fn new(raw: Raw, conn: Connection) -> Rc<Shared> {
        let tls = Tls {
            conn,
            raw_in: raw.input,
            raw_out: raw.output,
            out_sem: Semaphore::new(1),
            raw_eof: false,
            error: None,
        };
        Rc::new(Shared { tls: RefCell::new(tls), sock: raw.sock })
    }
}

/// Stops `sock` both ways, so that a pending read or write ends.
fn abort(sock: &RefCell<ConnectedSocket>) {
    let mut s = sock.borrow_mut();
    s.shutdown_input();
    s.shutdown_output();
}

/// Sends what the connection holds for the peer, after every send before it.
fn flush(st: &Rc<Shared>) -> Future<()> {
    let wait = st.tls.borrow_mut().out_sem.wait(1);
    let st = Rc::clone(st);
    wait.then(move |()| {
        let put = {
            let mut t = st.tls.borrow_mut();
            let out = t.conn.take_output();
            (!out.is_empty()).then(|| t.raw_out.put(Packet::from_buffer(owned(out))))
        };
        let put = put.unwrap_or_else(|| make_ready_future(()));
        put.finally(move || st.tls.borrow_mut().out_sem.signal(1))
    })
}

/// [`flush`] with nobody waiting: what the connection answers on its own (alerts, key updates).
/// A failure is kept for the next operation.
fn flush_in_background(st: &Rc<Shared>) {
    let keep = Rc::clone(st);
    run_in_background(flush(st).handle_exception(move |e: ExceptionPtr| {
        keep.tls.borrow_mut().error.get_or_insert(e);
        make_ready_future(())
    }));
}

/// Gives the connection bytes read from the socket; an empty buffer is the end of the input.
/// Returns whether it has something to send back.
fn ingest(st: &Shared, buf: &TemporaryBuffer<u8>) -> bool {
    let mut t = st.tls.borrow_mut();
    if buf.empty() {
        t.raw_eof = true;
        return false;
    }
    if let Err(e) = t.conn.feed(buf.as_slice()) {
        t.error.get_or_insert(tls_error(e));
    }
    t.conn.wants_write()
}

/// The plaintext the connection holds, the error it failed with or the end of the input, if any
/// of them.
fn ready_input(st: &Shared) -> Option<Result<TemporaryBuffer<u8>, ExceptionPtr>> {
    let mut t = st.tls.borrow_mut();
    let plain = t.conn.read();
    if !plain.is_empty() {
        return Some(Ok(owned(plain)));
    }
    if let Some(e) = &t.error {
        return Some(Err(e.clone()));
    }
    (t.conn.peer_closed() || t.raw_eof).then(|| Ok(TemporaryBuffer::empty_buffer()))
}

/// The next plaintext from the peer; empty at its end.
fn read(st: Rc<Shared>) -> Future<TemporaryBuffer<u8>> {
    match ready_input(&st) {
        Some(Ok(b)) => return make_ready_future(b),
        Some(Err(e)) => return make_exception_future(e),
        None => {}
    }
    make_coroutine(async move {
        loop {
            let raw = st.tls.borrow_mut().raw_in.get();
            let buf = raw.await.inspect_err(|e| {
                st.tls.borrow_mut().error.get_or_insert(e.clone());
            })?;
            if ingest(&st, &buf) {
                flush_in_background(&st);
            }
            if let Some(r) = ready_input(&st) {
                return r;
            }
        }
    })
}

/// Seals `p` and sends it.
fn write(st: &Rc<Shared>, mut p: Packet) -> Future<()> {
    {
        let mut t = st.tls.borrow_mut();
        if let Some(e) = &t.error {
            return make_exception_future(e.clone());
        }
        if p.nr_frags() > 1 && p.len() as usize <= MAX_RECORD {
            p.linearize_all();
        }
        for frag in p.fragments().iter() {
            // SAFETY: the packet owns its fragments and is alive for the whole loop.
            let bytes = unsafe { frag.as_slice() };
            if let Err(e) = t.conn.write(bytes) {
                let e = match e {
                    teseladb_tls::Error::Closed => closed_error("the TLS connection is closed"),
                    other => tls_error(other),
                };
                return make_exception_future(e);
            }
        }
    }
    flush(st)
}

/// Sends `close_notify`; nothing more can be written.
fn close_notify(st: &Rc<Shared>) -> Future<()> {
    let sent = st.tls.borrow_mut().conn.close();
    match sent {
        Ok(()) => flush(st),
        Err(e) => make_exception_future(tls_error(e)),
    }
}

/// Runs the handshake, with `first` already read from the socket.
async fn handshake(
    st: &Rc<Shared>,
    first: Option<TemporaryBuffer<u8>>,
) -> Result<(), ExceptionPtr> {
    if let Some(b) = first {
        ingest(st, &b);
    }
    loop {
        flush(st).await?;
        {
            let t = st.tls.borrow();
            if let Some(e) = &t.error {
                return Err(e.clone());
            }
            if !t.conn.is_handshaking() {
                return Ok(());
            }
        }
        let raw = st.tls.borrow_mut().raw_in.get();
        let buf = raw.await?;
        if buf.empty() {
            return Err(ExceptionPtr::from(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "the connection closed during the TLS handshake",
            )));
        }
        ingest(st, &buf);
    }
}

/// `f` with the deadline `timeout`: when it passes, `sock` is shut down, which ends whatever `f`
/// waits for, and the result fails with [`io::ErrorKind::TimedOut`].
fn bounded<T: 'static>(
    sock: &Rc<RefCell<ConnectedSocket>>,
    timeout: Option<Duration>,
    f: Future<T>,
) -> Future<T> {
    let Some(timeout) = timeout else { return f };
    let sock = Rc::clone(sock);
    within(timeout, f, "the TLS handshake timed out".into(), move || abort(&sock))
}

/// What the handshake of one connection agreed, and the client's session tickets. The clones
/// share the connection.
#[derive(Clone)]
pub struct TlsSession {
    st: Rc<Shared>,
}

impl std::fmt::Debug for TlsSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsSession").field("version", &self.version()).finish_non_exhaustive()
    }
}

impl TlsSession {
    /// `f` of the connection, for what the accessors below do not answer.
    pub fn with_connection<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        f(&self.st.tls.borrow().conn)
    }

    /// The peer's validated chain, leaf first; empty when it sent none.
    pub fn peer_certificates(&self) -> Vec<Certificate> {
        self.with_connection(|c| c.peer_certificates().to_vec())
    }

    /// The protocol ALPN agreed, if any.
    pub fn alpn(&self) -> Option<Vec<u8>> {
        self.with_connection(|c| c.alpn_protocol().map(<[u8]>::to_vec))
    }

    /// The server name: the one the client asked for (server side) or connected to (client).
    pub fn server_name(&self) -> Option<String> {
        self.with_connection(|c| c.server_name().map(str::to_string))
    }

    /// The version agreed.
    pub fn version(&self) -> Option<Version> {
        self.with_connection(Connection::version)
    }

    /// Whether the session was resumed with a ticket.
    pub fn is_resumed(&self) -> bool {
        self.with_connection(Connection::is_resumed)
    }

    /// The session tickets the server sent so far (client side); they arrive with the reads.
    pub fn take_tickets(&self) -> Vec<Ticket> {
        self.st.tls.borrow_mut().conn.take_tickets()
    }
}

/// A connection whose TLS handshake is done: `socket` carries plaintext.
pub struct TlsSocket {
    /// The socket: its input and output streams carry plaintext; closing its output sends
    /// `close_notify`.
    pub socket: ConnectedSocket,
    /// What the handshake agreed.
    pub session: TlsSession,
}

impl std::fmt::Debug for TlsSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsSocket").field("session", &self.session).finish_non_exhaustive()
    }
}

/// Runs the handshake of `conn` over `raw`, `first` already read from it.
fn established(
    raw: Raw,
    conn: Connection,
    first: Option<TemporaryBuffer<u8>>,
    timeout: Option<Duration>,
) -> Future<TlsSocket> {
    let st = Shared::new(raw, conn);
    let hs = {
        let st = Rc::clone(&st);
        make_coroutine(async move { handshake(&st, first).await })
    };
    let done = Rc::clone(&st);
    bounded(&st.sock, timeout, hs).then(move |()| {
        let socket = ConnectedSocket::from_impl(Box::new(TlsSocketImpl { st: Rc::clone(&done) }));
        make_ready_future(TlsSocket { socket, session: TlsSession { st: done } })
    })
}

/// The server side of a TLS connection with `config`, before its handshake (the sans-I/O state
/// machine, for a transport that is not a [`ConnectedSocket`]: the clients' port on AF_XDP).
pub(crate) fn server_conn(config: Arc<ServerConfig>) -> Result<Connection, ExceptionPtr> {
    Ok(Connection::server(config, now_ms(), &entropy()?))
}

/// Accepts a TLS client over `sock` with `config`: resolves once the handshake is done. With
/// `timeout`, a handshake that takes longer fails with [`io::ErrorKind::TimedOut`] and the socket
/// is shut down.
pub fn server(
    sock: ConnectedSocket,
    config: Arc<ServerConfig>,
    timeout: Option<Duration>,
) -> Future<TlsSocket> {
    match server_conn(config).and_then(|c| Ok((c, Raw::of(sock)?))) {
        Ok((conn, raw)) => established(raw, conn, None, timeout),
        Err(e) => make_exception_future(e),
    }
}

/// Connects as a TLS client over `sock` with `config`. `server_name` is sent by SNI and checked
/// against the server's certificate (none: any certificate of the trusted anchors, as between
/// nodes); `resume` offers a ticket of an earlier session. Resolves once the handshake is done.
pub fn client(
    sock: ConnectedSocket,
    config: Arc<ClientConfig>,
    server_name: Option<&str>,
    resume: Option<Ticket>,
    timeout: Option<Duration>,
) -> Future<TlsSocket> {
    let conn = entropy().and_then(|e| {
        Connection::client(config, server_name, now_ms(), &e, resume).map_err(tls_error)
    });
    match conn.and_then(|c| Ok((c, Raw::of(sock)?))) {
        Ok((conn, raw)) => established(raw, conn, None, timeout),
        Err(e) => make_exception_future(e),
    }
}

/// How a port takes its connections (`--tlsMode`).
#[derive(Clone, Debug)]
pub enum Sniff {
    /// Plain bytes only (`disabled`).
    Plain,
    /// TLS only (`requireTLS`).
    Tls(Arc<ServerConfig>),
    /// TLS when the first byte opens a handshake record, plain bytes otherwise (`allowTLS`,
    /// `preferTLS`).
    Either(Arc<ServerConfig>),
}

/// A connection a port accepted, in clear or with its TLS handshake done.
pub struct Accepted {
    /// The socket: plaintext both ways.
    pub socket: ConnectedSocket,
    /// The TLS session; `None` in clear.
    pub tls: Option<TlsSession>,
}

impl std::fmt::Debug for Accepted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Accepted").field("tls", &self.tls).finish_non_exhaustive()
    }
}

/// Takes the connection `sock` the way `sniff` says. With [`Sniff::Either`] the first bytes the
/// client sends choose: TLS when they open a handshake record, plain otherwise, and the bytes
/// already read are the first the plain socket gives. `timeout` bounds the wait for the first
/// bytes and the handshake.
pub fn accept(sock: ConnectedSocket, sniff: Sniff, timeout: Option<Duration>) -> Future<Accepted> {
    match sniff {
        Sniff::Plain => make_ready_future(Accepted { socket: sock, tls: None }),
        Sniff::Tls(config) => server(sock, config, timeout).then(Accepted::of),
        Sniff::Either(config) => match Raw::of(sock) {
            Ok(raw) => sniffed(raw, config, timeout),
            Err(e) => make_exception_future(e),
        },
    }
}

impl Accepted {
    #[expect(clippy::unnecessary_wraps, reason = "the continuation of a future returns one")]
    fn of(t: TlsSocket) -> Result<Accepted, ExceptionPtr> {
        Ok(Accepted { socket: t.socket, tls: Some(t.session) })
    }
}

fn sniffed(raw: Raw, config: Arc<ServerConfig>, timeout: Option<Duration>) -> Future<Accepted> {
    let sock = Rc::clone(&raw.sock);
    // The raw streams stay alive until the first read ends, also past a deadline.
    let first = make_coroutine(async move {
        let mut raw = raw;
        let first = raw.input.get().await?;
        Ok((raw, first))
    });
    let first = match timeout {
        None => first,
        Some(t) => within(t, first, "no bytes before the deadline".into(), move || abort(&sock)),
    };
    let start = std::time::Instant::now();
    first.then(move |(raw, first): (Raw, TemporaryBuffer<u8>)| {
        if first.as_slice().first() != Some(&TLS_HANDSHAKE) {
            let plain = PlainSocketImpl {
                sock: raw.sock,
                prefix: Some(first),
                raw_in: Some(raw.input),
                raw_out: Some(raw.output),
            };
            let socket = ConnectedSocket::from_impl(Box::new(plain));
            return make_ready_future(Accepted { socket, tls: None });
        }
        let left = timeout.map(|t| t.saturating_sub(start.elapsed()));
        match server_conn(config) {
            Ok(conn) => established(raw, conn, Some(first), left).then(Accepted::of),
            Err(e) => make_exception_future(e),
        }
    })
}

/// The plaintext source of a TLS socket.
struct TlsSource {
    st: Rc<Shared>,
}

impl DataSourceImpl for TlsSource {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        read(Rc::clone(&self.st))
    }
}

/// The plaintext sink of a TLS socket.
struct TlsSink {
    st: Rc<Shared>,
}

impl DataSinkImpl for TlsSink {
    fn put(&mut self, data: Packet) -> Future<()> {
        write(&self.st, data)
    }

    fn flush(&mut self) -> Future<()> {
        let wait = self.st.tls.borrow_mut().out_sem.wait(1);
        let st = Rc::clone(&self.st);
        wait.then(move |()| {
            let f = st.tls.borrow_mut().raw_out.flush();
            f.finally(move || st.tls.borrow_mut().out_sem.signal(1))
        })
    }

    /// Sends `close_notify`, then closes the socket's output.
    fn close(&mut self) -> Future<()> {
        let st = Rc::clone(&self.st);
        close_notify(&self.st).then_wrapped(move |f: Future<()>| {
            let sent = f.get();
            let closed = st.tls.borrow_mut().raw_out.close();
            closed.then(move |()| match sent {
                Ok(()) => make_ready_future(()),
                Err(e) => make_exception_future(e),
            })
        })
    }

    fn buffer_size(&self) -> usize {
        MAX_RECORD
    }
}

/// A TLS socket: plaintext streams over the TLS connection, everything else from the socket
/// under it.
struct TlsSocketImpl {
    st: Rc<Shared>,
}

impl ConnectedSocketImpl for TlsSocketImpl {
    fn source(&mut self) -> DataSource {
        DataSource::new(Box::new(TlsSource { st: Rc::clone(&self.st) }))
    }

    fn sink(&mut self) -> DataSink {
        DataSink::new(Box::new(TlsSink { st: Rc::clone(&self.st) }))
    }

    fn shutdown_input(&mut self) {
        self.st.sock.borrow_mut().shutdown_input();
    }

    /// Sends `close_notify` in the background, then shuts the socket's output down.
    fn shutdown_output(&mut self) {
        let st = Rc::clone(&self.st);
        run_in_background(close_notify(&self.st).then_wrapped(move |mut f: Future<()>| {
            f.ignore_ready_future();
            st.sock.borrow_mut().shutdown_output();
            make_ready_future(())
        }));
    }

    fn set_nodelay(&mut self, nodelay: bool) -> Result<(), ExceptionPtr> {
        self.st.sock.borrow_mut().set_nodelay(nodelay)
    }

    fn get_nodelay(&self) -> Result<bool, ExceptionPtr> {
        self.st.sock.borrow().get_nodelay()
    }

    fn set_keepalive(&mut self, keepalive: bool) -> Result<(), ExceptionPtr> {
        self.st.sock.borrow_mut().set_keepalive(keepalive)
    }

    fn get_keepalive(&self) -> Result<bool, ExceptionPtr> {
        self.st.sock.borrow().get_keepalive()
    }

    fn set_keepalive_parameters(&mut self, p: &KeepaliveParams) -> Result<(), ExceptionPtr> {
        self.st.sock.borrow_mut().set_keepalive_parameters(p)
    }

    fn get_keepalive_parameters(&self) -> Result<KeepaliveParams, ExceptionPtr> {
        self.st.sock.borrow().get_keepalive_parameters()
    }

    unsafe fn set_sockopt(
        &mut self,
        level: i32,
        optname: i32,
        data: *const u8,
        len: usize,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.st.sock.borrow_mut().set_sockopt(level, optname, data, len) }
    }

    unsafe fn get_sockopt(
        &self,
        level: i32,
        optname: i32,
        data: *mut u8,
        len: usize,
    ) -> Result<i32, ExceptionPtr> {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.st.sock.borrow().get_sockopt(level, optname, data, len) }
    }

    fn local_address(&self) -> SocketAddress {
        self.st.sock.borrow().local_address()
    }

    fn remote_address(&self) -> SocketAddress {
        self.st.sock.borrow().remote_address()
    }

    fn wait_input_shutdown(&mut self) -> Future<()> {
        self.st.sock.borrow_mut().wait_input_shutdown()
    }
}

/// The source of a plain connection of an `allowTLS` or `preferTLS` port: the bytes read to
/// choose first, then the socket's.
struct PrefixSource {
    prefix: Option<TemporaryBuffer<u8>>,
    raw: DataSource,
}

impl DataSourceImpl for PrefixSource {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        match self.prefix.take() {
            Some(b) => make_ready_future(b),
            None => self.raw.get(),
        }
    }

    fn close(&mut self) -> Future<()> {
        self.raw.close()
    }
}

/// A plain connection of an `allowTLS` or `preferTLS` port, whose first bytes were read to
/// choose.
struct PlainSocketImpl {
    sock: Rc<RefCell<ConnectedSocket>>,
    prefix: Option<TemporaryBuffer<u8>>,
    raw_in: Option<DataSource>,
    raw_out: Option<DataSink>,
}

impl ConnectedSocketImpl for PlainSocketImpl {
    /// The bytes read to choose, then the socket's; once.
    fn source(&mut self) -> DataSource {
        match self.raw_in.take() {
            Some(raw) => {
                DataSource::new(Box::new(PrefixSource { prefix: self.prefix.take(), raw }))
            }
            None => self
                .sock
                .borrow_mut()
                .input(ConnectedSocketInputStreamConfig::default())
                .detach()
                .unwrap_or_default(),
        }
    }

    fn sink(&mut self) -> DataSink {
        match self.raw_out.take() {
            Some(raw) => raw,
            None => self.sock.borrow_mut().output(RAW_OUTPUT).detach().unwrap_or_default(),
        }
    }

    fn shutdown_input(&mut self) {
        self.sock.borrow_mut().shutdown_input();
    }

    fn shutdown_output(&mut self) {
        self.sock.borrow_mut().shutdown_output();
    }

    fn set_nodelay(&mut self, nodelay: bool) -> Result<(), ExceptionPtr> {
        self.sock.borrow_mut().set_nodelay(nodelay)
    }

    fn get_nodelay(&self) -> Result<bool, ExceptionPtr> {
        self.sock.borrow().get_nodelay()
    }

    fn set_keepalive(&mut self, keepalive: bool) -> Result<(), ExceptionPtr> {
        self.sock.borrow_mut().set_keepalive(keepalive)
    }

    fn get_keepalive(&self) -> Result<bool, ExceptionPtr> {
        self.sock.borrow().get_keepalive()
    }

    fn set_keepalive_parameters(&mut self, p: &KeepaliveParams) -> Result<(), ExceptionPtr> {
        self.sock.borrow_mut().set_keepalive_parameters(p)
    }

    fn get_keepalive_parameters(&self) -> Result<KeepaliveParams, ExceptionPtr> {
        self.sock.borrow().get_keepalive_parameters()
    }

    unsafe fn set_sockopt(
        &mut self,
        level: i32,
        optname: i32,
        data: *const u8,
        len: usize,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.sock.borrow_mut().set_sockopt(level, optname, data, len) }
    }

    unsafe fn get_sockopt(
        &self,
        level: i32,
        optname: i32,
        data: *mut u8,
        len: usize,
    ) -> Result<i32, ExceptionPtr> {
        // SAFETY: the caller's contract, passed on.
        unsafe { self.sock.borrow().get_sockopt(level, optname, data, len) }
    }

    fn local_address(&self) -> SocketAddress {
        self.sock.borrow().local_address()
    }

    fn remote_address(&self) -> SocketAddress {
        self.sock.borrow().remote_address()
    }

    fn wait_input_shutdown(&mut self) -> Future<()> {
        self.sock.borrow_mut().wait_input_shutdown()
    }
}
