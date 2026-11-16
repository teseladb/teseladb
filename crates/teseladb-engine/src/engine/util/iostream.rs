//! Stream helpers.
//!
//! [`util::write_to_stream_and_close`] runs a writer against an [`OutputStream`] and then
//! always flushes and closes the stream, even when the writer fails, so that buffers are
//! released on the owning shard.

use crate::engine::core::coroutine::make_coroutine;
use crate::engine::core::future::{ExceptionPtr, Future};
use crate::engine::core::iostream::OutputStream;

/// Stream utilities.
pub mod util {
    use super::*;

    /// Writes data to `out` with `writer`, then flushes and closes the stream.
    ///
    /// Takes ownership of the stream; it is unusable once the returned future resolves.
    /// The stream is closed even if `writer` or the flush fails.
    ///
    /// # Errors
    ///
    /// Resolves to the error of `close` if closing fails. Otherwise, resolves to the error
    /// of `writer` or of the flush if either failed (reported after the stream is closed).
    pub fn write_to_stream_and_close<W>(out_: OutputStream<u8>, writer: W) -> Future<()>
    where
        W: FnOnce(&mut OutputStream<u8>) -> Future<()> + 'static,
    {
        make_coroutine(async move {
            let mut out = out_;
            let mut ex: Option<ExceptionPtr> = None;
            let r: Result<(), ExceptionPtr> = async {
                writer(&mut out).await?;
                out.flush().await?;
                Ok(())
            }
            .await;
            if let Err(e) = r {
                ex = Some(e);
            }
            out.close().await?;
            if let Some(ex) = ex {
                // Report the writer/flush error now that the stream is closed.
                return Err(ex);
            }
            Ok(())
        })
    }
}
