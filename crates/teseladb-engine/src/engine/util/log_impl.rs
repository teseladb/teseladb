//! The buffer log messages are formatted into.
//!
//! [`internal::LogBuf`] lets the logger format a whole message (prefix, timestamp, text)
//! into one buffer without intermediate allocations. It can start on a caller-supplied
//! buffer (typically on the stack) and then only allocates when that fills up. Writes go
//! through [`internal::InserterIterator`], which implements [`std::fmt::Write`].

/// Internal logging buffer; not part of the stable API.
pub mod internal {
    use std::ptr;

    /// A buffer to format log messages into.
    ///
    /// It is designed to hold the entire formatted message, without intermediate buffers.
    /// To minimize reallocations it can start on an external buffer; when that is full it
    /// switches to buffers it allocates itself, doubling the size each time. If an
    /// allocation fails, the last bytes of the buffer are overwritten with
    /// `(log buffer allocation failure)` and further writes are dropped.
    pub struct LogBuf {
        pub(crate) _begin: *mut u8,
        pub(crate) _end: *mut u8,
        pub(crate) _current: *mut u8,
        pub(crate) _own_buf: bool,
        pub(crate) _alloc_failure: bool,
    }

    /// An output cursor into a [`LogBuf`].
    ///
    /// All the work happens in [`assign`](Self::assign), which appends one byte; `deref`,
    /// `inc` and `post_inc` are no-ops kept for iterator-style call sites. The cursor stores
    /// no position of its own: every copy appends at the buffer's current end.
    ///
    /// The copies share the buffer; the lifetime keeps the buffer mutably borrowed while any
    /// of them exists, so it cannot be used or freed meanwhile.
    #[derive(Clone, Copy)]
    pub struct InserterIterator<'a> {
        _buf: *mut LogBuf,
        _borrow: std::marker::PhantomData<&'a mut LogBuf>,
    }

    impl<'a> InserterIterator<'a> {
        /// Creates a cursor that appends to `buf`.
        #[inline]
        pub fn new(buf: &'a mut LogBuf) -> Self {
            InserterIterator { _buf: buf as *mut LogBuf, _borrow: std::marker::PhantomData }
        }

        /// Appends the byte `c` to the buffer, growing it when full. Hot path: one compare
        /// and one store unless the buffer must grow.
        #[inline]
        pub fn assign(&mut self, c: u8) -> &mut Self {
            // SAFETY: the buffer is borrowed for 'a, so it is alive and reached only through
            // the copies of this iterator, one call at a time; _current is within
            // [_begin, _end].
            unsafe {
                let buf = &mut *self._buf;
                // Unlikely: the buffer is full.
                if buf._current == buf._end {
                    buf.realloc_buffer_and_append(c);
                    return self;
                }
                *buf._current = c;
                buf._current = buf._current.add(1);
            }
            self
        }

        /// No-op; returns `self`.
        #[inline]
        pub fn deref(&mut self) -> &mut Self {
            self
        }

        /// No-op; returns `self`.
        #[inline]
        pub fn inc(&mut self) -> &mut Self {
            self
        }

        /// No-op; returns a copy of `self`.
        #[inline]
        pub fn post_inc(&mut self) -> Self {
            *self
        }

        /// Appends the byte `c` (same as [`assign`](Self::assign)).
        #[inline]
        pub fn push(&mut self, c: u8) {
            self.assign(c);
        }
    }

    /// Appends the UTF-8 bytes of formatted text to the buffer. Never fails; on an
    /// allocation failure the bytes are dropped (see [`LogBuf`]).
    impl std::fmt::Write for InserterIterator<'_> {
        #[inline]
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            for &b in s.as_bytes() {
                self.assign(b);
            }
            Ok(())
        }
    }

    /// Formats `args` into the buffer through `it` and returns the cursor.
    #[inline]
    pub fn format_to<'a>(
        mut it: InserterIterator<'a>,
        args: std::fmt::Arguments<'_>,
    ) -> InserterIterator<'a> {
        let _ = std::fmt::Write::write_fmt(&mut it, args);
        it
    }

    impl LogBuf {
        /// Creates a buffer with an internally allocated capacity of 512 bytes.
        pub fn new() -> Self {
            let v: Box<[u8]> = vec![0u8; 512].into_boxed_slice();
            let begin = Box::into_raw(v) as *mut u8;
            LogBuf {
                _begin: begin,
                // SAFETY: begin points to an allocation of 512 bytes.
                _end: unsafe { begin.add(512) },
                _current: begin,
                _own_buf: true,
                _alloc_failure: false,
            }
        }

        /// Creates a buffer that writes into `external_buf` until it is full, then switches
        /// to internally allocated buffers. The external buffer is not owned or freed.
        ///
        /// # Safety
        /// `external_buf` must be valid for writes of `size` bytes for the lifetime of the
        /// returned `LogBuf`, and not accessed through other pointers meanwhile.
        pub unsafe fn with_external(external_buf: *mut u8, size: usize) -> Self {
            LogBuf {
                _begin: external_buf,
                // SAFETY: the caller guarantees external_buf is valid for size bytes.
                _end: unsafe { external_buf.add(size) },
                _current: external_buf,
                _own_buf: false,
                _alloc_failure: false,
            }
        }

        pub(crate) fn free_buffer(&mut self) {
            if self._own_buf {
                // SAFETY: when _own_buf, _begin.._end is a Box<[u8]> allocated by us.
                unsafe {
                    let len = self.capacity();
                    drop(Box::from_raw(ptr::slice_from_raw_parts_mut(self._begin, len)));
                }
            }
        }

        pub(crate) fn realloc_buffer_and_append(&mut self, c: u8) {
            if self._alloc_failure {
                // Already failed to reallocate once, don't try again
                return;
            }
            let old_size = self.size();
            let new_size = old_size * 2;
            let mut v: Vec<u8> = Vec::new();
            if v.try_reserve_exact(new_size).is_err() {
                self._alloc_failure = true;
                let msg = b"(log buffer allocation failure)";
                let can_copy = std::cmp::min(msg.len(), self.size());
                // SAFETY: can_copy <= size(), so the destination is within the buffer.
                unsafe {
                    ptr::copy_nonoverlapping(msg.as_ptr(), self._current.sub(can_copy), can_copy);
                }
                return;
            }
            v.resize(new_size, 0);
            let new_buf = Box::into_raw(v.into_boxed_slice()) as *mut u8;
            // SAFETY: new_buf has new_size >= old_size bytes; _begin has old_size valid bytes.
            unsafe {
                ptr::copy_nonoverlapping(self._begin, new_buf, old_size);
            }
            self.free_buffer();
            self._begin = new_buf;
            // SAFETY: offsets are within the new allocation.
            unsafe {
                self._current = self._begin.add(old_size);
                self._end = self._begin.add(new_size);
            }
            self._own_buf = true;
            // SAFETY: old_size < new_size (old_size > 0 since the buffer was full).
            unsafe {
                *self._current = c;
                self._current = self._current.add(1);
            }
        }

        /// Resets the write position to the start without freeing anything: the size
        /// becomes zero and the capacity is unchanged.
        pub fn clear(&mut self) {
            self._current = self._begin;
        }

        /// Returns a cursor that appends to the buffer.
        pub fn back_insert_begin(&mut self) -> InserterIterator<'_> {
            InserterIterator::new(self)
        }

        /// The number of bytes written so far.
        pub fn size(&self) -> usize {
            self._current as usize - self._begin as usize
        }

        /// The size of the current buffer.
        pub fn capacity(&self) -> usize {
            self._end as usize - self._begin as usize
        }

        /// Read-only pointer to the start of the buffer.
        ///
        /// The content is not NUL-terminated; a writer that needs a terminator must add it.
        pub fn data(&self) -> *const u8 {
            self._begin
        }

        /// The bytes written so far. The formatted text is UTF-8, but the allocation
        /// failure message overwrites its last bytes and may cut a character.
        pub fn view(&self) -> &[u8] {
            // SAFETY: _begin.._current is the initialized, written part of the buffer.
            unsafe { std::slice::from_raw_parts(self._begin, self.size()) }
        }
    }

    impl Default for LogBuf {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for LogBuf {
        fn drop(&mut self) {
            self.free_buffer();
        }
    }
}
