//! The HTTP/1.1 server.
//!
//! [`httpd::HttpServer`] is one shard's server: it accepts connections on its listeners
//! (plain or TLS), parses requests with the request parser, dispatches them through its
//! [`Routes`](crate::engine::http::routes::Routes) and writes the replies in order,
//! supporting keep-alive, pipelining, `Expect: 100-continue` and streamed request bodies.
//! It keeps a cached `Date` header refreshed every second and exports connection and
//! request metrics. [`httpd::HttpServerControl`] runs one server per shard (a sharded
//! service) and forwards route setup and `listen` calls to all of them.
//!
//! The server and its connections register pointers to themselves (the date timer, the
//! metrics callbacks, the intrusive list of connections), so neither may move once linked:
//! a connection lives on the heap at a fixed address, and a server is `!Unpin` and makes
//! its links in [`httpd::HttpServer::start`], once it is at its final place.

use std::sync::LazyLock;
use std::time::Duration;

use crate::engine::util::log::Logger;

static HLOGGER: LazyLock<std::pin::Pin<Box<Logger>>> =
    LazyLock::new(|| Logger::new("httpd").expect("logger registered once"));

/// The server, its connections, metrics and the sharded control object.
#[allow(clippy::module_inception)]
pub mod httpd {
    use std::marker::PhantomPinned;
    use std::pin::Pin;

    use super::{Duration, HLOGGER};
    use crate::engine::core::api::intrusive::{List, ListHook};
    use crate::engine::core::future::{
        ExceptionPtr, Future, LogicError, make_exception_future, make_ready_future,
    };
    use crate::engine::core::gate::{Gate, GateClosedException, try_with_gate};
    use crate::engine::core::iostream::{DataSource, InputStream, ObsoleteConsumer, OutputStream};
    use crate::engine::core::loop_::{do_until, keep_doing};
    use crate::engine::core::metrics::{self as sm, MetricGroups};
    use crate::engine::core::queue::Queue;
    use crate::engine::core::sharded::Sharded;
    use crate::engine::core::shared_ptr::SharedPtr;
    use crate::engine::core::sstring::Sstring;
    use crate::engine::core::temporary_buffer::TemporaryBuffer;
    use crate::engine::core::timer::Timer;
    use crate::engine::core::when_all::when_all;
    use crate::engine::http::common::support::{RawBox, u64_to_sstring};
    use crate::engine::http::internal::content_source::internal::{
        ChunkTargets, ChunkedSourceImpl, ContentLengthSourceImpl,
    };
    use crate::engine::http::reply::{Reply, StatusType};
    use crate::engine::http::request::Request;
    use crate::engine::http::request_parser::HttpRequestParser;
    use crate::engine::http::routes::Routes;
    use crate::engine::net::api::{AcceptResult, ConnectedSocket, ListenOptions, ServerSocket};
    use crate::engine::net::socket_defs::SocketAddress;
    use crate::engine::net::tls::ServerCredentials;
    use crate::engine::util::short_streams::util::read_entire_stream_contiguous;
    use crate::engine::util::string_utils::internal::{CaseInsensitiveCmp, CaseInsensitiveKey};
    use crate::{log_debug, log_error};

    /// The error of an aborted accept, which http_server::stop causes.
    #[cfg(target_os = "linux")]
    const ECONNABORTED: i32 = 103;
    /// The error of an aborted accept, which http_server::stop causes.
    #[cfg(not(target_os = "linux"))]
    const ECONNABORTED: i32 = 53;

    /// The metrics of a server.
    pub struct HttpStats {
        _metric_groups: MetricGroups,
    }

    impl HttpStats {
        /// Registers the metrics of `server` under the label service=`name`.
        ///
        /// # Safety
        /// `server` stays at its address and alive while the stats live (they are one of its
        /// members, set by [`HttpServer::start`] on the pinned server).
        pub unsafe fn new(server: *const HttpServer, name: &Sstring) -> Self {
            let labels = vec![sm::LabelInstance::new("service", name)];
            let mut _metric_groups = MetricGroups::default();
            // Registration fails only on a double registration, which cannot happen for a
            // fresh server; this constructor cannot fail, so the result is ignored.
            // Every callback relies on the constructor's contract: `server` stays alive and
            // in place while the metric groups that call back into it exist.
            let _ = _metric_groups.add_group(
                "httpd",
                vec![
                    sm::make_counter(
                        "connections_total",
                        // SAFETY: by the constructor's contract `server` outlives the stats.
                        move || unsafe { (*server).total_connections() },
                        sm::Description::new("The total number of connections opened"),
                        labels.clone(),
                    ),
                    sm::make_gauge(
                        "connections_current",
                        // SAFETY: by the constructor's contract `server` outlives the stats.
                        move || unsafe { (*server).current_connections() },
                        sm::Description::new("The current number of open  connections"),
                        labels.clone(),
                    ),
                    sm::make_counter(
                        "read_errors",
                        // SAFETY: by the constructor's contract `server` outlives the stats.
                        move || unsafe { (*server).read_errors() },
                        sm::Description::new(
                            "The total number of errors while reading http requests",
                        ),
                        labels.clone(),
                    ),
                    sm::make_counter(
                        "reply_errors",
                        // SAFETY: by the constructor's contract `server` outlives the stats.
                        move || unsafe { (*server).reply_errors() },
                        sm::Description::new("The total number of errors while replying to http"),
                        labels.clone(),
                    ),
                    sm::make_counter(
                        "requests_served",
                        // SAFETY: by the constructor's contract `server` outlives the stats.
                        move || unsafe { (*server).requests_served() },
                        sm::Description::new("The total number of http requests served"),
                        labels,
                    ),
                ],
            );
            HttpStats { _metric_groups }
        }
    }

    /// The size of the queue of replies of a connection.
    const REPLIES_QUEUE_SIZE: usize = 10;

    /// The value of the `Server` header of every reply.
    pub const SERVER_HEADER: &str = "TeselaDB httpd";

