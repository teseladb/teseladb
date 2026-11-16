//! The administration port on the engine's shards (023 FR-022, 007 FR-020): each connection is
//! a task of the shard that took it, with the port's limits (fields up to
//! [`super::MAX_HEAD`], a body up to [`super::MAX_BODY`], [`IO_TIMEOUT`] to send the request
//! and to read the answer) and one request per connection. On a node with a PKI the first byte chooses TLS (the engine's TLS stream) or
//! plain bytes, and each [`Caller`] is named by its client certificate (013 FR-017).
//!
//! The client side ([`request_with`]) is one exchange of [`crate::shard_client`].

use std::cell::RefCell;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::iostream::DataSource;
use teseladb::engine::net::api::{ConnectedSocket, ConnectedSocketInputStreamConfig};
use teseladb_http::http1::Request;
use teseladb_tls::ClientConfig;

use super::{
    ANSWER_WAIT, AdminTls, Caller, HttpRequest, HttpResponse, IO_TIMEOUT, Method, parse_request,
};
use crate::shard_client::{self, Target};
use crate::shard_port::{Link, Port, PortConfig, bounded, packet};
use crate::shard_tls::{Policy, ShardTls};

/// What answers the port's requests on one shard.
pub type Handler = Rc<dyn Fn(HttpRequest, Caller) -> Future<HttpResponse>>;

/// Makes a shard's handler; it runs on that shard.
pub type MakeHandler = Arc<dyn Fn() -> Handler + Send + Sync>;

/// Most connections open at once on the node's administration port.
pub const MAX_CONNECTIONS: usize = 1_000;

/// What the reader asks of the socket at a time.
const INPUT: ConnectedSocketInputStreamConfig = ConnectedSocketInputStreamConfig {
    buffer_size: 4096,
    min_buffer_size: 512,
    max_buffer_size: 64 << 10,
};

/// The administration port running on every shard.
#[derive(Clone, Debug)]
pub struct AdminPort {
    port: Port,
    tls: Option<ShardTls>,
}

impl AdminPort {
    /// Listens on `addr` on every shard and answers with the handler `make` gives each shard,
    /// over TLS with `tls`. Runs on shard 0.
    ///
    /// # Errors
    ///
    /// A failed future when the address does not listen.
    pub fn start(
        addr: SocketAddr,
        make: MakeHandler,
        tls: Option<AdminTls>,
    ) -> Future<AdminPort> {
        let shard_tls = tls.as_ref().map(|t| ShardTls::new(&t.port, None, Policy::FirstByte));
        let cfg = PortConfig {
            addrs: vec![addr],
            tls: shard_tls.clone().unwrap_or_else(ShardTls::plain),
            handshake: IO_TIMEOUT,
            max_connections: MAX_CONNECTIONS,
        };
        let handler = Arc::new(move || {
            let (handler, tls) = (make(), tls.clone().map(Rc::new));
            let h: crate::shard_port::Handler =
                Rc::new(move |link| serve(link, handler.clone(), tls.clone()));
            h
        });
        Port::start(cfg, handler).then(move |port| -> Result<AdminPort, ExceptionPtr> {
            Ok(AdminPort { port, tls: shard_tls })
        })
    }

    /// The address it listens on.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.port.addrs()[0]
    }

    /// Its TLS on the shards, which a rotation publishes to; `None` on a node without a PKI.
    #[must_use]
    pub fn tls(&self) -> Option<&ShardTls> {
        self.tls.as_ref()
    }

    /// Stops listening; resolves once the open connections ended.
    pub fn stop(&self) -> Future<()> {
        self.port.stop()
    }
}

