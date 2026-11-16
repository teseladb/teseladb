//! A data sink that collects packets in a vector.
//!
//! [`VectorDataSink`] implements [`DataSinkImpl`] by pushing every packet it receives
//! onto a caller-owned `Vec<Packet>`; it is mainly useful in tests, to inspect what an
//! output stream wrote. Writes complete immediately.

use crate::engine::core::future::{Future, make_ready_future};
use crate::engine::core::iostream::DataSinkImpl;
use crate::engine::net::packet::Packet;

/// The vector a [`VectorDataSink`] appends to.
pub type VectorType = Vec<Packet>;

/// A sink that appends each packet to a vector it does not own. The vector must outlive
/// the sink.
pub struct VectorDataSink {
    v: *mut VectorType,
}

impl VectorDataSink {
    /// Creates a sink that appends to the vector at `v`.
    ///
    /// # Safety
    /// The vector must outlive the sink, stay at its address, and not be borrowed while
    /// the sink writes to it: the sink keeps only a raw pointer to it.
    #[inline]
    pub unsafe fn new(v: *mut VectorType) -> Self {
        Self { v }
    }
}

impl DataSinkImpl for VectorDataSink {
    fn put(&mut self, p: Packet) -> Future<()> {
        // SAFETY: `new`'s contract keeps the vector alive, in place and unborrowed while
        // the sink exists.
        unsafe { (*self.v).push(p) };
        make_ready_future(())
    }

    fn close(&mut self) -> Future<()> {
        // Closing the local side needs no work for a vector.
        make_ready_future(())
    }
}