    /// A connection to the server.
    pub struct Connection {
        /// The hook that links the connection into the server's intrusive list.
        _hook: ListHook,
        _server: *mut HttpServer,
        _fd: ConnectedSocket,
        _read_buf: InputStream<u8>,
        _write_buf: OutputStream<u8>,
        _client_addr: SocketAddress,
        _server_addr: SocketAddress,
        _parser: HttpRequestParser,
        _req: Option<Box<Request>>,
        _resp: Option<Box<Reply>>,
        // null element marks eof
        _replies: Queue<Option<Box<Reply>>>,
        _done: bool,
        _tls: bool,
    }

    crate::intrusive_adapter!(pub ConnectionAdapter = Connection { _hook });

    impl Connection {
        /// The connection buffer size limit, in bytes.
        pub const LIMIT: usize = 4096;

        // A constructor kept for API compatibility that nothing in the crate calls.
        #[allow(dead_code)]
        /// A connection of `server` over `fd`; the address argument is ignored.
        ///
        /// # Safety
        /// As [`Connection::with_addresses`].
        #[deprecated(note = "use Connection::new(server, fd, tls)")]
        pub(crate) unsafe fn with_address(
            server: *mut HttpServer,
            fd: ConnectedSocket,
            _addr: SocketAddress,
            tls: bool,
        ) -> RawBox<Self> {
            // SAFETY: the caller's contract.
            unsafe { Self::new(server, fd, tls) }
        }

        // A constructor kept for API compatibility that nothing in the crate calls.
        #[allow(dead_code)]
        /// A connection of `server` over `fd`, with the addresses of the socket.
        ///
        /// # Safety
        /// As [`Connection::with_addresses`].
        pub(crate) unsafe fn new(
            server: *mut HttpServer,
            fd: ConnectedSocket,
            tls: bool,
        ) -> RawBox<Self> {
            let client_addr = fd.remote_address();
            let server_addr = fd.local_address();
            // SAFETY: the caller's contract.
            unsafe { Self::with_addresses(server, fd, client_addr, server_addr, tls) }
        }

        /// A connection of `server` over `fd`, with the given addresses. It lives on the
        /// heap at a fixed address (the server's list and the continuations point at it), and
        /// is owned through that address by the returned [`RawBox`].
        ///
        /// # Safety
        /// `server` is pinned and outlives the connection (it waits for its connections in
        /// `stop()` through the task gate).
        pub(crate) unsafe fn with_addresses(
            server: *mut HttpServer,
            mut fd: ConnectedSocket,
            client_addr: SocketAddress,
            server_addr: SocketAddress,
            tls: bool,
        ) -> RawBox<Self> {
            let read_buf =
                fd.input(crate::engine::net::api::ConnectedSocketInputStreamConfig::default());
            let write_buf = fd.output(8192);
            let c = RawBox::new(Box::new(Connection {
                _hook: ListHook::new(),
                _server: server,
                _fd: fd,
                _read_buf: read_buf,
                _write_buf: write_buf,
                _client_addr: client_addr,
                _server_addr: server_addr,
                _parser: HttpRequestParser::default(),
                _req: None,
                _resp: None,
                _replies: Queue::new(REPLIES_QUEUE_SIZE),
                _done: false,
                _tls: tls,
            }));
            // SAFETY: the connection was just allocated; this is the only access to it.
            unsafe { (*c.as_ptr()).on_new_connection() };
            c
        }

        #[inline(always)]
        fn server(&mut self) -> &mut HttpServer {
            // SAFETY: the server outlives its connections (stop() waits for them through
            // the task gate).
            unsafe { &mut *self._server }
        }

        /// Counts the connection and links it into the server's list; called once, by
        /// [`Connection::with_addresses`], on the connection at its heap address.
        fn on_new_connection(&mut self) {
            let this: *mut Connection = self;
            let server = self.server();
            server._total_connections += 1;
            server._current_connections += 1;
            // SAFETY: the connection is heap-allocated at a fixed address (RawBox) and
            // unlinks itself in drop().
            unsafe { server._connections.push_back(this) };
            // This constructor cannot fail: a socket that refuses TCP_NODELAY reports its
            // fault on the first read.
            let _ = self._fd.set_nodelay(true);
        }

        /// Serves the connection until it closes, then drops it.
        ///
        /// The returned future owns the connection: the steps of the read and write loops
        /// reach it through its heap address, and the box that owns it is dropped only by
        /// the last step, once both loops and the close of the input have ended.
        pub(crate) fn process(conn: RawBox<Self>) -> Future<()> {
            let this: *mut Connection = conn.as_ptr();
            // SAFETY: `conn` keeps the connection alive at `this` until the last continuation
            // below, which drops it, so both loops run on a live connection.
            let (read, respond) = unsafe { ((*this).read(), (*this).respond()) };
            // Launch read and write "threads" simultaneously:
            when_all((read, respond))
                .then(|(read, respond): (Future<()>, Future<()>)| {
                    if let Err(e) = read.get() {
                        log_debug!(HLOGGER, "Read exception encountered: {}", e);
                    }
                    if let Err(e) = respond.get() {
                        log_debug!(HLOGGER, "Response exception encountered: {}", e);
                    }
                    make_ready_future(())
                })
                .finally(move || {
                    // SAFETY: `conn`, moved into the next step, still owns the connection.
                    unsafe { &mut (*this)._read_buf }.close().handle_exception(|e: ExceptionPtr| {
                        log_debug!(HLOGGER, "Close exception encountered: {}", e);
                    })
                })
                .finally(move || drop(conn))
        }

        /// Shuts the socket down in both directions.
        pub fn shutdown(&mut self) {
            self._fd.shutdown_input();
            self._fd.shutdown_output();
        }

