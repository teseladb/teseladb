//! In-memory serialization cursors.
//!
//! Output cursors write into caller-provided memory and input cursors read from it, each
//! over a contiguous buffer (`Simple*`) or a fragmented one (`Fragmented*`, walking a
//! [`FragmentIterator`]). [`MemoryOutputStream`] and [`MemoryInputStream`] erase the
//! difference with an enum optimized for the contiguous case; [`with_serialized_stream`]
//! dispatches once on the representation so that a whole deserialization runs on the
//! concrete type.
//!
//! The cursors do not own memory: their constructors are `unsafe` and state how long the
//! bytes must stay valid. Running past the end of a cursor fails with an [`OutOfRange`]
//! error instead of touching memory.

use crate::engine::core::future::{ExceptionPtr, OutOfRange};

#[cold]
fn overflow() -> ExceptionPtr {
    ExceptionPtr::new(OutOfRange::new("serialization buffer overflow"))
}

#[cold]
fn underflow() -> ExceptionPtr {
    ExceptionPtr::new(OutOfRange::new("deserialization buffer underflow"))
}

/// A sink that only counts the bytes written to it.
#[derive(Debug, Clone, Copy, Default)]
pub struct MeasuringOutputStream {
    _size: usize,
}

impl MeasuringOutputStream {
    /// Counts `data.len()` more bytes.
    #[inline(always)]
    pub fn write(&mut self, data: &[u8]) {
        self._size += data.len();
    }

    /// Bytes counted so far.
    #[inline(always)]
    pub fn size(&self) -> usize {
        self._size
    }
}

/// The output sinks `copy_to` writes into.
pub trait StreamOutput {
    /// Writes `data`.
    ///
    /// # Errors
    /// The sink's error when `data` does not fit.
    fn write(&mut self, data: &[u8]) -> Result<(), ExceptionPtr>;
}

impl StreamOutput for MeasuringOutputStream {
    #[inline(always)]
    fn write(&mut self, data: &[u8]) -> Result<(), ExceptionPtr> {
        MeasuringOutputStream::write(self, data);
        Ok(())
    }
}

/// An iterator over the fragments of a fragmented buffer; each position yields one
/// fragment's start and size.
///
/// # Safety
/// Every fragment that `fragment()` returns, at any position of the iterator or of its
/// clones, must be valid for reads of its size, and for writes when the iterator is given
/// to an output stream, for as long as the iterator or any stream built on it is used.
pub unsafe trait FragmentIterator: Clone {
    /// The current fragment's start and size.
    fn fragment(&self) -> (*mut u8, usize);
    /// Moves to the next fragment.
    fn advance(&mut self);
}

/// A write cursor over a contiguous buffer.
#[derive(Debug, Clone, Copy)]
pub struct SimpleMemoryOutputStream {
    _p: *mut u8,
    _size: usize,
}

impl Default for SimpleMemoryOutputStream {
    fn default() -> Self {
        Self { _p: std::ptr::null_mut(), _size: 0 }
    }
}

impl SimpleMemoryOutputStream {
    /// Whether the type dispatches through `with_stream` (it is a concrete cursor).
    pub const HAS_WITH_STREAM: bool = false;

    /// A cursor over the `size` bytes at `p + start`.
    ///
    /// # Safety
    /// The `size` bytes at `p + start` must be valid for writes and reads, and not otherwise
    /// accessed, for as long as this cursor, its copies and the cursors made from it are used.
    #[inline(always)]
    pub unsafe fn new(p: *mut u8, size: usize, start: usize) -> Self {
        Self { _p: p.wrapping_add(start), _size: size }
    }

    /// The write position.
    #[inline(always)]
    pub fn begin(&self) -> *mut u8 {
        self._p
    }

    /// Advances past `size` bytes.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline(always)]
    pub fn skip(&mut self, size: usize) -> Result<(), ExceptionPtr> {
        if size > self._size {
            return Err(overflow());
        }
        self._p = self._p.wrapping_add(size);
        self._size -= size;
        Ok(())
    }

    /// A cursor over the next `size` bytes, which this one skips.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline(always)]
    pub fn write_substream(
        &mut self,
        size: usize,
    ) -> Result<SimpleMemoryOutputStream, ExceptionPtr> {
        if size > self._size {
            return Err(overflow());
        }
        // SAFETY: the next `size` bytes are part of this cursor's region (checked above).
        let substream = unsafe { SimpleMemoryOutputStream::new(self._p, size, 0) };
        self.skip(size)?;
        Ok(substream)
    }

