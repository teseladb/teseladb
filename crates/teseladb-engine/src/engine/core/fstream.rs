//! Byte-stream adapters over DMA files.
//!
//! Files are block-based because they rely on direct (DMA) I/O: reads and writes must be
//! on sector boundaries. This module gives files a byte stream interface while keeping
//! their zero-copy behaviour:
//!
//! - [`make_file_data_source`] and the `make_file_input_stream*` functions read a file
//!   through a data source that issues aligned, read-ahead DMA reads and trims the first
//!   and last buffers. With [`FileInputStreamOptions::dynamic_adjustments`] the buffer
//!   size and read-ahead adapt to how much read data the consumer drops (see
//!   [`FileInputStreamHistory`]).
//! - [`make_file_data_sink`] and the `make_file_output_stream*` functions write a new
//!   file from position zero with aligned buffers and up to
//!   [`FileOutputStreamOptions::write_behind`] writes in flight.
//!
//! The source and sink keep pointers to themselves in their continuations, so they live
//! in a node that never moves. Variants that differ only by arguments have distinct names:
//! `make_file_input_stream` (whole file), `make_file_input_stream_from` (offset),
//! `make_file_input_stream_range` (offset and length), `make_file_data_source` (range),
//! `make_file_data_source_whole`, `make_file_output_stream` (options) and
//! `make_file_output_stream_with_size`.

use std::pin::Pin;

use crate::engine::core::align::{align_down, align_up};
use crate::engine::core::circular_buffer::CircularBuffer;
use crate::engine::core::file::File;
use crate::engine::core::future::{Future, Promise, make_exception_future, make_ready_future};
use crate::engine::core::io_intent::IoIntent;
use crate::engine::core::io_priority_class::internal::MaybePriorityClassRef;
use crate::engine::core::iostream::{
    DataSink, DataSinkImpl, DataSource, DataSourceImpl, InputStream, OutputStream,
};
use crate::engine::core::reactor::{Reactor, engine};
use crate::engine::core::reactor_backend::engine_assert;
use crate::engine::core::semaphore::Semaphore;
use crate::engine::core::shared_ptr::LwSharedPtr;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::core::when_all::when_all;
use crate::engine::net::packet::Packet;

/// One window of read statistics of a file input stream.
#[derive(Clone, Copy, Debug, Default)]
pub struct Window {
    /// Bytes read in the window.
    pub total_read: u64,
    /// Bytes read in the window but dropped unconsumed (after a skip or close).
    pub unused_read: u64,
}

/// Read statistics shared by the file input streams that use it, driving the dynamic
/// adjustment of their buffer size and read-ahead.
#[derive(Clone, Debug)]
pub struct FileInputStreamHistory {
    pub(crate) current_window: Window,
    pub(crate) previous_window: Window,
    pub(crate) read_ahead: u32,
}

impl FileInputStreamHistory {
    /// The size of a statistics window, in bytes read (4 MiB).
    pub const WINDOW_SIZE: u64 = 4 * 1024 * 1024;
}

impl Default for FileInputStreamHistory {
    #[inline]
    fn default() -> Self {
        FileInputStreamHistory {
            current_window: Window::default(),
            previous_window: Window::default(),
            read_ahead: 1,
        }
    }
}

/// Options for opening a file input stream.
#[derive(Clone)]
pub struct FileInputStreamOptions {
    /// I/O buffer size in bytes (default 8192).
    pub buffer_size: usize,
    /// Maximum number of extra read-ahead operations (default 0).
    pub read_ahead: u32,
    /// Shared read statistics; when `None`, dynamic adjustments are disabled.
    pub dynamic_adjustments: Option<LwSharedPtr<FileInputStreamHistory>>,
}

impl Default for FileInputStreamOptions {
    #[inline]
    fn default() -> Self {
        FileInputStreamOptions { buffer_size: 8192, read_ahead: 0, dynamic_adjustments: None }
    }
}

