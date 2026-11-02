//! The HTTP port (019 FR-001 to FR-004): `--http-port` on the addresses of `--http-bind`, TLS as
//! the clients' port with ALPN choosing HTTP/2 or HTTP/1.1, or plain bytes with `--insecure`
//! (HTTP/2 then by prior knowledge). Each connection has a thread that reads, writes and keeps
//! its deadlines; each request runs on a thread of its own, so that a slow command or a change
//! stream does not hold up the other streams of an HTTP/2 connection. The protocols are
//! `teseladb-http`'s, without I/O; this file only moves bytes.
//!
//! Before a request runs, the port answers a CORS preflight, applies the request limits of its
//! credential (`429` with `Retry-After`) and, after it, adds the CORS fields of
//! `httpOptions.cors.allowedOrigins` to the answer.

use std::collections::BTreeMap;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use teseladb_http::cors::Cors;
use teseladb_http::http1::Request;
use teseladb_http::http2;
use teseladb_http::limits::{RateLimiter, RateLimits};
use teseladb_http::metrics::{Family, HttpMetrics};
use teseladb_http::server::{Conn, Incoming, Response, Settings};
use teseladb_wire::commands::http_parameters::HttpOptions;
use teseladb_x509::Certificate;

use crate::tls_port::{TlsPort, Transport};

/// The default `--http-port`.
pub const DEFAULT_HTTP_PORT: u16 = 27_080;
/// How long a read waits before the connection looks at its answers and deadlines.
const POLL: Duration = Duration::from_millis(20);
/// Bytes read from a socket at a time.
const READ_CHUNK: usize = 64 * 1024;
/// Most connections open at once; past it a new one is closed at once.
pub const MAX_CONNECTIONS: usize = 10_000;

/// What the HTTP port needs.
#[derive(Clone, Debug)]
pub struct HttpPortConfig {
    /// Addresses to listen on.
    pub addrs: Vec<SocketAddr>,
    /// TLS; `None` with `--insecure`.
    pub tls: Option<TlsPort>,
    /// The settings of each connection.
    pub settings: Settings,
}

/// Who sent a request.
#[derive(Clone, Debug)]
pub struct Caller {
    /// The client's address.
    pub peer: SocketAddr,
    /// The client's certificate chain over TLS, leaf first.
    pub certificates: Vec<Certificate>,
    /// Whether the request came over TLS.
    pub tls: bool,
}

impl Caller {
    /// The credential the request limits count under, until the request authenticates: the
    /// subject of the client's certificate, or its address.
    #[must_use]
    pub fn credential(&self) -> String {
        match self.certificates.first() {
            Some(leaf) => format!("x509:{}", leaf.subject.x509_user_name()),
            None => format!("ip:{}", self.peer.ip()),
        }
    }
}

/// The rest of an answer sent in pieces: each call gives the next piece, `None` ends it.
/// `cancelled` turns true when the client gave the request up.
pub type Pieces = Box<dyn FnMut(&AtomicBool) -> Option<Vec<u8>> + Send>;

/// An answer.
pub enum Reply {
    /// Whole.
    Whole(Response),
    /// Status and fields now, the body in pieces (Server-Sent Events).
    Stream {
        /// Status.
        status: u16,
        /// Fields.
        headers: Vec<(String, String)>,
        /// The body.
        pieces: Pieces,
    },
}

/// What answers the port's requests.
pub trait HttpService: Send + Sync {
    /// The answer to `request` of `caller`; `cancelled` turns true when the client gives it up.
    fn serve(&self, request: &Request, caller: &Caller, cancelled: &AtomicBool) -> Reply;

    /// The options of the port now (`httpOptions`).
    fn options(&self) -> HttpOptions;

    /// The route template of `request` for the metrics (`/v1/databases/{db}/collections`):
    /// never data of the request itself (FR-030).
    fn route(&self, request: &Request) -> String {
        let _ = request;
        UNROUTED.to_owned()
    }
}

/// The route of the metrics of a request no route takes.
pub const UNROUTED: &str = "unrouted";
/// The route of the metrics of a CORS preflight.
pub const PREFLIGHT: &str = "preflight";

/// The HTTP port's listener.
pub struct HttpListener {
    addrs: Vec<SocketAddr>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for HttpListener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpListener").field("addrs", &self.addrs).finish_non_exhaustive()
    }
}