    /// Copies `data` at the write position.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `data.len()` bytes are left.
    #[inline(always)]
    pub fn write(&mut self, data: &[u8]) -> Result<(), ExceptionPtr> {
        let size = data.len();
        if size > self._size {
            return Err(overflow());
        }
        // A default cursor has a null pointer, which `copy_nonoverlapping` rejects even for
        // zero bytes, so zero-byte writes return before touching the pointer.
        if size == 0 {
            return Ok(());
        }
        // SAFETY: the cursor covers at least `size` writable bytes (constructor contract), so
        // its pointer is not null; `data` holds `size` bytes and does not overlap them: the
        // region is not otherwise accessed (constructor contract), and a read cursor over it
        // is not read while it is written (`to_input_stream` contract).
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self._p, size) };
        self.skip(size)
    }

    /// Writes `size` copies of `c`.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline(always)]
    pub fn fill(&mut self, c: u8, size: usize) -> Result<(), ExceptionPtr> {
        if size > self._size {
            return Err(overflow());
        }
        // As in `write`: no pointer use for zero bytes.
        if size == 0 {
            return Ok(());
        }
        // SAFETY: the cursor covers at least `size` writable bytes (constructor contract), so
        // its pointer is not null.
        unsafe { std::ptr::write_bytes(self._p, c, size) };
        self.skip(size)
    }

    /// Bytes left.
    #[inline(always)]
    pub fn size(&self) -> usize {
        self._size
    }

    /// A read cursor over the same part of the buffer this write cursor covers.
    ///
    /// # Safety
    /// The two cursors share the bytes: while the returned cursor or a copy of it reads them
    /// (`read`, `copy_to`, the slice of a contiguous one), nothing writes them through this
    /// cursor or its copies. In particular the returned cursor is never copied (`copy_to`)
    /// into a cursor over the same bytes.
    #[inline(always)]
    pub unsafe fn to_input_stream(&self) -> SimpleMemoryInputStream {
        // SAFETY: this cursor's region is readable for as long as it is (constructor
        // contract); reads and writes do not overlap in time by the caller.
        unsafe { SimpleMemoryInputStream::new(self._p, self._size) }
    }
}

impl StreamOutput for SimpleMemoryOutputStream {
    #[inline(always)]
    fn write(&mut self, data: &[u8]) -> Result<(), ExceptionPtr> {
        SimpleMemoryOutputStream::write(self, data)
    }
}

/// A write cursor over a fragmented buffer.
#[derive(Debug, Clone)]
pub struct FragmentedMemoryOutputStream<I: FragmentIterator> {
    _it: I,
    _current: SimpleMemoryOutputStream,
    _size: usize,
}

impl<I: FragmentIterator> FragmentedMemoryOutputStream<I> {
    /// Whether the type dispatches through `with_stream` (it is a concrete cursor).
    pub const HAS_WITH_STREAM: bool = false;

    #[inline]
    fn for_each_fragment<F: FnMut(SimpleMemoryOutputStream)>(
        &mut self,
        mut size: usize,
        mut func: F,
    ) -> Result<(), ExceptionPtr> {
        if size > self._size {
            return Err(overflow());
        }
        self._size -= size;
        while size != 0 {
            if self._current.size() == 0 {
                let (p, s) = self._it.fragment();
                // SAFETY: fragments are writable for as long as the iterator is used
                // (`FragmentIterator` contract).
                self._current = unsafe { SimpleMemoryOutputStream::new(p, s, 0) };
                self._it.advance();
            }
            let this_size = self._current.size().min(size);
            func(self._current.write_substream(this_size)?);
            size -= this_size;
        }
        Ok(())
    }

    fn with_current(it: I, bv: SimpleMemoryOutputStream, size: usize) -> Self {
        Self { _it: it, _current: bv, _size: size }
    }

    /// A cursor over `size` bytes starting at the fragment `it` points to.
    #[inline]
    pub fn new(it: I, size: usize) -> Self {
        Self { _it: it, _current: SimpleMemoryOutputStream::default(), _size: size }
    }