/// Options for opening a file output stream.
///
/// For small files, setting `preallocation_size` can make it impossible for XFS to find an
/// aligned extent; without it, XFS divides the file into `file_size / buffer_size`
/// extents. To avoid fragmentation the defaults are a 64 KiB `buffer_size` (so each extent
/// is at least 64 KiB) and no preallocation. Large files should increase both.
#[derive(Clone, Copy, Debug)]
pub struct FileOutputStreamOptions {
    /// I/O buffer size in bytes (default 65536).
    pub buffer_size: u32,
    /// Bytes of extents to preallocate. For large files, set a large number (a few
    /// megabytes) to reduce fragmentation.
    pub preallocation_size: u32,
    /// Number of buffers to write in parallel (default 1).
    pub write_behind: u32,
}

impl Default for FileOutputStreamOptions {
    #[inline]
    fn default() -> Self {
        FileOutputStreamOptions { buffer_size: 65536, preallocation_size: 0, write_behind: 1 }
    }
}

/// The buffer size to use: `configured_value` if it is within `maximum_value`, otherwise
/// the largest power of two not above the limit, which uses the buffer memory better.
#[inline]
fn select_buffer_size(configured_value: u64, maximum_value: u64) -> u64 {
    if configured_value <= maximum_value {
        configured_value
    } else {
        1u64 << log2floor(maximum_value)
    }
}

/// The floor of the base-2 logarithm of `n` (0 for 0).
#[inline]
const fn log2floor(n: u64) -> u32 {
    if n == 0 { 0 } else { u64::BITS - 1 - n.leading_zeros() }
}

/// The priority class of the stream's reads and writes: always the caller's scheduling
/// group.
#[inline]
fn get_io_priority<Options>(_opts: &Options) -> MaybePriorityClassRef {
    MaybePriorityClassRef
}

/// A read-ahead in flight: the file range it covers and its future.
struct IssuedRead {
    _pos: u64,
    _size: u64,
    _ready: Future<TemporaryBuffer<u8>>,
}

impl IssuedRead {
    /// A read of `size` bytes at `pos`, resolving through `f`.
    #[inline]
    fn new(pos: u64, size: u64, f: Future<TemporaryBuffer<u8>>) -> Self {
        IssuedRead { _pos: pos, _size: size, _ready: f }
    }
}

/// Whether a buffer size update follows a skip.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AfterSkip {
    No,
    Yes,
}

// The target maximum fraction of read bytes that are dropped unconsumed: 1/4.
const UNUSED_RATIO_TARGET_NUM: i64 = 1;
const UNUSED_RATIO_TARGET_DEN: i64 = 4;

/// The data source of a file input stream: issues aligned DMA reads with read-ahead and
/// trims them to the requested range.
///
/// It is only made by [`make_file_data_source`], which places it in the node of its
/// [`DataSource`] and records the node's root pointer in `_this`. The reads point at its
/// I/O intent and the continuations keep `_this`, so it never moves; it is `!Unpin` (`_pinned`), so the
/// `&mut self` of its operations asserts no uniqueness over the continuations' accesses
/// through `_this` (one thread; they never run during an operation's own body except as
/// children of it, through ready futures).
struct FileDataSourceImpl {
    /// The root pointer of the node the source lives in.
    _this: *mut FileDataSourceImpl,
    _pinned: std::marker::PhantomPinned,
    _reactor: *mut Reactor,
    _file: File,
    _options: FileInputStreamOptions,
    _pos: u64,
    _remain: u64,
    _read_buffers: CircularBuffer<IssuedRead>,
    _reads_in_progress: u32,
    _current_read_ahead: u32,
    _dropped_reads: Future<()>,
    _done: Option<Promise<()>>,
    _current_buffer_size: usize,
    _in_slow_start: bool,
    _intent: IoIntent,
}

impl FileDataSourceImpl {
    /// The smallest buffer size the dynamic adjustment may use: a quarter of the configured
    /// size, at least 8192 bytes, at most the configured size.
    #[inline]
    fn minimal_buffer_size(&self) -> usize {
        (self._options.buffer_size / 4).max(8192).min(self._options.buffer_size)
    }

    /// Increases the read-ahead by one when allowed.
    fn try_increase_read_ahead(&mut self) {
        // Read-ahead can be increased up to the configured limit when the consumer has to
        // wait for a buffer and the source is not in its slow start phase.
        if self._current_read_ahead < self._options.read_ahead && !self._in_slow_start {
            self._current_read_ahead += 1;
            let cra = self._current_read_ahead;
            if let Some(h) = self.history() {
                h.read_ahead = h.read_ahead.max(cra);
            }
        }
    }

