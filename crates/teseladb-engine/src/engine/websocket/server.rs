//! The WebSocket server.
//!
//! [`websocket::Server`] listens on its sockets, and for each accepted connection a
//! [`websocket::ServerConnection`] reads the HTTP upgrade request, checks the WebSocket
//! headers, answers the handshake (with the subprotocol when one was asked for and is
//! registered) and then runs the registered handler over the connection's streams. The
//! server lives on one shard; it is `!Unpin` because its accept loops and connections
//! point at it, and `stop()` aborts the accept loops, shuts down the input of every
//! connection and waits for them to finish.

/// The server and its connections (interface still subject to change).
pub mod websocket {
    use std::collections::BTreeMap;
    use std::marker::PhantomPinned;
    use std::pin::Pin;

    use crate::engine::core::api::intrusive::{List, ListHook};
    use crate::engine::core::coroutine::make_coroutine;
    use crate::engine::core::future::{
        ExceptionPtr, Future, make_exception_future, make_ready_future,
    };
    use crate::engine::core::gate::{Gate, GateClosedException, try_with_gate};
    use crate::engine::core::iostream::ObsoleteConsumer;
    use crate::engine::core::loop_::{StopIteration, do_until, parallel_for_each, repeat};
    use crate::engine::core::temporary_buffer::TemporaryBuffer;
    use crate::engine::core::when_all::when_all_succeed;
    use crate::engine::http::common::support::RawBox;
    use crate::engine::http::request_parser::HttpRequestParser;
    use crate::engine::net::api::{AcceptResult, ConnectedSocket, ListenOptions, ServerSocket};
    use crate::engine::net::socket_defs::SocketAddress;
    use crate::engine::websocket::common::websocket::{
        Connection, Exception, HandlerT, MAGIC_KEY_SUFFIX, WEBSOCKET_LOGGER, sha1_base64,
    };
    use crate::{log_debug, log_error, log_info};

    #[cfg(target_os = "linux")]
    const ECONNABORTED: i32 = 103;
    #[cfg(not(target_os = "linux"))]
    const ECONNABORTED: i32 = 53;

    /// The reply to the upgrade request, up to the accept key.
    const HTTP_UPGRADE_REPLY_TEMPLATE: &str = "HTTP/1.1 101 Switching Protocols\r\n\
        Upgrade: websocket\r\n\
        Connection: Upgrade\r\n\
        Sec-WebSocket-Version: 13\r\n\
        Sec-WebSocket-Accept: ";

    /// A WebSocket connection accepted by a [`Server`].
    pub struct ServerConnection {
        /// The connection this server connection drives.
        pub conn: Connection,
        /// The hook that links the connection into the server's intrusive list.
        _hook: ListHook,
        _server: *mut Server,
        _http_parser: HttpRequestParser,
    }

    crate::intrusive_adapter!(ServerConnectionAdapter = ServerConnection { _hook });

    /// A server connection extends a [`Connection`]: its members are reached through it.
    impl core::ops::Deref for ServerConnection {
        type Target = Connection;
        #[inline]
        fn deref(&self) -> &Connection {
            &self.conn
        }
    }

    impl core::ops::DerefMut for ServerConnection {
        #[inline]
        fn deref_mut(&mut self) -> &mut Connection {
            &mut self.conn
        }
    }

    impl ServerConnection {
        /// Creates a connection of the server `server` over the established socket `fd`.
        ///
        /// The connection lives on the heap at a fixed address (the server's list, its own
        /// streams and the continuations point at it), owned through that address.
        ///
        /// # Safety
        /// `server` is pinned and outlives the connection (it waits for its connections in
        /// `stop()` through the task gate).
        pub unsafe fn new(server: *mut Server, fd: ConnectedSocket) -> RawBox<Self> {
            let c = RawBox::new(Box::new(ServerConnection {
                conn: Connection::new(fd),
                _hook: ListHook::new(),
                _server: server,
                _http_parser: HttpRequestParser::default(),
            }));
            // SAFETY: the connection was just allocated at its final address and is reached
            // only through it; the server's contract is the caller's.
            unsafe { (*c.as_ptr()).on_new_connection() };
            c
        }

        /// Serves the WebSocket protocol on this connection: runs its read loop and its
        /// response loop until both end.
        ///
        /// # Safety
        /// The connection stays alive and at its address (the one [`ServerConnection::new`]
        /// gave it) until the returned future resolves: the loops reach it through that
        /// address.
        pub unsafe fn process(&mut self) -> Future<()> {
            // SAFETY: the contract above covers both loops.
            let read = unsafe { self.read_loop() };
            // SAFETY: as above.
            let respond = unsafe { self.conn.response_loop() };
            when_all_succeed((read, respond)).discard_result().handle_exception(
                |e: ExceptionPtr| {
                    log_debug!(WEBSOCKET_LOGGER, "Processing failed: {}", e);
                },
            )
        }

