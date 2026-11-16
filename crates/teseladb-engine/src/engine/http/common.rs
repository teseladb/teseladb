//! Shared HTTP building blocks.
//!
//! - [`http::internal`]: output streams that frame a body on top of a connection's output
//!   stream, with chunked transfer coding or with a fixed content length.
//! - [`httpd`]: request parameters ([`httpd::Parameters`]) and the HTTP method enumeration
//!   ([`httpd::OperationType`]) with its string conversions.
//! - [`support`]: small helpers shared by the HTTP, WebSocket and RPC modules.
//!
//! The streams run on the shard that owns the connection and hold raw pointers to the
//! underlying stream; their constructors are `unsafe` and state how long it must live.

/// Body framing output streams.
pub mod http {
    /// Output streams for chunked and content-length bodies.
    pub mod internal {
        use crate::engine::core::future::{
            ExceptionPtr, Future, make_exception_future, make_ready_future,
        };
        use crate::engine::core::iostream::{
            DataSink, DataSinkImpl, OutputStream, OutputStreamOptions,
        };
        use crate::engine::core::temporary_buffer::TemporaryBuffer;
        use crate::engine::net::packet::Packet;

        const DEFAULT_BODY_SINK_BUFFER_SIZE: usize = 32000;

        // Data sinks below are running "on top" of socket output stream and provide
        // reliable and handy way of generating request bodies according to selected
        // encoding type and content-length.
        //
        // Respectively, both .close() methods should not close the underlying stream,
        // because the socket in question may continue being in use for keep-alive
        // connections, and closing it would just break the keep-alive-ness

        struct HttpChunkedDataSinkImpl {
            _out: *mut OutputStream<u8>,
        }

        impl HttpChunkedDataSinkImpl {
            fn write_size(&mut self, s: usize) -> Future<()> {
                let req = format!("{:x}\r\n", s);
                // SAFETY: the contract of make_http_chunked_output_stream.
                unsafe { &mut *self._out }.write_str(&req)
            }
        }

        impl DataSinkImpl for HttpChunkedDataSinkImpl {
            fn put(&mut self, data: Packet) -> Future<()> {
                crate::engine::core::iostream::fallback_put(self, data)
            }

            fn put_buffer(&mut self, buf: TemporaryBuffer<u8>) -> Future<()> {
                if buf.size() == 0 {
                    // size 0 buffer should be ignored, some server
                    // may consider it an end of message
                    return make_ready_future(());
                }
                let size = buf.size();
                let out = self._out;
                self.write_size(size)
                    // SAFETY: as in write_size().
                    .then(move |()| unsafe { &mut *out }.write(buf.as_slice()))
                    // SAFETY: as in write_size().
                    .then(move |()| unsafe { &mut *out }.write(b"\r\n"))
            }

            fn close(&mut self) -> Future<()> {
                make_ready_future(())
            }
        }

        /// Wraps `out` in a stream that writes the http chunked transfer encoding.
        ///
        /// # Safety
        /// `out` stays alive and in place until the returned stream and every future of it
        /// are gone, and is not used otherwise while one of its writes is pending (the
        /// sink keeps a raw pointer to it).
        pub unsafe fn make_http_chunked_output_stream(
            out: &mut OutputStream<u8>,
        ) -> OutputStream<u8> {
            let opts = OutputStreamOptions { trim_to_size: true, ..Default::default() };
            OutputStream::with_options(
                DataSink::new(Box::new(HttpChunkedDataSinkImpl { _out: out })),
                DEFAULT_BODY_SINK_BUFFER_SIZE,
                opts,
            )
        }

        struct HttpContentLengthDataSinkImpl {
            _out: *mut OutputStream<u8>,
            _limit: usize,
            _bytes_written: *mut usize,
        }

        impl DataSinkImpl for HttpContentLengthDataSinkImpl {
            fn put(&mut self, data: Packet) -> Future<()> {
                crate::engine::core::iostream::fallback_put(self, data)
            }

