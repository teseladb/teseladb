//! Data sources that read a request or response body from a connection.
//!
//! [`internal::ContentLengthSourceImpl`] yields at most a fixed number of bytes of the
//! underlying stream (bodies with `Content-Length`); [`internal::ChunkedSourceImpl`]
//! decodes chunked transfer coding, saving chunk extensions and trailer fields into maps
//! reached through [`internal::ChunkTargets`]. Both wrap the connection's input stream
//! by raw pointer on the shard that owns the connection, so their constructors are
//! `unsafe`; bytes left over after the body stay in the stream for the next message.

/// The body data sources.
pub mod internal {
    use std::cell::Cell;
    use std::collections::HashMap;

    use crate::engine::core::future::{
        ExceptionPtr, Future, make_exception_future, make_ready_future,
    };
    use crate::engine::core::iostream::{
        ConsumptionResult, DataSourceImpl, InputStream, continue_consuming, stop_consuming,
    };
    use crate::engine::core::parser_support::UnconsumedRemainder;
    use crate::engine::core::sstring::Sstring;
    use crate::engine::core::temporary_buffer::TemporaryBuffer;
    use crate::engine::http::chunk_parsers::{HttpChunkSizeAndExtParser, HttpChunkTrailerParser};
    use crate::engine::http::exception::{bad_chunk_exception, bad_request_exception};

    /// The longest chunk size, in hexadecimal digits, that fits a 64-bit size.
    const MAX_CHUNK_SIZE_DIGITS: usize = 16;

    /// An input stream wrapper that allows to read only "length" bytes from it, used to
    /// handle requests with large bodies.
    pub struct ContentLengthSourceImpl {
        _inp: *mut InputStream<u8>,
        _remaining_bytes: usize,
        _consumed_bytes: *mut usize,
    }

    impl ContentLengthSourceImpl {
        /// Reads at most `length` bytes of `inp`, counting them into `consumed_bytes` when
        /// it is not null.
        ///
        /// # Safety
        /// `inp` and `consumed_bytes` (when not null) stay valid and in place for the whole
        /// life of the source, and nothing else uses them while one of its reads is pending
        /// (the source keeps both as raw pointers).
        #[inline]
        pub unsafe fn with_counter(
            inp: *mut InputStream<u8>,
            length: usize,
            consumed_bytes: *mut usize,
        ) -> Self {
            ContentLengthSourceImpl {
                _inp: inp,
                _remaining_bytes: length,
                _consumed_bytes: consumed_bytes,
            }
        }

        /// Reads at most `length` bytes of `inp`.
        ///
        /// # Safety
        /// As [`ContentLengthSourceImpl::with_counter`].
        #[inline]
        pub unsafe fn new(inp: *mut InputStream<u8>, length: usize) -> Self {
            // SAFETY: the caller's contract.
            unsafe { Self::with_counter(inp, length, std::ptr::null_mut()) }
        }
    }

    impl DataSourceImpl for ContentLengthSourceImpl {
        fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
            if self._remaining_bytes == 0 {
                return make_ready_future(TemporaryBuffer::empty_buffer());
            }
            let this: *mut ContentLengthSourceImpl = self;
            // SAFETY: the stream outlives the source (the constructor's contract).
            unsafe { &mut *self._inp }.read_up_to(self._remaining_bytes).then(
                move |tmp_buf: TemporaryBuffer<u8>| {
                    // SAFETY: the source lives boxed in its DataSource, which does not move it, and
                    // the stream does not drop it while a read is pending.
                    let s = unsafe { &mut *this };
                    s._remaining_bytes -= tmp_buf.size();
                    if !s._consumed_bytes.is_null() {
                        // SAFETY: the counter outlives the source (the constructor's contract).
                        unsafe { *s._consumed_bytes += tmp_buf.size() };
                    }
                    make_ready_future(tmp_buf)
                },
            )
        }

        fn skip(&mut self, n: u64) -> Future<TemporaryBuffer<u8>> {
            let skip_bytes = n.min(self._remaining_bytes as u64);
            self._remaining_bytes -= skip_bytes as usize;
            if !self._consumed_bytes.is_null() {
                // SAFETY: the counter outlives the source.
                unsafe { *self._consumed_bytes += skip_bytes as usize };
            }
            // SAFETY: the stream outlives the source.
            unsafe { &mut *self._inp }
                .skip(skip_bytes)
                .then(|()| make_ready_future(TemporaryBuffer::<u8>::empty_buffer()))
        }