        /// Reads the upgrade request, then runs the handler and the frame read loop side by
        /// side. The handler uses the streams of the connection's handler block and the loop
        /// borrows the connection apart from that block, so their borrows never overlap.
        ///
        /// # Safety
        /// As [`ServerConnection::process`].
        unsafe fn read_loop(&mut self) -> Future<()> {
            let this: *mut ServerConnection = self;
            // Every closure below relies on the connection outliving its processing, which
            // awaits this loop, at a fixed heap address.
            // SAFETY: the contract of this function.
            unsafe { self.read_http_upgrade_request() }
                .then(move |()| {
                    // SAFETY: the connection outlives its processing; the upgrade has finished.
                    let c = unsafe { &mut *this };
                    let handler = match c.conn._handler.clone() {
                        Some(h) => h,
                        None => return make_ready_future(()),
                    };
                    // SAFETY: the connection, and so its handler block, outlives its processing;
                    // the read loop below borrows the connection apart from that block and
                    // reaches the handler's streams only through their states, to close them.
                    let handled = unsafe { c.conn.run_handler(&handler) };
                    when_all_succeed((
                        handled.handle_exception(move |e: ExceptionPtr| {
                            // SAFETY: the connection outlives its processing.
                            unsafe { (*this).conn._read_buf.close() }
                                .then(move |()| make_exception_future::<()>(e))
                        }),
                        do_until(
                            // SAFETY: the connection outlives its processing; the condition and the
                            // body run one at a time.
                            move || unsafe { (*this).conn._done },
                            // SAFETY: as above.
                            move || unsafe { (*this).conn.read_one() },
                        ),
                    ))
                    .discard_result()
                })
                // SAFETY: the connection outlives its processing; the loops have ended.
                .finally(move || unsafe { (*this).conn._read_buf.close() })
        }

        /// Reads the upgrade request and answers the handshake.
        ///
        /// # Safety
        /// As [`ServerConnection::process`].
        unsafe fn read_http_upgrade_request(&mut self) -> Future<()> {
            let this: *mut ServerConnection = self;
            // The response loop and the handler use the connection while this coroutine is
            // suspended, so no reference to it lives across an await: each step reaches it
            // through `this` again.
            //
            // Every `unsafe { &mut *this }` below is sound because the connection is heap-allocated
            // at a fixed address and outlives its processing, which awaits this coroutine;
            // each reference ends before the next await.
            make_coroutine(async move {
                let consumed = {
                    // SAFETY: the connection outlives its processing at a fixed address; this reference ends before the next await.
                    let c = unsafe { &mut *this };
                    c._http_parser.init();
                    let parser: *mut HttpRequestParser = &mut c._http_parser;
                    // SAFETY: the parser is a member of the connection (see above).
                    c.conn._read_buf.consume(ObsoleteConsumer(
                        move |buf: TemporaryBuffer<u8>| unsafe { (*parser).call(buf) },
                    ))
                };
                consumed.await?;

                let (sha1_output, has_subprotocol) = {
                    // SAFETY: the connection outlives its processing at a fixed address; this reference ends before the next await.
                    let c = unsafe { &mut *this };
                    if c._http_parser.eof() {
                        c.conn._done = true;
                        return Ok(());
                    }
                    let req = c._http_parser.get_parsed_request();
                    if c._http_parser.failed() {
                        return Err(ExceptionPtr::new(Exception::new("Incorrect upgrade request")));
                    }
                    let Some(req) = req else {
                        return Err(ExceptionPtr::new(Exception::new("Incorrect upgrade request")));
                    };

                    let upgrade_header = req.get_header("Upgrade");
                    if upgrade_header.as_bytes() != b"websocket" {
                        return Err(ExceptionPtr::new(Exception::new("Upgrade header missing")));
                    }

                    let subprotocol = req.get_header("Sec-WebSocket-Protocol");

                    // SAFETY: the server outlives its connections (the constructor's contract).
                    let server = unsafe { &*c._server };
                    if !server.is_handler_registered(subprotocol.as_str()) {
                        return Err(ExceptionPtr::new(Exception::new(
                            "Subprotocol not supported.",
                        )));
                    }
                    c.conn._handler = server._handlers.get(subprotocol.as_str()).cloned();
                    c.conn._subprotocol = subprotocol.clone();
                    log_debug!(WEBSOCKET_LOGGER, "Sec-WebSocket-Protocol: {}", subprotocol);

                    let sec_key = req.get_header("Sec-Websocket-Key");
                    let sec_version = req.get_header("Sec-Websocket-Version");

                    let mut sha1_input = sec_key.clone();
                    sha1_input.append_bytes(MAGIC_KEY_SUFFIX.as_bytes());

                    log_debug!(
                        WEBSOCKET_LOGGER,
                        "Sec-Websocket-Key: {}, Sec-Websocket-Version: {}",
                        sec_key,
                        sec_version
                    );

                    let sha1_output = sha1_base64(sha1_input.as_bytes())?;
                    log_debug!(
                        WEBSOCKET_LOGGER,
                        "SHA1 output: {} of size {}",
                        String::from_utf8_lossy(&sha1_output),
                        sha1_output.len()
                    );
                    (sha1_output, !c.conn._subprotocol.empty())
                };

                // SAFETY: the connection outlives its processing at a fixed address; this reference ends before the next await.
                unsafe { &mut *this }
                    .conn
                    ._write_buf
                    .write_str(HTTP_UPGRADE_REPLY_TEMPLATE)
                    .await?;
                // SAFETY: the connection outlives its processing at a fixed address; this reference ends before the next await.
                unsafe { &mut *this }.conn._write_buf.write(&sha1_output).await?;
                if has_subprotocol {
                    // SAFETY: the connection outlives its processing at a fixed address; this reference ends before the next await.
                    unsafe { &mut *this }
                        .conn
                        ._write_buf
                        .write(b"\r\nSec-WebSocket-Protocol: ")
                        .await?;
                    let write = {
                        // SAFETY: the connection outlives its processing at a fixed address; this reference ends before the next await.
                        let c = unsafe { &mut *this };
                        let sp = c.conn._subprotocol.clone();
                        c.conn._write_buf.write_sstring(&sp)
                    };
                    write.await?;
                }
                // SAFETY: the connection outlives its processing at a fixed address; this reference ends before the next await.
                unsafe { &mut *this }.conn._write_buf.write(b"\r\n\r\n").await?;
                // SAFETY: the connection outlives its processing at a fixed address; this reference ends before the next await.
                unsafe { &mut *this }.conn._write_buf.flush().await?;
                Ok(())
            })
        }

