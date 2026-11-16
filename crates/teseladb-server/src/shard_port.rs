//! A port of the node on the engine's shards (023 FR-022): the HTTP port (019) and the
//! administration port (007) listen with the engine's POSIX stack, every shard takes the
//! connections the engine hands it, and each connection is a task of its shard.
//!
//! [`Port::start`] runs on shard 0, which owns the listening sockets: it listens on each address,
//! then every other shard listens on the address the kernel gave (a port 0 becomes a real one), so
//! that the engine spreads the accepted connections over all shards. A connection is taken the
//! way the port's [`ShardTls`] says (in clear, TLS with its handshake done, or chosen by its first
//! byte) within the handshake deadline, and then handed to the port's handler with its peer and
//! TLS session. [`Port::stop`] stops accepting on every shard, shuts the open connections down and
//! resolves once their tasks ended.
//!
//! The open connections of a node are counted across its shards in one atomic: past the port's
//! limit a new connection is closed at once.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::deleter::make_object_deleter;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, make_exception_future, make_ready_future,
};
use teseladb::engine::core::gate::Gate;
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb::engine::core::reactor::listen_with;
use teseladb::engine::core::smp::{Smp, this_shard_id};
use teseladb::engine::core::timed_out_error::TimedOutError;
use teseladb::engine::core::timer::{Clock, SteadyClockType, SteadyTimePoint};
use teseladb::engine::core::with_timeout::with_timeout;
use teseladb::engine::net::api::{ConnectedSocket, ListenOptions, ServerSocket};
use teseladb::engine::net::packet::{Fragment, Packet};
use teseladb::engine::net::socket_defs::SocketAddress;
use teseladb_io::engine::connect::socket_address;
use teseladb_io::engine::tls_stream::{self, TlsSession};

use crate::shard_tls::ShardTls;

const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;

/// The standard address of the engine's `sa`; `None` for a family other than IPv4 and IPv6.
#[must_use]
pub fn std_addr(sa: &SocketAddress) -> Option<SocketAddr> {
    match sa.family() {
        AF_INET => {
            let a = sa.as_posix_sockaddr_in();
            let ip = Ipv4Addr::from(a.sin_addr.s_addr.to_ne_bytes());
            Some(SocketAddr::new(IpAddr::V4(ip), u16::from_be(a.sin_port)))
        }
        AF_INET6 => {
            let a = sa.as_posix_sockaddr_in6();
            let ip = Ipv6Addr::from(a.sin6_addr.s6_addr);
            Some(SocketAddr::new(IpAddr::V6(ip), u16::from_be(a.sin6_port)))
        }
        _ => None,
    }
}

fn io_error(what: String) -> ExceptionPtr {
    ExceptionPtr::from(io::Error::other(what))
}

/// A packet of `bytes` without copying them: the packet's deleter owns the vector.
#[must_use]
pub fn packet(mut bytes: Vec<u8>) -> Packet {
    let frag = Fragment { base: bytes.as_mut_ptr(), size: bytes.len() };
    Packet::from_fragment_deleter(frag, make_object_deleter(bytes))
}

/// `f` within `timeout`: past it `sock` is shut down both ways, which ends what `f` waits for,
/// and the result fails with [`io::ErrorKind::TimedOut`] and the text `what`.
pub fn bounded<T: 'static>(
    sock: &Rc<RefCell<ConnectedSocket>>,
    timeout: Duration,
    what: &'static str,
    f: Future<T>,
) -> Future<T> {
    let nanos = i64::try_from(timeout.as_nanos()).unwrap_or(i64::MAX);
    let until = SteadyTimePoint(SteadyClockType::now().0.saturating_add(nanos));
    let sock = Rc::clone(sock);
    with_timeout::<SteadyClockType, T>(until, f).handle_exception(move |e: ExceptionPtr| {
        if e.downcast_ref::<TimedOutError>().is_some() {
            let mut s = sock.borrow_mut();
            s.shutdown_input();
            s.shutdown_output();
            let timed_out = io::Error::new(io::ErrorKind::TimedOut, what);
            return make_exception_future(ExceptionPtr::from(timed_out));
        }
        make_exception_future(e)
    })
}