    /// Advances past `size` bytes.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline]
    pub fn skip(&mut self, size: usize) -> Result<(), ExceptionPtr> {
        self.for_each_fragment(size, |_| {})
    }

    /// A cursor over the next `size` bytes, which this one skips.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    pub fn write_substream(&mut self, size: usize) -> Result<MemoryOutputStream<I>, ExceptionPtr> {
        if size > self._size {
            return Err(overflow());
        }
        if self._current.size() >= size {
            self._size -= size;
            return Ok(MemoryOutputStream::Simple(self._current.write_substream(size)?));
        }
        let substream =
            FragmentedMemoryOutputStream::with_current(self._it.clone(), self._current, size);
        self.skip(size)?;
        Ok(MemoryOutputStream::Fragmented(substream))
    }

    /// Copies `data` at the write position.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `data.len()` bytes are left.
    #[inline]
    pub fn write(&mut self, data: &[u8]) -> Result<(), ExceptionPtr> {
        let mut p = data.as_ptr();
        self.for_each_fragment(data.len(), |bv| {
            // An empty fragment may have a null pointer: nothing to copy.
            if bv.size() != 0 {
                // SAFETY: `bv` covers `bv.size()` writable bytes, so its pointer is not null;
                // the fragments handed out add up to `data.len()`, so `p` has that many left.
                unsafe { std::ptr::copy_nonoverlapping(p, bv.begin(), bv.size()) };
            }
            p = p.wrapping_add(bv.size());
        })
    }

    /// Writes `size` copies of `c`.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline]
    pub fn fill(&mut self, c: u8, size: usize) -> Result<(), ExceptionPtr> {
        self.for_each_fragment(size, |fragment| {
            if fragment.size() != 0 {
                // SAFETY: the fragment covers `fragment.size()` writable bytes, so its pointer
                // is not null.
                unsafe { std::ptr::write_bytes(fragment.begin(), c, fragment.size()) };
            }
        })
    }

    /// Bytes left.
    #[inline]
    pub fn size(&self) -> usize {
        self._size
    }

    /// A read cursor over the same part of the fragmented buffer this write cursor covers.
    ///
    /// # Safety
    /// The two cursors share the bytes: while the returned cursor or a copy of it reads them
    /// (`read`, `copy_to`, the slice of a contiguous one), nothing writes them through this
    /// cursor or its copies. In particular the returned cursor is never copied (`copy_to`)
    /// into a cursor over the same bytes.
    #[inline]
    pub unsafe fn to_input_stream(&self) -> FragmentedMemoryInputStream<I> {
        // SAFETY: the caller's contract.
        FragmentedMemoryInputStream::with_current(
            self._it.clone(),
            unsafe { self._current.to_input_stream() },
            self._size,
        )
    }
}

/// A visitor of either stream representation, called by `with_stream`.
pub trait StreamVisitor<S, F> {
    /// The visitor's result.
    type Output;
    /// Called with the simple stream.
    fn simple(self, s: &mut S) -> Self::Output;
    /// Called with the fragmented stream.
    fn fragmented(self, f: &mut F) -> Self::Output;
}

/// A write cursor over a simple or a fragmented buffer, optimized for the simple case.
#[derive(Debug, Clone)]
pub enum MemoryOutputStream<I: FragmentIterator> {
    /// Contiguous buffer.
    Simple(SimpleMemoryOutputStream),
    /// Fragmented buffer.
    Fragmented(FragmentedMemoryOutputStream<I>),
}

impl<I: FragmentIterator> Default for MemoryOutputStream<I> {
    fn default() -> Self {
        Self::Simple(SimpleMemoryOutputStream::default())
    }
}

impl<I: FragmentIterator> MemoryOutputStream<I> {
    /// Whether the type dispatches through `with_stream` (it is type-erased).
    pub const HAS_WITH_STREAM: bool = true;

    /// Calls `visitor` with the active representation.
    #[inline(always)]
    pub fn with_stream<V>(&mut self, visitor: V) -> V::Output
    where
        V: StreamVisitor<SimpleMemoryOutputStream, FragmentedMemoryOutputStream<I>>,
    {
        match self {
            Self::Simple(s) => visitor.simple(s),
            Self::Fragmented(f) => visitor.fragmented(f),
        }
    }