        fn on_new_connection(&mut self) {
            let this: *mut ServerConnection = self;
            // SAFETY: the connection is heap-allocated at a fixed address (RawBox) and unlinks
            // itself in drop(); the server outlives it (the constructor's contract).
            unsafe { (*self._server)._connections.push_back(this) };
        }
    }

    impl Drop for ServerConnection {
        fn drop(&mut self) {
            self._hook.unlink();
        }
    }

    /// A WebSocket server: accepts connections and serves the WebSocket protocol on them.
    ///
    /// The accept loops and the connections point at the server, so it must not move: it is
    /// `!Unpin` and `listen` takes it pinned.
    #[derive(Default)]
    pub struct Server {
        _listeners: Vec<ServerSocket>,
        _connections: List<ServerConnectionAdapter>,
        _handlers: BTreeMap<String, HandlerT>,
        _task_gate: Gate,
        /// The stops whose last step has not run yet; they reach the server through its
        /// address.
        _pending_stops: std::cell::Cell<u32>,
        _pinned: PhantomPinned,
    }

    impl Drop for Server {
        /// A server dropped while a stop of it is pending would leave the stop's last step
        /// writing into freed memory, so it aborts instead (as its gate does for pending
        /// accepts and connections).
        fn drop(&mut self) {
            if self._pending_stops.get() != 0 {
                eprintln!("websocket server destroyed while its stop() is pending");
                std::process::abort();
            }
        }
    }

    impl Server {
        /// Listens for WebSocket connections on `addr` with the listen options `lo`, and
        /// starts accepting them.
        ///
        /// # Errors
        ///
        /// Returns an error if listening on `addr` fails.
        pub fn listen_opts(
            self: Pin<&mut Self>,
            addr: SocketAddress,
            lo: ListenOptions,
        ) -> Result<(), ExceptionPtr> {
            // SAFETY: the server is not moved; the accept loop keeps a pointer to its pinned
            // place, which its gate keeps alive until stop() resolves.
            let me = unsafe { self.get_unchecked_mut() };
            me._listeners.push(crate::engine::core::api::listen_with(addr, lo)?);
            let last = me._listeners.len() - 1;
            me.accept(last);
            Ok(())
        }

        /// Listens for WebSocket connections on `addr`, with address reuse enabled, and
        /// starts accepting them.
        ///
        /// # Errors
        ///
        /// Returns an error if listening on `addr` fails.
        pub fn listen(self: Pin<&mut Self>, addr: SocketAddress) -> Result<(), ExceptionPtr> {
            let lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
            self.listen_opts(addr, lo)
        }