    /// The read-ahead to start with: the history's, capped by the configured limit, when
    /// there is a history; otherwise 1 if any read-ahead is configured.
    #[inline]
    fn get_initial_read_ahead(options: &FileInputStreamOptions) -> u32 {
        match options.dynamic_adjustments.as_ref() {
            Some(d) => d.read_ahead.min(options.read_ahead),
            None => (options.read_ahead != 0) as u32,
        }
    }

    /// The shared read statistics, if dynamic adjustments are enabled.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    fn history(&self) -> Option<&mut FileInputStreamHistory> {
        // SAFETY: the history is alive while the options hold it; the reactor is
        // single-threaded and no other reference to it is in use here.
        self._options.dynamic_adjustments.as_ref().map(|d| unsafe { &mut *d.get() })
    }

    /// Records `total` bytes read, of which `unused` were dropped.
    fn update_history(&mut self, unused: u64, total: u64) {
        // Two windows are kept, each no larger than WINDOW_SIZE. The adjustment uses both,
        // so the effective window is variable, in [WINDOW_SIZE, 2 * WINDOW_SIZE].
        let Some(h) = self.history() else { return };
        h.current_window.total_read += total;
        h.current_window.unused_read += unused;
        if h.current_window.total_read >= FileInputStreamHistory::WINDOW_SIZE {
            h.previous_window = h.current_window;
            h.current_window = Window::default();
        }
    }

    /// Whether `unused / total` is below the target ratio of dropped bytes.
    #[inline]
    fn below_target(unused: u64, total: u64) -> bool {
        unused * (UNUSED_RATIO_TARGET_DEN as u64) < total * (UNUSED_RATIO_TARGET_NUM as u64)
    }

    /// Records `bytes` consumed and grows the buffer size while the history allows it.
    fn update_history_consumed(&mut self, bytes: u64) {
        if self._options.dynamic_adjustments.is_none() {
            return;
        }
        self.update_history(0, bytes);
        if !self._in_slow_start {
            return;
        }
        let new_size = (self._current_buffer_size * 2).min(self._options.buffer_size) as u32;
        let Some(h) = self.history() else { return };
        let total = h.current_window.total_read + h.previous_window.total_read + new_size as u64;
        let unused = h.current_window.unused_read + h.previous_window.unused_read + new_size as u64;
        // Check whether the buffer size can grow to `new_size` and stay below the target
        // ratio even if the whole buffer is dropped.
        if Self::below_target(unused, total) {
            self._current_buffer_size = new_size as usize;
            self._in_slow_start = self._current_buffer_size < self._options.buffer_size;
        }
    }

    /// Recomputes the buffer size from the history after bytes were dropped.
    fn set_new_buffer_size(&mut self, skip: AfterSkip) {
        let Some(h) = self.history() else { return };
        let total: i64 = (h.current_window.total_read + h.previous_window.total_read) as i64;
        let unused: i64 = (h.current_window.unused_read + h.previous_window.unused_read) as i64;
        if skip == AfterSkip::Yes && Self::below_target(unused as u64, total as u64) {
            // Do not shrink the buffer while still below the target; otherwise this would
            // fight update_history_consumed(), which grows the buffer as much as possible
            // while a single drop keeps it below the target.
            return;
        }
        // Compute the largest buffer size that keeps the ratio below the target even if
        // the next reads are dropped. If it is at least the current size nothing changes;
        // if it is smaller, the source goes back to its slow start phase.
        let new_target = (UNUSED_RATIO_TARGET_NUM * total - UNUSED_RATIO_TARGET_DEN * unused)
            / (UNUSED_RATIO_TARGET_DEN - UNUSED_RATIO_TARGET_NUM);
        let mut new_size: u64 = new_target.max(self.minimal_buffer_size() as i64) as u64;
        new_size = (1u64 << log2floor(new_size)).max(self.minimal_buffer_size() as u64);
        if new_size >= self._current_buffer_size as u64 {
            return;
        }
        self._in_slow_start = true;
        self._current_read_ahead = self._current_read_ahead.min(1);
        self._current_buffer_size = new_size as usize;
    }

    /// Records `bytes` read but dropped, and adjusts the buffer size.
    fn update_history_unused(&mut self, bytes: u64) {
        if self._options.dynamic_adjustments.is_none() {
            return;
        }
        self.update_history(bytes, bytes);
        self.set_new_buffer_size(AfterSkip::Yes);
    }

    /// Drops a read's future safely even if it is not resolved yet: it is chained into
    /// `_dropped_reads`, which close() waits for.
    fn ignore_read_future(&mut self, mut read_future: Future<TemporaryBuffer<u8>>) {
        if read_future.available() {
            read_future.ignore_ready_future();
            return;
        }
        let f =
            read_future.then_wrapped(|mut f: Future<TemporaryBuffer<u8>>| f.ignore_ready_future());
        let dropped = std::mem::replace(&mut self._dropped_reads, make_ready_future(()));
        self._dropped_reads = dropped.then(move |()| f);
    }

    /// A source reading `len` bytes of `f` from `offset`.
    fn new(f: File, offset: u64, len: u64, options: FileInputStreamOptions) -> Self {
        let current_read_ahead = Self::get_initial_read_ahead(&options);
        let mut me = FileDataSourceImpl {
            _this: std::ptr::null_mut(),
            _pinned: std::marker::PhantomPinned,
            _reactor: engine(),
            _file: f,
            _options: options,
            _pos: offset,
            _remain: len,
            _read_buffers: CircularBuffer::new(),
            _reads_in_progress: 0,
            _current_read_ahead: current_read_ahead,
            _dropped_reads: make_ready_future(()),
            _done: None,
            _current_buffer_size: 0,
            _in_slow_start: false,
            _intent: IoIntent::new(),
        };
        me._options.buffer_size = select_buffer_size(
            me._options.buffer_size as u64,
            me._file.disk_read_max_length() as u64,
        ) as usize;
        me._current_buffer_size = me._options.buffer_size;
        // Prevent wraparound of `_pos + _remain`.
        me.set_new_buffer_size(AfterSkip::No);
        me._remain = (u64::MAX - me._pos).min(me._remain);
        me
    }

    /// The reactor, as a pointer: its statistics are reached as places of it.
    #[inline]
    fn reactor(&self) -> *mut Reactor {
        self._reactor
    }

    /// The root pointer of the source's node (see the type).
    #[inline]
    fn this_ptr(&self) -> *mut Self {
        debug_assert!(!self._this.is_null());
        self._this
    }

    /// The source's I/O intent, as a place of the root pointer; the source never moves (see
    /// the type).
    #[inline]
    fn intent_ptr(&self) -> *mut IoIntent {
        // SAFETY: the root pointer of the live source; no reference is formed.
        unsafe { std::ptr::addr_of_mut!((*self.this_ptr())._intent) }
    }

    /// Issues reads until `read_ahead + additional` are queued or the range is exhausted.
    fn issue_read_aheads(&mut self, additional: u32) {
        if self._done.is_some() {
            return;
        }
        let ra = (self._current_read_ahead + additional) as usize;
        self._read_buffers.reserve(ra); // prevent push_back() failure
        while self._read_buffers.size() < ra {
            if self._remain == 0 {
                if self._read_buffers.size() >= additional as usize {
                    return;
                }
                self._read_buffers.push_back(IssuedRead::new(
                    self._pos,
                    0,
                    make_ready_future(TemporaryBuffer::default()),
                ));
                continue;
            }
            self._reads_in_progress += 1;
            // If `_pos` is not DMA-aligned the read is short; account for that, and do not
            // read beyond `_remain`.
            let align: u64 = self._file.disk_read_dma_alignment();
            let start = align_down(self._pos, align);
            let end = align_up(start + self._current_buffer_size as u64, align)
                .min(self._pos + self._remain);
            let len = end - start;
            let actual_size = (end - self._pos).min(self._remain);
            let intent = self.intent_ptr();
            let pc = get_io_priority(&self._options);
            let this = self.this_ptr();
            let (pos, remain) = (self._pos, self._remain);
            // SAFETY: `intent` is the source's own intent, which does not move (the source is
            // boxed) and outlives its reads (close() waits for them).
            let fut = unsafe { self._file.dma_read_bulk_impl(start, len as usize, pc, intent) }
                .then_wrapped(
                    move |ret: Future<TemporaryBuffer<u8>>| -> Future<TemporaryBuffer<u8>> {
                        // SAFETY: close() waits for every read in progress before the source dies.
                        let me = unsafe { &mut *this };
                        me._reads_in_progress -= 1;
                        if me._reads_in_progress == 0
                            && let Some(d) = me._done.as_mut()
                        {
                            d.set_value(());
                        }
                        // A failed read is handed on as is.
                        match ret.get() {
                            Err(e) => make_exception_future(e),
                            Ok(mut tmp) => {
                                // The first or last buffer needs trimming.
                                let real_end = start + tmp.size() as u64;
                                if real_end <= pos {
                                    return make_ready_future(TemporaryBuffer::default());
                                }
                                if real_end > pos + remain {
                                    tmp.trim((pos + remain - start) as usize);
                                }
                                if start < pos {
                                    tmp.trim_front((pos - start) as usize);
                                }
                                make_ready_future(tmp)
                            }
                        }
                    },
                );
            self._read_buffers.push_back(IssuedRead::new(self._pos, actual_size, fut));
            self._remain -= end - self._pos;
            self._pos = end;
        }
    }
}