/// A connection the port took: its socket (plaintext both ways), shared with the port so that a
/// stop can shut it down, its peer and its TLS session.
pub struct Link {
    /// The socket.
    pub socket: Rc<RefCell<ConnectedSocket>>,
    /// The client's address.
    pub peer: SocketAddr,
    /// The TLS session; `None` in clear.
    pub tls: Option<TlsSession>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link").field("peer", &self.peer).field("tls", &self.tls).finish()
    }
}

/// What serves a port's connections on one shard: the task of each connection.
pub type Handler = Rc<dyn Fn(Link) -> Future<()>>;

/// Makes a shard's [`Handler`]; it runs on that shard, so the handler need not be `Send`.
pub type MakeHandler = Arc<dyn Fn() -> Handler + Send + Sync>;

/// How a port takes its connections.
#[derive(Clone)]
pub struct PortConfig {
    /// The port's name in errors (`crate::listen::HTTP`, `crate::listen::ADMIN`).
    pub name: &'static str,
    /// Addresses to listen on.
    pub addrs: Vec<SocketAddr>,
    /// Its TLS on each shard.
    pub tls: ShardTls,
    /// How long a connection may take to send its first bytes and finish its TLS handshake.
    pub handshake: Duration,
    /// Most connections open at once on the node; past it a new one is closed at once.
    pub max_connections: usize,
}

impl std::fmt::Debug for PortConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortConfig").field("addrs", &self.addrs).finish_non_exhaustive()
    }
}

/// What a port keeps on one shard.
struct ShardPort {
    listeners: Vec<Rc<RefCell<ServerSocket>>>,
    open: RefCell<Vec<(u64, Rc<RefCell<ConnectedSocket>>)>>,
    next: std::cell::Cell<u64>,
    /// The accept loops and the connections: a stop waits for them.
    gate: Pin<Rc<Gate>>,
}

thread_local! {
    /// The ports running on this shard, by id.
    static PORTS: RefCell<HashMap<u64, Rc<ShardPort>>> = RefCell::new(HashMap::new());
}

/// The ids of the ports of the process.
static NEXT_PORT: AtomicU64 = AtomicU64::new(1);

/// A port running on every shard. Clones are handles to the same port; any shard may stop it.
#[derive(Clone, Debug)]
pub struct Port {
    id: u64,
    addrs: Vec<SocketAddr>,
    /// Whether a stop already began.
    stopped: Arc<Mutex<bool>>,
}

impl Port {
    /// Listens on the addresses of `cfg` on every shard and serves each connection with the
    /// handler `make` gives on the shard that took it. Runs on shard 0.
    ///
    /// # Errors
    ///
    /// A failed future when an address does not listen (in use, not local); nothing keeps
    /// listening then.
    pub fn start(cfg: PortConfig, make: MakeHandler) -> Future<Port> {
        if this_shard_id() != 0 {
            return make_exception_future(io_error(
                "a port starts on shard 0".into(),
            ));
        }
        let id = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        let open = Arc::new(AtomicUsize::new(0));
        let addrs = match listen_here(id, &cfg, &cfg.addrs, &make, &open) {
            Ok(a) => a,
            Err(e) => return make_exception_future(e),
        };
        let port = Port { id, addrs: addrs.clone(), stopped: Arc::new(Mutex::new(false)) };
        let others = {
            let (cfg, make, open, addrs) = (cfg.clone(), make.clone(), open.clone(), addrs);
            Smp::invoke_on_others(move || -> Result<(), ExceptionPtr> {
                listen_here(id, &cfg, &addrs, &make, &open).map(drop)
            })
        };
        let failed = port.clone();
        others.then_wrapped(move |f: Future<()>| match f.get() {
            Ok(()) => make_ready_future(port),
            Err(e) => failed.stop().then(move |()| -> Result<Port, ExceptionPtr> { Err(e) }),
        })
    }

    /// The addresses it listens on.
    #[must_use]
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// Stops accepting on every shard, shuts the open connections down and resolves once their
    /// tasks ended. A second stop resolves at once.
    pub fn stop(&self) -> Future<()> {
        {
            let mut stopped = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
            if *stopped {
                return make_ready_future(());
            }
            *stopped = true;
        }
        let id = self.id;
        Smp::invoke_on_all(move || stop_here(id))
    }
}