        /// Stops accepting connections and shuts down all active ones; the returned future
        /// resolves once they have all finished.
        pub fn stop(&mut self) -> Future<()> {
            for l in &mut self._listeners {
                l.abort_accept();
            }

            for c in self._connections.iter() {
                // SAFETY: the linked connections are alive (they unlink in drop()).
                unsafe { (*c.as_ptr()).conn.shutdown_input() };
            }

            // The stop counts as pending until its last step has used the server: dropping
            // the server before then aborts (see `Drop for Server`).
            self._pending_stops.set(self._pending_stops.get() + 1);
            let this: *mut Server = self;
            self._task_gate.close().finally(move || {
                // SAFETY: the server aborts instead of going while this stop is pending, so
                // it is alive here; the step only reads its list.
                let conns = unsafe { (*this)._connections.iter() }.map(|c| c.as_ptr());
                parallel_for_each(conns, |conn: *mut ServerConnection| {
                    // SAFETY: the connections are alive until their processing ends, which
                    // waits for their close.
                    unsafe { (*conn).conn.close(true) }
                        .handle_exception(|_ignored: ExceptionPtr| {})
                })
                .finally(move || {
                    // SAFETY: the stop is still pending, so the server is alive.
                    let stops = unsafe { &(*this)._pending_stops };
                    stops.set(stops.get() - 1);
                })
            })
        }

        /// Whether a handler is registered for subprotocol `name`.
        pub fn is_handler_registered(&self, name: &str) -> bool {
            self._handlers.contains_key(name)
        }

        /// Registers `handler` as the handler of incoming WebSocket messages for the
        /// subprotocol `name`.
        ///
        /// With an empty `name`, the handler serves connections that specify no
        /// subprotocol.
        pub fn register_handler(&mut self, name: &str, handler: HandlerT) {
            self._handlers.insert(name.to_string(), handler);
        }

        fn accept(&mut self, listener: usize) {
            let this: *mut Server = self;
            // SAFETY: the gate keeps the server alive until stop() resolves.
            // The gate is a member of the pinned server, and every accept runs inside it.
            let _ = try_with_gate(
                unsafe { Pin::new_unchecked(&(*this)._task_gate) },
                // SAFETY: as above, the gate keeps the server alive for every accept.
                move || repeat(move || unsafe { (*this).accept_one(listener) }),
            )
            .handle_exception_type(|_e: &GateClosedException| {});
        }

        fn accept_one(&mut self, listener: usize) -> Future<StopIteration> {
            let this: *mut Server = self;
            self._listeners[listener]
                .accept()
                .then(move |ar: AcceptResult| {
                    // SAFETY: the gate keeps the server alive while it accepts.
                    // SAFETY: the server is pinned and its gate keeps it alive until the
                    // connection's processing ends.
                    let conn = unsafe { ServerConnection::new(this, ar.connection) };
                    // If the gate is closed the closure is dropped unrun, and with it the
                    // connection.
                    // SAFETY: the gate is a member of the pinned server.
                    let _ = try_with_gate(
                        unsafe { Pin::new_unchecked(&(*this)._task_gate) },
                        move || {
                            let c: *mut ServerConnection = conn.as_ptr();
                            // SAFETY: `conn` owns the connection at its address and is moved into the
                            // continuation that ends its life, after the processing.
                            unsafe { (*c).process() }.finally(move || {
                                drop(conn);
                                log_debug!(WEBSOCKET_LOGGER, "Connection is finished");
                            })
                        },
                    )
                    .handle_exception_type(|_e: &GateClosedException| {});
                    make_ready_future(StopIteration::NO)
                })
                // A system error from accept is a `SystemError` (reactor) or an
                // `std::io::Error`; both are handled the same way.
                .handle_exception_type(|e: &crate::engine::core::future::SystemError| {
                    // We expect a ECONNABORTED when server::stop is called,
                    // no point in warning about that.
                    if e.code() != ECONNABORTED {
                        log_error!(WEBSOCKET_LOGGER, "accept failed: {}", e);
                    }
                    make_ready_future(StopIteration::YES)
                })
                .handle_exception_type(|e: &std::io::Error| {
                    // We expect a ECONNABORTED when server::stop is called,
                    // no point in warning about that.
                    if e.raw_os_error() != Some(ECONNABORTED) {
                        log_error!(WEBSOCKET_LOGGER, "accept failed: {}", e);
                    }
                    make_ready_future(StopIteration::YES)
                })
                .handle_exception(|ex: ExceptionPtr| {
                    log_info!(WEBSOCKET_LOGGER, "accept failed: {}", ex);
                    make_ready_future(StopIteration::YES)
                })
        }
    }

    // }@
}
