//! Late buffer allocation for reads.
//!
//! Read paths take a [`BufferAllocator`] instead of a buffer so that the allocation strategy
//! (where to allocate from, and what sizes) is decoupled from the point where the allocation
//! happens, which can then be as late as possible (for example only once data is ready).

use crate::engine::core::temporary_buffer::TemporaryBuffer;

/// Allocates buffers for reads on demand.
pub trait BufferAllocator {
    /// Returns a fresh buffer to read into; its size is chosen by the allocator.
    fn allocate_buffer(&mut self) -> TemporaryBuffer<u8>;
}