        /// Reads requests until the connection is done.
        ///
        /// # Safety
        /// The connection stays alive at its heap address until the returned future
        /// resolves ([`Connection::process`] owns it for that long).
        unsafe fn read(&mut self) -> Future<()> {
            let this: *mut Connection = self;
            // SAFETY: the connection outlives process(), which awaits this, and the loop's
            // condition and body run one at a time, so no two references overlap.
            do_until(move || unsafe { (*this)._done }, move || unsafe { (*this).read_one() })
                .then_wrapped(move |mut f: Future<()>| {
                    // SAFETY: as above; the loop has ended.
                    let c = unsafe { &mut *this };
                    // swallow error
                    if f.failed() {
                        c.server()._read_errors += 1;
                    }
                    f.ignore_ready_future();
                    // SAFETY: the connection, and so its reply queue, outlives process(), which
                    // awaits this, and does not move.
                    unsafe { c._replies.push_eventually(None) }
                })
        }

        /// Reads and serves one request.
        ///
        /// # Safety
        /// The connection stays alive at its heap address until the returned future
        /// resolves ([`Connection::process`] owns it for that long).
        unsafe fn read_one(&mut self) -> Future<()> {
            self._parser.init();
            let this: *mut Connection = self;
            let parser: *mut HttpRequestParser = &mut self._parser;
            // Every closure below relies on the connection outliving process(), which awaits
            // this; the steps run one at a time on the shard.
            // SAFETY: the parser is a member of the connection, which outlives the read, and
            // the consumer is its only user while the stream is consumed.
            self._read_buf.consume(ObsoleteConsumer(move |buf: TemporaryBuffer<u8>| unsafe { (*parser).call(buf) })).then(move |()| {
                // SAFETY: the connection outlives process(); the consume has finished.
                let c = unsafe { &mut *this };
                if c._parser.eof() {
                    c._done = true;
                    return make_ready_future(());
                }
                c.server()._requests_served += 1;
                let Some(mut req) = c._parser.get_parsed_request() else {
                    c._done = true;
                    return make_ready_future(());
                };

                req._server_address = c._server_addr;
                req._client_address = c._client_addr;

                if c._tls {
                    req.protocol_name = Sstring::from("https");
                }
                if c._parser.failed() {
                    if req._version.empty() {
                        // we might have failed to parse even the version
                        req._version = Sstring::from("1.1");
                    }
                    c.generate_error_reply_and_close(req, StatusType::BAD_REQUEST, &Sstring::from("Can't parse the request"));
                    return make_ready_future(());
                }

                let content_length_limit = c.server().get_content_length_limit();
                let length_header = req.get_header("Content-Length");
                req.content_length = strtol10(length_header.as_bytes());

                if req.content_length > content_length_limit {
                    let msg = Sstring::from(format!(
                        "Content length limit ({}) exceeded: {}",
                        content_length_limit, req.content_length
                    ));
                    c.generate_error_reply_and_close(req, StatusType::PAYLOAD_TOO_LARGE, &msg);
                    return make_ready_future(());
                }

                let encoding = req.get_header("Transfer-Encoding");
                if !encoding.empty() && !CaseInsensitiveCmp.call(encoding.as_bytes(), b"chunked") {
                    //TODO: add "identity", "gzip"("x-gzip"), "compress"("x-compress"), and "deflate" encodings and their combinations
                    let msg = Sstring::from(format!(
                        "Encodings other than \"chunked\" are not implemented (received encoding: \"{}\")",
                        encoding
                    ));
                    c.generate_error_reply_and_close(req, StatusType::NOT_IMPLEMENTED, &msg);
                    return make_ready_future(());
                }

                let maybe_reply_continue = move |req: Box<Request>| -> Future<Box<Request>> {
                    // SAFETY: the connection outlives process(), which awaits this.
                    let c = unsafe { &mut *this };
                    if req._version.as_bytes() == b"1.1"
                        && CaseInsensitiveCmp.call(req.get_header("Expect").as_bytes(), b"100-continue")
                    {
                        return c._replies.not_full().then(move |()| {
                            // SAFETY: the connection outlives process(), which awaits this.
                            let c = unsafe { &mut *this };
                            let mut continue_reply = Box::new(Reply::new());
                            c.set_headers(&mut continue_reply);
                            continue_reply.set_version(&req._version);
                            continue_reply.set_status(StatusType::CONTINUE, Sstring::new()).done();
                            // A full queue drops the reply (push returns false).
                            let _ = c._replies.push(Some(continue_reply));
                            make_ready_future(req)
                        });
                    }
                    make_ready_future(req)
                };

                maybe_reply_continue(req).then(move |req: Box<Request>| {
                    let version = req._version.clone();
                    crate::engine::core::do_with::do_with(
                        (InputStream::<u8>::default(), version, Some(req), ChunkTargets::new()),
                        move |st: &mut ContentState| {
                            let st: *mut ContentState = st;
                            // SAFETY: the connection outlives process(), which awaits this.
                            let c = unsafe { &mut *this };
                            // SAFETY: do_with keeps the stream, the version, the request and the
                            // chunk targets alive and in place until the chain resolves.
                            let (content_stream, _, req, targets) = unsafe { &mut *st };
                            let Some(mut req) = req.take() else {
                                return make_ready_future(());
                            };
                            // SAFETY: the connection's stream and the targets (in this
                            // do_with) outlive the content stream, which is dropped first with
                            // the state; the request's maps stay in its box while the handler
                            // runs, and the targets are detached before the last read below.
                            *content_stream = unsafe { make_content_stream(&mut req, &mut c._read_buf, targets) };
                            let content_stream: *mut InputStream<u8> = content_stream;
                            let streaming = c.server().get_content_streaming();
                            // SAFETY: the content stream lives in the do_with state, alive, in
                            // place and used by nothing else until the chain resolves.
                            unsafe { set_request_content(req, content_stream, streaming) }
                                .then(move |req: Box<Request>| {
                                    // SAFETY: the connection outlives process(), which awaits this.
                                    let c = unsafe { &mut *this };
                                    c._replies
                                        .not_full()
                                        // SAFETY: the connection outlives process(); the wait
                                        // for queue room has finished with it.
                                        .then(move |()| unsafe { (*this).generate_reply(req) })
                                        .then(move |done: bool| {
                                            // SAFETY: the connection outlives process(), which
                                            // awaits this.
                                            let c = unsafe { &mut *this };
                                            c._done = done;
                                            // The handler may have dropped the request: nothing
                                            // parsed from now on goes into its maps.
                                            // SAFETY: do_with keeps the state alive until the
                                            // chain resolves, and no reference to the targets
                                            // is live here.
                                            unsafe { (*st).3.detach() };
                                            // If the handler did not read the entire request
                                            // content, this connection cannot be reused so we
                                            // need to close it (via "_done = true"). But we can't
                                            // just check content_stream.eof(): It may only become
                                            // true after read().
                                            // SAFETY: the content stream lives in the do_with
                                            // state, alive and in place until the chain resolves,
                                            // and the handler has finished with it.
                                            unsafe { &mut *content_stream }.read().then(move |buf: TemporaryBuffer<u8>| {
                                                if !buf.empty() {
                                                    // SAFETY: the connection outlives process().
                                                    unsafe { (*this)._done = true };
                                                }
                                            })
                                        })
                                })
                                .handle_exception_type(move |e: &crate::engine::http::exception::BaseException| {
                                    // If the request had a "Transfer-Encoding: chunked" header and content streaming wasn't enabled, we might have failed
                                    // before passing the request to handler - when we were parsing chunks
                                    // SAFETY: the connection outlives process(), which awaits this.
                                    let c = unsafe { &mut *this };
                                    let mut err_req = Box::new(Request::default());
                                    // SAFETY: do_with keeps the state, and its version, alive
                                    // until the chain resolves.
                                    err_req._version = unsafe { (*st).1.clone() };
                                    c.generate_error_reply_and_close(err_req, e.status(), &Sstring::from(e.str().as_str()));
                                })
                        },
                    )
                })
            })
        }

