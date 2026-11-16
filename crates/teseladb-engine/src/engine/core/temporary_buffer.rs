//! Temporary, self-managed byte buffers.
//!
//! [`TemporaryBuffer`] is the unit of data that flows through the I/O stack: file reads,
//! network packets and stream buffers hand it around. It points at bytes whose lifetime is
//! managed by a [`Deleter`], so sharing a buffer or a sub-range of it
//! ([`TemporaryBuffer::share`], [`TemporaryBuffer::share_range`]) costs a reference count
//! increment, not a copy. Buffers belong to one shard: the deleter's count is not atomic.
//!
//! Most accessors come in a checked form and an `_unchecked` form for hot paths, where the
//! caller proves the precondition.

use std::marker::PhantomData;

use crate::engine::core::deleter::{Deleter, make_free_deleter};

unsafe extern "C" {
    fn malloc(size: usize) -> *mut std::ffi::c_void;
    fn posix_memalign(memptr: *mut *mut std::ffi::c_void, alignment: usize, size: usize) -> i32;
}

/// The byte types a buffer can hold (`u8` and `i8`): they are one byte wide, so sizes in
/// elements and in bytes are the same.
pub trait ByteType: Copy + Eq + 'static {
    /// Compile-time check that the type is one byte wide, evaluated when a buffer of the
    /// type is made.
    const IS_ONE_BYTE: () = assert!(std::mem::size_of::<Self>() == 1);
}
impl ByteType for u8 {}
impl ByteType for i8 {}

#[cold]
fn bad_alloc(size: usize) -> ! {
    std::alloc::handle_alloc_error(
        std::alloc::Layout::from_size_align(size.max(1), 1)
            .unwrap_or(std::alloc::Layout::new::<u8>()),
    )
}

/// Temporary, self-managed byte buffer.
///
/// Like a `Box<[u8]>`, but with more flexible memory management: a buffer can own the
/// memory it points to, share it with another buffer, or point at a sub-range of a
/// buffer. Its [`Deleter`] manages the memory.
///
/// A buffer should not be held indefinitely. It can be held while a request is processed,
/// or for a similar duration, but not longer, as it can tie up more memory than its size
/// indicates.
///
/// Two buffers can point to the same data, or a subset of it; see
/// [`TemporaryBuffer::share`].
///
/// Unless you created a buffer yourself, do not modify its contents: they may be shared
/// with another user that does not expect the data to change.
///
/// Use cases include:
///  - passing a sub-range of a TCP packet for the user to consume (zero-copy TCP input);
///  - passing a reference-counted blob held in memory to TCP, so that when the TCP ACK is
///    received the blob is released by decrementing its reference count (zero-copy TCP
///    output).
///
/// `CharType` is the element type, a one-byte [`ByteType`].
#[repr(C)]
pub struct TemporaryBuffer<CharType: ByteType = u8> {
    _buffer: *mut CharType,
    _size: usize,
    _deleter: Deleter,
    _marker: PhantomData<CharType>,
}

impl<CharType: ByteType> TemporaryBuffer<CharType> {
    /// Creates a buffer of `size` bytes, allocated with `malloc`. The buffer is not shared
    /// with anyone, and is not initialized.
    ///
    /// # Panics
    /// Aborts the process through the global allocation error handler when `malloc` fails.
    #[inline]
    pub fn new(size: usize) -> Self {
        let () = CharType::IS_ONE_BYTE;
        // SAFETY: plain malloc; the result is checked below.
        let buffer = unsafe { malloc(size) }.cast::<CharType>();
        if size != 0 && buffer.is_null() {
            bad_alloc(size);
        }
        TemporaryBuffer {
            _buffer: buffer,
            _size: size,
            _deleter: make_free_deleter(buffer.cast()),
            _marker: PhantomData,
        }
    }

