//! The hand-off point between the I/O queues and the reactor backend.
//!
//! I/O queues decide when a request may be dispatched and push it, with its completion, into
//! the shard's [`IoSink`]. The reactor backend later drains the sink and submits the requests
//! to the kernel; a request the backend cannot take yet (for example when its submission ring
//! is full) stays at the front for the next poll. Submission into the sink is implemented by
//! the I/O queue module.

use crate::engine::core::chunked_fifo::ChunkedFifo;
use crate::engine::core::internal::io_request::IoRequest;
use crate::engine::core::reactor::IoCompletion;

/// A dispatched request waiting for the backend, with the completion to notify.
pub struct PendingIoRequest {
    /// The request.
    pub(crate) req: IoRequest,
    /// The completion notified when the kernel finishes the request.
    pub(crate) _completion: *mut IoCompletion,
}

impl PendingIoRequest {
    /// Pairs `req` with its completion `desc`.
    #[inline]
    pub fn new(req: IoRequest, desc: *mut IoCompletion) -> Self {
        PendingIoRequest { req, _completion: desc }
    }
}

/// The requests dispatched by the I/O queues, waiting for the backend.
pub struct IoSink {
    /// Pending requests, oldest first.
    pub(crate) _pending_io: ChunkedFifo<PendingIoRequest>,
}

impl IoSink {
    /// An empty sink.
    pub fn new() -> Self {
        IoSink { _pending_io: ChunkedFifo::new() }
    }

    /// Hands pending requests to `consume`, oldest first, until it returns `false` (the
    /// request was not consumed and draining should stop). Consumed requests are removed;
    /// returns how many.
    #[inline]
    pub fn drain<Fn: FnMut(&IoRequest, *mut IoCompletion) -> bool>(
        &mut self,
        mut consume: Fn,
    ) -> usize {
        let mut drained = 0usize;
        for req in self._pending_io.iter() {
            if !consume(&req.req, req._completion) {
                break;
            }
            drained += 1;
        }
        self._pending_io.pop_front_n(drained);
        drained
    }
}

impl Default for IoSink {
    fn default() -> Self {
        Self::new()
    }
}
