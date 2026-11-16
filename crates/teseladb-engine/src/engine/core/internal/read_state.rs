//! State of a DMA read that may need several kernel reads.
//!
//! A read of an arbitrary byte range is widened to the disk alignment: [`FileReadState`] owns
//! an aligned buffer for the widened range, tracks how much of it has been filled, and trims
//! it at the end so the caller receives exactly the bytes it asked for (dropping the
//! `front` bytes read only for alignment).

use crate::engine::core::future::ExceptionPtr;
use crate::engine::core::internal::io_intent::IntentReference;
use std::pin::Pin;

use crate::engine::core::io_intent::IoIntent;
use crate::engine::core::temporary_buffer::{ByteType, TemporaryBuffer};

/// Progress of a DMA read into an aligned buffer.
pub struct FileReadState<CharType: ByteType> {
    /// Set when the file ended before the requested length was read.
    pub eof: bool,
    /// The aligned buffer being filled.
    pub buf: TemporaryBuffer<CharType>,
    /// Number of bytes of `buf` filled so far.
    pub pos: usize,
    _offset: u64,
    _to_read: usize,
    _front: u64,
    _iref: IntentReference,
}

/// The buffer type of a [`FileReadState`].
pub type TmpBufType<CharType> = TemporaryBuffer<CharType>;

impl<CharType: ByteType> FileReadState<CharType> {
    /// A state for reading `to_read` bytes at `offset`, of which the first `front` bytes are
    /// only there for alignment. The buffer is aligned to `memory_alignment` and its size
    /// rounded up to `disk_alignment`. `intent` (null for none) allows cancelling the read;
    /// call [`bind_intent`](Self::bind_intent) once the state is at its final address.
    pub fn new(
        offset: u64,
        front: u64,
        to_read: usize,
        memory_alignment: usize,
        disk_alignment: usize,
        intent: *mut IoIntent,
    ) -> Self {
        FileReadState {
            eof: false,
            buf: TmpBufType::<CharType>::aligned(
                memory_alignment,
                to_read.next_multiple_of(disk_alignment),
            ),
            pos: 0,
            _offset: offset,
            _to_read: to_read,
            _front: front,
            _iref: IntentReference::new(intent),
        }
    }

    /// Links the intent reference once the state sits at its final address (the reference
    /// is unlinked when the state is dropped; see `IntentReference`).
    #[inline]
    pub fn bind_intent(self: Pin<&mut Self>) {
        // SAFETY: projecting the pin onto the field; it is never moved out.
        unsafe { self.map_unchecked_mut(|s| &mut s._iref) }.bind();
    }

    /// Whether the read is complete (end of file reached or all bytes read).
    #[inline]
    pub fn done(&self) -> bool {
        self.eof || self.pos >= self._to_read
    }

    /// Trims the buffer to the bytes actually read and drops the leading `front` alignment
    /// bytes; leaves it empty if no byte past `front` was read.
    pub fn trim_buf_before_ret(&mut self) {
        if self.have_good_bytes() {
            self.buf.trim(self.pos);
            self.buf.trim_front(self._front as usize);
        } else {
            self.buf.trim(0);
        }
    }

    /// The file offset of the next byte to read.
    #[inline]
    pub fn cur_offset(&self) -> u64 {
        self._offset + self.pos as u64
    }

    /// Free bytes left in the buffer.
    #[inline]
    pub fn left_space(&self) -> usize {
        self.buf.size() - self.pos
    }

    /// Bytes left to read; positive as long as [`done`](Self::done) is `false`.
    #[inline]
    pub fn left_to_read(&self) -> usize {
        // positive as long as (done() == false)
        self._to_read - self.pos
    }

    /// Copies as much of `new_data` as fits into the buffer and advances `pos`.
    #[inline]
    pub fn append_new_data(&mut self, new_data: &mut TmpBufType<CharType>) {
        let to_copy = self.left_space().min(new_data.size());
        // SAFETY: `buf` has `left_space()` writable bytes after `pos`; `new_data` has `size()` bytes.
        unsafe {
            std::ptr::copy_nonoverlapping(
                new_data.get(),
                self.buf.get_write().add(self.pos),
                to_copy,
            )
        };
        self.pos += to_copy;
    }

    /// Whether any byte past the alignment `front` has been read.
    #[inline]
    pub fn have_good_bytes(&self) -> bool {
        self.pos as u64 > self._front
    }

    /// The read's intent, or null if it has none.
    ///
    /// # Errors
    ///
    /// Returns a cancellation error if the intent was cancelled.
    #[inline]
    pub fn get_intent(&mut self) -> Result<*mut IoIntent, ExceptionPtr> {
        self._iref.retrieve()
    }
}
