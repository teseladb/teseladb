//! The HTTP/1.1 client.
//!
//! [`experimental::Client`] sends requests to one server over a pool of persistent
//! connections, opening new ones through a
//! [`ConnectionFactory`](crate::engine::http::connection_factory::experimental::ConnectionFactory)
//! up to a configurable maximum, and optionally retrying a request once when a pooled
//! connection turns out to be closed by the server. [`experimental::Connection`] is one
//! HTTP connection: it writes a request (with `Expect: 100-continue`, content-length or
//! chunked body) and parses the reply with the response parser.
//!
//! A client and its connections live on one shard. Connections are reference-counted
//! [`SharedPtr`](crate::engine::core::shared_ptr::SharedPtr)s and sit in the client's
//! intrusive pool through an auto-unlink hook, so a connection leaves the pool when it is
//! destroyed. The client must be closed (`close()`) before it is dropped.

use std::sync::LazyLock;

use crate::engine::util::log::Logger;

static HTTP_LOG: LazyLock<std::pin::Pin<Box<Logger>>> =
    LazyLock::new(|| Logger::new("http").expect("logger registered once"));

/// Bookkeeping shared between the client and its connections.
pub mod internal {
    use super::experimental::Client;

    /// Counts a connection of a client while it lives.
    pub struct ClientRef {
        _c: *mut Client,
    }

    impl ClientRef {
        /// Counts one more connection of `c`.
        ///
        /// # Safety
        /// `c` is a pinned client; it outlives the reference because a client aborts when it is
        /// dropped with connections counted.
        #[inline]
        pub unsafe fn new(c: *mut Client) -> Self {
            // SAFETY: the contract above.
            unsafe { (*c)._nr_connections += 1 };
            ClientRef { _c: c }
        }
    }

    impl Drop for ClientRef {
        fn drop(&mut self) {
            if !self._c.is_null() {
                // SAFETY: as in new(); the condition variable has interior state, reached
                // through a shared reference.
                unsafe {
                    (*self._c)._nr_connections -= 1;
                    (*self._c)._wait_con.broadcast();
                }
            }
        }
    }
}

/// The client and its connections (interface still subject to change).
pub mod experimental {
    use super::internal::ClientRef;
    use std::marker::PhantomPinned;
    use std::pin::Pin;

    use super::HTTP_LOG;

    use crate::engine::core::abort_source::{AbortSource, NaiveCallback, Subscription};
    use crate::engine::core::api::intrusive::{List, ListHook};
    use crate::engine::core::condition_variable::ConditionVariable;
    use crate::engine::core::do_with::do_with;
    use crate::engine::core::future::{
        ExceptionPtr, Future, SystemError, make_exception_future, make_ready_future,
    };
    use crate::engine::core::iostream::{DataSource, InputStream, ObsoleteConsumer, OutputStream};
    use crate::engine::core::shared_ptr::{EnableSharedFromThis, SharedPtr, make_shared_esft};
    use crate::engine::core::sstring::Sstring;
    use crate::engine::core::temporary_buffer::TemporaryBuffer;
    use crate::engine::core::when_all::when_all;
    use crate::engine::http::common::http::internal::{
        make_http_chunked_output_stream, make_http_content_length_output_stream,
    };
    use crate::engine::http::common::support::{RawBox, u64_to_sstring};
    use crate::engine::http::connection_factory::experimental::{
        BasicConnectionFactory, ConnectionFactory, TlsConnectionFactory,
    };
    use crate::engine::http::exception::{
        BaseException, ExceptionKind, response_parsing_exception, unexpected_status_error,
    };
    use crate::engine::http::internal::content_source::internal::{
        ChunkTargets, ChunkedSourceImpl, ContentLengthSourceImpl,
    };
    use crate::engine::http::reply::{Reply, StatusType};
    use crate::engine::http::request::Request;
    use crate::engine::http::response_parser::HttpResponseParser;
    use crate::engine::net::api::ConnectedSocket;
    use crate::engine::net::socket_defs::SocketAddress;
    use crate::engine::net::tls::{CertificateCredentials, ERROR_PREMATURE_TERMINATION, TlsError};
    use crate::engine::util::bool_class::BoolClass;
    use crate::engine::util::log::LogLevel;
    use crate::engine::util::noncopyable_function::{NoncopyableFunction, RefFn};
    use crate::engine::util::short_streams::util::read_entire_stream_contiguous;
    use crate::engine::util::string_utils::internal::{CaseInsensitiveCmp, CaseInsensitiveKey};
    use crate::{log_debug, log_trace};

    #[cfg(target_os = "linux")]
    mod errno {
        pub const EPIPE: i32 = 32;
        pub const ECONNABORTED: i32 = 103;
        pub const ECONNRESET: i32 = 104;
    }
    #[cfg(not(target_os = "linux"))]
    mod errno {
        pub const EPIPE: i32 = 32;
        pub const ECONNABORTED: i32 = 53;
        pub const ECONNRESET: i32 = 54;
    }

    type ReplyPtr = Option<Box<Reply>>;

    /// An HTTP connection over a given transport socket.
    pub struct Connection {
        _esft: EnableSharedFromThis,
        _fd: ConnectedSocket,
        _read_buf: InputStream<u8>,
        _write_buf: OutputStream<u8>,
        /// The hook of the client's pool list; it unlinks itself when dropped.
        _hook: ListHook,
        _closed: Future<()>,
        _ref: ClientRef,
        // Client sends HTTP-1.1 version and assumes the server is 1.1-compatible
        // too and thus the connection will be persistent by default. If the server
        // responds with older version, this flag will be dropped (see recv_reply())
        _persistent: bool,
        /// Where the chunked body of the current reply saves its extensions and trailers.
        _chunk_targets: ChunkTargets,
    }

