//! A port of the node on the engine's shards (023 FR-022): the HTTP port (019) and the
//! administration port (007). They take their connections the way every port of the node does
//! ([`teseladb_io::engine::accept`]: shard 0 owns the listening sockets, every shard takes its part
//! of the connections, TLS as the port's [`ShardTls`] says within the handshake deadline, the
//! node's open connections bounded); this module hands each one to the port's handler with its
//! peer and TLS session, and on a stop shuts the open connections down.

use std::cell::{Cell, RefCell};
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use teseladb::engine::core::deleter::make_object_deleter;
use teseladb::engine::core::future::{ExceptionPtr, Future, make_exception_future};
use teseladb::engine::core::timed_out_error::TimedOutError;
use teseladb::engine::core::timer::{Clock, SteadyClockType, SteadyTimePoint};
use teseladb::engine::core::with_timeout::with_timeout;
use teseladb::engine::net::api::ConnectedSocket;
use teseladb::engine::net::packet::{Fragment, Packet};
use teseladb_io::engine::accept::{AcceptConfig, Acceptor, MakeSide, Side, Taken};
use teseladb_io::engine::tls_stream::{Sniff, TlsSession};

use crate::shard_tls::ShardTls;

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

/// The port on one shard: its handler and the sockets of the connections it serves.
struct PortSide {
    handler: Handler,
    tls: ShardTls,
    next: Cell<u64>,
    open: RefCell<Vec<(u64, Rc<RefCell<ConnectedSocket>>)>>,
}

impl Side for PortSide {
    fn sniff(&self) -> Sniff {
        self.tls.sniff()
    }

    fn serve(self: Rc<Self>, conn: Taken) -> impl std::future::Future<Output = ()> + 'static {
        async move {
            let socket = Rc::new(RefCell::new(conn.socket));
            let key = self.next.get();
            self.next.set(key + 1);
            self.open.borrow_mut().push((key, Rc::clone(&socket)));
            let peer = conn.peer.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
            let link = Link { socket, peer, tls: conn.tls };
            // A connection that fails only loses itself.
            let _ = (self.handler)(link).await;
            self.open.borrow_mut().retain(|(k, _)| *k != key);
        }
    }

    /// Shuts the open connections down, which ends what their handlers wait for.
    fn stop(&self) {
        for (_, s) in self.open.borrow().iter() {
            let mut s = s.borrow_mut();
            s.shutdown_input();
            s.shutdown_output();
        }
    }
}

/// A port running on every shard. Clones are handles to the same port; any shard may stop it.
#[derive(Clone, Debug)]
pub struct Port(Acceptor);

impl Port {
    /// Listens on the addresses of `cfg` on every shard and serves each connection with the
    /// handler `make` gives on the shard that took it. Runs on shard 0.
    ///
    /// # Errors
    ///
    /// A failed future when an address does not listen (in use, not local); nothing keeps
    /// listening then.
    pub fn start(cfg: PortConfig, make: MakeHandler) -> Future<Port> {
        let PortConfig { name, addrs, tls, handshake, max_connections } = cfg;
        let accept = AcceptConfig {
            name,
            addrs,
            handshake,
            max_connections: Some(max_connections),
            local_address: false,
        };
        let side: MakeSide<PortSide> = Arc::new(move |_| {
            Rc::new(PortSide {
                handler: make(),
                tls: tls.clone(),
                next: Cell::new(0),
                open: RefCell::new(Vec::new()),
            })
        });
        Acceptor::start(accept, side).then(|a| -> Result<Port, ExceptionPtr> { Ok(Port(a)) })
    }

    /// The addresses it listens on.
    #[must_use]
    pub fn addrs(&self) -> &[SocketAddr] {
        self.0.addrs()
    }

    /// Stops accepting on every shard, shuts the open connections down and resolves once their
    /// tasks ended. A second stop resolves at once.
    pub fn stop(&self) -> Future<()> {
        self.0.stop()
    }
}