    /// [`TemporaryBuffer::new`] that reports allocation failure instead of aborting, so the
    /// caller can fail one operation.
    ///
    /// # Errors
    /// A [`BadAlloc`](crate::engine::core::future::BadAlloc) exception when `malloc` fails.
    #[inline]
    pub fn try_new(size: usize) -> Result<Self, crate::engine::core::future::ExceptionPtr> {
        let () = CharType::IS_ONE_BYTE;
        // SAFETY: plain malloc; the result is checked below.
        let buffer = unsafe { malloc(size) }.cast::<CharType>();
        if size != 0 && buffer.is_null() {
            return Err(crate::engine::core::future::ExceptionPtr::new(
                crate::engine::core::future::BadAlloc,
            ));
        }
        Ok(TemporaryBuffer {
            _buffer: buffer,
            _size: size,
            _deleter: make_free_deleter(buffer.cast()),
            _marker: PhantomData,
        })
    }

    /// Creates an empty buffer that does not point at anything.
    #[inline]
    pub fn empty_buffer() -> Self {
        TemporaryBuffer {
            _buffer: std::ptr::null_mut(),
            _size: 0,
            _deleter: Deleter::default(),
            _marker: PhantomData,
        }
    }

    /// Creates a buffer of `size` elements at `buf`, whose memory is managed by `d`. The
    /// deleter is dropped when there are no longer any users of the buffer.
    ///
    /// # Safety
    /// `buf` is valid for reads (and, through `get_write`, writes) of `size` elements for as
    /// long as `d`, or any deleter shared from it, is alive; `buf` may be dangling only when
    /// `size` is 0.
    #[inline]
    pub unsafe fn with_deleter(buf: *mut CharType, size: usize, d: Deleter) -> Self {
        TemporaryBuffer { _buffer: buf, _size: size, _deleter: d, _marker: PhantomData }
    }

    /// Creates a buffer containing a copy of `src`.
    ///
    /// # Panics
    /// Aborts the process through the global allocation error handler when `malloc` fails.
    #[inline]
    pub fn copy_from(src: &[CharType]) -> Self {
        let b = Self::new(src.len());
        // SAFETY: `b` has `src.len()` writable bytes that do not overlap `src`.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), b._buffer, src.len()) };
        b
    }

    /// Gets a pointer to the beginning of the buffer.
    #[inline]
    pub fn get(&self) -> *const CharType {
        self._buffer
    }

    /// Gets a writable pointer to the beginning of the buffer.  Use only
    /// when you are certain no user expects the buffer data not to change.
    #[inline]
    pub fn get_write(&mut self) -> *mut CharType {
        self._buffer
    }

    /// Gets the buffer size.
    #[inline]
    pub fn size(&self) -> usize {
        self._size
    }

    /// Gets a pointer to the beginning of the buffer.
    #[inline]
    pub fn begin(&self) -> *const CharType {
        self._buffer
    }

    /// Gets a pointer to the end of the buffer.
    #[inline]
    pub fn end(&self) -> *const CharType {
        self._buffer.wrapping_add(self._size)
    }