/// Reads one request (within [`IO_TIMEOUT`]); `None` when the peer closed before it was whole.
async fn read_request(
    sock: &Rc<RefCell<ConnectedSocket>>,
    mut input: DataSource,
) -> Result<Option<Result<HttpRequest, HttpResponse>>, ExceptionPtr> {
    let read = make_coroutine(async move {
        let mut buf = Vec::new();
        loop {
            let b = input.get().await?;
            if b.empty() {
                return Ok(None);
            }
            buf.extend_from_slice(b.as_slice());
            match parse_request(&buf) {
                Ok(Some(req)) => return Ok(Some(Ok(req))),
                Ok(None) => {}
                Err(e) => return Ok(Some(Err(e.response()))),
            }
        }
    });
    bounded(sock, IO_TIMEOUT, "the request timed out", read).await
}

/// One connection: its request read, answered and the connection closed.
fn serve(link: Link, handler: Handler, tls: Option<Rc<AdminTls>>) -> Future<()> {
    let caller = match (&tls, &link.tls) {
        (None, _) => Caller::Open,
        (Some(t), Some(session)) => t.caller(&session.peer_certificates()),
        (Some(_), None) => Caller::Anonymous,
    };
    let sock = link.socket;
    make_coroutine(async move {
        let (input, mut output) = {
            let mut s = sock.borrow_mut();
            let input = s.input(INPUT).detach()?;
            (input, s.output(8 << 10).detach()?)
        };
        let answer = match read_request(&sock, input).await? {
            None => return Ok(()),
            Some(Err(refused)) => refused,
            // The handler may wait for a core and then for Group 0: no deadline but its own.
            Some(Ok(req)) => handler(req, caller).await?,
        };
        let write = make_coroutine(async move {
            output.put(packet(answer.encode())).await?;
            output.close().await
        });
        bounded(&sock, IO_TIMEOUT, "the answer timed out", write).await
    })
}

/// Sends `method target` with `body` to the administration port at `addr` (`host:port`), over
/// TLS with `tls` (the server's certificate has to name the host); the answer's status and
/// body. Runs on a shard.
pub fn fetch(
    addr: &str,
    tls: Option<Arc<ClientConfig>>,
    method: Method,
    target: &str,
    body: &[u8],
) -> Future<(u16, Vec<u8>)> {
    let request = encode_request(addr, method, target, body);
    let split = shard_client::split_addr(addr);
    make_coroutine(async move {
        let (host, port) = split?;
        let target = Target { host, port, tls, connect: IO_TIMEOUT, answer: ANSWER_WAIT };
        let answer = shard_client::exchange(target, request?).await?;
        Ok((answer.status, answer.body))
    })
}

/// The bytes of a request of the port.
fn encode_request(addr: &str, method: Method, target: &str, body: &[u8]) -> io::Result<Vec<u8>> {
    let verb = match method {
        Method::Get => "GET",
        Method::Post => "POST",
    };
    Request::new(verb, target)
        .header("Host", addr)
        .header("Connection", "close")
        .header("Content-Length", &body.len().to_string())
        .body(body.to_vec())
        .encode()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))
}

/// [`fetch`] from a thread that is not a shard (a command of the command line).
///
/// # Errors
///
/// The connection's, the TLS handshake's or the answer's.
pub fn request_with(
    addr: &str,
    tls: Option<Arc<ClientConfig>>,
    method: Method,
    target: &str,
    body: &[u8],
) -> io::Result<(u16, Vec<u8>)> {
    let (addr, target, body) = (addr.to_owned(), target.to_owned(), body.to_vec());
    shard_client::block_on(move || fetch(&addr, tls, method, &target, &body))
}

/// [`request_with`] without TLS.
///
/// # Errors
///
/// The connection's or the answer's.
pub fn request(
    addr: &str,
    method: Method,
    target: &str,
    body: &[u8],
) -> io::Result<(u16, Vec<u8>)> {
    request_with(addr, None, method, target, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_names_its_host_and_closes() {
        let r = encode_request("h:1", Method::Post, "/a", b"{}").unwrap();
        let text = String::from_utf8(r).unwrap();
        assert!(text.starts_with("POST /a HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("Host: h:1\r\n") && text.contains("Connection: close\r\n"));
        assert!(text.ends_with("\r\n\r\n{}"), "{text}");
    }
}