    crate::shared_from_this_impl!(Connection { _esft });

    crate::intrusive_adapter!(ConnectionAdapter = Connection { _hook });

    /// A reference-counted connection, as the client's pool and requests hold it.
    pub type ConnectionPtr = SharedPtr<Connection>;

    #[inline(always)]
    fn conn(p: &ConnectionPtr) -> *mut Connection {
        match p.get() {
            Some(c) => c.as_ptr(),
            None => std::ptr::null_mut(),
        }
    }

    impl Connection {
        /// Creates an HTTP connection over the transport socket `fd`, counted in its client
        /// by `cr`, and returns it in a shared pointer.
        pub fn new(mut fd: ConnectedSocket, cr: ClientRef) -> ConnectionPtr {
            let read_buf =
                fd.input(crate::engine::net::api::ConnectedSocketInputStreamConfig::default());
            let write_buf = fd.output(8192);
            let p = make_shared_esft(Connection {
                _esft: EnableSharedFromThis::new(),
                _fd: fd,
                _read_buf: read_buf,
                _write_buf: write_buf,
                _hook: ListHook::new(),
                _closed: make_ready_future(()),
                _ref: cr,
                _persistent: true,
                _chunk_targets: ChunkTargets::new(),
            });
            let c = conn(&p);
            let me = p.clone();
            // SAFETY: `p` was just made and keeps the connection alive; `me` keeps it alive
            // until the input is shut down.
            unsafe { (*c)._closed = (*c)._fd.wait_input_shutdown().finally(move || drop(me)) };
            p
        }

        /// Sends the request `req` to the server and returns a future that resolves to the
        /// server's reply.
        ///
        /// If the request was set up with [`Request::set_expects_continue`] and the server
        /// replied early with an error status, that early reply is returned.
        ///
        /// The reply holds only the status and the headers; read the body from the stream
        /// returned by [`Connection::in_`].
        ///
        /// # Errors
        ///
        /// The returned future fails if writing the request or reading the reply fails.
        ///
        /// The returned future holds a reference to the connection (and owns the request),
        /// so the connection lives until the reply has arrived even if every other pointer
        /// to it goes first.
        pub fn make_request(&mut self, req: Request) -> Future<Reply> {
            let me = self.shared();
            let this: *mut Connection = conn(&me);
            do_with((req, me), move |st: &mut (Request, ConnectionPtr)| {
                // SAFETY: `st.1` keeps the connection alive until the chain resolves, and
                // do_with keeps the request alive and in place until then; the chain is
                // the only user of both meanwhile.
                unsafe { (*this).do_make_request(&mut st.0) }
                    .then(|rep: ReplyPtr| make_ready_future(rep.map_or_else(Reply::new, |r| *r)))
            })
        }

        /// A new reference to this connection.
        #[inline]
        fn shared(&self) -> ConnectionPtr {
            // SAFETY: every connection lives in the block of `make_shared_esft`: `new` is its
            // only constructor and returns it in a shared pointer, which gives no way to
            // move it out.
            unsafe { SharedPtr::shared_from_this(self) }
        }

        /// Returns an input stream that reads the body of the reply `rep` from the
        /// connection.
        ///
        /// Once the stream reaches its end, `rep` may also have been updated with trailing
        /// headers and chunk extensions.
        ///
        /// # Safety
        /// The connection (it lives in its shared pointer) and `rep` stay alive and in place
        /// until the returned stream is dropped, and `rep`'s maps and counter are not used
        /// elsewhere while one of its reads is pending (the stream keeps raw pointers to them).
        pub unsafe fn in_(&mut self, rep: *mut Reply) -> InputStream<u8> {
            // SAFETY: the contract above.
            unsafe {
                if CaseInsensitiveCmp
                    .call((*rep).get_header("Transfer-Encoding").as_bytes(), b"chunked")
                {
                    self._chunk_targets
                        .set(&mut (*rep).chunk_extensions, &mut (*rep).trailing_headers);
                    return InputStream::new(DataSource::new(Box::new(ChunkedSourceImpl::new(
                        &mut self._read_buf,
                        &self._chunk_targets,
                    ))));
                }

                InputStream::new(DataSource::new(Box::new(ContentLengthSourceImpl::with_counter(
                    &mut self._read_buf,
                    (*rep).content_length,
                    &mut (*rep).consumed_content,
                ))))
            }
        }

        /// Closes the connection.
        ///
        /// A connection must be closed whether or not making a request failed. Errors from
        /// closing the streams are suppressed.
        ///
        /// The returned future holds a reference to the connection until the close ends.
        pub fn close(&mut self) -> Future<()> {
            let me = self.shared();
            let this: *mut Connection = conn(&me);
            // At least output stream close can fail with exception, because
            // the stream will do a flush. If connection never managed to send data, we
            // will throw again here. Need to suppress these exceptions.
            when_all((
                self._read_buf.close().handle_exception(|_e: ExceptionPtr| {}),
                self._write_buf.close().handle_exception(|_e: ExceptionPtr| {}),
            ))
            .discard_result()
            .then(move |()| {
                // SAFETY: `me` keeps the connection alive; the stream closes have resolved,
                // so no other reference to it is live.
                let c = unsafe { &mut *this };
                let la = c._fd.local_address();
                std::mem::replace(&mut c._closed, make_ready_future(())).then(move |()| {
                    drop(me);
                    log_trace!(HTTP_LOG, "destroyed connection {}", la);
                })
            })
        }

