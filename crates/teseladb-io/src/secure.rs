//! TLS 1.3 between nodes (013 FR-006): the settings of a node with a PKI, and the channel a peer
//! connection's bytes go through, in clear (`--insecure`) or encrypted. The blocking side (the TLS
//! handshake, then whole frames on a `TcpStream`) serves the callers off the engine, as a node
//! that asks to join before its own starts ([`crate::handshake`]); on the shards the connections
//! are the engine's streams ([`crate::engine::tls_stream`]).
//!
//! The settings are read without a lock: each shard holds its own copy ([`PeerTls`]), and a
//! certificate rotation, a task of shard 0, puts a copy of the new ones on every shard with `smp`
//! before it counts as done (023 FR-023).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use teseladb_tls::{ClientConfig, Connection, ENTROPY_LEN, ServerConfig};
use teseladb_x509::Certificate;

/// A node's TLS settings between nodes: as a server (its chain with the CA, client certificates
/// asked for and checked against the CA) and as a client (its chain, the CA as the anchor).
///
/// Each shard (and each thread that is not one) reads its own copy, without a lock (012 FR-008):
/// the settings it was made with until a rotation puts others there. [`PeerTls::publish`] puts
/// new settings on every shard of the engine (013 FR-006, 023 FR-023) and
/// [`PeerTls::replace_here`] on this thread only; the connections already open keep theirs. The
/// clones share the copies.
#[derive(Clone)]
pub struct PeerTls {
    /// The number of these settings in the process: what the copies are kept under.
    id: u64,
    /// The settings it was made with, for a thread that has no copy yet.
    first: Arc<Sides>,
}

#[derive(Clone)]
struct Sides {
    server: Arc<ServerConfig>,
    client: Arc<ClientConfig>,
}

/// The numbers of the settings of the process.
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// This thread's copy of each [`PeerTls`] a rotation replaced, by number.
    static COPIES: RefCell<HashMap<u64, Arc<Sides>>> = RefCell::new(HashMap::new());
}

impl fmt::Debug for PeerTls {
    /// Without the keys.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PeerTls")
    }
}

impl PeerTls {
    /// The settings `server` for the connections the node accepts and `client` for those it
    /// dials.
    pub fn new(server: ServerConfig, client: ClientConfig) -> PeerTls {
        let first = Arc::new(Sides { server: Arc::new(server), client: Arc::new(client) });
        PeerTls { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), first }
    }

    /// What `f` reads of this thread's copy.
    fn read<R>(&self, f: impl FnOnce(&Sides) -> R) -> R {
        COPIES.with(|c| match c.borrow().get(&self.id) {
            Some(sides) => f(sides),
            None => f(&self.first),
        })
    }

    /// For the connections the node accepts.
    pub fn server(&self) -> Arc<ServerConfig> {
        self.read(|s| s.server.clone())
    }

    /// For the connections the node dials.
    pub fn client(&self) -> Arc<ClientConfig> {
        self.read(|s| s.client.clone())
    }

    /// The settings of `other` as this thread reads them now, to be copied elsewhere.
    fn sides_of(other: &PeerTls) -> Arc<Sides> {
        other.read(|s| Arc::new(s.clone()))
    }

    /// Puts the settings of `other` in place of these on this thread: on a shard, for the
    /// connections it accepts and dials from now on; on a thread that is not a shard (a process
    /// without an engine), for what that thread does.
    pub fn replace_here(&self, other: &PeerTls) {
        let sides = PeerTls::sides_of(other);
        COPIES.with(|c| c.borrow_mut().insert(self.id, sides));
    }

    /// Puts the settings of `other` in place of these on every shard of the engine, each its own
    /// copy (023 FR-023); called on a shard (the rotation's task on shard 0), it resolves once
    /// every shard has them.
    #[cfg(target_os = "linux")]
    pub fn publish(&self, other: &PeerTls) -> teseladb::engine::core::future::Future<()> {
        let (id, sides) = (self.id, PeerTls::sides_of(other));
        teseladb::engine::core::smp::Smp::invoke_on_all(move || {
            COPIES.with(|c| c.borrow_mut().insert(id, Arc::clone(&sides)));
        })
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn entropy() -> io::Result<[u8; ENTROPY_LEN]> {
    let mut e = [0u8; ENTROPY_LEN];
    teseladb_sys::fill_random(&mut e)?;
    Ok(e)
}

fn tls_error(e: teseladb_tls::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("TLS: {e:?}"))
}