/// What the connections of a port share.
struct Shared {
    service: Arc<dyn HttpService>,
    tls: Option<TlsPort>,
    settings: Settings,
    limiter: Mutex<RateLimiter>,
    metrics: Arc<Mutex<HttpMetrics>>,
    open: AtomicUsize,
    stop: Arc<AtomicBool>,
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl HttpListener {
    /// Listens on the addresses of `cfg` and answers with `service`, counting each request in
    /// `metrics`.
    pub fn spawn(
        cfg: HttpPortConfig,
        service: Arc<dyn HttpService>,
        metrics: Arc<Mutex<HttpMetrics>>,
    ) -> io::Result<HttpListener> {
        let stop = Arc::new(AtomicBool::new(false));
        let tls = cfg.tls.map(|t| t.with_alpn(&http2::server_alpn()));
        let shared = Arc::new(Shared {
            service,
            tls,
            settings: cfg.settings,
            limiter: Mutex::new(RateLimiter::default()),
            metrics,
            open: AtomicUsize::new(0),
            stop: stop.clone(),
        });
        let mut addrs = Vec::new();
        let mut listeners = Vec::new();
        for a in &cfg.addrs {
            let l = TcpListener::bind(a)?;
            addrs.push(l.local_addr()?);
            listeners.push(l);
        }
        let mut threads = Vec::new();
        for l in listeners {
            let shared = shared.clone();
            let t = thread::Builder::new()
                .name("teseladb-http".into())
                .spawn(move || accept(&l, &shared))?;
            threads.push(t);
        }
        Ok(HttpListener { addrs, stop, threads })
    }

    /// The addresses it listens on.
    #[must_use]
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// Stops listening; the open connections end at their next poll.
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        for a in &self.addrs {
            // The thread waits in `accept`: a connection wakes it, and it sees the stop.
            // A port that already closed needs no waking.
            let _woken = TcpStream::connect(a);
        }
        for t in self.threads {
            // A thread that panicked has nothing left to stop.
            let _joined = t.join();
        }
    }
}