        /// Sends `req` and reads the reply head.
        ///
        /// # Safety
        /// The connection and `req` stay alive and in place until the returned future
        /// resolves, and are used by nothing else meanwhile (the continuations reach them
        /// through their addresses).
        unsafe fn do_make_request(&mut self, req: &mut Request) -> Future<ReplyPtr> {
            if let Err(e) = self.setup_request(req) {
                return make_exception_future(e);
            }
            let this: *mut Connection = self;
            let reqp: *mut Request = req;
            // The caller keeps the connection and the request alive until the reply
            // arrives (the contract above), so `this` and `reqp` are valid in every closure
            // below.
            // SAFETY: the contract above.
            unsafe { self.send_request_head(req) }.then(move |()| {
                // SAFETY: the caller keeps the connection and the request alive until the
                // reply arrives, and the steps of the chain run one at a time on the shard.
                unsafe { (*this).maybe_wait_for_continue(&*reqp) }.then(move |cont: ReplyPtr| {
                    if cont.is_some() {
                        return make_ready_future(cont);
                    }

                    // SAFETY: as above; no other step borrows the connection or the request.
                    unsafe { (*this).write_body(&mut *reqp) }.then(move |()| {
                        // SAFETY: the caller keeps the connection alive until the reply
                        // arrives; the body write has finished, so the buffer is not borrowed.
                        unsafe { &mut (*this)._write_buf }
                            .flush()
                            .then(move |()| unsafe { (*this).recv_reply() })
                    })
                })
            })
        }

        fn setup_request(&mut self, req: &mut Request) -> Result<(), ExceptionPtr> {
            if req._version.empty() {
                req._version = Sstring::from("1.1");
            }
            if req.content_length != 0 {
                if !req.body_writer.is_some() && req.content.empty() {
                    return Err(ExceptionPtr::new(crate::engine::core::future::RuntimeError(
                        "Request body writer not set and content is empty".to_string(),
                    )));
                }
                req._headers.insert(
                    CaseInsensitiveKey(Sstring::from("Content-Length")),
                    u64_to_sstring(req.content_length as u64),
                );
            }
            Ok(())
        }

        /// Writes the request line and the headers.
        ///
        /// # Safety
        /// The connection and `req` stay alive and in place until the returned future
        /// resolves, and are used by nothing else meanwhile (the continuations reach them
        /// through their addresses).
        unsafe fn send_request_head(&mut self, req: &Request) -> Future<()> {
            let this: *mut Connection = self;
            let reqp: *const Request = req;
            // The caller keeps the connection and the request alive until the reply arrives.
            self._write_buf.write_sstring(&req.request_line()).then(move |()| {
                // SAFETY: the caller keeps the connection and the request alive until the
                // reply arrives; the request is only read and the buffer has no other user.
                unsafe { (*reqp).write_request_headers(&mut (*this)._write_buf) }
                    // SAFETY: as above; the header write has finished with the buffer.
                    .then(move |()| unsafe { &mut (*this)._write_buf }.write(b"\r\n"))
            })
        }

        /// Waits for the `100 Continue` reply when the request expects one.
        ///
        /// # Safety
        /// The connection and `req` stay alive and in place until the returned future
        /// resolves, and are used by nothing else meanwhile (the continuations reach them
        /// through their addresses).
        unsafe fn maybe_wait_for_continue(&mut self, req: &Request) -> Future<ReplyPtr> {
            if req.get_header("Expect").empty() {
                return make_ready_future(None);
            }

            let this: *mut Connection = self;
            self._write_buf.flush().then(move |()| {
                // SAFETY: as in do_make_request().
                unsafe { (*this).recv_reply() }.then(|rep: ReplyPtr| {
                    make_ready_future(match rep {
                        Some(r) if r._status == StatusType::CONTINUE => None,
                        other => other,
                    })
                })
            })
        }

        /// Writes the request body.
        ///
        /// # Safety
        /// The connection and `req` stay alive and in place until the returned future
        /// resolves, and are used by nothing else meanwhile (the continuations reach them
        /// through their addresses).
        unsafe fn write_body(&mut self, req: &mut Request) -> Future<()> {
            if req.body_writer.is_some() {
                if req.content_length != 0 {
                    let reqp: *const Request = req;
                    // `_bytes_written` is the `Cell` counter of the request, written by the
                    // sink through this pointer and only read (`get()`) elsewhere.
                    let bytes_written: *mut usize = req._bytes_written.as_ptr();
                    // SAFETY: the connection's write buffer and the request outlive the body
                    // write (the caller keeps both until the reply),
                    // and nothing else writes to the buffer while the body writer runs.
                    let body = unsafe {
                        make_http_content_length_output_stream(
                            &mut self._write_buf,
                            req.content_length,
                            bytes_written,
                        )
                    };
                    return req.body_writer.call(body).then(move |()| {
                        // SAFETY: as above.
                        let req = unsafe { &*reqp };
                        if req.content_length == req._bytes_written.get() {
                            make_ready_future(())
                        } else {
                            make_exception_future(ExceptionPtr::new(
                                crate::engine::core::future::RuntimeError(format!(
                                    "partial request body write, need {} sent {}",
                                    req.content_length,
                                    req._bytes_written.get()
                                )),
                            ))
                        }
                    });
                }
                let this: *mut Connection = self;
                // SAFETY: as above, for the write buffer.
                let body = unsafe { make_http_chunked_output_stream(&mut self._write_buf) };
                return req.body_writer.call(body).then(move |()| {
                    // SAFETY: as in do_make_request().
                    unsafe { &mut (*this)._write_buf }.write_str("0\r\n\r\n")
                });
            }
            if !req.content.empty() {
                self._write_buf.write_sstring(&req.content)
            } else {
                make_ready_future(())
            }
        }

