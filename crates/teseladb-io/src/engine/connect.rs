//! Outbound connections on the engine (023 FR-024, FR-015): resolve a name with the system
//! resolver off the shard ([`blocking::resolve`]), connect with the engine's network stack under a
//! deadline on its timers, and optionally run a TLS client over the connection
//! ([`tls_stream::client`]). The LDAP, KMIP, STS, OIDC, HTTPS and OCSP clients use it, as does the
//! dialer between nodes (which knows its addresses and calls [`connect`]).
//!
//! The deadlines are the ones the blocking clients had: `timeout` for the lookup, the same for
//! each address tried in turn, and the same again for the TLS handshake. A connection attempt the
//! deadline overtakes is cancelled by shutting its socket down; its socket lives until the attempt
//! ends, as the engine's socket API requires.
//!
//! Name resolution does not use the engine's syscall pool; [`blocking`] says why.

use std::cell::RefCell;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::reactor::make_socket;
use teseladb::engine::net::api::ConnectedSocket;
use teseladb::engine::net::socket_defs::{
    SocketAddress, Transport, in_addr, in6_addr, sockaddr_in, sockaddr_in6,
};
use teseladb_tls::{ClientConfig, Ticket};

use super::tls_stream::{self, TlsSocket};
use super::{blocking, within};

const AF_INET: u16 = 2;
const AF_INET6: u16 = 10;

/// The engine's socket address of `addr`.
pub fn socket_address(addr: SocketAddr) -> SocketAddress {
    match addr {
        SocketAddr::V4(a) => SocketAddress::from_sockaddr_in(&sockaddr_in {
            sin_family: AF_INET,
            sin_port: a.port().to_be(),
            sin_addr: in_addr { s_addr: u32::from_ne_bytes(a.ip().octets()) },
            sin_zero: [0; 8],
        }),
        SocketAddr::V6(a) => SocketAddress::from_sockaddr_in6(&sockaddr_in6 {
            sin6_family: AF_INET6,
            sin6_port: a.port().to_be(),
            sin6_flowinfo: a.flowinfo().to_be(),
            sin6_addr: in6_addr { s6_addr: a.ip().octets() },
            sin6_scope_id: a.scope_id(),
        }),
    }
}

/// A TCP connection to `addr`, made within `timeout`; past it the attempt is cancelled and the
/// result fails with [`io::ErrorKind::TimedOut`].
pub fn connect(addr: SocketAddr, timeout: Duration) -> Future<ConnectedSocket> {
    let sock = Rc::new(RefCell::new(make_socket()));
    let attempt =
        sock.borrow_mut().connect(socket_address(addr), SocketAddress::default(), Transport::TCP);
    let keep = Rc::clone(&sock);
    let attempt = attempt.finally(move || drop(keep));
    within(timeout, attempt, format!("{addr}: connection timed out"), move || {
        // Cancels the attempt still in flight.
        let _ = sock.borrow_mut().shutdown();
    })
}

/// A TCP connection to `host:port`: the name resolved within `timeout`, then each of its
/// addresses tried in order, each within `timeout`. Fails with the last address's error.
pub fn dial(host: &str, port: u16, timeout: Duration) -> Future<ConnectedSocket> {
    let lookup = blocking::resolve(host, port, timeout);
    let host = host.to_string();
    make_coroutine(async move {
        let addrs = lookup.await?;
        let mut last = None;
        for addr in addrs {
            match connect(addr, timeout).await {
                Ok(sock) => return Ok(sock),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| {
            ExceptionPtr::from(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{host}: no address"),
            ))
        }))
    })
}

/// What a TLS client offers: its settings, the name it checks the server's certificate against
/// and sends by SNI (none between nodes), and a ticket of an earlier session.
#[derive(Clone, Debug)]
pub struct TlsClient {
    /// The client's settings.
    pub config: Arc<ClientConfig>,
    /// The server name; `None` accepts any certificate of the trusted anchors.
    pub server_name: Option<String>,
    /// A ticket to resume an earlier session with.
    pub resume: Option<Ticket>,
}

/// [`dial`], then a TLS handshake as `tls` says within `timeout`.
pub fn dial_tls(host: &str, port: u16, timeout: Duration, tls: TlsClient) -> Future<TlsSocket> {
    let sock = dial(host, port, timeout);
    make_coroutine(async move {
        let sock = sock.await?;
        let TlsClient { config, server_name, resume } = tls;
        tls_stream::client(sock, config, server_name.as_deref(), resume, Some(timeout)).await
    })
}