        /// Writes the replies as they are queued.
        ///
        /// # Safety
        /// The connection stays alive at its heap address until the returned future
        /// resolves ([`Connection::process`] owns it for that long).
        unsafe fn respond(&mut self) -> Future<()> {
            let this: *mut Connection = self;
            // SAFETY: the contract of this function.
            unsafe { self.do_response_loop() }.then_wrapped(move |mut f: Future<()>| {
                // SAFETY: the connection outlives process(), which awaits this.
                let c = unsafe { &mut *this };
                // swallow error
                if f.failed() {
                    c.server()._respond_errors += 1;
                }
                f.ignore_ready_future();
                c._write_buf.close()
            })
        }

        /// Pops the replies and writes them, until the null reply.
        ///
        /// # Safety
        /// The connection stays alive at its heap address until the returned future
        /// resolves ([`Connection::process`] owns it for that long).
        unsafe fn do_response_loop(&mut self) -> Future<()> {
            let this: *mut Connection = self;
            // SAFETY: the connection, and so its reply queue, outlives process(), which awaits
            // this, and does not move.
            unsafe { self._replies.pop_eventually() }.then(move |resp: Option<Box<Reply>>| {
                // SAFETY: the connection outlives process(), which awaits this.
                let c = unsafe { &mut *this };
                let Some(resp) = resp else {
                    // eof
                    return make_ready_future(());
                };
                c._resp = Some(resp);
                // SAFETY: the connection outlives process(), which awaits this.
                let written = unsafe { c.start_response() };
                // SAFETY: the connection outlives process(); the response has been written.
                written.then(move |()| unsafe { (*this).do_response_loop() })
            })
        }

        /// Sets the Server and Date headers.
        pub fn set_headers(&mut self, resp: &mut Reply) {
            resp._headers
                .insert(CaseInsensitiveKey(Sstring::from("Server")), Sstring::from(SERVER_HEADER));
            let date = self.server()._date.clone();
            resp._headers.insert(CaseInsensitiveKey(Sstring::from("Date")), date);
        }

        /// Ends the connection after the replies queued so far.
        fn close_on_body_error(&mut self) {
            self._done = true;
            self._replies.abort(ExceptionPtr::new(LogicError(
                "Unknown exception during body creation".to_string(),
            )));
            // A full queue drops the reply (push returns false).
            let _ = self._replies.push(None);
        }

        /// Writes the current reply.
        ///
        /// # Safety
        /// The connection stays alive at its heap address until the returned future
        /// resolves ([`Connection::process`] owns it for that long).
        unsafe fn start_response(&mut self) -> Future<()> {
            let this: *mut Connection = self;
            let Some(resp) = self._resp.as_deref_mut() else {
                return make_ready_future(());
            };
            // Every closure below relies on the connection outliving process(), which awaits
            // this; the steps run one at a time on the shard.
            if resp._body_writer.is_some() {
                let resp: *mut Reply = resp;
                // SAFETY: the reply is boxed in `_resp`, which only the last step below resets,
                // and the connection outlives process(); the write reaches the connection only
                // through its output stream, never through `_resp`.
                return unsafe { (*resp).write_reply_to_connection(&mut *this) }
                    .then_wrapped(move |mut f: Future<()>| {
                        // SAFETY: the connection outlives process(); the write has finished.
                        let c = unsafe { &mut *this };
                        if f.failed() {
                            // In case of an error during the write close the connection
                            c.server()._respond_errors += 1;
                            c.close_on_body_error();
                            f.ignore_ready_future();
                            return make_ready_future(());
                        }
                        c._write_buf.write(b"0\r\n\r\n")
                    })
                    .then_wrapped(move |mut f: Future<()>| {
                        // SAFETY: the connection outlives process(); the previous step is done.
                        let c = unsafe { &mut *this };
                        if f.failed() {
                            // We could not write the closing sequence
                            // Something is probably wrong with the connection,
                            // we should close it, so the client will disconnect
                            c.close_on_body_error();
                            f.ignore_ready_future();
                            make_ready_future(())
                        } else {
                            c._write_buf.flush()
                        }
                    })
                    .then_wrapped(move |mut f: Future<()>| {
                        // SAFETY: the connection outlives process(); the previous step is done.
                        let c = unsafe { &mut *this };
                        if f.failed() {
                            // flush failed. just close the connection
                            c.close_on_body_error();
                            f.ignore_ready_future();
                        }
                        c._resp = None;
                        make_ready_future(())
                    });
            }
            let resp: *mut Reply = resp;
            // SAFETY: `_resp` is set until the chain resets it.
            let r = unsafe { &mut *resp };
            self.set_headers(r);
            r._headers.insert(
                CaseInsensitiveKey(Sstring::from("Content-Length")),
                u64_to_sstring(r._content.size() as u64),
            );
            self._write_buf
                .write(r._response_line.as_bytes())
                // SAFETY: the reply stays boxed in `_resp` until the last step resets it, the
                // connection outlives process(), and the header write uses only its stream.
                .then(move |()| unsafe { (*resp).write_reply_headers(&mut *this) })
                // SAFETY: the connection outlives process(); the previous step is done.
                .then(move |()| unsafe { &mut (*this)._write_buf }.write(b"\r\n"))
                // SAFETY: the connection outlives process(); the previous step is done.
                .then(move |()| unsafe { (*this).write_body() })
                // SAFETY: the connection outlives process(); the previous step is done.
                .then(move |()| unsafe { &mut (*this)._write_buf }.flush())
                // SAFETY: the connection outlives process(); nothing borrows the reply now.
                .then(move |()| unsafe { (*this)._resp = None })
        }

