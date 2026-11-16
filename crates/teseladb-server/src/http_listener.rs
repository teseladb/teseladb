//! The HTTP port (019 FR-001 to FR-004): `--http-port` on the addresses of `--http-bind`, TLS as
//! the clients' port with ALPN choosing HTTP/2 or HTTP/1.1, or plain bytes with `--insecure`
//! (HTTP/2 then by prior knowledge). The port runs on the engine's shards ([`shard`], 023
//! FR-022): a task per connection and per request. The protocols are `teseladb-http`'s, without
//! I/O.
//!
//! This file keeps what the port and the node's service share: the port's settings, the caller
//! of a request and the node's service, whose calls still wait for the cores on channels and so
//! run off the shards (`node_ports`) until the backend answers with futures (023 T049).

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;

use teseladb_http::http1::Request;
use teseladb_http::metrics::Family;
use teseladb_http::server::{Response, Settings};
use teseladb_wire::commands::http_parameters::HttpOptions;
use teseladb_x509::Certificate;

use crate::tls_port::TlsPort;

/// The series of the HTTP port's routes: `teseladb_http_*` by `route`.
pub const ROUTES: Family = Family::new("teseladb_http", "route");

/// The port on the engine's shards (023 FR-022).
pub mod shard;

/// The default `--http-port`.
pub const DEFAULT_HTTP_PORT: u16 = 27_080;
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

/// What answers the port's requests on the node: called off the shards (`node_ports`).
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