    /// Advances past `size` bytes.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline(always)]
    pub fn skip(&mut self, size: usize) -> Result<(), ExceptionPtr> {
        match self {
            Self::Simple(s) => s.skip(size),
            Self::Fragmented(f) => f.skip(size),
        }
    }

    /// A cursor over the next `size` bytes, which this one skips.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline(always)]
    pub fn write_substream(&mut self, size: usize) -> Result<MemoryOutputStream<I>, ExceptionPtr> {
        match self {
            Self::Simple(s) => Ok(Self::Simple(s.write_substream(size)?)),
            Self::Fragmented(f) => f.write_substream(size),
        }
    }

    /// Copies `data` at the write position.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `data.len()` bytes are left.
    #[inline(always)]
    pub fn write(&mut self, data: &[u8]) -> Result<(), ExceptionPtr> {
        match self {
            Self::Simple(s) => s.write(data),
            Self::Fragmented(f) => f.write(data),
        }
    }

    /// Writes `size` copies of `c`.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline(always)]
    pub fn fill(&mut self, c: u8, size: usize) -> Result<(), ExceptionPtr> {
        match self {
            Self::Simple(s) => s.fill(c, size),
            Self::Fragmented(f) => f.fill(c, size),
        }
    }

    /// Bytes left.
    #[inline(always)]
    pub fn size(&self) -> usize {
        match self {
            Self::Simple(s) => s.size(),
            Self::Fragmented(f) => f.size(),
        }
    }

    /// A read cursor over the same bytes.
    ///
    /// # Safety
    /// The two cursors share the bytes: while the returned cursor or a copy of it reads them
    /// (`read`, `copy_to`, the slice of a contiguous one), nothing writes them through this
    /// cursor or its copies. In particular the returned cursor is never copied (`copy_to`)
    /// into a cursor over the same bytes.
    #[inline]
    pub unsafe fn to_input_stream(&self) -> MemoryInputStream<I> {
        // SAFETY: the caller's contract.
        unsafe {
            match self {
                Self::Simple(s) => MemoryInputStream::Simple(s.to_input_stream()),
                Self::Fragmented(f) => MemoryInputStream::Fragmented(f.to_input_stream()),
            }
        }
    }
}

/// A read cursor over a contiguous buffer.
#[derive(Debug, Clone, Copy)]
pub struct SimpleMemoryInputStream {
    _p: *const u8,
    _size: usize,
}

impl Default for SimpleMemoryInputStream {
    fn default() -> Self {
        Self { _p: std::ptr::null(), _size: 0 }
    }
}

impl SimpleMemoryInputStream {
    /// Whether the type dispatches through `with_stream` (it is a concrete cursor).
    pub const HAS_WITH_STREAM: bool = false;

    /// A cursor over the `size` bytes at `p`.
    ///
    /// # Safety
    /// The `size` bytes at `p` must be valid for reads, and not written, for as long as this
    /// cursor, its copies and the cursors made from it are used.
    #[inline(always)]
    pub unsafe fn new(p: *const u8, size: usize) -> Self {
        Self { _p: p, _size: size }
    }

    /// The read position.
    #[inline(always)]
    pub fn begin(&self) -> *const u8 {
        self._p
    }

    /// Advances past `size` bytes.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline(always)]
    pub fn skip(&mut self, size: usize) -> Result<(), ExceptionPtr> {
        if size > self._size {
            return Err(underflow());
        }
        self._p = self._p.wrapping_add(size);
        self._size -= size;
        Ok(())
    }

    /// A cursor over the next `size` bytes, which this one skips.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline(always)]
    pub fn read_substream(&mut self, size: usize) -> Result<SimpleMemoryInputStream, ExceptionPtr> {
        if size > self._size {
            return Err(underflow());
        }
        // SAFETY: the next `size` bytes are part of this cursor's region (checked above).
        let substream = unsafe { SimpleMemoryInputStream::new(self._p, size) };
        self.skip(size)?;
        Ok(substream)
    }

    /// Fills `out` from the read position.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `out.len()` bytes are left.
    #[inline(always)]
    pub fn read(&mut self, out: &mut [u8]) -> Result<(), ExceptionPtr> {
        let size = out.len();
        if size > self._size {
            return Err(underflow());
        }
        // As in the output cursor: no pointer use for zero bytes (a default cursor is null).
        if size == 0 {
            return Ok(());
        }
        // SAFETY: the cursor covers at least `size` readable bytes (constructor contract), so
        // its pointer is not null; `out` has room for them and, being a unique borrow, does
        // not overlap them.
        unsafe { std::ptr::copy_nonoverlapping(self._p, out.as_mut_ptr(), size) };
        self.skip(size)
    }

