//! The HTTP port on the engine's shards (023 FR-022, 019 FR-001 to FR-004): each connection is a
//! task of the shard that took it and each request a task of that shard, with HTTP/1.1 and
//! HTTP/2 of `teseladb-http` unchanged. TLS is the engine's TLS stream with ALPN choosing HTTP/2
//! or HTTP/1.1; plain bytes (`--insecure`) choose by prior knowledge. The deadlines (the header
//! timeout and the idle one) run on the engine's timers.
//!
//! A connection runs three tasks: the reader feeds the protocol and starts the requests, the
//! writer sends what the protocol has to send, and the deadline closes the connection when it
//! has nothing to do for too long. Before a request runs, the port answers a CORS preflight and
//! applies the request limits of its credential (`429` with `Retry-After`); after it, the CORS
//! fields of `httpOptions.cors.allowedOrigins` go with the answer. The limits and the metrics are
//! the node's, shared by its shards.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use teseladb::engine::core::condition_variable::ConditionVariable;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future, make_exception_future};
use teseladb::engine::core::iostream::DataSink;
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb::engine::core::timer::{Clock, SteadyClockType, SteadyTimePoint};
use teseladb::engine::net::api::ConnectedSocketInputStreamConfig;
use teseladb_http::cors::Cors;
use teseladb_http::http1::Request;
use teseladb_http::http2;
use teseladb_http::limits::{HEADER_TIMEOUT_MS, RateLimiter, RateLimits};
use teseladb_http::metrics::HttpMetrics;
use teseladb_http::server::{Conn, Incoming, Response, Settings};
use teseladb_wire::commands::http_parameters::HttpOptions;

use super::{Caller, HttpPortConfig, MAX_CONNECTIONS, PREFLIGHT, ROUTES, UNROUTED};
use crate::shard_port::{Link, Port, PortConfig, packet};
use crate::shard_tls::{Policy, ShardTls};

/// A request's cancellation: set when its client gives it up (the connection closed, or an
/// HTTP/2 `RST_STREAM`). The clones share it; [`Cancel::flag`] hands it to work done off the
/// shard.
#[derive(Clone, Debug, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// Whether the client gave the request up.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    /// The flag itself.
    #[must_use]
    pub fn flag(&self) -> Arc<AtomicBool> {
        self.0.clone()
    }

    fn set(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// The rest of an answer sent in pieces: each call gives the next piece, `None` ends it.
pub type Pieces = Box<dyn FnMut(&Cancel) -> Future<Option<Vec<u8>>>>;

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

impl std::fmt::Debug for Reply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reply::Whole(r) => f.debug_tuple("Whole").field(&r.status).finish(),
            Reply::Stream { status, .. } => f.debug_tuple("Stream").field(status).finish(),
        }
    }
}

/// What answers the port's requests on one shard.
pub trait HttpService {
    /// The answer to `request` of `caller`; `cancel` turns true when the client gives it up.
    fn serve(&self, request: Request, caller: Caller, cancel: Cancel) -> Future<Reply>;

    /// The options of the port now (`httpOptions`).
    fn options(&self) -> HttpOptions;

    /// The route template of `request` for the metrics (`/v1/databases/{db}/collections`):
    /// never data of the request itself (019 FR-030).
    fn route(&self, request: &Request) -> String {
        let _ = request;
        UNROUTED.to_owned()
    }
}

/// Makes a shard's service; it runs on that shard.
pub type MakeService = Arc<dyn Fn() -> Rc<dyn HttpService> + Send + Sync>;

/// The HTTP port running on every shard.
#[derive(Clone, Debug)]
pub struct HttpPort {
    port: Port,
    tls: ShardTls,
}