impl Drop for FileDataSourceImpl {
    /// Asserts that the source was closed.
    fn drop(&mut self) {
        // A source dropped without close() could have reads in progress that would access
        // freed memory.
        engine_assert!(self._reads_in_progress == 0);
    }
}

impl DataSourceImpl for FileDataSourceImpl {
    /// Returns the next buffer, issuing read-aheads; empty at the end of the range.
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        if !self._read_buffers.empty() && !self._read_buffers.front()._ready.available() {
            self.try_increase_read_ahead();
        }
        self.issue_read_aheads(1);
        let Some(ret) = self._read_buffers.take_front() else {
            // Only after close(): issue_read_aheads() adds nothing once `_done` is set.
            return make_exception_future(crate::engine::core::future::ExceptionPtr::new(
                crate::engine::core::future::LogicError::new(
                    "file_data_source: get() after close()".to_string(),
                ),
            ));
        };
        self.update_history_consumed(ret._size);
        let r = self.reactor();
        // SAFETY: the reactor of this shard outlives every data source made on it; places of it.
        unsafe {
            (*r)._io_stats.fstream_reads += 1;
            (*r)._io_stats.fstream_read_bytes += ret._size;
        }
        if !ret._ready.available() {
            // SAFETY: as above.
            unsafe {
                (*r)._io_stats.fstream_reads_blocked += 1;
                (*r)._io_stats.fstream_read_bytes_blocked += ret._size;
            }
        }
        ret._ready
    }

    /// Skips `n` bytes, dropping read-aheads that fall inside the skip and moving the read
    /// position past it; resolves with an empty buffer.
    fn skip(&mut self, mut n: u64) -> Future<TemporaryBuffer<u8>> {
        let mut dropped: u64 = 0;
        while n != 0 {
            if self._read_buffers.empty() {
                engine_assert!(n <= self._remain);
                self._pos += n;
                self._remain -= n;
                break;
            }
            let front = self._read_buffers.front_mut();
            if n < front._size {
                front._size -= n;
                front._pos += n;
                let ready = std::mem::replace(
                    &mut front._ready,
                    make_ready_future(TemporaryBuffer::default()),
                );
                front._ready = ready.then(move |mut buf: TemporaryBuffer<u8>| {
                    buf.trim_front(n as usize);
                    make_ready_future(buf)
                });
                break;
            } else {
                let ready = std::mem::replace(
                    &mut front._ready,
                    make_ready_future(TemporaryBuffer::default()),
                );
                let size = front._size;
                self.ignore_read_future(ready);
                n -= size;
                dropped += size;
                let r = self.reactor();
                // SAFETY: the reactor of this shard outlives every data source made on it; places of it.
                unsafe {
                    (*r)._io_stats.fstream_read_aheads_discarded += 1;
                    (*r)._io_stats.fstream_read_ahead_discarded_bytes += size;
                }
                self._read_buffers.pop_front();
            }
        }
        self.update_history_unused(dropped);
        make_ready_future(TemporaryBuffer::default())
    }

    /// Stops issuing reads, cancels the reads in flight through the source's I/O intent, and
    /// waits for them and for the dropped reads.
    fn close(&mut self) -> Future<()> {
        let reads_in_progress = self._reads_in_progress;
        let done = self._done.insert(Promise::new());
        if reads_in_progress == 0 {
            done.set_value(());
        }
        let fut = done.get_future();
        // SAFETY: the intent is pinned inside the boxed source.
        unsafe { Pin::new_unchecked(&mut self._intent) }.cancel();
        let this = self.this_ptr();
        fut.then(move |()| {
            // SAFETY: the caller keeps the source alive until close() resolves.
            let me = unsafe { &mut *this };
            let mut dropped: u64 = 0;
            while let Some(c) = me._read_buffers.take_front() {
                let r = me.reactor();
                // SAFETY: the reactor of this shard outlives every data source made on it; places of it.
                unsafe {
                    (*r)._io_stats.fstream_read_aheads_discarded += 1;
                    (*r)._io_stats.fstream_read_ahead_discarded_bytes += c._size;
                }
                dropped += c._size;
                me.ignore_read_future(c._ready);
            }
            me.update_history_unused(dropped);
            std::mem::replace(&mut me._dropped_reads, make_ready_future(()))
        })
    }
}

