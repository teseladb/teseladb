//! The WebSocket connection shared by the server side, and the handshake key.
//!
//! [`websocket::Connection`] owns the socket and two in-memory queues: the read loop
//! decodes frames from the socket into the input queue, the handler reads the application
//! data from its input stream and writes replies into its output stream, and the write
//! loop frames them back onto the socket. The queues and the handler's streams live in a
//! heap block of their own, apart from the rest of the connection: the loops borrow the
//! connection while the handler uses its streams, and the two never overlap. The
//! handshake accept key is the base64 SHA-1 of the client key and the RFC 6455 suffix,
//! computed with gnutls.

/// The WebSocket connection, its handler type and errors (interface still subject to change).
pub mod websocket {
    use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
    use std::fmt;
    use std::rc::Rc;
    use std::sync::LazyLock;

    use crate::engine::core::byteorder::write_be;
    use crate::engine::core::future::{
        ExceptionPtr, Future, make_exception_future, make_ready_future,
    };
    use crate::engine::core::iostream::{
        DataSink, DataSinkImpl, DataSource, DataSourceImpl, InputStream, OutputStream,
    };
    use crate::engine::core::loop_::do_until;
    use crate::engine::core::queue::Queue;
    use crate::engine::core::scattered_message::ScatteredMessage;
    use crate::engine::core::sstring::Sstring;
    use crate::engine::core::temporary_buffer::TemporaryBuffer;
    use crate::engine::core::when_all::when_all_succeed;
    use crate::engine::http::common::support::RawBox;
    use crate::engine::net::api::ConnectedSocket;
    use crate::engine::net::packet::Packet;
    use crate::engine::util::log::Logger;
    pub use crate::engine::websocket::parser::websocket::{Opcodes, WebsocketParser};
    use crate::{engine_assert, log_debug};

    /// The suffix of the key whose sha1 the server returns in Sec-WebSocket-Accept.
    pub const MAGIC_KEY_SUFFIX: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

    /// The handler of a subprotocol: it reads the messages from the input stream and
    /// writes the replies into the output stream.
    ///
    /// The server hands each connection its own reference to the handler; all connections
    /// share the same callable, which an `Fn` handler cannot tell apart from a copy.
    pub type HandlerT = Rc<dyn Fn(&mut InputStream<u8>, &mut OutputStream<u8>) -> Future<()>>;

    /// An error in handling a WebSocket connection; holds its message.
    #[derive(Clone, Debug)]
    pub struct Exception {
        _msg: String,
    }

    impl Exception {
        /// An error with message `msg`.
        pub fn new(msg: &str) -> Self {
            Exception { _msg: msg.to_string() }
        }

        /// The message.
        pub fn what(&self) -> &str {
            &self._msg
        }
    }

