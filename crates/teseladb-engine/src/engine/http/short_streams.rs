//! Deprecated aliases of the whole-stream readers, kept under `http::short_streams::httpd`.
//!
//! The functions forward to [`util`](crate::engine::util::short_streams::util), where the
//! readers now live; new code calls those directly.

/// The deprecated forwarding functions.
pub mod httpd {
    use crate::engine::core::future::Future;
    use crate::engine::core::iostream::InputStream;
    use crate::engine::core::sstring::Sstring;
    use crate::engine::core::temporary_buffer::TemporaryBuffer;
    use crate::engine::util::short_streams::util;

    /// Returns all bytes from the stream until eof, accessible in chunks
    #[deprecated(
        note = "read_entire_stream was moved to teseladb::engine::util::short_streams::util; use it from there instead"
    )]
    ///
    /// # Safety
    /// As [`util::read_entire_stream`]: `inp` outlives the returned future and nothing else uses it.
    #[inline]
    pub unsafe fn read_entire_stream(
        inp: &mut InputStream<u8>,
    ) -> Future<Vec<TemporaryBuffer<u8>>> {
        // SAFETY: forwarded from the caller.
        unsafe { util::read_entire_stream(inp) }
    }

    /// Returns all bytes from the stream until eof as a single buffer, use only on short streams
    #[deprecated(
        note = "read_entire_stream_contiguous was moved to teseladb::engine::util::short_streams::util; use it from there instead"
    )]
    ///
    /// # Safety
    /// As [`util::read_entire_stream_contiguous`]: `inp` outlives the returned future and nothing else uses it.
    #[inline]
    pub unsafe fn read_entire_stream_contiguous(inp: &mut InputStream<u8>) -> Future<Sstring> {
        // SAFETY: forwarded from the caller.
        unsafe { util::read_entire_stream_contiguous(inp) }
    }

    /// Ignores all bytes until eof
    #[deprecated(
        note = "skip_entire_stream was moved to teseladb::engine::util::short_streams::util; use it from there instead"
    )]
    ///
    /// # Safety
    /// As [`util::skip_entire_stream`]: `inp` outlives the returned future and nothing else uses it.
    #[inline]
    pub unsafe fn skip_entire_stream(inp: &mut InputStream<u8>) -> Future<()> {
        // SAFETY: forwarded from the caller.
        unsafe { util::skip_entire_stream(inp) }
    }
}