/// Creates a data source reading the `len` bytes of `f` that start at `offset`.
pub fn make_file_data_source(
    f: File,
    offset: u64,
    len: u64,
    opt: FileInputStreamOptions,
) -> DataSource {
    let node =
        crate::engine::net::node_ptr::NodePtr::new(FileDataSourceImpl::new(f, offset, len, opt));
    let p = node.into_raw();
    // SAFETY: `p` is the fresh node's pointer, with no reference to it live; it goes back to
    // a node (as the trait object) at once.
    unsafe {
        (*p)._this = p;
        DataSource::from_node(crate::engine::net::node_ptr::NodePtr::from_raw(
            p as *mut dyn DataSourceImpl,
        ))
    }
}

/// Creates a data source reading the whole file from start to end.
pub fn make_file_data_source_whole(f: File, opt: FileInputStreamOptions) -> DataSource {
    make_file_data_source(f, 0, u64::MAX, opt)
}

/// Creates an input stream reading a portion of a file.
///
/// `offset` has no alignment restriction; `len` is the maximum number of bytes to read,
/// and the stream stops at end of file even if `offset + len` lies beyond it.
///
/// Multiple input streams, opened from any continuations, may exist concurrently for the
/// same file.
pub fn make_file_input_stream_range(
    f: File,
    offset: u64,
    len: u64,
    options: FileInputStreamOptions,
) -> InputStream<u8> {
    InputStream::new(make_file_data_source(f, offset, len, options))
}

