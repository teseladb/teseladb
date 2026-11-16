//! Reading or discarding a whole input stream.
//!
//! Helpers for short streams (HTTP bodies, small files): [`util::read_entire_stream`]
//! collects every chunk until end of stream, [`util::read_entire_stream_contiguous`]
//! joins them into one string, and [`util::skip_entire_stream`] drains the stream. They
//! buffer everything in memory, so they are only for streams known to be small.

/// The stream helpers.
pub mod util {
    use crate::engine::core::do_with::do_with;
    use crate::engine::core::future::{ExceptionPtr, Future, make_ready_future};
    use crate::engine::core::iostream::{
        ConsumptionResult, InputStream, continue_consuming, stop_consuming,
    };
    use crate::engine::core::sstring::{InitializedLater, Sstring};
    use crate::engine::core::temporary_buffer::TemporaryBuffer;

    type TmpBuf = TemporaryBuffer<u8>;
    type ConsumptionResultType = ConsumptionResult<u8>;

    /// Reads all bytes from `inp` until end of stream and returns them as the list of
    /// chunks the stream produced.
    ///
    /// Use only on short streams, to avoid running out of memory.
    ///
    /// # Errors
    ///
    /// The future fails with the stream's read error.
    ///
    /// # Safety
    /// `inp` stays alive and in place, and is used by nothing else, until the returned
    /// future resolves (the future keeps a raw pointer to it).
    pub unsafe fn read_entire_stream(inp: &mut InputStream<u8>) -> Future<Vec<TmpBuf>> {
        let inp: *mut InputStream<u8> = inp;
        do_with(Vec::<TmpBuf>::new(), move |bufs: &mut Vec<TmpBuf>| {
            let bufs: *mut Vec<TmpBuf> = bufs;
            // SAFETY: the contract of this function keeps `inp` alive and unused elsewhere
            // until the future resolves; do_with keeps `bufs` alive likewise.
            unsafe {
                (*inp)
                    .consume(move |buf: TmpBuf| {
                        if buf.empty() {
                            return make_ready_future::<ConsumptionResultType>(stop_consuming(buf));
                        }
                        (*bufs).push(buf);
                        make_ready_future::<ConsumptionResultType>(continue_consuming())
                    })
                    .then(move |()| -> Result<Vec<TmpBuf>, ExceptionPtr> {
                        Ok(std::mem::take(&mut *bufs))
                    })
            }
        })
    }

    /// Reads all bytes from `inp` until end of stream and returns them as one string.
    ///
    /// Use only on short streams, to avoid running out of memory. Allocates once for the
    /// total size and copies each chunk into it.
    ///
    /// # Errors
    ///
    /// The future fails with the stream's read error or an allocation failure.
    ///
    /// # Safety
    /// `inp` stays alive and in place, and is used by nothing else, until the returned
    /// future resolves (the future keeps a raw pointer to it).
    pub unsafe fn read_entire_stream_contiguous(inp: &mut InputStream<u8>) -> Future<Sstring> {
        // SAFETY: the caller upholds the same contract as `read_entire_stream`'s.
        unsafe { read_entire_stream(inp) }.then(
            |bufs: Vec<TmpBuf>| -> Result<Sstring, ExceptionPtr> {
                let mut total_size: usize = 0;
                for buf in &bufs {
                    total_size += buf.size();
                }
                let mut ret = Sstring::initialized_later(InitializedLater, total_size)?;
                let mut pos: usize = 0;
                let dst = ret.data();
                for buf in &bufs {
                    // SAFETY: `ret` has total_size bytes and the buffers add up to total_size.
                    unsafe {
                        std::ptr::copy_nonoverlapping(buf.get(), dst.add(pos), buf.size());
                    }
                    pos += buf.size();
                }
                Ok(ret)
            },
        )
    }

    /// Reads and discards all bytes of `inp` until end of stream.
    ///
    /// # Errors
    ///
    /// The future fails with the stream's read error.
    ///
    /// # Safety
    /// `inp` stays alive and in place, and is used by nothing else, until the returned
    /// future resolves.
    pub unsafe fn skip_entire_stream(inp: &mut InputStream<u8>) -> Future<()> {
        inp.consume(|tmp: TmpBuf| {
            if tmp.empty() {
                make_ready_future::<ConsumptionResultType>(stop_consuming(TmpBuf::empty_buffer()))
            } else {
                make_ready_future::<ConsumptionResultType>(continue_consuming())
            }
        })
    }
}