impl HttpPort {
    /// Listens on the addresses of `cfg` on every shard and answers with the service `make`
    /// gives each shard, counting each request in `metrics`. Runs on shard 0.
    ///
    /// # Errors
    ///
    /// A failed future when an address does not listen.
    pub fn start(
        cfg: HttpPortConfig,
        make: MakeService,
        metrics: Arc<Mutex<HttpMetrics>>,
    ) -> Future<HttpPort> {
        let tls = match &cfg.tls {
            Some(port) => ShardTls::new(port, Some(http2::server_alpn()), Policy::ByMode),
            None => ShardTls::plain(),
        };
        let limiter = Arc::new(Mutex::new(RateLimiter::default()));
        let settings = cfg.settings;
        let handler = Arc::new(move || {
            let shared = Rc::new(Shared {
                service: make(),
                settings,
                limiter: limiter.clone(),
                metrics: metrics.clone(),
            });
            let h: crate::shard_port::Handler =
                Rc::new(move |link: Link| serve(link, shared.clone()));
            h
        });
        let port_cfg = PortConfig {
            name: crate::node_ports::HTTP,
            addrs: cfg.addrs,
            tls: tls.clone(),
            handshake: Duration::from_millis(HEADER_TIMEOUT_MS),
            max_connections: MAX_CONNECTIONS,
        };
        Port::start(port_cfg, handler)
            .then(move |port| -> Result<HttpPort, ExceptionPtr> { Ok(HttpPort { port, tls }) })
    }

    /// The addresses it listens on.
    #[must_use]
    pub fn addrs(&self) -> &[SocketAddr] {
        self.port.addrs()
    }

    /// Its TLS on the shards, which a rotation publishes to.
    #[must_use]
    pub fn tls(&self) -> &ShardTls {
        &self.tls
    }

    /// Stops listening and closes the open connections; resolves once they ended.
    pub fn stop(&self) -> Future<()> {
        self.port.stop()
    }
}

/// What the connections of a shard share.
struct Shared {
    service: Rc<dyn HttpService>,
    settings: Settings,
    limiter: Arc<Mutex<RateLimiter>>,
    metrics: Arc<Mutex<HttpMetrics>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// The status the metrics give a request whose client went away first (nginx's 499).
const CLIENT_GONE: u16 = 499;
/// The status of a request whose service failed without answering.
const UNAVAILABLE: u16 = 503;
/// How soon a connection with requests running looks at its deadline again.
const RECHECK: Duration = Duration::from_secs(1);
/// What the reader asks of the socket at a time.
const INPUT: ConnectedSocketInputStreamConfig = ConnectedSocketInputStreamConfig {
    buffer_size: 16 << 10,
    min_buffer_size: 512,
    max_buffer_size: 128 << 10,
};
/// The output stream's buffer: unused, the sink is detached before any write.
const OUTPUT: usize = 8 << 10;

/// A request of a connection that is running.
struct Running {
    cancel: Cancel,
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
            m.stream_closed(ROUTES, &self.route);
        }
        m.record(ROUTES, &self.route, status, self.started.elapsed().as_secs_f64());
    }
}

/// A part of a request's answer.
enum Part {
    Whole(Response),
    Head(u16, Vec<(String, String)>),
    Piece(Vec<u8>),
    End,
}

/// One connection's state, shared by its tasks and its requests.
struct Ctx {
    shared: Rc<Shared>,
    caller: Caller,
    link: Link,
    conn: RefCell<Conn>,
    running: RefCell<BTreeMap<u32, Running>>,
    /// The connection ended: nothing more is read or written.
    ended: Cell<bool>,
    /// The protocol has output the writer has not taken.
    dirty: Cell<bool>,
    /// Wakes the writer.
    write: Pin<Rc<ConditionVariable>>,
    /// Wakes the deadline.
    deadline: Pin<Rc<ConditionVariable>>,
}

impl Ctx {
    fn kick(&self) {
        self.dirty.set(true);
        self.write.signal();
    }

    /// Ends the connection: the requests still running are cancelled and counted as given up,
    /// and the socket is shut down both ways.
    fn end(&self) {
        if self.ended.replace(true) {
            return;
        }
        for (_, r) in std::mem::take(&mut *self.running.borrow_mut()) {
            r.cancel.set();
            r.end(&self.shared, CLIENT_GONE);
        }
        self.write.signal();
        self.deadline.signal();
        let mut s = self.link.socket.borrow_mut();
        s.shutdown_input();
        s.shutdown_output();
    }
}

