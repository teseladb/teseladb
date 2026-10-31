//! TLS 1.3 between nodes (013 FR-006): the settings of a node with a PKI, and the channel a peer
//! connection's bytes go through, in clear (`--insecure`) or encrypted. The handshake threads use
//! the blocking side (the TLS handshake, then whole frames); the cores the non-blocking side, on
//! the bytes their poller reads and writes.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use teseladb_sys::latest::Latest;
use teseladb_tls::{ClientConfig, Connection, ENTROPY_LEN, ServerConfig};
use teseladb_x509::Certificate;

/// A node's TLS settings between nodes: as a server (its chain with the CA, client certificates
/// asked for and checked against the CA) and as a client (its chain, the CA as the anchor). The
/// clones share the settings: [`PeerTls::replace`] rotates them for every connection that comes
/// after (013 FR-006), and those already open keep theirs. Reading them takes no lock (012
/// FR-008): each rotation keeps its settings until the last clone goes.
#[derive(Clone)]
pub struct PeerTls {
    sides: Arc<Latest<Sides>>,
}

struct Sides {
    server: Arc<ServerConfig>,
    client: Arc<ClientConfig>,
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
        let sides = Sides { server: Arc::new(server), client: Arc::new(client) };
        PeerTls { sides: Arc::new(Latest::new(sides)) }
    }

    fn read(&self) -> &Sides {
        self.sides.get()
    }

    /// For the connections the node accepts.
    pub fn server(&self) -> Arc<ServerConfig> {
        self.read().server.clone()
    }

    /// For the connections the node dials.
    pub fn client(&self) -> Arc<ClientConfig> {
        self.read().client.clone()
    }

    /// Puts the settings of `other` in place of these, in every clone.
    pub fn replace(&self, other: &PeerTls) {
        let (server, client) = (other.server(), other.client());
        self.sides.set(Sides { server, client });
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
            if !open {
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
