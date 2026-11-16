//! How every port of a node takes its connections on the engine (023 FR-018, FR-022; D-182: one
//! way to do one thing). The clients' port ([`super::client_port`]) and the HTTP and
//! administration ports of the server build on it and keep only their protocol.
//!
//! [`Acceptor::start`] runs on shard 0, which owns the listening sockets: it listens on each
//! address with the engine's POSIX stack, then every other shard listens on the address the
//! kernel gave (a port 0 becomes a real one), so that the engine spreads the accepted connections
//! over all shards (no `SO_REUSEPORT`). Each shard runs one accept loop per address; each
//! connection is a task of the shard that took it: its first bytes are taken the way the port's
//! [`Side::sniff`] says (in clear, TLS, or chosen by the first byte) within the handshake
//! deadline ([`tls_stream::accept`]), and the result goes to [`Side::serve`] on that shard.
//!
//! A port may bound the connections open at once on the node: they are counted across the shards
//! in one atomic, and past the bound a new connection is closed at once. [`Acceptor::stop`] stops
//! accepting on every shard, asks the port's [`Side`] on each to end what it serves
//! ([`Side::stop`]) and resolves once every task of the port ended (a connection still taking its
//! first bytes ends at its handshake deadline at the latest, and is not served).
//!
//! The awaits on the engine's futures are in the non-generic functions of this module; the
//! generic ones only await those and the port's own future, polled inline: a connection costs one
//! task, as when each port had its own loop.

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, make_exception_future, make_ready_future,
};
use teseladb::engine::core::gate::Gate;
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb::engine::core::reactor::listen_with;
use teseladb::engine::core::smp::{Smp, this_shard_id};
use teseladb::engine::net::api::{AcceptResult, ConnectedSocket, ListenOptions, ServerSocket};

use super::connect::{socket_address, std_addr};
use super::tls_stream::{self, Sniff, TlsSession};

/// A connection a port took, in clear or with its TLS handshake done.
pub struct Taken {
    /// The socket: plaintext both ways, with Nagle's delay off.
    pub socket: ConnectedSocket,
    /// The client's address.
    pub peer: Option<SocketAddr>,
    /// The address it connected to, when the port asked for it ([`AcceptConfig::local_address`]).
    pub local: Option<SocketAddr>,
    /// The TLS session; `None` in clear.
    pub tls: Option<TlsSession>,
    /// How long its first bytes and TLS handshake took from the accept; `None` in clear.
    pub handshake: Option<Duration>,
}

impl std::fmt::Debug for Taken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Taken")
            .field("peer", &self.peer)
            .field("local", &self.local)
            .field("tls", &self.tls)
            .field("handshake", &self.handshake)
            .finish_non_exhaustive()
    }
}

/// What a port does with its connections on one shard: its protocol. It lives on its shard.
pub trait Side: 'static {
    /// How the next connection's first bytes are taken (`--tlsMode` with the certificates of
    /// this shard now).
    fn sniff(&self) -> Sniff;
    /// Serves `conn` until it ends; the port's stop waits for it.
    fn serve(self: Rc<Self>, conn: Taken) -> impl std::future::Future<Output = ()> + 'static;
    /// The port stops on this shard: end what it serves (the stop then waits for it).
    fn stop(&self);
}

/// Makes a port's [`Side`] on a shard, on that shard, given the port's id.
pub type MakeSide<S> = Arc<dyn Fn(u64) -> Rc<S> + Send + Sync>;

/// How a port takes its connections.
#[derive(Clone, Debug)]
pub struct AcceptConfig {
    /// The port's name in errors.
    pub name: &'static str,
    /// Addresses to listen on.
    pub addrs: Vec<SocketAddr>,
    /// How long a connection may take to send its first bytes and finish its TLS handshake.
    pub handshake: Duration,
    /// Most connections open at once on the node; past it a new one is closed at once. `None`
    /// counts nothing (the port bounds them itself).
    pub max_connections: Option<usize>,
    /// Whether [`Taken::local`] is read (one system call per connection).
    pub local_address: bool,
}

/// What a port keeps on one shard.
struct Here {
    listeners: Vec<Rc<RefCell<ServerSocket>>>,
    /// The accept loops and the connections: a stop waits for them.
    gate: Pin<Rc<Gate>>,
    /// [`Side::stop`] of the port's side here.
    stop: Rc<dyn Fn()>,
}

thread_local! {
    /// The ports running on this shard, by id.
    static PORTS: RefCell<HashMap<u64, Here>> = RefCell::new(HashMap::new());
}

/// The ids of the ports of the process.
static NEXT_PORT: AtomicU64 = AtomicU64::new(1);