    /// The remaining bytes.
    #[inline(always)]
    fn as_slice(&self) -> &[u8] {
        if self._size == 0 {
            return &[];
        }
        // SAFETY: a non-empty cursor covers `_size` readable bytes at `_p` (constructor contract).
        unsafe { std::slice::from_raw_parts(self._p, self._size) }
    }

    /// Writes the remaining bytes to `out`.
    #[inline(always)]
    pub fn copy_to<O: StreamOutput>(&self, out: &mut O) -> Result<(), ExceptionPtr> {
        out.write(self.as_slice())
    }

    /// Bytes left.
    #[inline(always)]
    pub fn size(&self) -> usize {
        self._size
    }
}

/// A read cursor over a fragmented buffer.
#[derive(Debug, Clone)]
pub struct FragmentedMemoryInputStream<I: FragmentIterator> {
    _it: I,
    _current: SimpleMemoryInputStream,
    _size: usize,
}

impl<I: FragmentIterator> FragmentedMemoryInputStream<I> {
    /// Whether the type dispatches through `with_stream` (it is a concrete cursor).
    pub const HAS_WITH_STREAM: bool = false;

    #[inline]
    fn for_each_fragment<F: FnMut(SimpleMemoryInputStream) -> Result<(), ExceptionPtr>>(
        &mut self,
        mut size: usize,
        mut func: F,
    ) -> Result<(), ExceptionPtr> {
        if size > self._size {
            return Err(underflow());
        }
        self._size -= size;
        while size != 0 {
            if self._current.size() == 0 {
                let (p, s) = self._it.fragment();
                // SAFETY: fragments are readable for as long as the iterator is used
                // (`FragmentIterator` contract).
                self._current = unsafe { SimpleMemoryInputStream::new(p, s) };
                self._it.advance();
            }
            let this_size = self._current.size().min(size);
            func(self._current.read_substream(this_size)?)?;
            size -= this_size;
        }
        Ok(())
    }

    fn with_current(it: I, bv: SimpleMemoryInputStream, size: usize) -> Self {
        Self { _it: it, _current: bv, _size: size }
    }

    /// A cursor over `size` bytes starting at the fragment `it` points to.
    #[inline]
    pub fn new(it: I, size: usize) -> Self {
        Self { _it: it, _current: SimpleMemoryInputStream::default(), _size: size }
    }

    /// Advances past `size` bytes.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline]
    pub fn skip(&mut self, size: usize) -> Result<(), ExceptionPtr> {
        self.for_each_fragment(size, |_| Ok(()))
    }

    /// A cursor over the next `size` bytes, which this one skips.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    pub fn read_substream(
        &mut self,
        size: usize,
    ) -> Result<FragmentedMemoryInputStream<I>, ExceptionPtr> {
        if size > self._size {
            return Err(underflow());
        }
        let substream =
            FragmentedMemoryInputStream::with_current(self._it.clone(), self._current, size);
        self.skip(size)?;
        Ok(substream)
    }

    /// Fills `out` from the read position.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `out.len()` bytes are left.
    #[inline]
    pub fn read(&mut self, out: &mut [u8]) -> Result<(), ExceptionPtr> {
        let mut p = out.as_mut_ptr();
        self.for_each_fragment(out.len(), |bv| {
            if bv.size() != 0 {
                // SAFETY: `bv` covers `bv.size()` readable bytes, so its pointer is not null;
                // the fragments handed out add up to `out.len()`, so `p` has room in `out`.
                unsafe { std::ptr::copy_nonoverlapping(bv.begin(), p, bv.size()) };
            }
            p = p.wrapping_add(bv.size());
            Ok(())
        })
    }

    /// Writes the remaining bytes to `out`.
    #[inline]
    pub fn copy_to<O: StreamOutput>(&mut self, out: &mut O) -> Result<(), ExceptionPtr> {
        let size = self._size;
        self.for_each_fragment(size, |bv| bv.copy_to(out))
    }

    /// Bytes left.
    #[inline]
    pub fn size(&self) -> usize {
        self._size
    }

    /// Start of the current fragment.
    #[inline]
    pub fn first_fragment_data(&self) -> *const u8 {
        self._current.begin()
    }

    /// Bytes left in the current fragment.
    #[inline]
    pub fn first_fragment_size(&self) -> usize {
        self._current.size()
    }

    /// The iterator at the next fragment.
    #[inline]
    pub fn fragment_iterator(&self) -> I {
        self._it.clone()
    }
}