fn accept(listener: &TcpListener, shared: &Arc<Shared>) {
    for stream in listener.incoming() {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else { continue };
        if shared.open.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            continue;
        }
        shared.open.fetch_add(1, Ordering::SeqCst);
        let shared2 = shared.clone();
        let spawned = thread::Builder::new().name("teseladb-http-conn".into()).spawn(move || {
            // A connection that fails only loses itself.
            let _ended = serve(stream, &shared2);
            shared2.open.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            shared.open.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// The status the metrics give a request whose client went away first (nginx's 499).
const CLIENT_GONE: u16 = 499;

/// What a request's thread tells its connection.
enum Part {
    Whole(Response),
    Head(u16, Vec<(String, String)>),
    Piece(Vec<u8>),
    End,
}

/// The requests of a connection that are running.
struct Running {
    cancelled: Arc<AtomicBool>,
    credential: String,
    route: String,
    started: Instant,
    /// The status of a stream's answer, once it started.
    streaming: Option<u16>,
}

impl Running {
    /// The request ended with `status`: the limits give its place back and the metrics count it.
    fn end(self, shared: &Shared, status: u16) {
        lock(&shared.limiter).finish(&self.credential);
        let mut m = lock(&shared.metrics);
        if self.streaming.is_some() {
            m.stream_closed(Family::Route, &self.route);
        }
        m.record(Family::Route, &self.route, status, self.started.elapsed().as_secs_f64());
    }
}

/// One connection: reads, hands requests to their threads, writes their answers.
fn serve(mut sock: TcpStream, shared: &Arc<Shared>) -> io::Result<()> {
    sock.set_read_timeout(Some(POLL))?;
    sock.set_nodelay(true)?;
    let peer = sock.peer_addr()?;
    let mut transport = match &shared.tls {
        Some(port) => Transport::new(port).ok_or_else(|| io::Error::other("no randomness"))?,
        None => Transport::Plain,
    };
    let mut conn: Option<Conn> = None;
    let (tx, rx) = mpsc::channel::<(u32, Part)>();
    let mut running: BTreeMap<u32, Running> = BTreeMap::new();
    let mut buf = vec![0u8; READ_CHUNK];
    let opened = now_ms();
    loop {
        let now = now_ms();
        let mut plain = Vec::new();
        let mut out = Vec::new();
        let peer_gone = match sock.read(&mut buf) {
            Ok(0) => true,
            Ok(n) => !transport.incoming(&buf[..n], &mut plain, &mut out),
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => false,
            Err(e) => return Err(e),
        };
        if conn.is_none() && (transport.tls_established() || transport.is_plain()) {
            let protocol = match transport.tls_established() {
                true => Some(http2::negotiated(transport.alpn())),
                false => None,
            };
            conn = Some(Conn::new(protocol, shared.settings, opened));
        }
        if let Some(c) = conn.as_mut() {
            if !plain.is_empty() {
                let caller = Caller {
                    peer,
                    certificates: transport.peer_certificates().to_vec(),
                    tls: transport.tls_established(),
                };
                for incoming in c.feed(&plain, now) {
                    start(incoming, c, &caller, shared, &tx, &mut running);
                }
            }
            while let Ok((id, part)) = rx.try_recv() {
                c.touch(now);
                deliver(c, id, part, shared, &mut running);
            }
            let bytes = c.take_output();
            if !bytes.is_empty() && !transport.outgoing(&bytes, &mut out) {
                break;
            }
        }
        if !out.is_empty() {
            sock.write_all(&out)?;
        }
        let expired =
            conn.as_ref().map_or(now > opened + teseladb_http::limits::HEADER_TIMEOUT_MS, |c| {
                running.is_empty() && now > c.deadline_ms()
            });
        let done = conn.as_ref().is_some_and(|c| c.is_closed() && running.is_empty());
        if peer_gone || expired || done || shared.stop.load(Ordering::SeqCst) {
            break;
        }
    }
    for (_, r) in std::mem::take(&mut running) {
        r.cancelled.store(true, Ordering::SeqCst);
        r.end(shared, CLIENT_GONE);
    }
    Ok(())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn header<'a>(r: &'a Request, name: &str) -> Option<&'a str> {
    r.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Starts what `incoming` asks for: a preflight answered here, a request refused by the limits,
/// or a request on its own thread.
fn start(
    incoming: Incoming,
    c: &mut Conn,
    caller: &Caller,
    shared: &Arc<Shared>,
    tx: &mpsc::Sender<(u32, Part)>,
    running: &mut BTreeMap<u32, Running>,
) {
    let (id, request) = match incoming {
        Incoming::Cancelled { id } => {
            // The request's thread sees the flag and stops; the limits count it as ended now.
            if let Some(r) = running.remove(&id) {
                r.cancelled.store(true, Ordering::SeqCst);
                r.end(shared, CLIENT_GONE);
            }
            return;
        }
        Incoming::Request { id, request } => (id, request),
    };
    let options = shared.service.options();
    let cors = Cors::new(options.allowed_origins.clone());
    let origin = header(&request, "origin");
    if Cors::is_preflight(&request.method, &request.headers) {
        let headers = cors.response_headers(origin, true);
        c.respond(id, Response { status: 204, headers, body: Vec::new() });
        lock(&shared.metrics).record(Family::Route, PREFLIGHT, 204, 0.0);
        return;
    }
    let credential = caller.credential();
    let route = shared.service.route(&request);
    let limits = RateLimits {
        concurrent: options.concurrent_requests,
        per_second: options.requests_per_second,
    };
    if let Err(limited) = lock(&shared.limiter).start(&credential, now_ms(), limits) {
        let mut r = Response::problem(429);
        r.headers.push(("Retry-After".to_owned(), limited.retry_after_secs.to_string()));
        r.headers.extend(cors.response_headers(origin, false));
        c.respond(id, r);
        lock(&shared.metrics).record(Family::Route, &route, 429, 0.0);
        return;
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let started = Instant::now();
    running.insert(
        id,
        Running { cancelled: cancelled.clone(), credential, route, started, streaming: None },
    );
    let cors_fields = cors.response_headers(origin, false);
    let (service, caller, tx) = (shared.service.clone(), caller.clone(), tx.clone());
    let spawned = thread::Builder::new().name("teseladb-http-req".into()).spawn(move || {
        let send = |part| tx.send((id, part)).is_ok();
        match service.serve(&request, &caller, &cancelled) {
            Reply::Whole(mut r) => {
                r.headers.extend(cors_fields);
                send(Part::Whole(r));
            }
            Reply::Stream { status, mut headers, mut pieces } => {
                headers.extend(cors_fields);
                if !send(Part::Head(status, headers)) {
                    return;
                }
                while let Some(p) = pieces(&cancelled) {
                    if cancelled.load(Ordering::SeqCst) || !send(Part::Piece(p)) {
                        return;
                    }
                }
                send(Part::End);
            }
        }
    });
    if spawned.is_err() {
        if let Some(r) = running.remove(&id) {
            r.end(shared, 503);
        }
        c.respond(id, Response::problem(503));
    }
}

/// Writes what a request's thread sent.
fn deliver(
    c: &mut Conn,
    id: u32,
    part: Part,
    shared: &Shared,
    running: &mut BTreeMap<u32, Running>,
) {
    let ended = match part {
        Part::Whole(r) => {
            let status = r.status;
            c.respond(id, r);
            Some(status)
        }
        Part::Head(status, headers) => {
            c.start_stream(id, status, &headers);
            if let Some(r) = running.get_mut(&id) {
                r.streaming = Some(status);
                lock(&shared.metrics).stream_opened(Family::Route, &r.route);
            }
            None
        }
        Part::Piece(p) => {
            c.stream(id, &p);
            None
        }
        Part::End => {
            c.end_stream(id);
            Some(running.get(&id).and_then(|r| r.streaming).unwrap_or(200))
        }
    };
    if let Some(status) = ended
        && let Some(r) = running.remove(&id)
    {
        r.end(shared, status);
    }
}