/// A port running on every shard. Clones are handles to the same port; any shard may stop it.
#[derive(Clone, Debug)]
pub struct Acceptor {
    id: u64,
    addrs: Vec<SocketAddr>,
    open: Arc<AtomicUsize>,
    stopped: Arc<AtomicBool>,
}

impl Acceptor {
    /// Listens on the addresses of `cfg` on every shard and serves each connection with the
    /// side `make` gives on the shard that took it. Runs on shard 0.
    ///
    /// # Errors
    ///
    /// A failed future when an address does not listen (in use, not local), with the system's
    /// kind and a text that names the port and the address; nothing keeps listening then.
    pub fn start<S: Side>(cfg: AcceptConfig, make: MakeSide<S>) -> Future<Acceptor> {
        if this_shard_id() != 0 {
            return make_exception_future(ExceptionPtr::from(io::Error::other(
                "a port starts on shard 0",
            )));
        }
        let id = NEXT_PORT.fetch_add(1, Ordering::Relaxed);
        let open = Arc::new(AtomicUsize::new(0));
        let addrs = match listen_here(id, &cfg, &cfg.addrs, &make, &open) {
            Ok(a) => a,
            Err(e) => return make_exception_future(e),
        };
        let port = Acceptor {
            id,
            addrs: addrs.clone(),
            open: open.clone(),
            stopped: Arc::new(AtomicBool::new(false)),
        };
        let others = Smp::invoke_on_others(move || -> Result<(), ExceptionPtr> {
            listen_here(id, &cfg, &addrs, &make, &open).map(drop)
        });
        let failed = port.clone();
        others.then_wrapped(move |f: Future<()>| match f.get() {
            Ok(()) => make_ready_future(port),
            Err(e) => failed.stop().then(move |()| -> Result<Acceptor, ExceptionPtr> { Err(e) }),
        })
    }

    /// Its number in the process.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The addresses it listens on, with the ports the system chose for port 0.
    #[must_use]
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    /// Connections open on the node now, when the port bounds them; 0 otherwise.
    #[must_use]
    pub fn open(&self) -> usize {
        self.open.load(Ordering::Relaxed)
    }

    /// Stops accepting on every shard, ends what the port serves and resolves once its tasks
    /// ended. A second stop resolves at once. Runs on a shard.
    pub fn stop(&self) -> Future<()> {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return make_ready_future(());
        }
        let id = self.id;
        Smp::invoke_on_all(move || stop_here(id))
    }
}

/// Listens on `addrs` on this shard under port `id`; the addresses it listens on.
fn listen_here<S: Side>(
    id: u64,
    cfg: &AcceptConfig,
    addrs: &[SocketAddr],
    make: &MakeSide<S>,
    open: &Arc<AtomicUsize>,
) -> Result<Vec<SocketAddr>, ExceptionPtr> {
    let (listeners, bound) = listen_all(cfg.name, addrs)?;
    let side = make(id);
    let stop: Rc<dyn Fn()> = {
        let side = Rc::clone(&side);
        Rc::new(move || side.stop())
    };
    let gate = Rc::pin(Gate::new());
    let ctx = Rc::new(Ctx {
        side,
        gate: gate.clone(),
        handshake: cfg.handshake,
        limit: cfg.max_connections.map(|max| (max, open.clone())),
        local_address: cfg.local_address,
    });
    for l in &listeners {
        // Entered before the loop runs, so that a stop waits for it.
        if gate.enter().is_ok() {
            let (ctx, l) = (Rc::clone(&ctx), Rc::clone(l));
            run_in_background(make_coroutine(async move {
                accept_loop(ctx, l).await;
                Ok(())
            }));
        }
    }
    PORTS.with(|p| p.borrow_mut().insert(id, Here { listeners, gate, stop }));
    Ok(bound)
}

/// Listens on each of `addrs` on this shard: the listeners and the addresses they got.
fn listen_all(
    name: &str,
    addrs: &[SocketAddr],
) -> Result<(Vec<Rc<RefCell<ServerSocket>>>, Vec<SocketAddr>), ExceptionPtr> {
    let options = ListenOptions { reuse_address: true, ..ListenOptions::default() };
    let mut listeners = Vec::with_capacity(addrs.len());
    let mut bound = Vec::with_capacity(addrs.len());
    for a in addrs {
        let l = listen_with(socket_address(*a), options.clone()).map_err(|e| {
            let kind = e.downcast_ref::<io::Error>().map_or(io::ErrorKind::Other, io::Error::kind);
            ExceptionPtr::from(io::Error::new(kind, format!("{name} {a}: {e}")))
        })?;
        bound.push(l.local_address().ok().as_ref().and_then(std_addr).unwrap_or(*a));
        listeners.push(Rc::new(RefCell::new(l)));
    }
    Ok((listeners, bound))
}