/// One connection.
fn serve(link: Link, shared: Rc<Shared>) -> Future<()> {
    let protocol = link.tls.as_ref().map(|t| http2::negotiated(t.alpn().as_deref()));
    let caller = Caller {
        peer: link.peer,
        certificates: link.tls.as_ref().map(|t| t.peer_certificates()).unwrap_or_default(),
        tls: link.tls.is_some(),
    };
    let streams = {
        let mut s = link.socket.borrow_mut();
        s.input(INPUT).detach().and_then(|i| Ok((i, s.output(OUTPUT).detach()?)))
    };
    let (mut input, output) = match streams {
        Ok(s) => s,
        Err(e) => return make_exception_future(e),
    };
    let conn = Conn::new(protocol, shared.settings, now_ms());
    let ctx = Rc::new(Ctx {
        shared,
        caller,
        link,
        conn: RefCell::new(conn),
        running: RefCell::new(BTreeMap::new()),
        ended: Cell::new(false),
        dirty: Cell::new(true),
        write: Rc::pin(ConditionVariable::new()),
        deadline: Rc::pin(ConditionVariable::new()),
    });
    let writer = writer(ctx.clone(), output);
    let deadline = deadline(ctx.clone());
    make_coroutine(async move {
        while !ctx.ended.get() {
            let read = input.get().await;
            let bytes = match read {
                Ok(b) if !b.empty() => b,
                _ => break,
            };
            let incoming = ctx.conn.borrow_mut().feed(bytes.as_slice(), now_ms());
            for i in incoming {
                start(&ctx, i);
            }
            ctx.kick();
            // The bytes may move the deadline earlier (the header timeout of fields not whole
            // yet starts at their first byte): the deadline task looks at it again.
            ctx.deadline.signal();
        }
        ctx.end();
        let _ = writer.await;
        let _ = deadline.await;
        Ok(())
    })
}

/// Sends what the protocol has to send, until the connection ends; once the protocol closed and
/// no request runs, ends it.
fn writer(ctx: Rc<Ctx>, mut sink: DataSink) -> Future<()> {
    make_coroutine(async move {
        loop {
            if !ctx.dirty.get() && !ctx.ended.get() {
                let _ = ctx.write.as_ref().wait().await;
            }
            if ctx.ended.get() {
                break;
            }
            ctx.dirty.set(false);
            let out = ctx.conn.borrow_mut().take_output();
            if !out.is_empty() && sink.put(packet(out)).await.is_err() {
                ctx.end();
                break;
            }
            let done = ctx.conn.borrow().is_closed() && ctx.running.borrow().is_empty();
            if done && !ctx.dirty.get() {
                ctx.end();
                break;
            }
        }
        Ok(())
    })
}

/// Ends the connection when it stays idle past its deadline with no request running.
fn deadline(ctx: Rc<Ctx>) -> Future<()> {
    make_coroutine(async move {
        while !ctx.ended.get() {
            let (due, idle) = (ctx.conn.borrow().deadline_ms(), ctx.running.borrow().is_empty());
            let now = now_ms();
            if idle && now >= due {
                ctx.end();
                break;
            }
            let wait = if now >= due { RECHECK } else { Duration::from_millis(due - now) };
            let nanos = i64::try_from(wait.as_nanos()).unwrap_or(i64::MAX);
            let until = SteadyTimePoint(SteadyClockType::now().0.saturating_add(nanos));
            // Woken early when the connection ends; a timeout is the deadline to look at.
            let _ = ctx.deadline.as_ref().wait_until::<SteadyClockType>(until).await;
        }
        Ok(())
    })
}

fn header<'a>(r: &'a Request, name: &str) -> Option<&'a str> {
    r.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Starts what `incoming` asks for: a preflight answered here, a request refused by the limits,