/// Creates an input stream reading `f` from `offset` (no alignment restriction) to its end.
///
/// Multiple input streams, opened from any continuations, may exist concurrently for the
/// same file.
pub fn make_file_input_stream_from(
    f: File,
    offset: u64,
    options: FileInputStreamOptions,
) -> InputStream<u8> {
    make_file_input_stream_range(f, offset, u64::MAX, options)
}

/// Creates an input stream reading the whole of `f`.
///
/// Multiple input streams, opened from any continuations, may exist concurrently for the
/// same file.
pub fn make_file_input_stream(f: File, options: FileInputStreamOptions) -> InputStream<u8> {
    make_file_input_stream_from(f, 0, options)
}

/// The data sink of a file output stream: writes aligned buffers sequentially from
/// position zero, with up to `write_behind` writes in flight.
///
/// It is only made by [`make_file_data_sink`], which places it in the node of its
/// [`DataSink`] and records the root pointer in `_this`,
/// which its continuations keep; `!Unpin` for the reason given at `FileDataSourceImpl`.
struct FileDataSinkImpl {
    /// The root pointer of the node the sink lives in.
    _this: *mut FileDataSinkImpl,
    _pinned: std::marker::PhantomPinned,
    _file: File,
    _options: FileOutputStreamOptions,
    _pos: u64,
    _write_behind_sem: Semaphore,
    _background_writes_done: Future<()>,
    _failed: bool,
}