        /// Hands the request to the routes and queues the reply; resolves to whether the
        /// connection is done.
        ///
        /// # Safety
        /// The connection stays alive at its heap address until the returned future
        /// resolves ([`Connection::process`] owns it for that long).
        unsafe fn generate_reply(&mut self, mut req: Box<Request>) -> Future<bool> {
            let mut resp = Box::new(Reply::new());
            resp.set_version(&req._version);
            self.set_headers(&mut resp);
            let keep_alive = req.should_keep_alive();
            if keep_alive && req._version.as_bytes() == b"1.0" {
                resp._headers.insert(
                    CaseInsensitiveKey(Sstring::from("Connection")),
                    Sstring::from("Keep-Alive"),
                );
            }

            let url = req.parse_query_param();
            let version = req._version.clone();
            let this: *mut Connection = self;
            self.server()._routes.handle(&url, req, resp).then(
                // Caller guarantees enough room
                move |mut rep: Box<Reply>| {
                    rep.set_version(&version).done();
                    // A full queue drops the reply (push returns false).
                    // SAFETY: the connection outlives process(), which awaits this.
                    let _ = unsafe { (*this)._replies.push(Some(rep)) };
                    make_ready_future(!keep_alive)
                },
            )
        }

        /// Queues an error reply and ends the connection.
        pub fn generate_error_reply_and_close(
            &mut self,
            req: Box<Request>,
            status: StatusType,
            msg: &Sstring,
        ) {
            let mut resp = Box::new(Reply::new());
            // TODO: Handle HTTP/2.0 when it releases
            resp.set_version(&req._version);
            resp.set_status(status, msg.clone());
            resp.done();
            self._done = true;
            // A full queue drops the reply (push returns false).
            let _ = self._replies.push(Some(resp));
        }

        /// Writes the content of the current reply.
        pub fn write_body(&mut self) -> Future<()> {
            match self._resp.as_deref() {
                Some(r) => self._write_buf.write(r._content.as_bytes()),
                None => make_ready_future(()),
            }
        }

        /// The output stream of the connection.
        #[inline]
        pub fn out(&mut self) -> &mut OutputStream<u8> {
            &mut self._write_buf
        }
    }