    impl fmt::Display for Exception {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self._msg)
        }
    }

    impl std::error::Error for Exception {}

    type BuffT = TemporaryBuffer<u8>;

    /// The data source of a connection's input stream: reads the buffers the connection
    /// queues from the socket.
    ///
    /// `data` is a queue of the connection's [`HandlerSide`], taken from the block's own
    /// pointer; the block outlives the stream, which is one of its members.
    struct ConnectionSourceImpl {
        data: *mut Queue<BuffT>,
    }

    impl DataSourceImpl for ConnectionSourceImpl {
        fn get(&mut self) -> Future<BuffT> {
            // SAFETY: the queue is a member of the connection that owns this stream.
            unsafe { (*self.data).pop_eventually() }.then_wrapped(|f: Future<BuffT>| {
                match f.get() {
                    Ok(b) => make_ready_future(b),
                    Err(e) => make_exception_future(e),
                }
            })
        }

        fn close(&mut self) -> Future<()> {
            // A full queue drops the end-of-stream buffer (push returns false).
            // SAFETY: as in get().
            let _ = unsafe { (*self.data).push(BuffT::new(0)) };
            make_ready_future(())
        }
    }

    /// The data sink of a connection's output stream: queues the buffers the connection
    /// sends to the socket.
    ///
    /// `data` points at a queue of the [`HandlerSide`], as for [`ConnectionSourceImpl`].
    struct ConnectionSinkImpl {
        data: *mut Queue<BuffT>,
    }

    impl DataSinkImpl for ConnectionSinkImpl {
        fn put(&mut self, d: Packet) -> Future<()> {
            let f = d.frag(0);
            // temporary_buffer<char>{f.base, f.size} copies the first fragment.
            // SAFETY: a fragment of a packet points at `size` valid bytes.
            let b = BuffT::copy_from(unsafe { std::slice::from_raw_parts(f.base, f.size) });
            // SAFETY: the queue is a member of the connection that owns this stream.
            unsafe { (*self.data).push_eventually(b) }
        }

        fn buffer_size(&self) -> usize {
            // SAFETY: as in put().
            unsafe { (*self.data).max_size() }
        }

        fn close(&mut self) -> Future<()> {
            // A full queue drops the end-of-stream buffer (push returns false).
            // SAFETY: as in put().
            let _ = unsafe { (*self.data).push(BuffT::new(0)) };
            make_ready_future(())
        }
    }

    /// The part of a connection the handler uses: the queues between the socket loops and
    /// the handler, and the handler's streams over them.
    ///
    /// It lives in its own heap block, owned by the connection through a [`RawBox`], so a
    /// borrow of the connection never covers it: the handler keeps using its streams while
    /// the read and write loops borrow the connection. Every access to the block goes
    /// through the block's pointer (`HandlerSide::ptr`), and the streams' own sources and
    /// sinks reach the queues through pointers taken from it the same way.
    pub(crate) struct HandlerSide {
        input_buffer: Queue<TemporaryBuffer<u8>>,
        input: InputStream<u8>,
        output_buffer: Queue<TemporaryBuffer<u8>>,
        output: OutputStream<u8>,
    }

    /// A server WebSocket connection.
    ///
    /// The handler's streams and their queues live in a heap block of their own (`HandlerSide`),
    /// so the connection itself may move.
    pub struct Connection {
        pub(crate) _fd: ConnectedSocket,
        pub(crate) _read_buf: InputStream<u8>,
        pub(crate) _write_buf: OutputStream<u8>,
        pub(crate) _done: bool,

        pub(crate) _websocket_parser: WebsocketParser,
        /// The handler's streams and queues (see [`HandlerSide`]).
        _side: RawBox<HandlerSide>,

        pub(crate) _subprotocol: Sstring,
        pub(crate) _handler: Option<HandlerT>,
    }

    impl Connection {
        /// The size of the queues between the socket and the handler.
        pub const PIPE_SIZE: usize = 512;

        /// Creates a connection over the established socket `fd`, with the handler's
        /// streams connected to their queues.
        pub fn new(mut fd: ConnectedSocket) -> Self {
            let read_buf =
                fd.input(crate::engine::net::api::ConnectedSocketInputStreamConfig::default());
            let write_buf = fd.output(8192);
            let side = RawBox::new(Box::new(HandlerSide {
                input_buffer: Queue::new(Self::PIPE_SIZE),
                input: InputStream::default(),
                output_buffer: Queue::new(Self::PIPE_SIZE),
                output: OutputStream::default(),
            }));
            let sp = side.as_ptr();
            // SAFETY: `sp` is the fresh block's pointer and no reference to it is live; the
            // queues stay at these addresses until the block, and the streams in it, go.
            unsafe {
                (*sp).input = InputStream::new(DataSource::new(Box::new(ConnectionSourceImpl {
                    data: std::ptr::addr_of_mut!((*sp).input_buffer),
                })));
                (*sp).output = OutputStream::new(DataSink::new(Box::new(ConnectionSinkImpl {
                    data: std::ptr::addr_of_mut!((*sp).output_buffer),
                })));
            }
            Connection {
                _fd: fd,
                _read_buf: read_buf,
                _write_buf: write_buf,
                _done: false,
                _websocket_parser: WebsocketParser::default(),
                _side: side,
                _subprotocol: Sstring::new(),
                _handler: None,
            }
        }

        /// The pointer to the handler's block, from which every access to it is made (see
        /// [`HandlerSide`]). Valid while the connection lives.
        #[inline]
        pub(crate) fn side(&self) -> *mut HandlerSide {
            self._side.as_ptr()
        }

        /// Runs `handler` over the handler's streams.
        ///
        /// # Safety
        /// The connection stays alive until the returned future resolves, and no other
        /// reference to the streams is made meanwhile except through
        /// [`Connection::close`], which reaches only their states.
        pub(crate) unsafe fn run_handler(&self, handler: &HandlerT) -> Future<()> {
            let sp = self.side();
            // SAFETY: the block is alive (the contract above); these are the only references
            // made to the stream handles, and nothing else borrows them while the handler
            // runs.
            unsafe { handler(&mut (*sp).input, &mut (*sp).output) }
        }

        /// Ends the response loop of a connection on which no handler will run (its upgrade
        /// was refused, or the client left before sending it): the loop would otherwise
        /// wait for output forever. The output queue is empty, as nothing has written to it.
        pub(crate) fn end_response_loop(&mut self) {
            self._done = true;
            let side = self.side();
            // SAFETY: the block is alive while the connection is; this is the only reference
            // made to the output queue during the call.
            unsafe {
                (*side).output_buffer.abort(ExceptionPtr::new(Exception::new(
                    "the connection ended before its upgrade",
                )))
            };
        }

        /// Handles a received PING frame
        /// (<https://datatracker.ietf.org/doc/html/rfc6455#section-5.5.2>); not implemented
        /// yet, it resolves at once.
        pub(crate) fn handle_ping(&mut self) -> Future<()> {
            // TODO
            make_ready_future(())
        }

        /// Handles a received PONG frame
        /// (<https://datatracker.ietf.org/doc/html/rfc6455#section-5.5.3>); not implemented
        /// yet, it resolves at once.
        pub(crate) fn handle_pong(&mut self) -> Future<()> {
            // TODO
            make_ready_future(())
        }

        /// Packs `buff` in a final WebSocket frame with opcode `opcode` and sends it to the
        /// client.
        ///
        /// The flush that follows the write holds the output stream's state, not the
        /// connection: dropping the connection before it ends aborts (see `StateRef`).
        pub(crate) fn send_data(
            &mut self,
            opcode: Opcodes,
            buff: TemporaryBuffer<u8>,
        ) -> Future<()> {
            let mut header = [0u8; 10];
            header[0] = 0x80;
            let mut header_size = 2;

            header[0] = header[0].wrapping_add(opcode.0);

            if 126 <= buff.size() && buff.size() <= usize::from(u16::MAX) {
                header[1] = 0x7E;
                // SAFETY: `header` has ten bytes; two plus `size_of::<u16>()` fit.
                unsafe { write_be::<u16>(header.as_mut_ptr().add(2), buff.size() as u16) };
                header_size += size_of::<u16>();
            } else if usize::from(u16::MAX) < buff.size() {
                header[1] = 0x7F;
                // SAFETY: `header` has ten bytes; two plus `size_of::<u64>()` fit.
                unsafe { write_be::<u64>(header.as_mut_ptr().add(2), buff.size() as u64) };
                header_size += size_of::<u64>();
            } else {
                header[1] = buff.size() as u8;
            }

            let mut msg = ScatteredMessage::new();
            msg.append_sstring(Sstring::from_bytes(&header[..header_size]));
            msg.append_buffer(buff);
            let out = self._write_buf.hold();
            self._write_buf.write_scattered(msg).then(move |()| {
                // SAFETY: `out` keeps the stream's state alive; the write has resolved and the
                // stream's operations run one at a time, so no other reference to it is live.
                unsafe { (*out.ptr()).flush() }
            })
        }

        /// Frames what the handler writes and sends it, until the connection is done.
        ///
        /// # Safety
        /// The connection stays alive and in place until the returned future resolves.
        pub(crate) unsafe fn response_loop(&mut self) -> Future<()> {
            let this: *mut Connection = self;
            let side = self.side();
            // Every closure below relies on the connection outliving its processing, which
            // awaits this loop, at a fixed heap address.
            do_until(
                // SAFETY: the connection outlives its processing; this only reads a flag.
                move || unsafe { (*this)._done },
                move || {
                    // FIXME: implement error handling
                    // SAFETY: the connection, and so the handler's block with the output
                    // queue, outlives its processing, which awaits this loop; the block does
                    // not move, and only this place of it is borrowed, for the call.
                    unsafe { (*side).output_buffer.pop_eventually() }
                        // SAFETY: the connection outlives its processing; the pop has finished.
                        .then(move |buf: TemporaryBuffer<u8>| unsafe {
                            (*this).send_data(Opcodes::BINARY, buf)
                        })
                },
            )
            // SAFETY: the connection outlives its processing; the loop has ended.
            .finally(move || unsafe { (*this)._write_buf.close() })
        }

        /// Shuts down the input side of the socket.
        pub fn shutdown_input(&mut self) {
            self._fd.shutdown_input();
        }

        /// Ends the connection, sending a close frame first when `send_close`, then closing
        /// the handler's streams and shutting the socket down.
        ///
        /// The handler's streams are closed through their states, not their handles, so a
        /// handler still using its streams is not disturbed by the close.
        ///
        /// # Safety
        /// The connection stays alive and in place until the returned future resolves.
        pub unsafe fn close(&mut self, send_close: bool) -> Future<()> {
            let this: *mut Connection = self;
            let first = if send_close {
                self.send_data(Opcodes::CLOSE, TemporaryBuffer::new(0))
            } else {
                make_ready_future(())
            };
            first.finally(move || {
                // SAFETY: the connection is alive (the contract above); this reference covers
                // the connection only, not the handler's block.
                let c = unsafe { &mut *this };
                c._done = true;
                let sp = c.side();
                // SAFETY: the block is alive with the connection. Only the handles are read,
                // to reach the states; each state is borrowed only for its close call, and
                // the handler, which runs one step at a time on the shard, holds no
                // reference to a state between its steps.
                let closes = unsafe {
                    let (input, output) = ((*sp).input.hold(), (*sp).output.hold());
                    ((*input.ptr()).close(), (*output.ptr()).close())
                };
                when_all_succeed(closes)
                    .discard_result()
                    // SAFETY: the connection outlives its processing.
                    .finally(move || unsafe { (*this)._fd.shutdown_output() })
            })
        }

        /// Reads and handles one frame.
        ///
        /// # Safety
        /// The connection stays alive and in place until the returned future resolves.
        pub(crate) unsafe fn read_one(&mut self) -> Future<()> {
            let this: *mut Connection = self;
            let side = self.side();
            let parser: *mut WebsocketParser = &mut self._websocket_parser;
            // SAFETY: the connection outlives its processing and owns the parser, which only
            // the consumer uses while the stream is consumed.
            self._read_buf
                .consume(move |data: TemporaryBuffer<u8>| unsafe { (*parser).call(data) })
                .then(move |()| {
                    // SAFETY: the connection outlives its processing; the consume has finished.
                    let c = unsafe { &mut *this };
                    if c._websocket_parser.is_valid() {
                        // FIXME: implement error handling
                        match c._websocket_parser.opcode() {
                            // We do not distinguish between these 3 types.
                            Opcodes::CONTINUATION | Opcodes::TEXT | Opcodes::BINARY => {
                                let r = c._websocket_parser.result();
                                // SAFETY: the connection, and so the handler's block with the
                                // input queue, outlives its read loop, which awaits this; the block
                                // does not move, and only this place of it is borrowed.
                                return unsafe { (*side).input_buffer.push_eventually(r) };
                            }
                            Opcodes::CLOSE => {
                                log_debug!(WEBSOCKET_LOGGER, "Received close frame.");
                                // datatracker.ietf.org/doc/html/rfc6455#section-5.5.1
                                // SAFETY: the contract of this function covers the close.
                                return unsafe { c.close(true) };
                            }
                            Opcodes::PING => {
                                log_debug!(WEBSOCKET_LOGGER, "Received ping frame.");
                                return c.handle_ping();
                            }
                            Opcodes::PONG => {
                                log_debug!(WEBSOCKET_LOGGER, "Received pong frame.");
                                return c.handle_pong();
                            }
                            // Invalid - do nothing.
                            _ => {}
                        }
                    } else if c._websocket_parser.eof() {
                        // SAFETY: the contract of this function covers the close.
                        return unsafe { c.close(false) };
                    }
                    log_debug!(WEBSOCKET_LOGGER, "Reading from socket has failed.");
                    // SAFETY: the contract of this function covers the close.
                    unsafe { c.close(true) }
                })
        }
    }

    /// `GNUTLS_DIG_SHA1`
    const GNUTLS_DIG_SHA1: c_int = 3;
    /// `GNUTLS_E_SUCCESS`
    const GNUTLS_E_SUCCESS: c_int = 0;
    /// The size of a sha1 digest.
    const SHA1_SIZE: usize = 20;

    #[repr(C)]
    struct GnutlsDatum {
        data: *mut u8,
        size: c_uint,
    }

    #[link(name = "gnutls")]
    unsafe extern "C" {
        fn gnutls_hash_get_len(algorithm: c_int) -> c_uint;
        fn gnutls_hash_fast(
            algorithm: c_int,
            text: *const c_void,
            textlen: usize,
            digest: *mut c_void,
        ) -> c_int;
        fn gnutls_base64_encode2(data: *const GnutlsDatum, result: *mut GnutlsDatum) -> c_int;
        fn gnutls_strerror(error: c_int) -> *const c_char;
        static gnutls_free: unsafe extern "C" fn(ptr: *mut c_void);
    }

    fn gnutls_error(what: &str, ret: c_int) -> ExceptionPtr {
        // SAFETY: gnutls_strerror returns a static nul-terminated string.
        let msg = unsafe { CStr::from_ptr(gnutls_strerror(ret)) }.to_string_lossy();
        ExceptionPtr::new(Exception::new(&format!("{}: {}", what, msg)))
    }

    /// The base64 of the sha1 of `source`.
    pub fn sha1_base64(source: &[u8]) -> Result<Vec<u8>, ExceptionPtr> {
        let mut hash = [0u8; SHA1_SIZE];
        // SAFETY: a pure query of the digest size.
        engine_assert!(SHA1_SIZE == unsafe { gnutls_hash_get_len(GNUTLS_DIG_SHA1) } as usize);
        // SAFETY: `source` and `hash` are valid for their lengths.
        let ret = unsafe {
            gnutls_hash_fast(
                GNUTLS_DIG_SHA1,
                source.as_ptr().cast(),
                source.len(),
                hash.as_mut_ptr().cast(),
            )
        };
        if ret != GNUTLS_E_SUCCESS {
            return Err(gnutls_error("gnutls_hash_fast", ret));
        }
        encode_base64(&hash)
    }

    /// The base64 of `source`.
    pub fn encode_base64(source: &[u8]) -> Result<Vec<u8>, ExceptionPtr> {
        let src_data =
            GnutlsDatum { data: source.as_ptr() as *mut u8, size: source.len() as c_uint };
        let mut encoded_data = GnutlsDatum { data: std::ptr::null_mut(), size: 0 };
        // SAFETY: gnutls only reads the source and allocates the result.
        let ret = unsafe { gnutls_base64_encode2(&src_data, &mut encoded_data) };
        if ret != GNUTLS_E_SUCCESS {
            return Err(gnutls_error("gnutls_base64_encode2", ret));
        }
        // base64_encoded.data is "unsigned char *"
        // SAFETY: gnutls returned `size` bytes at `data`, which we free once copied.
        let out =
            unsafe { std::slice::from_raw_parts(encoded_data.data, encoded_data.size as usize) }
                .to_vec();
        // SAFETY: the buffer was allocated by gnutls and is freed once.
        unsafe { gnutls_free(encoded_data.data.cast()) };
        Ok(out)
    }

    /// The logger of the websocket code.
    pub static WEBSOCKET_LOGGER: LazyLock<std::pin::Pin<Box<Logger>>> =
        LazyLock::new(|| Logger::new("websocket").expect("logger registered once"));
}