/// or a request as a task of the shard.
fn start(ctx: &Rc<Ctx>, incoming: Incoming) {
    let shared = &ctx.shared;
    let (id, request) = match incoming {
        Incoming::Cancelled { id } => {
            if let Some(r) = ctx.running.borrow_mut().remove(&id) {
                r.cancel.set();
                r.end(shared, CLIENT_GONE);
            }
            return;
        }
        Incoming::Request { id, request } => (id, request),
    };
    let options = shared.service.options();
    let cors = Cors::new(options.allowed_origins.clone());
    let origin = header(&request, "origin").map(str::to_owned);
    if Cors::is_preflight(&request.method, &request.headers) {
        let headers = cors.response_headers(origin.as_deref(), true);
        ctx.conn.borrow_mut().respond(id, Response { status: 204, headers, body: Vec::new() });
        lock(&shared.metrics).record(ROUTES, PREFLIGHT, 204, 0.0);
        return;
    }
    let credential = ctx.caller.credential();
    let route = shared.service.route(&request);
    let limits = RateLimits {
        concurrent: options.concurrent_requests,
        per_second: options.requests_per_second,
    };
    if let Err(limited) = lock(&shared.limiter).start(&credential, now_ms(), limits) {
        let mut r = Response::problem(429);
        r.headers.push(("Retry-After".to_owned(), limited.retry_after_secs.to_string()));
        r.headers.extend(cors.response_headers(origin.as_deref(), false));
        ctx.conn.borrow_mut().respond(id, r);
        lock(&shared.metrics).record(ROUTES, &route, 429, 0.0);
        return;
    }
    let cancel = Cancel::default();
    let started = Instant::now();
    let running = Running { cancel: cancel.clone(), credential, route, started, streaming: None };
    ctx.running.borrow_mut().insert(id, running);
    let cors_fields = cors.response_headers(origin.as_deref(), false);
    let service = shared.service.clone();
    let (caller, c) = (ctx.caller.clone(), cancel.clone());
    // A service that panics answers `503`, as one whose future fails.
    let reply = catch_unwind(AssertUnwindSafe(move || service.serve(request, caller, c)))
        .unwrap_or_else(|p| make_exception_future(ExceptionPtr::from_panic(p)));
    run_in_background(answer_request(ctx.clone(), id, reply, cors_fields, cancel));
}

/// One request: its answer, whole or in pieces, delivered to the connection.
fn answer_request(
    ctx: Rc<Ctx>,
    id: u32,
    reply: Future<Reply>,
    cors: Vec<(String, String)>,
    cancel: Cancel,
) -> Future<()> {
    make_coroutine(async move {
        let (status, mut headers, mut pieces) = match reply.await {
            Err(_) => {
                deliver(&ctx, id, Part::Whole(Response::problem(UNAVAILABLE)));
                return Ok(());
            }
            Ok(Reply::Whole(mut r)) => {
                r.headers.extend(cors);
                deliver(&ctx, id, Part::Whole(r));
                return Ok(());
            }
            Ok(Reply::Stream { status, headers, pieces }) => (status, headers, pieces),
        };
        headers.extend(cors);
        deliver(&ctx, id, Part::Head(status, headers));
        loop {
            if cancel.is_set() || ctx.ended.get() {
                // Nobody waits for the end of the stream.
                break;
            }
            let next = catch_unwind(AssertUnwindSafe(|| pieces(&cancel)))
                .unwrap_or_else(|p| make_exception_future(ExceptionPtr::from_panic(p)));
            match next.await {
                Ok(Some(p)) if !cancel.is_set() => deliver(&ctx, id, Part::Piece(p)),
                Ok(Some(_)) => break,
                Ok(None) | Err(_) => {
                    deliver(&ctx, id, Part::End);
                    break;
                }
            }
        }
        Ok(())
    })
}

/// Gives the protocol a part of request `id`'s answer and wakes the writer.
fn deliver(ctx: &Ctx, id: u32, part: Part) {
    if ctx.ended.get() {
        return;
    }
    let ended = {
        let mut c = ctx.conn.borrow_mut();
        c.touch(now_ms());
        let mut running = ctx.running.borrow_mut();
        match part {
            Part::Whole(r) => {
                let status = r.status;
                c.respond(id, r);
                Some(status)
            }
            Part::Head(status, headers) => {
                c.start_stream(id, status, &headers);
                if let Some(r) = running.get_mut(&id) {
                    r.streaming = Some(status);
                    lock(&ctx.shared.metrics).stream_opened(ROUTES, &r.route);
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
        }
    };
    if let Some(status) = ended
        && let Some(r) = ctx.running.borrow_mut().remove(&id)
    {
        r.end(&ctx.shared, status);
    }
    ctx.kick();
}