/// Listens on `addrs` on this shard under port `id`; the addresses it listens on.
fn listen_here(
    id: u64,
    cfg: &PortConfig,
    addrs: &[SocketAddr],
    make: &MakeHandler,
    open: &Arc<AtomicUsize>,
) -> Result<Vec<SocketAddr>, ExceptionPtr> {
    let options = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    let mut listeners = Vec::new();
    let mut bound = Vec::new();
    for a in addrs {
        let l = listen_with(socket_address(*a), options.clone()).map_err(|e| {
            let kind = e.downcast_ref::<io::Error>().map_or(io::ErrorKind::Other, io::Error::kind);
            ExceptionPtr::from(io::Error::new(kind, format!("{} {a}: {e}", cfg.name)))
        })?;
        let at = l.local_address().ok().as_ref().and_then(std_addr).unwrap_or(*a);
        bound.push(at);
        listeners.push(Rc::new(RefCell::new(l)));
    }
    let shard = Rc::new(ShardPort {
        listeners,
        open: RefCell::new(Vec::new()),
        next: std::cell::Cell::new(0),
        gate: Rc::pin(Gate::new()),
    });
    PORTS.with(|p| p.borrow_mut().insert(id, shard.clone()));
    let handler = make();
    for l in &shard.listeners {
        let ctx = AcceptCtx {
            shard: shard.clone(),
            listener: l.clone(),
            handler: handler.clone(),
            tls: cfg.tls.clone(),
            handshake: cfg.handshake,
            max: cfg.max_connections,
            open: open.clone(),
        };
        // Entered before the loop runs, so that a stop waits for it.
        if shard.gate.enter().is_ok() {
            run_in_background(accept_loop(ctx));
        }
    }
    Ok(bound)
}

/// Stops port `id` on this shard.
fn stop_here(id: u64) -> Future<()> {
    let Some(shard) = PORTS.with(|p| p.borrow_mut().remove(&id)) else {
        return make_ready_future(());
    };
    for l in &shard.listeners {
        l.borrow_mut().abort_accept();
    }
    for (_, s) in shard.open.borrow().iter() {
        let mut s = s.borrow_mut();
        s.shutdown_input();
        s.shutdown_output();
    }
    let keep = shard.clone();
    shard.gate.close().finally(move || drop(keep))
}

/// What an accept loop needs.
struct AcceptCtx {
    shard: Rc<ShardPort>,
    listener: Rc<RefCell<ServerSocket>>,
    handler: Handler,
    tls: ShardTls,
    handshake: Duration,
    max: usize,
    open: Arc<AtomicUsize>,
}

/// A connection counted among the node's open ones while it lives.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn accept_loop(ctx: AcceptCtx) -> Future<()> {
    make_coroutine(async move {
        loop {
            let accept = ctx.listener.borrow_mut().accept();
            // A stop aborts the accept, and the loop ends.
            let Ok(accepted) = accept.await else { break };
            if ctx.open.fetch_add(1, Ordering::Relaxed) >= ctx.max {
                ctx.open.fetch_sub(1, Ordering::Relaxed);
                continue;
            }
            let counted = Counted(ctx.open.clone());
            if ctx.shard.gate.enter().is_err() {
                break;
            }
            let peer = std_addr(&accepted.remote_address)
                .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
            let task = connection(&ctx, accepted.connection, peer, counted);
            run_in_background(task);
        }
        ctx.shard.gate.leave();
        Ok(())
    })
}

/// One connection: taken as the port's TLS says, registered for a stop, served by the handler.
fn connection(
    ctx: &AcceptCtx,
    sock: ConnectedSocket,
    peer: SocketAddr,
    counted: Counted,
) -> Future<()> {
    let sniff = ctx.tls.sniff();
    let (shard, handler) = (ctx.shard.clone(), ctx.handler.clone());
    let handshake = ctx.handshake;
    make_coroutine(async move {
        let _counted = counted;
        if let Ok(mut accepted) = tls_stream::accept(sock, sniff, Some(handshake)).await {
            // Nagle's delay would hold an answer's last segment back.
            let _ = accepted.socket.set_nodelay(true);
            let socket = Rc::new(RefCell::new(accepted.socket));
            let key = shard.next.get();
            shard.next.set(key + 1);
            shard.open.borrow_mut().push((key, socket.clone()));
            let link = Link { socket, peer, tls: accepted.tls };
            // A connection that fails only loses itself.
            let _ = handler(link).await;
            shard.open.borrow_mut().retain(|(k, _)| *k != key);
        }
        shard.gate.leave();
        Ok(())
    })
}