            fn put_buffer(&mut self, buf: TemporaryBuffer<u8>) -> Future<()> {
                // SAFETY: the contract of make_http_content_length_output_stream.
                let written = unsafe { *self._bytes_written };
                if buf.size() == 0 || written == self._limit {
                    return make_ready_future(());
                }
                let size = buf.size();
                if written + size > self._limit {
                    return make_exception_future(ExceptionPtr::new(
                        crate::engine::core::future::RuntimeError(format!(
                            "body conent length overflow: want {} limit {}",
                            written + buf.size(),
                            self._limit
                        )),
                    ));
                }
                let bw = self._bytes_written;
                // SAFETY: as above, and the underlying stream outlives this one.
                unsafe { &mut *self._out }.write(buf.as_slice()).then(move |()| {
                    // SAFETY: as above.
                    unsafe { *bw += size };
                    drop(buf);
                })
            }

            fn close(&mut self) -> Future<()> {
                make_ready_future(())
            }
        }

        /// Wraps `out` in a stream that writes at most `total_len` bytes.
        ///
        /// Once the stream is closed, `bytes_written` holds the number of bytes actually
        /// written.
        ///
        /// # Safety
        /// As [`make_http_chunked_output_stream`] for `out`; `bytes_written` points to a
        /// counter that stays alive until the returned stream and its futures are gone, and
        /// that is only read, not written, by others meanwhile (the sink updates it through
        /// the pointer).
        pub unsafe fn make_http_content_length_output_stream(
            out: &mut OutputStream<u8>,
            total_len: usize,
            bytes_written: *mut usize,
        ) -> OutputStream<u8> {
            // at the very beginning, 0 bytes were written
            // SAFETY: the caller's contract.
            unsafe { *bytes_written = 0 };
            let opts = OutputStreamOptions { trim_to_size: true, ..Default::default() };
            OutputStream::with_options(
                DataSink::new(Box::new(HttpContentLengthDataSinkImpl {
                    _out: out,
                    _limit: total_len,
                    _bytes_written: bytes_written,
                })),
                DEFAULT_BODY_SINK_BUFFER_SIZE,
                opts,
            )
        }
    } // internal namespace
} // http namespace

/// Request parameters and HTTP methods.
pub mod httpd {
    use std::collections::HashMap;

    use crate::engine::core::sstring::Sstring;
    use crate::engine::http::url::internal::path_decode;

    /// The path parameters of a request.
    #[derive(Default, Clone, Debug)]
    pub struct Parameters {
        // Note: the path matcher adds parameters with the '/' prefix into the params map (eg. "/param1"), and some getters
        // remove this '/' to return just the path parameter value
        params: HashMap<Sstring, Sstring>,
    }

    impl Parameters {
        /// The raw value (with the '/' prefix) of `key`, if present.
        pub fn path(&self, key: &Sstring) -> Option<&Sstring> {
            self.params.get(key)
        }

        /// The value of `key` without the '/' prefix.
        #[deprecated(note = "Use request::get_path_param() instead.")]
        pub fn index(&self, key: &Sstring) -> Option<Sstring> {
            // The stored values carry the leading '/', so `substr(1)` does not throw.
            self.params.get(key).and_then(|v| v.substr(1, usize::MAX).ok())
        }

        /// Same as [`Parameters::path`].
        pub fn at(&self, key: &Sstring) -> Option<&Sstring> {
            self.path(key)
        }

        /// The decoded value of `key`, or "" if missing or malformed.
        pub fn get_decoded_param(&self, key: &Sstring) -> Sstring {
            let res = match self.params.get(key) {
                None => return Sstring::new(),
                Some(r) => r,
            };
            let raw_path_param = &res.as_bytes()[1.min(res.size())..];
            let mut decoded_path_param = Sstring::new();
            let ok = path_decode(raw_path_param, &mut decoded_path_param);
            if !ok {
                return Sstring::new();
            }
            decoded_path_param
        }

        /// Whether `key` is present.
        pub fn exists(&self, key: &Sstring) -> bool {
            self.params.contains_key(key)
        }

        /// Sets `key` to `value`.
        pub fn set(&mut self, key: &Sstring, value: &Sstring) {
            self.params.insert(key.clone(), value.clone());
        }