    impl Drop for Connection {
        fn drop(&mut self) {
            self.server()._current_connections -= 1;
            self._hook.unlink();
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

    /// The state of the content of a request: the content stream, the version, the request
    /// until it is handed to the routes, and where the chunked source saves its extensions
    /// and trailers.
    type ContentState = (InputStream<u8>, Sstring, Option<Box<Request>>, ChunkTargets);

    /// # Safety
    /// `buf` and `targets` outlive the returned stream and stay in place; the request's maps
    /// stay valid while `targets` points at them.
    unsafe fn make_content_stream(
        req: &mut Request,
        buf: *mut InputStream<u8>,
        targets: &ChunkTargets,
    ) -> InputStream<u8> {
        // Create an input stream based on the requests body encoding or lack thereof
        if CaseInsensitiveCmp.call(req.get_header("Transfer-Encoding").as_bytes(), b"chunked") {
            // SAFETY: the caller's contract.
            unsafe {
                targets.set(&mut req.chunk_extensions, &mut req.trailing_headers);
                InputStream::new(DataSource::new(Box::new(ChunkedSourceImpl::new(buf, targets))))
            }
        } else {
            // SAFETY: the caller's contract.
            InputStream::new(DataSource::new(Box::new(unsafe {
                ContentLengthSourceImpl::new(buf, req.content_length)
            })))
        }
    }

    /// Points the request at its content stream, and reads the whole content into the
    /// request unless the handlers stream it.
    ///
    /// # Safety
    /// `content_stream` stays alive and in place, and unused elsewhere, until the returned
    /// future resolves, and after that while the request points at it.
    unsafe fn set_request_content(
        mut req: Box<Request>,
        content_stream: *mut InputStream<u8>,
        streaming: bool,
    ) -> Future<Box<Request>> {
        req.content_stream = content_stream;

        if streaming {
            make_ready_future(req)
        } else {
            // Read the entire content into the request content string
            // SAFETY: the contract of this function.
            unsafe { read_entire_stream_contiguous(&mut *content_stream) }.then(
                move |content: Sstring| {
                    req.content = content;
                    make_ready_future(req)
                },
            )
        }
    }

    /// The tls credentials of a server.
    pub type ServerCredentialsPtr = Option<SharedPtr<ServerCredentials>>;

    /// The interval of the refresh of the cached date.
    const DATE_REFRESH_INTERVAL: Duration = Duration::from_secs(1);

    /// An http server.
    ///
    /// The date timer, the metrics, the accept loops and the connections point at the server,
    /// so it must not move once linked. It is `!Unpin`: it is built by [`HttpServer::new`],
    /// then [`HttpServer::start`] links it at its final place, and the methods that leave
    /// pointers to it behind take it pinned. A sharded server starts each instance where it
    /// lives.
    pub struct HttpServer {
        _listeners: Vec<ServerSocket>,
        _stats: Option<HttpStats>,
        _total_connections: u64,
        _current_connections: u64,
        _requests_served: u64,
        _read_errors: u64,
        _respond_errors: u64,
        _credentials: ServerCredentialsPtr,
        _date: Sstring,
        _date_format_timer: Timer,
        _content_length_limit: usize,
        _content_streaming: bool,
        _task_gate: Gate,
        /// The routes of the server.
        pub _routes: Routes,
        _connections: List<ConnectionAdapter>,
        /// The label of the metrics, kept until [`HttpServer::start`] registers them.
        _name: Sstring,
        _pinned: PhantomPinned,
    }

    /// A sharded server stops each shard's instance with [`HttpServer::stop`].
    impl crate::engine::core::sharded::ShardedService for HttpServer {
        #[inline]
        fn stop(&mut self) -> Future<()> {
            HttpServer::stop(self)
        }

        /// Called once the instance is in its shared object, where it stays until destroyed:
        /// links the server there ([`HttpServer::start`]).
        fn set_container(&mut self, _container: *mut Sharded<Self>) {
            // SAFETY: `sharded` keeps the instance in its shared object, at this address,
            // until it is destroyed.
            unsafe { Pin::new_unchecked(self) }.start();
        }
    }

    impl HttpServer {
        /// A server whose metrics carry the label service=`name`; [`HttpServer::start`]
        /// links it at its final place.
        pub fn new(name: &Sstring) -> Self {
            HttpServer {
                _listeners: Vec::new(),
                _stats: None,
                _total_connections: 0,
                _current_connections: 0,
                _requests_served: 0,
                _read_errors: 0,
                _respond_errors: 0,
                _credentials: None,
                _date: Self::http_date(),
                _date_format_timer: Timer::default(),
                _content_length_limit: usize::MAX,
                _content_streaming: false,
                _task_gate: Gate::default(),
                _routes: Routes::new(),
                _connections: List::new(),
                _name: name.clone(),
                _pinned: PhantomPinned,
            }
        }

        /// Links the server at its final place: registers the
        /// metrics and arms the date timer. A second call does nothing.
        pub fn start(self: Pin<&mut Self>) {
            // SAFETY: the server is not moved out of the pin; it stays at this address until
            // it is dropped, and the stats and the timer that point at it are its members,
            // dropped with it.
            let me = unsafe { self.get_unchecked_mut() };
            if me._stats.is_some() {
                return;
            }
            let this: *mut HttpServer = me;
            // SAFETY: as above.
            me._stats = Some(unsafe { HttpStats::new(this, &me._name) });
            // SAFETY: the timer is a member of the pinned server, so it does not move.
            let mut timer = unsafe { Pin::new_unchecked(&mut me._date_format_timer) };
            // SAFETY: the timer is a member of the server, so its callback runs while the
            // server is alive.
            timer.as_mut().set_callback(move || unsafe { (*this)._date = HttpServer::http_date() });
            timer.arm_periodic(DATE_REFRESH_INTERVAL);
        }

        /// Sets the TLS credentials of the server, which then serves HTTPS.
        ///
        /// Set them before the server starts: build a
        /// [`CredentialsBuilder`](crate::engine::net::tls::CredentialsBuilder) (DH level,
        /// certificate and key files, trust), share it with every shard, and on each shard
        /// pass the result of its `build_server_credentials` to this method. Prefer
        /// [`HttpServer::listen_creds`], which takes the credentials per listener.
        #[deprecated(note = "pass the credentials per listener with HttpServer::listen_creds")]
        pub fn set_tls_credentials(&mut self, credentials: ServerCredentialsPtr) {
            self._credentials = credentials;
        }

        /// The largest content length accepted.
        pub fn get_content_length_limit(&self) -> usize {
            self._content_length_limit
        }

        /// Sets the largest content length accepted.
        pub fn set_content_length_limit(&mut self, limit: usize) {
            self._content_length_limit = limit;
        }

        /// Whether the handlers read the content as a stream.
        pub fn get_content_streaming(&self) -> bool {
            self._content_streaming
        }

        /// Sets whether the handlers read the content as a stream.
        pub fn set_content_streaming(&mut self, b: bool) {
            self._content_streaming = b;
        }

        /// Listens on `addr` with options `lo`, over tls when credentials are given.
        pub fn listen_with(
            mut self: Pin<&mut Self>,
            addr: SocketAddress,
            lo: ListenOptions,
            listener_credentials: ServerCredentialsPtr,
        ) -> Future<()> {
            let with_tls = listener_credentials.is_some();
            // SAFETY: only the listener list changes; the server is not moved.
            let listeners = unsafe { &mut self.as_mut().get_unchecked_mut()._listeners };
            match listener_credentials {
                Some(c) => match crate::engine::net::tls::listen(c, addr, lo) {
                    Ok(listener) => listeners.push(listener),
                    Err(e) => return make_exception_future(e),
                },
                None => match crate::engine::core::api::listen_with(addr, lo) {
                    Ok(listener) => listeners.push(listener),
                    Err(e) => return make_exception_future(e),
                },
            }
            let which = listeners.len() - 1;
            self.do_accepts_tls(which, with_tls)
        }

        /// Listens on `addr` with options `lo` and the server credentials.
        pub fn listen_opts(
            self: Pin<&mut Self>,
            addr: SocketAddress,
            lo: ListenOptions,
        ) -> Future<()> {
            let c = self._credentials.clone();
            self.listen_with(addr, lo, c)
        }

        /// Listens on `addr`, reusing the address, with the given credentials.
        pub fn listen_creds(
            self: Pin<&mut Self>,
            addr: SocketAddress,
            listener_credentials: ServerCredentialsPtr,
        ) -> Future<()> {
            let lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
            self.listen_with(addr, lo, listener_credentials)
        }

        /// Listens on `addr`, reusing the address.
        pub fn listen(self: Pin<&mut Self>, addr: SocketAddress) -> Future<()> {
            let lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
            self.listen_opts(addr, lo)
        }

        /// Stops accepting and shuts the connections down.
        pub fn stop(&mut self) -> Future<()> {
            let tasks_done = self._task_gate.close();
            for l in &mut self._listeners {
                l.abort_accept();
            }
            for c in self._connections.iter() {
                // SAFETY: the linked connections are alive (they unlink in drop()).
                unsafe { (*c.as_ptr()).shutdown() };
            }
            tasks_done
        }

        // FIXME: This could return void
        /// Accepts connections of listener `which` in the background.
        pub fn do_accepts_tls(self: Pin<&mut Self>, which: usize, tls: bool) -> Future<()> {
            // SAFETY: the server is pinned; the pointer is kept by the accept loop.
            let this: *mut HttpServer = unsafe { self.get_unchecked_mut() };
            // SAFETY: the server is pinned, and its gate keeps it alive until stop() resolves;
            // the gate is a member of the pinned server, so it does not move.
            let _ = try_with_gate(unsafe { Pin::new_unchecked(&(*this)._task_gate) }, move || {
                keep_doing(move || {
                    // SAFETY: as above; each accept runs inside the gate, so the server is
                    // alive and pinned while it runs.
                    try_with_gate(
                        unsafe { Pin::new_unchecked(&(*this)._task_gate) },
                        move || unsafe { (*this).do_accept_one(which, tls) },
                    )
                })
                .handle_exception_type(|_e: &GateClosedException| {})
            })
            .handle_exception_type(|_e: &GateClosedException| {});
            make_ready_future(())
        }

        /// Accepts connections of listener `which`, over tls when the server has credentials.
        pub fn do_accepts(self: Pin<&mut Self>, which: usize) -> Future<()> {
            let tls = self._credentials.is_some();
            self.do_accepts_tls(which, tls)
        }

        /// The number of connections opened.
        pub fn total_connections(&self) -> u64 {
            self._total_connections
        }

        /// The number of connections open.
        pub fn current_connections(&self) -> u64 {
            self._current_connections
        }

        /// The number of requests served.
        pub fn requests_served(&self) -> u64 {
            self._requests_served
        }

        /// The number of errors while reading requests.
        pub fn read_errors(&self) -> u64 {
            self._read_errors
        }

        /// The number of errors while replying.
        pub fn reply_errors(&self) -> u64 {
            self._respond_errors
        }

        // Write the current date in the specific "preferred format" defined in
        // RFC 7231, Section 7.1.1.1, a.k.a. IMF (Internet Message Format) fixdate.
        // For example: Sun, 06 Nov 1994 08:49:37 GMT
        /// The current date as an http date.
        pub fn http_date() -> Sstring {
            // SAFETY: time() with a null argument only reads the clock.
            let t = unsafe { time(std::ptr::null_mut()) };
            let mut tm = Tm::default();
            // SAFETY: both pointers are valid for the call.
            unsafe { gmtime_r(&t, &mut tm) };
            // Using strftime() would have been easier, but unfortunately relies on
            // the current locale, and we need the month and day names in English.
            const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
            const MONTHS: [&str; 12] = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            Sstring::from(format!(
                "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
                DAYS[tm.tm_wday as usize % 7],
                tm.tm_mday,
                MONTHS[tm.tm_mon as usize % 12],
                1900 + tm.tm_year,
                tm.tm_hour,
                tm.tm_min,
                tm.tm_sec
            ))
        }

        fn do_accept_one(&mut self, which: usize, tls: bool) -> Future<()> {
            let this: *mut HttpServer = self;
            self._listeners[which]
                .accept()
                .then(move |ar: AcceptResult| {
                    // SAFETY: the gate keeps the server alive while it accepts.
                    let local_address = ar.connection.local_address();
                    // SAFETY: the server is pinned and its task gate keeps it alive until the
                    // connection's processing ends.
                    let conn = unsafe {
                        Connection::with_addresses(
                            this,
                            ar.connection,
                            ar.remote_address,
                            local_address,
                            tls,
                        )
                    };
                    // If the gate is closed the closure is dropped unrun, and with it the
                    // connection.
                    // SAFETY: the gate is a member of the pinned server.
                    let _ = try_with_gate(
                        unsafe { Pin::new_unchecked(&(*this)._task_gate) },
                        move || {
                            // The processing owns the connection and drops it when it ends.
                            Connection::process(conn).handle_exception(move |ex: ExceptionPtr| {
                                log_error!(HLOGGER, "request error: {}", ex);
                            })
                        },
                    )
                    .handle_exception_type(|_e: &GateClosedException| {});
                })
                // A system error from accept is a `SystemError` (reactor) or an
                // `std::io::Error`; both are handled the same way.
                .handle_exception_type(|e: &crate::engine::core::future::SystemError| {
                    // We expect a ECONNABORTED when http_server::stop is called,
                    // no point in warning about that.
                    if e.code() != ECONNABORTED {
                        log_error!(HLOGGER, "accept failed: {}", e);
                    }
                })
                .handle_exception_type(|e: &std::io::Error| {
                    // We expect a ECONNABORTED when http_server::stop is called,
                    // no point in warning about that.
                    if e.raw_os_error() != Some(ECONNABORTED) {
                        log_error!(HLOGGER, "accept failed: {}", e);
                    }
                })
                .handle_exception(|ex: ExceptionPtr| {
                    log_error!(HLOGGER, "accept failed: {}", ex);
                })
        }
    }

    /// Gives the tests access to the listeners of a server.
    pub struct HttpServerTester;

    impl HttpServerTester {
        /// The listeners of `server`.
        pub fn listeners(server: &mut HttpServer) -> &mut Vec<ServerSocket> {
            &mut server._listeners
        }
    }

    /// A broken-down calendar time, laid out as the C `struct tm` (for `gmtime_r`).
    #[repr(C)]
    #[derive(Default)]
    struct Tm {
        tm_sec: i32,
        tm_min: i32,
        tm_hour: i32,
        tm_mday: i32,
        tm_mon: i32,
        tm_year: i32,
        tm_wday: i32,
        tm_yday: i32,
        tm_isdst: i32,
        tm_gmtoff: i64,
        tm_zone: usize,
    }

    unsafe extern "C" {
        fn time(t: *mut i64) -> i64;
        fn gmtime_r(t: *const i64, tm: *mut Tm) -> *mut Tm;
    }

    /// A helper to start, set up and listen an http server on every shard.
    ///
    /// Typical use: create it, `start()` it (one server per shard), then
    /// `set_routes(...)` and `listen(addr)`; `stop()` stops every shard's server.
    pub struct HttpServerControl {
        _server_dist: Pin<Box<Sharded<HttpServer>>>,
        /// Tells a listen that resumes after a wait whether the control, and so the servers,
        /// still exist: it holds a weak reference to this token.
        _alive: std::rc::Rc<()>,
    }

    impl Default for HttpServerControl {
        fn default() -> Self {
            Self::new()
        }
    }

    /// One shard's server inside the sharded service, pinned where it lives.
    #[inline]
    fn pinned(server: &mut HttpServer) -> Pin<&mut HttpServer> {
        // SAFETY: `sharded` keeps each instance in its shared object, at a fixed address,
        // until it is destroyed, and HttpServerControl never moves one out.
        unsafe { Pin::new_unchecked(server) }
    }

    thread_local! {
        static IDGEN: std::cell::Cell<u16> = const { std::cell::Cell::new(0) };
    }

    impl HttpServerControl {
        /// A control of a not yet started server.
        pub fn new() -> Self {
            HttpServerControl {
                _server_dist: Box::pin(Sharded::new()),
                _alive: std::rc::Rc::new(()),
            }
        }

        /// `http-<n>`, unique per thread.
        pub fn generate_server_name() -> Sstring {
            let id = IDGEN.with(|g| {
                let v = g.get();
                g.set(v.wrapping_add(1));
                v
            });
            Sstring::from(format!("http-{}", id))
        }

        /// Starts a server on every shard.
        pub fn start(&mut self, name: &Sstring) -> Future<()> {
            let name = name.clone();
            self._server_dist.as_mut().start(move || HttpServer::new(&name))
        }

        /// Stops the servers.
        pub fn stop(&mut self) -> Future<()> {
            self._server_dist.as_mut().stop()
        }
    }

    /// The control object can be stopped through the
    /// [`Stoppable`](crate::engine::util::closeable::Stoppable) interface, for example by a
    /// deferred stop guard.
    impl crate::engine::util::closeable::Stoppable for HttpServerControl {
        fn stop(&mut self) -> Future<()> {
            HttpServerControl::stop(self)
        }
    }

    impl HttpServerControl {
        /// Calls `fun` with the routes of every shard.
        pub fn set_routes(
            &mut self,
            fun: impl Fn(&mut Routes) + Clone + Send + 'static,
        ) -> Future<()> {
            self._server_dist.as_mut().invoke_on_all(move |server: &mut HttpServer| {
                fun(&mut server._routes);
                make_ready_future(())
            })
        }

        /// Listens on `addr` on every shard.
        pub fn listen(&mut self, addr: SocketAddress) -> Future<()> {
            self._server_dist
                .as_mut()
                .invoke_on_all(move |server: &mut HttpServer| pinned(server).listen(addr))
        }

        /// Listens on `addr` over tls on every shard.
        ///
        /// Each shard gets credentials of its own over the same gnutls objects (see
        /// `ServerCredentials::share_with_shards`), since the credentials pointer is not
        /// thread-safe.
        pub fn listen_creds(
            &mut self,
            addr: SocketAddress,
            credentials: ServerCredentialsPtr,
        ) -> Future<()> {
            let lo = ListenOptions { reuse_address: true, ..ListenOptions::default() };
            self.listen_with(addr, lo, credentials)
        }

        /// Listens on `addr` with options `lo` on every shard.
        pub fn listen_opts(&mut self, addr: SocketAddress, lo: ListenOptions) -> Future<()> {
            self._server_dist.as_mut().invoke_on_all(move |server: &mut HttpServer| {
                pinned(server).listen_opts(addr, lo.clone())
            })
        }

        /// Listens on `addr` with options `lo` over tls on every shard.
        pub fn listen_with(
            &mut self,
            addr: SocketAddress,
            lo: ListenOptions,
            credentials: ServerCredentialsPtr,
        ) -> Future<()> {
            let Some(credentials) = credentials else {
                return self._server_dist.as_mut().invoke_on_all(move |server: &mut HttpServer| {
                    pinned(server).listen_with(addr, lo.clone(), None)
                });
            };
            // The sharded servers stay at this heap address while the control lives, even when
            // the control moves; whether it still lives is checked before the pointer is used.
            // SAFETY: only the address is taken; the servers are not moved out of the pin.
            let dist: *mut Sharded<HttpServer> =
                unsafe { self._server_dist.as_mut().get_unchecked_mut() };
            let alive = std::rc::Rc::downgrade(&self._alive);
            ServerCredentials::share_with_shards(&credentials).then(move |shared| {
                if alive.upgrade().is_none() {
                    return make_exception_future(ExceptionPtr::new(LogicError(
                        "http server control dropped before its listen started".to_string(),
                    )));
                }
                // SAFETY: the control is alive (its token is), so its boxed servers are, at
                // the address taken above, pinned; nothing else borrows them while this
                // step runs on the shard.
                unsafe { Pin::new_unchecked(&mut *dist) }.invoke_on_all(
                    move |server: &mut HttpServer| {
                        pinned(server).listen_with(addr, lo.clone(), Some(shared.into_local()))
                    },
                )
            })
        }

        /// The servers.
        pub fn server(&mut self) -> Pin<&mut Sharded<HttpServer>> {
            self._server_dist.as_mut()
        }
    }
}