        fn close(&mut self) -> Future<()> {
            make_ready_future(())
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u8)]
    enum ParsingState {
        SizeAndExt,
        Body,
        TrailerPart,
    }

    type ConsumptionResultType = ConsumptionResult<u8>;

    /// Where a chunked source saves the chunk extensions and the trailing headers, held by
    /// the caller of the source so that it can detach them (null: discarded) once the maps
    /// go away. The server detaches them before its last read of a request's content, when
    /// the handler may have dropped the request already.
    pub struct ChunkTargets {
        chunk_extensions: Cell<*mut HashMap<Sstring, Sstring>>,
        trailing_headers: Cell<*mut HashMap<Sstring, Sstring>>,
    }

    impl Default for ChunkTargets {
        fn default() -> Self {
            Self::new()
        }
    }

    impl ChunkTargets {
        /// Detached targets.
        #[inline]
        pub const fn new() -> Self {
            ChunkTargets {
                chunk_extensions: Cell::new(std::ptr::null_mut()),
                trailing_headers: Cell::new(std::ptr::null_mut()),
            }
        }

        /// Saves into the given maps from now on.
        ///
        /// # Safety
        /// The maps stay valid and in place, and nothing else uses them while a read of the
        /// source is pending, until [`ChunkTargets::detach`] or the end of the source.
        #[inline]
        pub unsafe fn set(
            &self,
            chunk_extensions: *mut HashMap<Sstring, Sstring>,
            trailing_headers: *mut HashMap<Sstring, Sstring>,
        ) {
            self.chunk_extensions.set(chunk_extensions);
            self.trailing_headers.set(trailing_headers);
        }

        /// Discards what is parsed from now on.
        #[inline]
        pub fn detach(&self) {
            self.chunk_extensions.set(std::ptr::null_mut());
            self.trailing_headers.set(std::ptr::null_mut());
        }
    }

    struct ChunkParser {
        _size_and_ext_parser: HttpChunkSizeAndExtParser,
        _trailer_parser: HttpChunkTrailerParser,

        _buf: TemporaryBuffer<u8>,
        _current_chunk_bytes_read: usize,
        _current_chunk_length: usize,
        _ps: ParsingState,
        _end_of_request: bool,
        // references to fields in the request structure
        _targets: *const ChunkTargets,
    }

    impl ChunkParser {
        fn new(targets: *const ChunkTargets) -> Self {
            let mut p = ChunkParser {
                _size_and_ext_parser: HttpChunkSizeAndExtParser::default(),
                _trailer_parser: HttpChunkTrailerParser::default(),
                _buf: TemporaryBuffer::empty_buffer(),
                _current_chunk_bytes_read: 0,
                _current_chunk_length: 0,
                _ps: ParsingState::SizeAndExt,
                _end_of_request: false,
                _targets: targets,
            };
            p._size_and_ext_parser.init();
            p
        }

        #[inline]
        fn buf(&mut self) -> TemporaryBuffer<u8> {
            self._current_chunk_bytes_read += self._buf.size();
            std::mem::take(&mut self._buf)
        }

        /// The size_and_ext part, once the parser has a result.
        fn on_size_and_ext(&mut self, res: UnconsumedRemainder) -> Future<ConsumptionResultType> {
            let Some(res) = res else {
                return make_ready_future(continue_consuming());
            };
            if self._size_and_ext_parser.failed() {
                return make_exception_future(ExceptionPtr::new(bad_request_exception(
                    "Can't parse chunk size and extensions",
                )));
            }
            // save extensions
            let parsed_extensions = self._size_and_ext_parser.get_parsed_extensions();
            if let Some(chunk_extensions) =
                // SAFETY: the targets outlive the source (its constructor's contract), and a set map
                // is valid and unused elsewhere during the read (the contract of ChunkTargets::set).
                unsafe { (*self._targets).chunk_extensions.get().as_mut() }
            {
                Self::merge_extensions(chunk_extensions, parsed_extensions);
            }

            // save size
            let size_string = self._size_and_ext_parser.get_size();
            if size_string.size() > MAX_CHUNK_SIZE_DIGITS {
                return make_exception_future(ExceptionPtr::new(bad_chunk_exception(
                    "Chunk length too big",
                )));
            }
            self._current_chunk_bytes_read = 0;
            // The hexadecimal value of the digits the parser accepted: at most 16 digits,
            // saturated at `i64::MAX`.
            let length = size_string.as_bytes().iter().fold(0u64, |n, &d| {
                let v = match d {
                    b'0'..=b'9' => d - b'0',
                    b'a'..=b'f' => d - b'a' + 10,
                    _ => d - b'A' + 10,
                };
                (n << 4) | u64::from(v)
            });
            self._current_chunk_length = length.min(i64::MAX as u64) as usize;

            if self._current_chunk_length == 0 {
                self._ps = ParsingState::TrailerPart;
                self._trailer_parser.init();
            } else {
                self._ps = ParsingState::Body;
            }
            if res.empty() {
                return make_ready_future(continue_consuming());
            }
            self.call(res)
        }

        /// `_chunk_extensions.merge(parsed_extensions)`, then the values of the keys already
        /// present appended after a comma.
        fn merge_extensions(
            chunk_extensions: &mut HashMap<Sstring, Sstring>,
            parsed_extensions: HashMap<Sstring, Sstring>,
        ) {
            for (key, val) in parsed_extensions {
                match chunk_extensions.get_mut(&key) {
                    Some(v) => {
                        v.append_bytes(b",");
                        v.append_bytes(val.as_bytes());
                    }
                    None => {
                        chunk_extensions.insert(key, val);
                    }
                }
            }
        }

        /// The trailer part, once the parser has a result.
        fn on_trailer(&mut self, res: UnconsumedRemainder) -> Future<ConsumptionResultType> {
            let Some(res) = res else {
                return make_ready_future(continue_consuming());
            };
            if self._trailer_parser.failed() {
                return make_exception_future(ExceptionPtr::new(bad_request_exception(
                    "Can't parse chunked request trailer",
                )));
            }
            // save trailing headers
            if let Some(trailing_headers) =
                // SAFETY: as for the chunk extensions in on_size_and_ext().
                unsafe { (*self._targets).trailing_headers.get().as_mut() }
            {
                *trailing_headers = self._trailer_parser.get_parsed_headers();
            }
            self._end_of_request = true;
            make_ready_future(stop_consuming(res))
        }

        fn call(&mut self, mut data: TemporaryBuffer<u8>) -> Future<ConsumptionResultType> {
            if self._buf.size() != 0 || self._end_of_request || data.empty() {
                // return if we have already read some content (_buf.size()), we have already reached the end of the chunked request (_end_of_request),
                // or the underlying stream reached eof (data.empty())
                return make_ready_future(stop_consuming(data));
            }
            let this: *mut ChunkParser = self;
            match self._ps {
                // "data" buffer is non-empty
                ParsingState::SizeAndExt => {
                    self._size_and_ext_parser.call(data).then(move |res: UnconsumedRemainder| {
                        // SAFETY: the source owns the parser and outlives the consumption.
                        unsafe { (*this).on_size_and_ext(res) }
                    })
                }
                ParsingState::Body => {
                    // read the new data into _buf
                    if self._current_chunk_bytes_read < self._current_chunk_length {
                        let to_read = (self._current_chunk_length - self._current_chunk_bytes_read)
                            .min(data.size());
                        if self._buf.empty() {
                            self._buf = data.share_range(0, to_read);
                        }
                        data.trim_front(to_read);
                        return make_ready_future(stop_consuming(data));
                    }

                    // chunk body is finished, we haven't entered the previous if, so "data" is still non-empty
                    if self._current_chunk_bytes_read == self._current_chunk_length {
                        // we haven't read \r yet
                        // SAFETY: data is not empty.
                        if unsafe { *data.get() } != b'\r' {
                            return make_exception_future(ExceptionPtr::new(bad_chunk_exception(
                                "The actual chunk length exceeds the specified length",
                            )));
                        }
                        self._current_chunk_bytes_read += 1;
                        data.trim_front(1);
                        if data.empty() {
                            return make_ready_future(continue_consuming());
                        }
                    }
                    if self._current_chunk_bytes_read == self._current_chunk_length + 1 {
                        // we haven't read \n but have \r
                        // SAFETY: data is not empty.
                        if unsafe { *data.get() } != b'\n' {
                            return make_exception_future(ExceptionPtr::new(bad_chunk_exception(
                                "The actual chunk length exceeds the specified length",
                            )));
                        }
                        self._ps = ParsingState::SizeAndExt;
                        self._size_and_ext_parser.init();
                        data.trim_front(1);
                        if data.empty() {
                            return make_ready_future(continue_consuming());
                        }
                    }
                    self.call(data)
                }
                ParsingState::TrailerPart => {
                    self._trailer_parser.call(data).then(move |res: UnconsumedRemainder| {
                        // SAFETY: as above.
                        unsafe { (*this).on_trailer(res) }
                    })
                }
            }
        }
    }

    /// An input stream wrapper that decodes a request body with "chunked" encoding.
    pub struct ChunkedSourceImpl {
        _inp: *mut InputStream<u8>,
        _chunk: ChunkParser,
    }

    impl ChunkedSourceImpl {
        /// Decodes the chunked body read from `inp`, saving the chunk extensions and the
        /// trailing headers into the maps `targets` points at.
        ///
        /// # Safety
        /// `inp` and `targets` stay valid and in place for the whole life of the source, and
        /// nothing else uses the stream while one of its reads is pending (the source
        /// keeps both as raw pointers).
        pub unsafe fn new(inp: *mut InputStream<u8>, targets: *const ChunkTargets) -> Self {
            ChunkedSourceImpl { _inp: inp, _chunk: ChunkParser::new(targets) }
        }
    }

    impl DataSourceImpl for ChunkedSourceImpl {
        fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
            let chunk: *mut ChunkParser = &mut self._chunk;
            // SAFETY: the stream outlives the source (the constructor's contract); the source
            // lives boxed in its DataSource, which neither moves nor drops it while the read
            // is pending.
            unsafe { &mut *self._inp }
                .consume(move |data: TemporaryBuffer<u8>| unsafe { (*chunk).call(data) })
                .then(move |()| make_ready_future(unsafe { (*chunk).buf() }))
        }

        fn close(&mut self) -> Future<()> {
            make_ready_future(())
        }
    }
}
