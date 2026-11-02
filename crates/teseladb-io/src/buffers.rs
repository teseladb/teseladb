//! A core's reserve of aligned buffers (012 FR-003, data model §2): `count` buffers of
//! [`BUFFER_BYTES`] aligned to 4 KiB, registered in the core's io_uring when there is one. Only the
//! core's own thread takes and returns them, so the free list needs no lock.

use teseladb_sys::aligned::{ALIGN, AlignedBuf};

/// Bytes of each buffer.
pub const BUFFER_BYTES: usize = 256 << 10;
/// Buffers of a core's reserve.
pub const BUFFERS_PER_CORE: u16 = 32;

/// Which buffers of a reserve are free. The buffers themselves live where the kernel can reach
/// them (in the ring that registered them, see [`BufferPool::allocate`]).
#[derive(Debug)]
pub struct BufferPool {
    free: Vec<u16>,
    taken: Vec<bool>,
}

impl BufferPool {
    /// A reserve of `count` buffers, all free.
    pub fn new(count: u16) -> BufferPool {
        BufferPool { free: (0..count).rev().collect(), taken: vec![false; usize::from(count)] }
    }

    /// The memory of `count` buffers, to register.
    pub fn allocate(count: u16) -> Vec<AlignedBuf> {
        (0..count).map(|_| AlignedBuf::new(BUFFER_BYTES)).collect()
    }

    /// Takes a free buffer, the lowest one; `None` when all are in use.
    pub fn take(&mut self) -> Option<u16> {
        let i = self.free.pop()?;
        self.taken[usize::from(i)] = true;
        Some(i)
    }

    /// Returns buffer `i`. `false` if it was not taken (nothing changes).
    pub fn give(&mut self, i: u16) -> bool {
        match self.taken.get_mut(usize::from(i)) {
            Some(t) if *t => {
                *t = false;
                self.free.push(i);
                true
            }
            _ => false,
        }
    }

    /// Free buffers.
    pub fn available(&self) -> usize {
        self.free.len()
    }

    /// Buffers of the reserve.
    pub fn len(&self) -> usize {
        self.taken.len()
    }

    /// Whether the reserve has no buffers.
    pub fn is_empty(&self) -> bool {
        self.taken.is_empty()
    }
}

/// The span of whole 4 KiB blocks that covers `len` bytes at `offset`: its start and length.
pub fn block_span(offset: u64, len: u64) -> (u64, u64) {
    let a = ALIGN as u64;
    let start = offset / a * a;
    let end = (offset + len).next_multiple_of(a);
    (start, (end - start).max(a))
}

/// Whether `len` bytes at `offset` are whole blocks, as `O_DIRECT` needs them.
pub fn is_aligned(offset: u64, len: u64) -> bool {
    let a = ALIGN as u64;
    offset.is_multiple_of(a) && len.is_multiple_of(a) && len > 0
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn buffers_are_taken_and_returned_without_duplicates() {
        let mut p = BufferPool::new(3);
        assert_eq!((p.len(), p.available(), p.is_empty()), (3, 3, false));
        assert_eq!(p.take(), Some(0));
        assert_eq!(p.take(), Some(1));
        assert_eq!(p.take(), Some(2));
        assert_eq!(p.take(), None);
        assert!(p.give(1));
        assert!(!p.give(1), "returned twice");
        assert!(!p.give(9), "not in the reserve");
        assert_eq!(p.available(), 1);
        assert_eq!(p.take(), Some(1));
        assert!(BufferPool::new(0).is_empty());
    }

    #[test]
    fn the_memory_is_aligned_and_of_the_buffer_size() {
        let bufs = BufferPool::allocate(2);
        assert_eq!(bufs.len(), 2);
        for b in &bufs {
            assert_eq!(b.len(), BUFFER_BYTES);
            assert_eq!(b.addr() % ALIGN as u64, 0);
        }
    }

    #[test]
    fn spans_cover_whole_blocks() {
        assert_eq!(block_span(0, 4096), (0, 4096));
        assert_eq!(block_span(1, 1), (0, 4096));
        assert_eq!(block_span(4095, 2), (0, 8192));
        assert_eq!(block_span(8192, 0), (8192, 4096));
        assert!(is_aligned(4096, 8192));
        assert!(!is_aligned(4096, 0));
        assert!(!is_aligned(1, 4096));
        assert!(!is_aligned(0, 100));
    }
}
