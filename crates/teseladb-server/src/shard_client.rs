//! The node's HTTP/1.1 clients on the engine (023 FR-022, FR-023): `teseladb admin` and the
//! backup commands against the administration port, `teseladb mcp` against `/mcp` of the HTTP
//! port, and the OCSP stapler against a responder. A request is one exchange on its own
//! connection (`Connection: close`): the name resolved off the shard, the connection made with the
//! engine's network stack, TLS over it with the engine's TLS stream when asked, the request
//! written and the answer read to its end, each step under its deadline on the engine's timers.
//!
//! A command of the command line is not a shard: [`block_on`] runs an exchange on the engine of
//! the process, starting one of a single shard the first time when none runs (D4: one engine per
//! process).

use std::cell::RefCell;
use std::io;
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future};
use teseladb::engine::core::reactor::engine_is_ready;
use teseladb::engine::core::reactor::internal_misc::run_in_background;
use teseladb::engine::net::api::{ConnectedSocket, ConnectedSocketInputStreamConfig};
use teseladb_http::http1::{Limits, Response, ResponseReader};
use teseladb_io::engine::connect::{TlsClient, dial, dial_tls};
use teseladb_tls::ClientConfig;

use crate::shard_port::{bounded, packet};

/// Where an exchange goes and how.
#[derive(Clone, Debug)]
pub struct Target {
    /// Host name or address.
    pub host: String,
    /// Port.
    pub port: u16,
    /// TLS; the server's certificate has to name `host`.
    pub tls: Option<Arc<ClientConfig>>,
    /// How long the lookup, the connection and the handshake may each take.
    pub connect: Duration,
    /// How long the answer may take once the request is sent.
    pub answer: Duration,
}

/// Splits `addr` (`host:port` or `[v6]:port`) into its host and port.
///
/// # Errors
///
/// When it has no port, or the port is not a number.
pub fn split_addr(addr: &str) -> io::Result<(String, u16)> {
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, format!("{addr}: not host:port"));
    let (host, port) = addr.rsplit_once(':').ok_or_else(bad)?;
    let port = port.parse::<u16>().map_err(|_| bad())?;
    let host = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).unwrap_or(host);
    Ok((host.to_owned(), port))
}

fn invalid(e: impl std::fmt::Debug) -> ExceptionPtr {
    ExceptionPtr::from(io::Error::new(io::ErrorKind::InvalidData, format!("{e:?}")))
}

/// Sends `request` (whole HTTP/1.1 bytes) to `target` and reads its answer: whole once its
/// length is read, or at the server's close for an answer that ends with the connection.
///
/// # Errors
///
/// A failed future with the lookup's, the connection's, the handshake's or the answer's error;
/// an answer that is not HTTP fails with [`io::ErrorKind::InvalidData`].
pub fn exchange(target: Target, request: Vec<u8>) -> Future<Response> {
    let sock = match &target.tls {
        None => dial(&target.host, target.port, target.connect),
        Some(config) => {
            let tls = TlsClient {
                config: config.clone(),
                server_name: Some(target.host.clone()),
                resume: None,
            };
            dial_tls(&target.host, target.port, target.connect, tls)
                .then(|t| -> Result<ConnectedSocket, ExceptionPtr> { Ok(t.socket) })
        }
    };
    let answer = target.answer;
    make_coroutine(async move {
        let sock = Rc::new(RefCell::new(sock.await?));
        let (mut input, mut output) = {
            let mut s = sock.borrow_mut();
            let input = s.input(ConnectedSocketInputStreamConfig::default()).detach()?;
            (input, s.output(8 << 10).detach()?)
        };
        let read = make_coroutine(async move {
            output.put(packet(request)).await?;
            let mut reader = ResponseReader::new(Limits::default());
            let response = loop {
                let b = input.get().await?;
                if b.empty() {
                    break reader.finish().map_err(invalid)?;
                }
                if let Some(r) = reader.feed(b.as_slice()).map_err(invalid)? {
                    break r;
                }
            };
            // The output closes after the answer: closing it first would end a TLS session
            // the server still writes on.
            let _ = output.close().await;
            Ok(response)
        });
        bounded(&sock, answer, "the answer timed out", read).await
    })
}

/// Runs `f` on shard 0 of the process's engine from a thread that is not a shard, and waits for
/// its result. Without an engine in the process it starts the client engine of one shard
/// ([`teseladb_io::engine::lease_engine`]), which gives its turn up to a node engine. In an engine
/// thread (a handler of the node's ports) `f` runs on its own shard and the thread parks until
/// its future resolves (023 FR-018).
///
/// # Errors
///
/// When called on a shard outside an engine thread (it would block it), when the engine does not
/// start, or with the error `f`'s future failed with.
pub fn block_on<T: Send + 'static>(
    f: impl FnOnce() -> Future<T> + Send + 'static,
) -> io::Result<T> {
    if teseladb_io::engine::wait::in_engine_thread() {
        let (tx, rx) = teseladb_io::engine::answer::channel::<Result<T, ExceptionPtr>>();
        run_in_background(f().then_wrapped(move |r: Future<T>| {
            let _ = tx.send(r.get());
            teseladb::engine::core::future::make_ready_future(())
        }));
        return match rx.recv() {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(to_io(&e)),
            Err(_) => Err(io::Error::other("the engine stopped")),
        };
    }
    if engine_is_ready() {
        return Err(io::Error::other("a shard cannot wait for an exchange"));
    }
    // A unit test runs its exchanges on an engine of its own (`test_engine::scoped`), never on
    // a client engine. Elsewhere the client engine takes its turn with the process's node
    // engines (D4): a node engine that starts waits for the exchanges under way on it, and one
    // that stops makes the exchanges posted to it fail instead of reaching no engine.
    let lease = teseladb_io::engine::lease_engine(cfg!(not(test)))?;
    let (tx, rx) = mpsc::sync_channel::<Result<T, ExceptionPtr>>(1);
    let posted = lease.post(move || {
        run_in_background(f().then_wrapped(move |r: Future<T>| {
            let _ = tx.send(r.get());
            teseladb::engine::core::future::make_ready_future(())
        }));
    });
    if !posted {
        return Err(io::Error::other("the engine stopped"));
    }
    let answer = rx.recv();
    drop(lease);
    match answer {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(to_io(&e)),
        Err(_) => Err(io::Error::other("the engine stopped")),
    }
}

/// The [`io::Error`] an engine failure carries, or one with its text.
#[must_use]
pub fn to_io(e: &ExceptionPtr) -> io::Error {
    match e.downcast_ref::<io::Error>() {
        Some(io) => io::Error::new(io.kind(), io.to_string()),
        None => io::Error::other(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_splits_into_host_and_port() {
        assert_eq!(split_addr("h:5").unwrap(), ("h".to_owned(), 5));
        assert_eq!(split_addr("[::1]:27080").unwrap(), ("::1".to_owned(), 27_080));
        assert_eq!(split_addr("h").unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(split_addr("h:x").unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}