        /// Reads and parses the reply head.
        ///
        /// # Safety
        /// The connection stays alive and in place until the returned future resolves.
        unsafe fn recv_reply(&mut self) -> Future<ReplyPtr> {
            let this: *mut Connection = self;
            do_with(HttpResponseParser::default(), move |parser: &mut HttpResponseParser| {
                parser.init();
                let pp: *mut HttpResponseParser = parser;
                // SAFETY: the caller keeps the connection alive until the reply arrives and
                // no other reference to it is live while the reply is read.
                let c = unsafe { &mut *this };
                // SAFETY: do_with keeps the parser alive and unmoved until the chain resolves,
                // and the consumer is its only user while the stream is read.
                c._read_buf
                    .consume(ObsoleteConsumer(move |buf: TemporaryBuffer<u8>| unsafe {
                        (*pp).call(buf)
                    }))
                    .then(move |()| {
                        // SAFETY: do_with keeps the parser alive; the consumer has finished with it.
                        let parser = unsafe { &mut *pp };
                        if parser.eof() {
                            log_trace!(HTTP_LOG, "Parsing response EOFed");
                            return make_exception_future(ExceptionPtr::new(
                                std::io::Error::from_raw_os_error(errno::ECONNABORTED),
                            ));
                        }
                        if parser.failed() {
                            log_trace!(HTTP_LOG, "Parsing response failed");
                            return make_exception_future(ExceptionPtr::new(
                                response_parsing_exception(format!(
                                    "Invalid http server response. Reason: {}",
                                    parser.error_message()
                                )),
                            ));
                        }

                        let Some(mut resp) = parser.get_parsed_response() else {
                            return make_ready_future(None);
                        };
                        let length_header = resp.get_header("Content-Length");
                        resp.content_length = strtol10(length_header.as_bytes());
                        if resp._version.as_bytes() != b"1.1"
                            || CaseInsensitiveCmp
                                .call(resp.get_header("Connection").as_bytes(), b"close")
                        {
                            // SAFETY: the caller keeps the connection alive until the reply arrives.
                            unsafe { (*this)._persistent = false };
                        }
                        make_ready_future(Some(resp))
                    })
            })
        }

        fn shutdown(&mut self) {
            self._persistent = false;
            self._fd.shutdown_input();
        }
    }