impl FileDataSinkImpl {
    /// A sink writing `f` with `options`.
    fn new(f: File, options: FileOutputStreamOptions) -> Self {
        let mut me = FileDataSinkImpl {
            _this: std::ptr::null_mut(),
            _pinned: std::marker::PhantomPinned,
            _file: f,
            _options: options,
            _pos: 0,
            _write_behind_sem: Semaphore::new(options.write_behind as usize),
            _background_writes_done: make_ready_future(()),
            _failed: false,
        };
        me._options.buffer_size = select_buffer_size(
            me._options.buffer_size as u64,
            me._file.disk_write_max_length() as u64,
        ) as u32;
        if me._options.write_behind != 0 {
            me._write_behind_sem.ensure_space_for_waiters(1); // So that wait() doesn't throw
        }
        me
    }

    /// The root pointer of the sink's node (see the type).
    #[inline]
    fn this_ptr(&self) -> *mut Self {
        debug_assert!(!self._this.is_null());
        self._this
    }

    /// Writes `buf` at `pos`, padding an unaligned tail into an aligned copy and finishing
    /// short writes; after an unaligned tail the file is truncated to its real length.
    fn do_put(&mut self, pos: u64, mut buf: TemporaryBuffer<u8>) -> Future<()> {
        // put() is normally called with chunks that are a multiple of the DMA alignment;
        // only the last one may have an unaligned length. An unaligned `pos` means the
        // caller wrote after an unaligned chunk, which is a bug.
        engine_assert!((pos & (self._file.disk_write_dma_alignment() - 1)) == 0);
        let mut truncate = false;
        let mut p = buf.get();
        let mut buf_size = buf.size();

        if (buf.size() as u64 & (self._file.disk_write_dma_alignment() - 1)) != 0 {
            // If the size is not aligned, copy the content into a new aligned buffer. This
            // only happens when the user flushes the output stream.
            let mut tmp = self
                .allocate_buffer(
                    align_up(buf.size() as u64, self._file.disk_write_dma_alignment()) as usize,
                );
            // SAFETY: `tmp` holds at least `buf.size()` bytes; the rest of it is zeroed.
            unsafe {
                std::ptr::copy_nonoverlapping(buf.get(), tmp.get_write(), buf.size());
                std::ptr::write_bytes(tmp.get_write().add(buf.size()), 0, tmp.size() - buf.size());
            }
            buf = tmp;
            p = buf.get();
            buf_size = buf.size();
            truncate = true;
        }

        let this = self.this_ptr();
        let pc = get_io_priority(&self._options);
        // SAFETY: `p` holds `buf_size` bytes of `buf`, which the continuation keeps until the
        // write completes; no io intent.
        unsafe { self._file.dma_write_impl_ptr(pos, p, buf_size, pc, std::ptr::null_mut()) }.then(
            move |size: usize| -> Future<()> {
                // SAFETY: the sink outlives its writes (close() waits for them).
                let me = unsafe { &mut *this };
                // Handle short writes.
                if size < buf_size {
                    buf.trim_front(size);
                    return me.do_put(pos + size as u64, buf).then(move |()| {
                        // SAFETY: as above.
                        let me = unsafe { &mut *this };
                        if truncate {
                            let p = me._pos;
                            return me._file.truncate(p);
                        }
                        make_ready_future(())
                    });
                }
                if truncate {
                    let p = me._pos;
                    return me._file.truncate(p);
                }
                make_ready_future(())
            },
        )
    }

    /// Waits for the writes in flight and returns their merged result, leaving a pristine
    /// state so that flush and close can each be called, or both.
    fn wait(&mut self) -> Future<()> {
        let this = self.this_ptr();
        // Restore the pristine state for a flush() + close() sequence (either, or both,
        // may be called).
        self._write_behind_sem
            .wait(self._options.write_behind as usize)
            .then(move |()| {
                // SAFETY: the sink outlives its flush and close.
                let me = unsafe { &mut *this };
                std::mem::replace(&mut me._background_writes_done, make_ready_future(()))
            })
            .finally(move || {
                // SAFETY: as above.
                let me = unsafe { &mut *this };
                me._write_behind_sem.signal(me._options.write_behind as usize);
            })
    }
}

impl DataSinkImpl for FileDataSinkImpl {
    /// Packets are not supported by the file sink: aborts the process.
    fn put(&mut self, _data: Packet) -> Future<()> {
        std::process::abort();
    }

    /// Allocates a buffer aligned for DMA to the file.
    fn allocate_buffer(&mut self, size: usize) -> TemporaryBuffer<u8> {
        TemporaryBuffer::<u8>::aligned(self._file.memory_dma_alignment() as usize, size)
    }

