//! A conversation of requests and answers with a server over an engine connection, plain or under
//! TLS (023 FR-024): the node's LDAP, KMIP and HTTPS clients send their protocol's bytes with
//! [`Link::send`] and feed what [`Link::recv`] gives to their sans-I/O state machines.
//!
//! Each step (one send, one receive) has the link's `timeout`, as the blocking clients' socket
//! timeouts had. A step the deadline overtakes shuts the socket down, waits for the operation to
//! end (it fails at once) and fails with [`io::ErrorKind::TimedOut`]: no operation of the
//! connection outlives the step, so the link can be dropped after any error.

use std::cell::{Cell, RefCell};
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::iostream::{InputStream, OutputStream};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::timer::Timer;
use teseladb::engine::net::api::{ConnectedSocket, ConnectedSocketInputStreamConfig};

use super::connect::{self, TlsClient};
use super::tls_stream::{self, TlsSession};

/// Bytes the output stream holds before it sends them.
const OUTPUT_BUFFER: usize = 16 << 10;

/// One connection to a server and its two streams.
pub struct Link {
    sock: Rc<RefCell<ConnectedSocket>>,
    input: InputStream<u8>,
    output: OutputStream<u8>,
    timeout: Duration,
    session: Option<TlsSession>,
}

impl std::fmt::Debug for Link {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Link")
            .field("timeout", &self.timeout)
            .field("tls", &self.session.is_some())
            .finish_non_exhaustive()
    }
}

/// Shuts the socket down when a step takes longer than its limit, and says whether it did.
struct Deadline {
    timer: Pin<Box<Timer>>,
    fired: Rc<Cell<bool>>,
}

impl Deadline {
    fn new(sock: &Rc<RefCell<ConnectedSocket>>, limit: Duration) -> Deadline {
        let sock = Rc::downgrade(sock);
        let fired = Rc::new(Cell::new(false));
        let flag = Rc::clone(&fired);
        let mut timer = Box::pin(Timer::with_callback(move || {
            flag.set(true);
            if let Some(s) = sock.upgrade()
                && let Ok(mut s) = s.try_borrow_mut()
            {
                s.shutdown_input();
                s.shutdown_output();
            }
        }));
        timer.as_mut().arm_in(limit);
        Deadline { timer, fired }
    }

    /// Disarms the timer: `result` when it had not fired, a timeout error naming `what` when it
    /// had.
    fn end<T>(mut self, result: Result<T, ExceptionPtr>, what: &str) -> Result<T, ExceptionPtr> {
        self.timer.as_mut().cancel();
        if self.fired.get() {
            let e = io::Error::new(io::ErrorKind::TimedOut, format!("{what} timed out"));
            return Err(ExceptionPtr::from(e));
        }
        result
    }
}

impl Link {
    /// A link over `socket` (its plaintext side when `session` is a TLS one).
    pub fn new(
        mut socket: ConnectedSocket,
        session: Option<TlsSession>,
        timeout: Duration,
    ) -> Link {
        // Requests are small and the client waits for each answer; Nagle would hold them.
        let _ = socket.set_nodelay(true);
        let input = socket.input(ConnectedSocketInputStreamConfig::default());
        let output = socket.output(OUTPUT_BUFFER);
        Link { sock: Rc::new(RefCell::new(socket)), input, output, timeout, session }
    }

    /// A link to `addr`, with a TLS handshake when `tls` says so; connecting and the handshake
    /// each within `timeout`, which each later step keeps.
    pub async fn open(
        addr: SocketAddr,
        tls: Option<TlsClient>,
        timeout: Duration,
    ) -> Result<Link, ExceptionPtr> {
        let sock = connect::connect(addr, timeout).await?;
        Link::over(sock, tls, timeout).await
    }

    /// A link to `host:port`: the name resolved off the shard, each address tried in turn, then
    /// the TLS handshake when `tls` says so, each within `timeout`.
    pub async fn dial(
        host: &str,
        port: u16,
        tls: Option<TlsClient>,
        timeout: Duration,
    ) -> Result<Link, ExceptionPtr> {
        let sock = connect::dial(host, port, timeout).await?;
        Link::over(sock, tls, timeout).await
    }

    async fn over(
        sock: ConnectedSocket,
        tls: Option<TlsClient>,
        timeout: Duration,
    ) -> Result<Link, ExceptionPtr> {
        let Some(TlsClient { config, server_name, resume }) = tls else {
            return Ok(Link::new(sock, None, timeout));
        };
        let name = server_name.as_deref();
        let t = tls_stream::client(sock, config, name, resume, Some(timeout)).await?;
        Ok(Link::new(t.socket, Some(t.session), timeout))
    }

    /// Sends `bytes` whole.
    pub async fn send(&mut self, bytes: &[u8]) -> Result<(), ExceptionPtr> {
        let deadline = Deadline::new(&self.sock, self.timeout);
        let wrote = self.output.write(bytes).await;
        let sent = match wrote {
            Ok(()) => self.output.flush().await,
            Err(e) => Err(e),
        };
        deadline.end(sent, "a send")
    }

    /// The next bytes from the server, in the stream's own buffer; empty once it closed the
    /// connection.
    pub async fn recv(&mut self) -> Result<TemporaryBuffer<u8>, ExceptionPtr> {
        let deadline = Deadline::new(&self.sock, self.timeout);
        let got = self.input.read().await;
        deadline.end(got, "a read")
    }

    /// Ends the conversation: the output is closed (over TLS with `close_notify`), then the
    /// input; failures are not reported, the server may have gone first.
    pub async fn close(mut self) {
        let deadline = Deadline::new(&self.sock, self.timeout);
        let _ = self.output.close().await;
        let _ = self.input.close().await;
        let _ = deadline.end(Ok(()), "a close");
    }
}