/// A read cursor over a simple or a fragmented buffer.
///
/// The type erasure is optimized for the simple case: the methods are `#[inline]` so the
/// contiguous path is inlined directly into the user of the stream.
#[derive(Debug, Clone)]
pub enum MemoryInputStream<I: FragmentIterator> {
    /// Contiguous buffer.
    Simple(SimpleMemoryInputStream),
    /// Fragmented buffer.
    Fragmented(FragmentedMemoryInputStream<I>),
}

impl<I: FragmentIterator> MemoryInputStream<I> {
    /// Whether the type dispatches through `with_stream` (it is type-erased).
    pub const HAS_WITH_STREAM: bool = true;

    /// Calls `visitor` with the active representation.
    #[inline(always)]
    pub fn with_stream<V>(&mut self, visitor: V) -> V::Output
    where
        V: StreamVisitor<SimpleMemoryInputStream, FragmentedMemoryInputStream<I>>,
    {
        match self {
            Self::Simple(s) => visitor.simple(s),
            Self::Fragmented(f) => visitor.fragmented(f),
        }
    }

    /// Advances past `size` bytes.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline(always)]
    pub fn skip(&mut self, size: usize) -> Result<(), ExceptionPtr> {
        match self {
            Self::Simple(s) => s.skip(size),
            Self::Fragmented(f) => f.skip(size),
        }
    }

    /// A cursor over the next `size` bytes, which this one skips.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `size` bytes are left.
    #[inline(always)]
    pub fn read_substream(&mut self, size: usize) -> Result<MemoryInputStream<I>, ExceptionPtr> {
        match self {
            Self::Simple(s) => Ok(Self::Simple(s.read_substream(size)?)),
            Self::Fragmented(f) => Ok(Self::Fragmented(f.read_substream(size)?)),
        }
    }

    /// Fills `out` from the read position.
    ///
    /// # Errors
    /// An [`OutOfRange`] error when fewer than `out.len()` bytes are left.
    #[inline(always)]
    pub fn read(&mut self, out: &mut [u8]) -> Result<(), ExceptionPtr> {
        match self {
            Self::Simple(s) => s.read(out),
            Self::Fragmented(f) => f.read(out),
        }
    }

    /// Writes the remaining bytes to `out`.
    #[inline(always)]
    pub fn copy_to<O: StreamOutput>(&mut self, out: &mut O) -> Result<(), ExceptionPtr> {
        match self {
            Self::Simple(s) => s.copy_to(out),
            Self::Fragmented(f) => f.copy_to(out),
        }
    }

    /// Bytes left.
    #[inline(always)]
    pub fn size(&self) -> usize {
        match self {
            Self::Simple(s) => s.size(),
            Self::Fragmented(f) => f.size(),
        }
    }
}

/// Calls `visitor` once with the concrete representation of `stream`.
///
/// This minimizes the number of dispatches: generated deserialization code that skips
/// several fields and then decodes one dispatches once for the whole sequence, instead of
/// once per skip and decode call.
#[inline(always)]
pub fn with_serialized_stream<I, V>(stream: &mut MemoryInputStream<I>, visitor: V) -> V::Output
where
    I: FragmentIterator,
    V: StreamVisitor<SimpleMemoryInputStream, FragmentedMemoryInputStream<I>>,
{
    stream.with_stream(visitor)
}

/// [`with_serialized_stream`] for a concrete stream: calls the visitor directly.
#[inline(always)]
pub fn with_serialized_stream_direct<S, R, V: FnOnce(&mut S) -> R>(
    stream: &mut S,
    visitor: V,
) -> R {
    visitor(stream)
}

/// The contiguous read cursor.
pub type SimpleInputStream = SimpleMemoryInputStream;
/// The contiguous write cursor.
pub type SimpleOutputStream = SimpleMemoryOutputStream;