        /// Removes all parameters.
        pub fn clear(&mut self) {
            self.params.clear();
        }
    }

    /// The http methods.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    #[repr(u8)]
    pub enum OperationType {
        /// GET
        Get,
        /// POST
        Post,
        /// PUT
        Put,
        /// DELETE
        Delete,
        /// HEAD
        Head,
        /// OPTIONS
        Options,
        /// TRACE
        Trace,
        /// CONNECT
        Connect,
        /// PATCH
        Patch,
    }

    /// The number of [`OperationType`] values.
    pub const NUM_OPERATION: usize = 9;

    /// Returns the operation type of the HTTP method name `type_`, such as `"GET"` or
    /// `"POST"`; an unknown name gives [`OperationType::Get`].
    pub fn str2type(type_: &str) -> OperationType {
        match type_ {
            "DELETE" => OperationType::Delete,
            "POST" => OperationType::Post,
            "PUT" => OperationType::Put,
            "HEAD" => OperationType::Head,
            "OPTIONS" => OperationType::Options,
            "TRACE" => OperationType::Trace,
            "CONNECT" => OperationType::Connect,
            "PATCH" => OperationType::Patch,
            _ => OperationType::Get,
        }
    }

    /// Returns the HTTP method name of the operation type `type_`, such as `"GET"` or
    /// `"POST"`.
    pub fn type2str(type_: OperationType) -> &'static str {
        match type_ {
            OperationType::Delete => "DELETE",
            OperationType::Post => "POST",
            OperationType::Put => "PUT",
            OperationType::Head => "HEAD",
            OperationType::Options => "OPTIONS",
            OperationType::Trace => "TRACE",
            OperationType::Connect => "CONNECT",
            OperationType::Patch => "PATCH",
            OperationType::Get => "GET",
        }
    }
}

/// Helpers shared by the HTTP, WebSocket and RPC modules: an owner whose raw pointer stays
/// valid when the owner moves, and integer formatting into an [`Sstring`](crate::engine::core::sstring::Sstring).
pub mod support {
    use std::ptr::NonNull;

    use crate::engine::core::sstring::Sstring;

    /// A heap object owned through its raw pointer, for an object whose address is shared
    /// with continuations. A `Box` asserts unique access whenever it moves,
    /// which would invalidate the raw pointers taken from it; this owner never does, and
    /// frees the object when dropped.
    pub struct RawBox<T>(NonNull<T>);

    impl<T> RawBox<T> {
        /// Takes over `b`.
        #[inline]
        pub fn new(b: Box<T>) -> Self {
            // SAFETY: Box::into_raw never returns null.
            RawBox(unsafe { NonNull::new_unchecked(Box::into_raw(b)) })
        }

        /// The object's address, valid until this owner is dropped.
        #[inline]
        pub fn as_ptr(&self) -> *mut T {
            self.0.as_ptr()
        }
    }

    impl<T> Drop for RawBox<T> {
        fn drop(&mut self) {
            // SAFETY: the pointer came from Box::into_raw in new() and is freed only here.
            unsafe { drop(Box::from_raw(self.0.as_ptr())) }
        }
    }

    /// The decimal digits of a `u64` at most.
    const MAX_DECIMAL_DIGITS: usize = 20;

    /// The decimal form of an unsigned integer: the digits are written on the stack and
    /// copied once into the string, which keeps them inline when they fit.
    #[inline]
    pub(crate) fn u64_to_sstring(mut n: u64) -> Sstring {
        let mut buf = [0u8; MAX_DECIMAL_DIGITS];
        let mut pos = MAX_DECIMAL_DIGITS;
        loop {
            pos -= 1;
            buf[pos] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        Sstring::from_bytes(&buf[pos..])
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn integers_as_to_sstring() {
            assert_eq!(u64_to_sstring(0).as_bytes(), b"0");
            assert_eq!(u64_to_sstring(1234).as_bytes(), b"1234");
            assert_eq!(u64_to_sstring(u64::MAX).as_bytes(), b"18446744073709551615");
        }
    }
}