    /// Writes `buf` at the current position. With `write_behind > 0` the write runs in the
    /// background, limited to `write_behind` in flight, and a failure is reported by a later
    /// put, flush or close; with 0 it is written directly.
    fn put_buffer(&mut self, buf: TemporaryBuffer<u8>) -> Future<()> {
        let pos = self._pos;
        self._pos += buf.size() as u64;
        if self._options.write_behind == 0 {
            return self.do_put(pos, buf);
        }
        // Write-behind strategy:
        //
        // 1. Issue N writes in parallel, using a semaphore to limit them to N.
        // 2. Collect the results in `_background_writes_done`, merging failed futures.
        // 3. Once a failure has been seen, issue no more writes.
        let this = self.this_ptr();
        self._write_behind_sem.wait(1).then(move |()| -> Future<()> {
            // SAFETY: the sink outlives its writes (close() waits for them).
            let me = unsafe { &mut *this };
            if me._failed {
                me._write_behind_sem.signal(1);
                return std::mem::replace(&mut me._background_writes_done, make_ready_future(()));
            }
            let this_write_done = me.do_put(pos, buf).finally(move || {
                // SAFETY: as above.
                unsafe { (*this)._write_behind_sem.signal(1) };
            });
            let prev = std::mem::replace(&mut me._background_writes_done, make_ready_future(()));
            me._background_writes_done = when_all((prev, this_write_done)).then(
                move |possible_errors: (Future<()>, Future<()>)| -> Future<()> {
                    // Merge the two errors, keeping the first.
                    let (e1, mut e2) = possible_errors;
                    if e1.failed() {
                        e2.ignore_ready_future();
                        e1
                    } else {
                        if e2.failed() {
                            // SAFETY: as above.
                            unsafe { (*this)._failed = true };
                        }
                        e2
                    }
                },
            );
            make_ready_future(())
        })
    }

    /// Waits for the writes in flight, then flushes the file.
    fn flush(&mut self) -> Future<()> {
        let this = self.this_ptr();
        self.wait().then(move |()| {
            // SAFETY: the sink outlives its flush.
            unsafe { (*this)._file.flush() }
        })
    }

    /// Waits for the writes in flight, then closes the file.
    fn close(&mut self) -> Future<()> {
        let this = self.this_ptr();
        self.wait().finally(move || {
            // SAFETY: the sink outlives its close.
            unsafe { (*this)._file.close() }
        })
    }

    /// The configured buffer size.
    fn buffer_size(&self) -> usize {
        self._options.buffer_size as usize
    }
}

/// Creates a data sink writing a newly created file from position zero.
///
/// The future is always ready: making the sink cannot fail (allocation failure aborts).
pub fn make_file_data_sink(f: File, options: FileOutputStreamOptions) -> Future<DataSink> {
    let node = crate::engine::net::node_ptr::NodePtr::new(FileDataSinkImpl::new(f, options));
    let p = node.into_raw();
    // SAFETY: as in make_file_data_source.
    let sink = unsafe {
        (*p)._this = p;
        DataSink::from_node(crate::engine::net::node_ptr::NodePtr::from_raw(
            p as *mut dyn DataSinkImpl,
        ))
    };
    make_ready_future(sink)
}

/// Creates an output stream writing a newly created file from position zero, with a
/// buffer of `buffer_size` bytes and default options otherwise.
///
/// Call `flush()` last on a file output stream, then `close()`. The future is always
/// ready: making the sink cannot fail.
pub fn make_file_output_stream_with_size(f: File, buffer_size: u64) -> Future<OutputStream<u8>> {
    let options = FileOutputStreamOptions { buffer_size: buffer_size as u32, ..Default::default() };
    make_file_output_stream(f, options)
}

/// Creates an output stream writing a newly created file from position zero.
///
/// Call `flush()` last on a file output stream, then `close()`. The future is always
/// ready: making the sink cannot fail.
pub fn make_file_output_stream(
    f: File,
    options: FileOutputStreamOptions,
) -> Future<OutputStream<u8>> {
    make_file_data_sink(f, options).then(|ds: DataSink| make_ready_future(OutputStream::new(ds)))
}