    /// The contents as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[CharType] {
        if self._size == 0 {
            return &[];
        }
        // SAFETY: a non-empty buffer points at `_size` readable bytes kept alive by the deleter.
        unsafe { std::slice::from_raw_parts(self._buffer, self._size) }
    }

    /// Returns the first `size` elements of the buffer; the buffer is consumed.
    ///
    /// The precondition is checked (one comparison); [`TemporaryBuffer::prefix_unchecked`]
    /// skips it.
    ///
    /// # Panics
    /// When `size > self.size()`.
    #[inline]
    pub fn prefix(self, size: usize) -> Self {
        assert!(size <= self._size, "temporary_buffer::prefix: {} > size {}", size, self._size);
        // SAFETY: checked above.
        unsafe { self.prefix_unchecked(size) }
    }

    /// [`TemporaryBuffer::prefix`] without the check.
    ///
    /// # Safety
    /// `size <= self.size()`.
    #[inline]
    pub unsafe fn prefix_unchecked(mut self, size: usize) -> Self {
        debug_assert!(size <= self._size);
        self._size = size;
        self
    }

    /// Reads the element at `pos`.
    ///
    /// [`TemporaryBuffer::at_unchecked`] skips the bounds check.
    ///
    /// # Panics
    /// When `pos >= self.size()`.
    #[inline]
    pub fn at(&self, pos: usize) -> CharType {
        self.as_slice()[pos]
    }

    /// [`TemporaryBuffer::at`] without the check.
    ///
    /// # Safety
    /// `pos < self.size()`.
    #[inline]
    pub unsafe fn at_unchecked(&self, pos: usize) -> CharType {
        debug_assert!(pos < self._size);
        // SAFETY: guaranteed by the caller.
        unsafe { *self._buffer.add(pos) }
    }

    /// Checks whether the buffer is empty.
    #[inline]
    pub fn empty(&self) -> bool {
        self.size() == 0
    }

    /// Checks whether the buffer is not empty.
    #[inline]
    pub fn is_nonempty(&self) -> bool {
        self.size() != 0
    }

    /// Returns a new buffer referring to the same data, without copying. The underlying
    /// [`Deleter`] is not dropped until both buffers have been dropped.
    #[inline]
    pub fn share(&mut self) -> Self {
        // SAFETY: the same bytes, kept alive by a share of the same deleter.
        unsafe { TemporaryBuffer::with_deleter(self._buffer, self._size, self._deleter.share()) }
    }

    /// Returns a new buffer referring to the `len` elements starting at `pos`, without
    /// copying. The underlying [`Deleter`] is not dropped until both buffers have been
    /// dropped.
    ///
    /// The range is checked against the size; [`TemporaryBuffer::share_range_unchecked`]
    /// skips the check.
    ///
    /// # Panics
    /// When `pos + len > self.size()`.
    #[inline]
    pub fn share_range(&mut self, pos: usize, len: usize) -> Self {
        assert!(
            pos <= self._size && len <= self._size - pos,
            "temporary_buffer::share: range {}+{} beyond size {}",
            pos,
            len,
            self._size
        );
        // SAFETY: checked above.
        unsafe { self.share_range_unchecked(pos, len) }
    }

    /// [`TemporaryBuffer::share_range`] without the check.
    ///
    /// # Safety
    /// `pos + len <= self.size()`.
    #[inline]
    pub unsafe fn share_range_unchecked(&mut self, pos: usize, len: usize) -> Self {
        debug_assert!(pos <= self._size && len <= self._size - pos);
        let mut ret = self.share();
        ret._buffer = ret._buffer.wrapping_add(pos);
        ret._size = len;
        ret
    }

    /// Returns a deep copy: a new buffer with the same length and data, in its own
    /// memory.
    #[inline]
    pub fn clone_buffer(&self) -> Self {
        Self::copy_from(self.as_slice())
    }

    /// Removes the first `pos` elements from the view; the underlying data is not
    /// modified.
    ///
    /// [`TemporaryBuffer::trim_front_unchecked`] skips the check.
    ///
    /// # Panics
    /// When `pos > self.size()`.
    #[inline]
    pub fn trim_front(&mut self, pos: usize) {
        assert!(pos <= self._size, "temporary_buffer::trim_front: {} > size {}", pos, self._size);
        // SAFETY: checked above.
        unsafe { self.trim_front_unchecked(pos) }
    }

    /// [`TemporaryBuffer::trim_front`] without the check.
    ///
    /// # Safety
    /// `pos <= self.size()`.
    #[inline]
    pub unsafe fn trim_front_unchecked(&mut self, pos: usize) {
        debug_assert!(pos <= self._size);
        self._buffer = self._buffer.wrapping_add(pos);
        self._size -= pos;
    }

    /// Shortens the view to its first `pos` elements; the underlying data is not
    /// modified.
    ///
    /// [`TemporaryBuffer::trim_unchecked`] skips the check.
    ///
    /// # Panics
    /// When `pos > self.size()`.
    #[inline]
    pub fn trim(&mut self, pos: usize) {
        assert!(pos <= self._size, "temporary_buffer::trim: {} > size {}", pos, self._size);
        // SAFETY: checked above.
        unsafe { self.trim_unchecked(pos) }
    }

    /// [`TemporaryBuffer::trim`] without the check.
    ///
    /// # Safety
    /// `pos <= self.size()`.
    #[inline]
    pub unsafe fn trim_unchecked(&mut self, pos: usize) {
        debug_assert!(pos <= self._size);
        self._size = pos;
    }

    /// Stops automatic memory management and returns the [`Deleter`] managing the data's
    /// lifetime. Dropping the buffer then no longer releases the data; the caller drops
    /// the returned deleter when the data is no longer needed.
    #[inline]
    pub fn release(&mut self) -> Deleter {
        std::mem::take(&mut self._deleter)
    }

    /// The same bytes seen as another byte type, without copying; the buffer is consumed.
    #[inline]
    pub fn retype<Other: ByteType>(mut self) -> TemporaryBuffer<Other> {
        let d = self.release();
        // SAFETY: the same bytes (both types are one byte wide), kept alive by the deleter.
        unsafe { TemporaryBuffer::<Other>::with_deleter(self._buffer as *mut Other, self._size, d) }
    }

    /// Creates a buffer of `size` bytes whose start is aligned to `alignment`, allocated
    /// with `posix_memalign` (as needed for direct I/O).
    ///
    /// `alignment` must be a power of two and a multiple of the pointer size; `size` should
    /// be a multiple of `alignment`.
    ///
    /// # Panics
    /// Aborts the process through the global allocation error handler when
    /// `posix_memalign` fails, including for an invalid alignment.
    #[inline]
    pub fn aligned(alignment: usize, size: usize) -> Self {
        let () = CharType::IS_ONE_BYTE;
        let mut ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        // SAFETY: `ptr` is a valid out-pointer.
        let ret = unsafe { posix_memalign(&mut ptr, alignment, size) };
        let buf = ptr.cast::<CharType>();
        if ret != 0 {
            bad_alloc(size);
        }
        // SAFETY: `buf` is a fresh reservation of `size` bytes, freed by the deleter.
        unsafe { TemporaryBuffer::with_deleter(buf, size, make_free_deleter(buf.cast())) }
    }

    /// Creates a buffer containing a copy of the bytes of `view`.
    ///
    /// # Panics
    /// Aborts the process through the global allocation error handler when `malloc` fails.
    #[inline]
    pub fn copy_of(view: &str) -> Self {
        // SAFETY: plain malloc; checked below.
        let ptr = unsafe { malloc(view.len()) };
        if ptr.is_null() {
            bad_alloc(view.len());
        }
        let buf = ptr.cast::<CharType>();
        // SAFETY: `buf` has `view.len()` writable bytes.
        unsafe { std::ptr::copy_nonoverlapping(view.as_ptr().cast::<CharType>(), buf, view.len()) };
        // SAFETY: `buf` is a fresh reservation of `view.len()` bytes, freed by the deleter.
        unsafe { TemporaryBuffer::with_deleter(buf, view.len(), make_free_deleter(buf.cast())) }
    }
}

impl<CharType: ByteType> Default for TemporaryBuffer<CharType> {
    /// An empty buffer, as [`TemporaryBuffer::empty_buffer`].
    #[inline]
    fn default() -> Self {
        Self::empty_buffer()
    }
}

impl<CharType: ByteType> PartialEq for TemporaryBuffer<CharType> {
    /// Compares the contents of two buffers: true if and only if they have the same length
    /// and bytes.
    #[inline]
    fn eq(&self, o: &Self) -> bool {
        self.size() == o.size() && self.as_slice() == o.as_slice()
    }
}

impl<CharType: ByteType> Eq for TemporaryBuffer<CharType> {}

impl<CharType: ByteType> std::ops::Index<usize> for TemporaryBuffer<CharType> {
    type Output = CharType;
    /// The element at an index, bounds-checked; hot paths use
    /// [`TemporaryBuffer::at_unchecked`].
    #[inline]
    fn index(&self, pos: usize) -> &CharType {
        &self.as_slice()[pos]
    }
}