    /// Parses a decimal integer the way the C library does for header values: leading
    /// whitespace and a sign are accepted, parsing stops at the first non-digit, the value
    /// saturates at the `i64` range, and the result is converted to `usize` with wrapping
    /// (a negative value becomes a huge length).
    #[inline]
    fn strtol10(s: &[u8]) -> usize {
        let mut i = 0;
        while i < s.len() && matches!(s[i], b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r') {
            i += 1;
        }
        let neg = i < s.len() && s[i] == b'-';
        if i < s.len() && (s[i] == b'-' || s[i] == b'+') {
            i += 1;
        }
        let mut n: i64 = 0;
        while i < s.len() && s[i].is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add(i64::from(s[i] - b'0'));
            i += 1;
        }
        (if neg { n.wrapping_neg() } else { n }) as usize
    }

    /// The handler of a reply: the reply and its body.
    ///
    /// The handler gets the reply only for the call (`const reply&`); what its future needs
    /// later it copies out. The stream reads the body into the reply's counters and maps.
    pub type ReplyHandler = NoncopyableFunction<RefFn<Reply, InputStream<u8>, Future<()>>>;

    /// Tag of [`RetryRequests`].
    pub struct RetryRequestsTag;
    /// Whether the client retries a request on a connection error.
    pub type RetryRequests = BoolClass<RetryRequestsTag>;

    /// An HTTP client: makes requests to one server and handles the replies.
    ///
    /// The caller provides the transport through a [`ConnectionFactory`], though
    /// [`Client::new`] and [`Client::with_tls`] cover the simple cases. Over a bare
    /// [`Connection`], the client adds transparent reuse of keep-alive connections from a
    /// pool.
    ///
    /// The client must be closed with [`Client::close`] before it is dropped.
    pub struct Client {
        _new_connections: Box<dyn ConnectionFactory>,
        pub(super) _nr_connections: u32,
        /// The requests and limit changes in progress (see `PendingRef`): their steps reach
        /// the client through its address, also while it has no connection counted.
        _pending: std::cell::Cell<u32>,
        _max_connections: u32,
        _max_bytes_to_drain: usize,
        _total_new_connections: u64,
        _retry: RetryRequests,
        pub(super) _wait_con: ConditionVariable,
        _pool: List<ConnectionAdapter>,
        _pinned: PhantomPinned,
    }

    impl Drop for Client {
        /// The client must be closed before it is dropped, because its connections point
        /// back at it: dropping it with connections still counted would
        /// leave them writing into freed memory, so it aborts instead.
        fn drop(&mut self) {
            if self._nr_connections != 0 {
                eprintln!(
                    "http::experimental::client destroyed with {} connections; close() it first",
                    self._nr_connections
                );
                std::process::abort();
            }
            if self._pending.get() != 0 {
                eprintln!(
                    "http::experimental::client destroyed with {} operations in progress",
                    self._pending.get()
                );
                std::process::abort();
            }
        }
    }

    /// Counts an operation of a client (a request or a change of its limit) while it is in
    /// progress, so that dropping the client under it aborts instead of leaving its steps
    /// with a dangling pointer.
    struct PendingRef {
        c: *const Client,
    }

    impl PendingRef {
        /// Counts one more operation of `c`.
        ///
        /// # Safety
        /// `c` is a pinned client, which checks the counter in its destructor.
        #[inline]
        unsafe fn new(c: *const Client) -> Self {
            // SAFETY: the contract above; the counter is a `Cell`, reached by a shared place.
            unsafe { (*c)._pending.set((*c)._pending.get() + 1) };
            PendingRef { c }
        }
    }

    impl Drop for PendingRef {
        #[inline]
        fn drop(&mut self) {
            // SAFETY: the client aborts instead of going while the counter is not zero, so
            // it is alive here.
            unsafe { (*self.c)._pending.set((*self.c)._pending.get() - 1) };
        }
    }

    /// Subscribes `f` to the abort source `as_` when it is not null (`None` otherwise). A
    /// linked subscription cannot move, so its slot is boxed once and the box travels into
    /// the continuation instead.
    ///
    /// # Safety
    /// A non-null `as_` points at a live abort source that does not move while the
    /// subscription exists, and `f` may run until the returned slot is dropped.
    unsafe fn subscribe_naive<F: FnMut() + 'static>(
        as_: *mut AbortSource,
        f: F,
    ) -> Option<Pin<Box<Subscription<NaiveCallback<F>>>>> {
        if as_.is_null() {
            return None;
        }
        let mut slot = Box::pin(Subscription::default());
        // SAFETY: guaranteed by the caller.
        let as_ = unsafe { Pin::new_unchecked(&mut *as_) };
        if as_.subscribe_naive(slot.as_mut(), f) { Some(slot) } else { None }
    }

    impl Client {
        /// The default limit of connections.
        pub const DEFAULT_MAX_CONNECTIONS: u32 = 100;
        /// The default limit of unread body bytes skipped to reuse a connection.
        pub const DEFAULT_MAX_BYTES_TO_DRAIN: usize = 128 * 1024;

        /// Creates a client that connects to `addr` over plain (non-TLS) sockets, with the
        /// default limits and without retries.
        pub fn new(addr: SocketAddress) -> Self {
            Self::with_factory(
                Box::new(BasicConnectionFactory::new(addr)),
                Self::DEFAULT_MAX_CONNECTIONS,
                RetryRequests::NO,
                Self::DEFAULT_MAX_BYTES_TO_DRAIN,
            )
        }

        /// Creates an HTTPS client: it connects to `addr` over TLS sockets with the
        /// credentials `creds`, with the default limits and without retries.
        ///
        /// `host` is the server name expected by TLS; it may be empty.
        pub fn with_tls(
            addr: SocketAddress,
            creds: SharedPtr<CertificateCredentials>,
            host: Sstring,
        ) -> Self {
            Self::with_factory(
                Box::new(TlsConnectionFactory::new(addr, creds, host)),
                Self::DEFAULT_MAX_CONNECTIONS,
                RetryRequests::NO,
                Self::DEFAULT_MAX_BYTES_TO_DRAIN,
            )
        }

        /// Creates a client whose transport sockets come from the connection factory `f`.
        ///
        /// The arguments:
        ///
        /// - `f`: the factory of connected sockets; the client may take several sockets
        ///   from it and reuses them on its own.
        /// - `max_connections`: the most connections the client keeps, active and pooled.
        /// - `retry`: whether a request is retried after a connection I/O error.
        /// - `max_bytes_to_drain`: the most unread body bytes the client skips to reuse a
        ///   connection.
        ///
        /// Once a request-response cycle is over, the client keeps its connection in a
        /// pool, and a later request picks it up, avoiding the latency of a new connection.
        /// The pool can hold several connections when requests are sent in parallel.
        ///
        /// A server can end a kept connection in several ways:
        ///
        /// - It adds a `Connection: close` header to its response: the client handles the
        ///   response and closes the connection instead of pooling it.
        /// - It closes the connection while it sits in the pool: the client removes it
        ///   from the pool in the background.
        /// - It closes the connection while the client is using it: the request fails
        ///   with a transport error. With `retry`, the client then closes the broken
        ///   connection, opens another one and sends the same request once more over it;
        ///   if that second attempt fails too, its error is returned.
        pub fn with_factory(
            f: Box<dyn ConnectionFactory>,
            max_connections: u32,
            retry: RetryRequests,
            max_bytes_to_drain: usize,
        ) -> Self {
            Client {
                _new_connections: f,
                _nr_connections: 0,
                _pending: std::cell::Cell::new(0),
                _max_connections: max_connections,
                _max_bytes_to_drain: max_bytes_to_drain,
                _total_new_connections: 0,
                _retry: retry,
                _wait_con: ConditionVariable::default(),
                _pool: List::new(),
                _pinned: PhantomPinned,
            }
        }

        fn pop_pooled(&mut self) -> Option<ConnectionPtr> {
            self._pool.pop_front().map(|c| {
                // SAFETY: the pooled connections were made by make_shared_esft().
                unsafe { SharedPtr::shared_from_this(c.as_ptr()) }
            })
        }

        fn get_connection(&mut self, as_: *mut AbortSource) -> Future<ConnectionPtr> {
            if let Some(con) = self.pop_pooled() {
                // SAFETY: `con` keeps the connection alive.
                log_trace!(HTTP_LOG, "pop http connection {} from pool", unsafe {
                    (*conn(&con))._fd.local_address()
                });
                return make_ready_future(con);
            }

            if self._nr_connections >= self._max_connections {
                let this: *mut Client = self;
                // SAFETY: the client outlives its requests (it must be closed first), so the
                // callback may reach it through `this`; a non-null abort source is kept alive
                // by the caller until the subscription, held below, is dropped.
                let sub = unsafe { subscribe_naive(as_, move || (*this)._wait_con.broadcast()) };
                // SAFETY: the condition variable is a member of the pinned client.
                return unsafe { Pin::new_unchecked(&(*this)._wait_con) }.wait().then(move |()| {
                    let _sub = sub;
                    // SAFETY: a non-null abort source is kept alive by the caller until the
                    // request ends.
                    if !as_.is_null() && unsafe { (*as_).abort_requested() } {
                        // SAFETY: `as_` is non-null here and alive, as above.
                        return make_exception_future(
                            unsafe { (*as_).abort_requested_exception_ptr() }.clone(),
                        );
                    }
                    // SAFETY: the client outlives its requests and is pinned.
                    unsafe { (*this).get_connection(as_) }
                });
            }

            self.make_connection(as_)
        }

        fn make_connection(&mut self, as_: *mut AbortSource) -> Future<ConnectionPtr> {
            self._total_new_connections += 1;
            // SAFETY: the client is pinned (the public entry points take it pinned) and aborts
            // when dropped with connections counted.
            let cr = unsafe { ClientRef::new(self) };
            self._new_connections.make(as_).then(move |cs: ConnectedSocket| {
                log_trace!(HTTP_LOG, "created new http connection {}", cs.local_address());
                make_ready_future(Connection::new(cs, cr))
            })
        }

        fn put_connection(&mut self, con: ConnectionPtr) -> Future<()> {
            let c = conn(&con);
            // SAFETY: `con` keeps the connection alive.
            let persistent = unsafe { (*c)._persistent };
            if persistent && self._nr_connections <= self._max_connections {
                // SAFETY: `con` keeps the connection alive.
                log_trace!(HTTP_LOG, "push http connection {} to pool", unsafe {
                    (*c)._fd.local_address()
                });
                // SAFETY: the connection stays in place in its shared pointer and unlinks
                // itself from the pool (auto-unlink hook) when destroyed.
                unsafe { self._pool.push_back(c) };
                self._wait_con.signal();
                return make_ready_future(());
            }

            // SAFETY: `con` keeps the connection alive.
            log_trace!(HTTP_LOG, "dropping connection {}", unsafe { (*c)._fd.local_address() });
            // SAFETY: `con` keeps the connection alive until the close resolves, since the
            // continuation that drops it runs after the close.
            unsafe { (*c).close() }.finally(move || drop(con))
        }

        fn shrink_connections(&mut self) -> Future<()> {
            if self._nr_connections <= self._max_connections {
                return make_ready_future(());
            }

            let this: *mut Client = self;
            if let Some(con) = self.pop_pooled() {
                // SAFETY: `con` keeps the connection alive; the client outlives the call.
                return unsafe { (*conn(&con)).close() }.finally(move || {
                    drop(con);
                    // SAFETY: the client outlives its connections (it must be closed first).
                    unsafe { (*this).shrink_connections() }
                });
            }

            // SAFETY: as above; the condition variable is a member of the pinned client.
            unsafe { Pin::new_unchecked(&(*this)._wait_con) }
                .wait()
                .then(move |()| unsafe { (*this).shrink_connections() })
        }

        /// Sets the most connections the client may have to `nr`.
        ///
        /// When the client has more connections than the new limit, the excess ones are
        /// closed, and the returned future resolves once they all are.
        pub fn set_maximum_connections(self: Pin<&mut Self>, nr: u32) -> Future<()> {
            // SAFETY: the client is not moved; the pointers the call leaves behind point at
            // its pinned place.
            let me = unsafe { self.get_unchecked_mut() };
            // SAFETY: the client is pinned (above).
            let pending = unsafe { PendingRef::new(me) };
            me.set_maximum_connections_impl(nr).finally(move || drop(pending))
        }

        fn set_maximum_connections_impl(&mut self, nr: u32) -> Future<()> {
            if nr > self._max_connections {
                self._max_connections = nr;
                self._wait_con.broadcast();
                return make_ready_future(());
            }

            self._max_connections = nr;
            self.shrink_connections()
        }

        fn with_connection<Fn_>(&mut self, fn_: Fn_, as_: *mut AbortSource) -> Future<()>
        where
            Fn_: FnOnce(&mut Connection) -> Future<()> + 'static,
        {
            let this: *mut Client = self;
            self.get_connection(as_).then(move |con: ConnectionPtr| {
                // SAFETY: `con` keeps the connection alive; the client outlives the call.
                unsafe { fn_(&mut *conn(&con)) }
                    .finally(move || unsafe { (*this).put_connection(con) })
            })
        }

        fn with_new_connection<Fn_>(&mut self, fn_: Fn_, as_: *mut AbortSource) -> Future<()>
        where
            Fn_: FnOnce(&mut Connection) -> Future<()> + 'static,
        {
            let this: *mut Client = self;
            self.make_connection(as_).then(move |con: ConnectionPtr| {
                // SAFETY: as in with_connection().
                unsafe { fn_(&mut *conn(&con)) }
                    .finally(move || unsafe { (*this).put_connection(con) })
            })
        }

        /// Sends the request `req` to the server and calls `handle` with the reply when it
        /// arrives; resolves with the handler's future.
        ///
        /// `expected` is the expected reply status, `None` to accept any. The handler may be
        /// called more than once: the client restarts the whole request when the server
        /// closes the connection in the middle of it and retries are enabled.
        ///
        /// # Errors
        ///
        /// The returned future fails, without calling the handler, if the reply status is
        /// not `expected`; it also fails if the request cannot be sent or the handler fails.
        pub fn make_request(
            self: Pin<&mut Self>,
            req: Request,
            handle: ReplyHandler,
            expected: Option<StatusType>,
        ) -> Future<()> {
            // SAFETY: a null abort source is never dereferenced.
            unsafe { self.make_request_abortable(req, handle, std::ptr::null_mut(), expected) }
        }

        /// [`Client::make_request`] that the abort source `as_` (when not null) can abort.
        ///
        /// # Safety
        /// A non-null `as_` points at an abort source that stays alive and in place until the
        /// returned future resolves.
        pub unsafe fn make_request_abortable(
            self: Pin<&mut Self>,
            req: Request,
            handle: ReplyHandler,
            as_: *mut AbortSource,
            expected: Option<StatusType>,
        ) -> Future<()> {
            // SAFETY: the client is pinned; the pointers the request leaves behind point at
            // its place, and it aborts when dropped with connections counted.
            let this: *mut Client = unsafe { self.get_unchecked_mut() };
            do_with((req, handle), move |st: &mut (Request, ReplyHandler)| {
                let (req, handle) = st;
                // SAFETY: do_with keeps the request and the handler alive until the future
                // resolves; the client and the abort source as above and by the contract.
                unsafe { (*this).make_request_ref_impl(req, handle, expected, as_) }
            })
        }

        /// [`Client::make_request`] that the abort source `as_` (when not null) can abort,
        /// borrowing the request and the handler instead of taking them.
        ///
        /// # Safety
        /// `req`, `handle` and a non-null `as_` stay alive and in place, and unused elsewhere,
        /// until the returned future resolves.
        pub unsafe fn make_request_ref(
            self: Pin<&mut Self>,
            req: *mut Request,
            handle: *mut ReplyHandler,
            expected: Option<StatusType>,
            as_: *mut AbortSource,
        ) -> Future<()> {
            // SAFETY: the client is pinned (see make_request_abortable()); the rest is the
            // caller's contract.
            unsafe {
                self.get_unchecked_mut().make_request_ref_impl(
                    &mut *req,
                    &mut *handle,
                    expected,
                    as_,
                )
            }
        }

        /// The body of [`Client::make_request_ref`], on the pinned client.
        fn make_request_ref_impl(
            &mut self,
            req: &mut Request,
            handle: &mut ReplyHandler,
            expected: Option<StatusType>,
            as_: *mut AbortSource,
        ) -> Future<()> {
            let this: *mut Client = self;
            let reqp: *mut Request = req;
            let handlep: *mut ReplyHandler = handle;
            // SAFETY: the public entry points reach this on the pinned client.
            let pending = unsafe { PendingRef::new(self) };
            // The caller keeps the request, the handler and a non-null abort source alive
            // until the returned future resolves, and the client aborts instead of going
            // while `pending` counts this request. `pending` lives in the error step, which
            // is dropped when the attempt resolves (no step of its own on the success path)
            // and, on a retry, moves into the retry's chain.
            self.with_connection(
                // SAFETY: the client, the request and the handler are alive (see above) and
                // only this attempt uses the request and the handler while it runs.
                move |con: &mut Connection| unsafe {
                    (*this).do_make_request(con, &mut *reqp, &mut *handlep, as_, expected)
                },
                as_,
            )
            .handle_exception(move |ex: ExceptionPtr| {
                let pending = pending;
                // SAFETY: a non-null abort source is kept alive by the caller.
                if !as_.is_null() && unsafe { (*as_).abort_requested() } {
                    // SAFETY: `as_` is non-null here and alive, as above.
                    return make_exception_future(
                        unsafe { (*as_).abort_requested_exception_ptr() }.clone(),
                    );
                }

                // SAFETY: the client outlives its requests; the failed attempt has ended, so
                // no other reference to it is live.
                let c = unsafe { &mut *this };
                if !c._retry.as_bool() || !is_retryable_exception(&ex) {
                    return make_exception_future(ex);
                }

                // The 'con' connection may not yet be freed, so the total connection
                // count still account for it and with_new_connection() may temporarily
                // break the limit. That's OK, the 'con' will be closed really soon
                c.with_new_connection(
                    // SAFETY: as for the first attempt; the failed one no longer uses the
                    // request or the handler.
                    move |con: &mut Connection| unsafe {
                        (*this).do_make_request(con, &mut *reqp, &mut *handlep, as_, expected)
                    },
                    as_,
                )
                .finally(move || drop(pending))
            })
        }

        fn do_make_request(
            &mut self,
            con: &mut Connection,
            req: &mut Request,
            handle: &mut ReplyHandler,
            as_: *mut AbortSource,
            expected: Option<StatusType>,
        ) -> Future<()> {
            let this: *mut Client = self;
            let conp: *mut Connection = con;
            let handlep: *mut ReplyHandler = handle;
            // The connection is held by with_connection() and the handler by the caller until
            // the request ends; the client outlives it.
            // SAFETY: the connection outlives the subscription, which is dropped in the last
            // continuation below; a non-null abort source is kept alive by the caller.
            let sub = unsafe { subscribe_naive(as_, move || (*conp).shutdown()) };
            // SAFETY: with_connection() holds the connection until the request ends, and the
            // caller keeps the request alive and unused elsewhere until then.
            unsafe { con.do_make_request(req) }
                .then(move |reply: ReplyPtr| {
                    let Some(reply) = reply else {
                        return make_ready_future(());
                    };
                    // SAFETY: with_connection() holds the connection until the request ends,
                    // and the request head and body writes have finished with it.
                    let con = unsafe { &mut *conp };
                    if let Some(expected) = expected
                        && reply._status != expected
                    {
                        if !HTTP_LOG.is_enabled(LogLevel::Debug) {
                            return make_exception_future(ExceptionPtr::new(unexpected_status_error(reply._status)));
                        }

                        let reply = RawBox::new(reply);
                        let rp: *mut Reply = reply.as_ptr();
                        // SAFETY: the reply is owned through its address by `reply`, which the
                        // last continuation keeps; the connection is held until the request ends.
                        let in_ = unsafe { con.in_(rp) };
                        return do_with(in_, move |in_: &mut InputStream<u8>| {
                            // SAFETY: do_with keeps the stream alive and in place until the read
                            // resolves, and nothing else uses it meanwhile.
                            unsafe { read_entire_stream_contiguous(in_) }.then(move |message: Sstring| {
                                // SAFETY: `reply`, moved into this closure, still owns the reply.
                                let status = unsafe { (*rp)._status };
                                log_debug!(HTTP_LOG, "request finished with {}: {}", status, message);
                                drop(reply);
                                make_exception_future::<()>(ExceptionPtr::new(unexpected_status_error(status)))
                            })
                        });
                    }

                    let reply = RawBox::new(reply);
                    let rp: *mut Reply = reply.as_ptr();
                    // SAFETY: as above; the handler gets the reply only for the call.
                    let in_ = unsafe { con.in_(rp) };
                    // SAFETY: the caller keeps the handler alive until the request ends, and
                    // `reply`, moved into the continuation, keeps the reply alive past the call.
                    unsafe { (*handlep).call(&*rp, in_) }.then(move |()| {
                        // SAFETY: with_connection() holds the connection until the request
                        // ends, and the handler has finished with its input stream.
                        let con = unsafe { &mut *conp };
                        // SAFETY: `reply`, moved into this continuation, owns the reply.
                        let reply = unsafe { &*reply.as_ptr() };
                        if reply.content_length > reply.consumed_content {
                            let bytes_left = reply.content_length - reply.consumed_content;
                            // SAFETY: the client outlives its requests.
                            if bytes_left <= unsafe { (*this)._max_bytes_to_drain } {
                                log_trace!(
                                    HTTP_LOG,
                                    "content was not fully consumed, {} bytes were left behind, skipping and returning the connection to the pool",
                                    bytes_left
                                );
                                return con._read_buf.skip(bytes_left as u64);
                            }
                            log_trace!(
                                HTTP_LOG,
                                "content was not fully consumed, content length is {} but consumed only {}, will close the connection",
                                reply.content_length,
                                reply.consumed_content
                            );
                            con._persistent = false;
                        }
                        make_ready_future(())
                    })
                })
                .handle_exception(move |ex: ExceptionPtr| {
                    // SAFETY: with_connection() holds the connection until the request ends.
                    unsafe { (*conp)._persistent = false };
                    make_exception_future::<()>(ex)
                })
                .finally(move || drop(sub))
        }

        /// Closes the client and all its pooled connections.
        ///
        /// A client must always be closed before it is dropped.
        pub fn close(self: Pin<&mut Self>) -> Future<()> {
            // SAFETY: as in set_maximum_connections().
            unsafe { self.get_unchecked_mut() }.close_impl()
        }

        fn close_impl(&mut self) -> Future<()> {
            let Some(con) = self.pop_pooled() else {
                return make_ready_future(());
            };

            let c = conn(&con);
            let this: *mut Client = self;
            // SAFETY: `con` keeps the connection alive; the client outlives the call.
            log_trace!(HTTP_LOG, "closing connection {}", unsafe { (*c)._fd.local_address() });
            // SAFETY: `con`, dropped only in the continuation, keeps the connection alive
            // until the close resolves.
            unsafe { (*c).close() }.then(move |()| {
                drop(con);
                // SAFETY: the client is pinned and stays alive until its close resolves.
                unsafe { (*this).close_impl() }
            })
        }

        /// Returns the number of connections the client has, active and pooled.
        #[inline]
        pub fn connections_nr(&self) -> u32 {
            self._nr_connections
        }

        /// Returns the number of idle connections in the pool.
        #[inline]
        pub fn idle_connections_nr(&self) -> u32 {
            self._pool.size() as u32
        }

        /// Returns the number of connections the client has requested from its factory so
        /// far.
        ///
        /// The counter only grows; it shows how often the client needs new connections.
        #[inline]
        pub fn total_new_connections_nr(&self) -> u64 {
            self._total_new_connections
        }
    }

    /// The code of a system error: the errno of `std::io::Error` and `SystemError`, or
    /// the GnuTLS code of `TlsError` (`tls::error_category()`), the three forms it takes here.
    #[inline]
    fn system_error_code(ex: &ExceptionPtr) -> Option<i32> {
        if let Some(e) = ex.downcast_ref::<std::io::Error>() {
            return Some(e.raw_os_error().unwrap_or(0));
        }
        if let Some(e) = ex.downcast_ref::<SystemError>() {
            return Some(e.code());
        }
        ex.downcast_ref::<TlsError>().map(|e| e.code)
    }

    fn is_retryable_exception(ex: &ExceptionPtr) -> bool {
        let mut ex = ex;
        while !ex.is_null() {
            if let Some(code) = system_error_code(ex) {
                if code == errno::EPIPE
                    || code == errno::ECONNABORTED
                    || code == errno::ECONNRESET
                    || code == ERROR_PREMATURE_TERMINATION
                {
                    return true;
                }
            } else if let Some(e) = ex.downcast_ref::<BaseException>()
                && *e.kind() == ExceptionKind::ResponseParsing
            {
                return true;
            }
            // Look into the nested error, if any.
            match ex.nested_ptr() {
                Some(n) => ex = n,
                None => return false,
            }
        }
        false
    }
}