/// How a peer connection's bytes travel.
pub enum Channel {
    /// In clear (`--insecure`).
    Plain,
    /// TLS.
    Tls(Box<Connection>),
}

impl fmt::Debug for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Channel::Plain => "Plain",
            Channel::Tls(_) => "Tls",
        })
    }
}

impl Channel {
    /// The channel of a connection this node accepted.
    pub fn accepted(tls: Option<&PeerTls>) -> io::Result<Channel> {
        Ok(match tls {
            None => Channel::Plain,
            Some(t) => {
                Channel::Tls(Box::new(Connection::server(t.server(), now_ms(), &entropy()?)))
            }
        })
    }

    /// The channel of a connection this node dials with `client` (no host name is checked: the
    /// peer is whoever holds a certificate of the cluster's CA).
    pub fn dialed(client: Option<&Arc<ClientConfig>>) -> io::Result<Channel> {
        Ok(match client {
            None => Channel::Plain,
            Some(c) => Channel::Tls(Box::new(
                Connection::client(c.clone(), None, now_ms(), &entropy()?, None)
                    .map_err(tls_error)?,
            )),
        })
    }

    /// The peer's certificate chain, leaf first (empty in clear or without one).
    pub fn peer_certificates(&self) -> &[Certificate] {
        match self {
            Channel::Plain => &[],
            Channel::Tls(c) => c.peer_certificates(),
        }
    }

    /// Runs the TLS handshake on `stream` (blocking); nothing in clear.
    pub fn handshake(&mut self, stream: &mut TcpStream) -> io::Result<()> {
        let Channel::Tls(c) = self else { return Ok(()) };
        let mut chunk = [0u8; 16 * 1024];
        loop {
            let out = c.take_output();
            if !out.is_empty() {
                stream.write_all(&out)?;
            }
            if !c.is_handshaking() {
                return Ok(());
            }
            let n = stream.read(&mut chunk)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            c.feed(&chunk[..n]).map_err(tls_error)?;
        }
    }

    /// Writes `data` to `stream` (blocking).
    pub fn send(&mut self, stream: &mut TcpStream, data: &[u8]) -> io::Result<()> {
        let mut out = Vec::new();
        if !self.outgoing(data, &mut out) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "TLS write"));
        }
        stream.write_all(&out)
    }

    /// Reads from `stream` (blocking) until some application bytes are appended to `plain`.
    pub fn recv(&mut self, stream: &mut TcpStream, plain: &mut Vec<u8>) -> io::Result<()> {
        let mut chunk = [0u8; 16 * 1024];
        let before = plain.len();
        // Bytes that came with the end of the TLS handshake are already decrypted.
        if let Channel::Tls(c) = self {
            plain.extend_from_slice(&c.read());
        }
        while plain.len() == before {
            let n = stream.read(&mut chunk)?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            let mut out = Vec::new();
            let open = self.incoming(&chunk[..n], plain, &mut out);
            if !out.is_empty() {
                stream.write_all(&out)?;
            }
            // What came before the peer's `close_notify` is still delivered; the next read
            // finds the end.
            if !open && plain.len() == before {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "TLS read"));
            }
        }
        Ok(())
    }

    /// Takes bytes read from the socket: application bytes to `plain`, what goes back (alerts,
    /// tickets, key updates) to `out`. `false` closes the connection.
    pub fn incoming(&mut self, raw: &[u8], plain: &mut Vec<u8>, out: &mut Vec<u8>) -> bool {
        match self {
            Channel::Plain => {
                plain.extend_from_slice(raw);
                true
            }
            Channel::Tls(c) => {
                let ok = c.feed(raw).is_ok();
                out.extend_from_slice(&c.take_output());
                plain.extend_from_slice(&c.read());
                ok && !c.peer_closed()
            }
        }
    }

    /// Turns application bytes into what goes to the socket, appended to `out`. `false` closes
    /// the connection.
    pub fn outgoing(&mut self, data: &[u8], out: &mut Vec<u8>) -> bool {
        match self {
            Channel::Plain => {
                out.extend_from_slice(data);
                true
            }
            Channel::Tls(c) => {
                let ok = c.write(data).is_ok();
                out.extend_from_slice(&c.take_output());
                ok
            }
        }
    }
}