/// Stops port `id` on this shard.
fn stop_here(id: u64) -> Future<()> {
    let Some(here) = PORTS.with(|p| p.borrow_mut().remove(&id)) else {
        return make_ready_future(());
    };
    for l in &here.listeners {
        l.borrow_mut().abort_accept();
    }
    // Closed before the side stops, so that a connection that ends its handshake now is not
    // served.
    let done = here.gate.close();
    (here.stop)();
    done.finally(move || drop(here))
}

/// What the accept loops of a port share on one shard.
struct Ctx<S> {
    side: Rc<S>,
    gate: Pin<Rc<Gate>>,
    handshake: Duration,
    /// The node-wide bound and count of open connections.
    limit: Option<(usize, Arc<AtomicUsize>)>,
    local_address: bool,
}

/// A connection counted among the node's open ones while it lives.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Counts a new connection against `limit`; `Err` past it.
fn count(limit: Option<&(usize, Arc<AtomicUsize>)>) -> Result<Option<Counted>, ()> {
    let Some((max, open)) = limit else { return Ok(None) };
    if open.fetch_add(1, Ordering::Relaxed) >= *max {
        open.fetch_sub(1, Ordering::Relaxed);
        return Err(());
    }
    Ok(Some(Counted(open.clone())))
}

async fn accept_loop<S: Side>(ctx: Rc<Ctx<S>>, listener: Rc<RefCell<ServerSocket>>) {
    loop {
        let Ok(accepted) = next(&listener).await else {
            // A stop aborts the accept, and the loop ends; a failed accept loses only that
            // connection.
            if ctx.gate.is_closed() {
                break;
            }
            continue;
        };
        // Past the bound the connection is dropped, which closes it.
        let Ok(counted) = count(ctx.limit.as_ref()) else { continue };
        if ctx.gate.enter().is_err() {
            break;
        }
        let peer = std_addr(&accepted.remote_address);
        let ctx = Rc::clone(&ctx);
        run_in_background(make_coroutine(async move {
            connection(&ctx, accepted.connection, peer, counted).await;
            ctx.gate.leave();
            Ok(())
        }));
    }
    ctx.gate.leave();
}

/// The next connection the engine hands this shard on `listener`.
async fn next(listener: &Rc<RefCell<ServerSocket>>) -> Result<AcceptResult, ExceptionPtr> {
    let accept = listener.borrow_mut().accept();
    accept.await
}

/// One connection: taken as the side says, then served by it unless the port stopped meanwhile.
async fn connection<S: Side>(
    ctx: &Ctx<S>,
    sock: ConnectedSocket,
    peer: Option<SocketAddr>,
    counted: Option<Counted>,
) {
    let _counted = counted;
    let taken = take(sock, peer, ctx.side.sniff(), ctx.handshake, ctx.local_address).await;
    if let Some(conn) = taken
        && !ctx.gate.is_closed()
    {
        Rc::clone(&ctx.side).serve(conn).await;
    }
}

/// Takes `sock` the way `sniff` says within `handshake`; `None` when the client sent nothing in
/// time or failed its handshake (the connection is then closed).
async fn take(
    mut sock: ConnectedSocket,
    peer: Option<SocketAddr>,
    sniff: Sniff,
    handshake: Duration,
    local_address: bool,
) -> Option<Taken> {
    // Nagle's delay would hold an answer's last segment back.
    let _ = sock.set_nodelay(true);
    let local = if local_address { std_addr(&sock.local_address()) } else { None };
    let from = Instant::now();
    let accepted = tls_stream::accept(sock, sniff, Some(handshake)).await.ok()?;
    let handshake = accepted.tls.as_ref().map(|_| from.elapsed());
    Some(Taken { socket: accepted.socket, peer, local, tls: accepted.tls, handshake })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_bound_counts_nothing() {
        assert!(matches!(count(None), Ok(None)));
    }

    #[test]
    fn a_bound_counts_while_the_connection_lives() {
        let open = Arc::new(AtomicUsize::new(0));
        let limit = (2, open.clone());
        let a = count(Some(&limit)).expect("first").expect("counted");
        let b = count(Some(&limit)).expect("second").expect("counted");
        assert_eq!(open.load(Ordering::Relaxed), 2);
        assert!(count(Some(&limit)).is_err(), "past the bound a connection is refused");
        assert_eq!(open.load(Ordering::Relaxed), 2, "a refused connection is not counted");
        drop(a);
        assert_eq!(open.load(Ordering::Relaxed), 1);
        let c = count(Some(&limit)).expect("room again").expect("counted");
        drop((b, c));
        assert_eq!(open.load(Ordering::Relaxed), 0);
    }
}
