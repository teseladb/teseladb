//! POSIX file implementations behind [`File`](crate::engine::core::file::File).
//!
//! [`PosixFileImpl`] holds the state every POSIX-backed file shares: the descriptor, its
//! cross-shard reference count, the DMA alignments and the shard's I/O queue. Each
//! concrete type holds a `PosixFileImpl` as `base` and implements [`FileImpl`]:
//!
//! - [`PosixFileReal`]: a regular file on a filesystem that handles concurrent appends;
//! - [`AppendChallengedPosixFileImpl`]: a regular file on a filesystem (such as XFS) where
//!   size-changing writes block other I/O, so operations are queued and serialized as
//!   needed;
//! - [`BlockdevFileImpl`]: a block device.
//!
//! [`PosixFileHandleImpl`] is the sendable handle that recreates a file on another shard.
//! The longer method bodies are in [`crate::engine::core::file`]. Internal reads that the
//! base issues through `read_dma` (for bulk reads) are dispatched statically to the
//! concrete type.

use std::cell::{Cell, UnsafeCell};
use std::collections::VecDeque;
use std::ffi::c_void;
use std::pin::Pin;
use std::sync::atomic::AtomicU32;

use crate::engine::core::file::internal::FsInfo;
use crate::engine::core::file::{
    DirectoryEntry, FileHandleImpl, FileImplBase, FileOpenOptions, OpenFlags,
};
use crate::engine::core::future::{Future, Futurize, Promise, futurize_invoke};
use crate::engine::core::internal::io_intent::IntentReference;
use crate::engine::core::io_intent::IoIntent;
use crate::engine::core::io_priority_class::internal::MaybePriorityClassRef;
use crate::engine::core::io_queue::IoQueue;
use crate::engine::core::posix::Stat;
use crate::engine::core::reactor_backend::Iovec;
use crate::engine::core::shared_ptr::EnableSharedFromThis;
use crate::engine::core::stream::{NextFn, Subscription};
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::coroutine::generator::experimental::Generator;

pub use crate::engine::core::file::FileImpl;

/// A handle to a POSIX file that can be sent to another shard and turned back into a file
/// there, sharing the descriptor and its reference count.
pub struct PosixFileHandleImpl {
    pub(crate) _fd: i32,
    pub(crate) _refcount: *mut AtomicU32,
    pub(crate) _device_id: u64,
    pub(crate) _open_flags: OpenFlags,
    pub(crate) _memory_dma_alignment: u32,
    pub(crate) _disk_read_dma_alignment: u32,
    pub(crate) _disk_write_dma_alignment: u32,
    pub(crate) _disk_overwrite_dma_alignment: u32,
    pub(crate) _nowait_works: bool,
}

// SAFETY: the handle is moved to another shard whole; its descriptor is a number and its
// reference count is an atomic shared by every handle and file of the descriptor, freed by
// the last of them.
unsafe impl Send for PosixFileHandleImpl {}

impl PosixFileHandleImpl {
    /// A handle for descriptor `fd`, sharing `refcount`, with the file's device and DMA
    /// parameters.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn new(
        fd: i32,
        f: OpenFlags,
        refcount: *mut AtomicU32,
        device_id: u64,
        memory_dma_alignment: u32,
        disk_read_dma_alignment: u32,
        disk_write_dma_alignment: u32,
        disk_overwrite_dma_alignment: u32,
        nowait_works: bool,
    ) -> Self {
        PosixFileHandleImpl {
            _fd: fd,
            _refcount: refcount,
            _device_id: device_id,
            _open_flags: f,
            _memory_dma_alignment: memory_dma_alignment,
            _disk_read_dma_alignment: disk_read_dma_alignment,
            _disk_write_dma_alignment: disk_write_dma_alignment,
            _disk_overwrite_dma_alignment: disk_overwrite_dma_alignment,
            _nowait_works: nowait_works,
        }
    }
    // The handle is neither `Clone` nor `Copy`; its drop, `to_file` and `clone` are in
    // file.rs.
}

/// The state shared by the POSIX file implementations.
#[repr(C)]
///
/// Every [`FileImpl`] operation takes `&self` (copies of a file share the implementation,
/// and the operations re-enter it), so the members that change after construction are cells.
pub struct PosixFileImpl {
    /// Alignments and maximum request lengths.
    pub(crate) _base: FileImplBase,
    pub(crate) _refcount: Cell<*mut AtomicU32>,
    pub(crate) _nowait_works: bool,
    pub(crate) _device_id: u64,
    pub(crate) _io_queue: *mut IoQueue,
    pub(crate) _open_flags: OpenFlags,
    pub(crate) _fd: Cell<i32>,
}

impl PosixFileImpl {
    /// The flags the file was opened with.
    #[inline]
    pub fn flags(&self) -> OpenFlags {
        self._open_flags
    }

    // The constructors, the drop, the `FileImpl` operations, `configure_dma_alignment`,
    // `configure_io_lengths`, `do_write_dma`, `do_read_dma`, `do_dma_read_bulk` and
    // `read_maybe_eof` are in file.rs.
}

/// The [`FileImpl`] operations that every concrete POSIX file forwards unchanged to its
/// `base`.
macro_rules! posix_file_impl_common {
    () => {
        #[inline]
        fn file_impl_base(&self) -> &FileImplBase {
            &self.base._base
        }
        #[inline]
        unsafe fn ioctl(&self, cmd: u64, argp: *mut c_void) -> Future<i32> {
            self.base.ioctl(cmd, argp)
        }
        #[inline]
        unsafe fn ioctl_short(&self, cmd: u64, argp: *mut c_void) -> Future<i32> {
            self.base.ioctl_short(cmd, argp)
        }
        #[inline]
        unsafe fn fcntl(&self, op: i32, arg: usize) -> Future<i32> {
            self.base.fcntl(op, arg)
        }
        #[inline]
        unsafe fn fcntl_short(&self, op: i32, arg: usize) -> Future<i32> {
            self.base.fcntl_short(op, arg)
        }
        #[inline]
        fn dup(
            &self,
        ) -> Result<Box<dyn FileHandleImpl>, crate::engine::core::future::ExceptionPtr> {
            self.base.dup()
        }
        #[inline]
        fn list_directory(&self, next: NextFn<DirectoryEntry>) -> Subscription<DirectoryEntry> {
            self.base.list_directory(next)
        }
        #[inline]
        fn experimental_list_directory(&self) -> Generator<DirectoryEntry> {
            self.base.experimental_list_directory()
        }
    };
}

/// A regular file on a filesystem that handles concurrent appending writes.
pub struct PosixFileReal {
    /// The shared POSIX file state.
    pub(crate) base: PosixFileImpl,
}

impl PosixFileReal {
    /// A file over the newly opened descriptor `fd`, configured from `options` and the
    /// filesystem information `fsi`.
    #[inline]
    pub fn new(
        fd: i32,
        of: OpenFlags,
        options: FileOpenOptions,
        fsi: &FsInfo,
        device_id: u64,
    ) -> Self {
        PosixFileReal { base: PosixFileImpl::with_fs_info(fd, of, options, device_id, fsi) }
    }

    /// A file over a descriptor shared through a handle: it reuses `refcount` and the
    /// given DMA parameters.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub fn from_dup(
        fd: i32,
        of: OpenFlags,
        refcount: *mut AtomicU32,
        device_id: u64,
        memory_dma_alignment: u32,
        disk_read_dma_alignment: u32,
        disk_write_dma_alignment: u32,
        disk_overwrite_dma_alignment: u32,
        nowait_works: bool,
    ) -> Self {
        PosixFileReal {
            base: PosixFileImpl::from_dup(
                fd,
                of,
                refcount,
                device_id,
                memory_dma_alignment,
                disk_read_dma_alignment,
                disk_write_dma_alignment,
                disk_overwrite_dma_alignment,
                nowait_works,
            ),
        }
    }
}

impl FileImpl for PosixFileReal {
    posix_file_impl_common!();

    #[inline]
    unsafe fn read_dma(
        &self,
        pos: u64,
        buffer: *mut u8,
        len: usize,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.read_dma_pc(pos, buffer, len, MaybePriorityClassRef, intent)
    }
    #[inline]
    unsafe fn read_dma_iov(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.read_dma_iov_pc(pos, iov, MaybePriorityClassRef, intent)
    }
    #[inline]
    unsafe fn write_dma(
        &self,
        pos: u64,
        buffer: *const u8,
        len: usize,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.write_dma_pc(pos, buffer, len, MaybePriorityClassRef, intent)
    }
    #[inline]
    unsafe fn write_dma_iov(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.write_dma_iov_pc(pos, iov, MaybePriorityClassRef, intent)
    }
    #[inline]
    unsafe fn dma_read_bulk(
        &self,
        offset: u64,
        range_size: usize,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        self.dma_read_bulk_pc(offset, range_size, MaybePriorityClassRef, intent)
    }

    #[inline]
    fn flush(&self) -> Future<()> {
        self.base.flush()
    }
    #[inline]
    fn stat(&self) -> Future<Stat> {
        self.base.stat()
    }
    #[inline]
    fn truncate(&self, length: u64) -> Future<()> {
        self.base.truncate(length)
    }
    #[inline]
    fn discard(&self, offset: u64, length: u64) -> Future<()> {
        self.base.discard(offset, length)
    }
    #[inline]
    fn allocate(&self, position: u64, length: u64) -> Future<()> {
        self.base.allocate(position, length)
    }
    #[inline]
    fn size(&self) -> Future<u64> {
        self.base.size()
    }
    // close() never fails: it reports errors and swallows them. Call flush() first when
    // stable storage semantics matter.
    #[inline]
    fn close(&self) -> Future<()> {
        self.base.close()
    }
}

// XFS on Linux is challenged with appends: a write that changes the end of file is blocked
// by any other concurrent AIO operation on the same file, whether it changes the size or
// not. ftruncate() also blocks and is blocked by AIO, so calls to it must be placed very
// carefully. Other Linux filesystems may have different locking rules.

/// The kind of a pending operation of an append-challenged file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opcode {
    /// No operation (placeholder).
    Invalid,
    /// A read.
    Read,
    /// A write.
    Write,
    /// A truncation.
    Truncate,
    /// A flush.
    Flush,
    /// A space allocation.
    Allocate,
}

/// The body of a queued operation, run once.
pub trait OpRun {
    /// Runs the operation and gives the future of its completion.
    fn run(self: Box<Self>) -> Future<()>;
}

/// A queued operation built by `enqueue`: the closure and the intent reference it uses,
/// linked in place inside the box (so the reference allocates nothing).
struct OpFunc<F> {
    iref: IntentReference,
    func: Option<F>,
}

impl<F: FnOnce(&IntentReference) -> Future<()> + 'static> OpRun for OpFunc<F> {
    #[inline]
    fn run(mut self: Box<Self>) -> Future<()> {
        match self.func.take() {
            Some(f) => f(&self.iref),
            None => unreachable!("an append_challenged operation runs once"),
        }
    }
}

/// A pending operation of an append-challenged file.
pub struct Op {
    /// The kind of operation.
    pub type_: Opcode,
    /// Its position in the file.
    pub pos: u64,
    /// Its length.
    pub len: usize,
    /// The operation itself.
    pub run: Box<dyn OpRun>,
}

/// The closing state of an append-challenged file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Open for operations.
    Open,
    /// Closing: waiting for the queued operations to finish.
    Draining,
    /// Closing the descriptor.
    Closing,
    /// Closed.
    Closed,
}

/// A regular file on a filesystem where size-changing writes block other I/O (see the
/// note on [`Opcode`]).
///
/// Operations are queued and dispatched so that size-changing ones run alone or within a
/// limit, writes past the end of file are tracked as a logical size, and the file is
/// truncated to that size when needed. It is shared (`EnableSharedFromThis`) so queued
/// continuations keep it alive.
pub struct AppendChallengedPosixFileImpl {
    /// The shared POSIX file state.
    pub(crate) base: PosixFileImpl,
    pub(crate) _esft: EnableSharedFromThis,
    // File size as a result of completed kernel operations (writes and truncates)
    pub(crate) _committed_size: Cell<u64>,
    // File size as a result of the file API calls
    pub(crate) _logical_size: Cell<u64>,
    // Offset from which the file has no blocks as far as this file knows: its size when
    // opened, raised by completed writes and allocations, lowered by truncations. A write
    // at or past it allocates, which RWF_NOWAIT refuses on every append-challenged
    // filesystem, so it goes to the syscall thread at once. An offset too low only costs
    // that hop for a write the reactor could have submitted; too high, a failed attempt.
    pub(crate) _data_end: Cell<u64>,
    // Queue of pending operations; processed from front to back to avoid starvation, but
    // can issue concurrent operations.
    //
    // The queue is reached only for short accesses that do not run an operation (see
    // `process_queue`), so a cell suffices.
    pub(crate) _q: UnsafeCell<VecDeque<Op>>,
    pub(crate) _max_size_changing_ops: u32,
    pub(crate) _current_non_size_changing_ops: Cell<u32>,
    pub(crate) _current_size_changing_ops: Cell<u32>,
    pub(crate) _fsync_is_exclusive: bool,

    pub(crate) _closing_state: Cell<State>,

    pub(crate) _sloppy_size: bool,
    pub(crate) _sloppy_size_hint: u64,
    // Fulfilled when closing and all I/O is complete.
    pub(crate) _completed: UnsafeCell<Promise<()>>,
}

crate::shared_from_this_impl!(AppendChallengedPosixFileImpl { _esft });

impl AppendChallengedPosixFileImpl {
    /// Queues an operation of kind `type_` on `len` bytes at `pos`; `func` runs when the
    /// queue dispatches it, and the returned future resolves with its result.
    ///
    /// An intent reference is built from `intent` and handed to `func` when it runs, so a
    /// cancelled intent can be observed by the operation.
    #[inline]
    pub(crate) fn enqueue<T: 'static, Func, R>(
        &self,
        type_: Opcode,
        pos: u64,
        len: usize,
        intent: *mut IoIntent,
        func: Func,
    ) -> Future<T>
    where
        Func: FnOnce(&IntentReference) -> R + 'static,
        R: Futurize<Output = T>,
    {
        let mut pr = Promise::<T>::new();
        let fut = pr.get_future();
        let op_func = move |iref: &IntentReference| -> Future<()> {
            futurize_invoke(move || func(iref)).then_wrapped(move |f: Future<T>| {
                f.forward_to(pr);
            })
        };
        let mut run = Box::new(OpFunc { iref: IntentReference::new(intent), func: Some(op_func) });
        // SAFETY: the box gives the reference its final address; it is never moved out.
        unsafe { Pin::new_unchecked(&mut run.iref) }.bind();
        self.enqueue_op(Op { type_, pos, len, run });
        fut
    }

    // The queue logic (`commit_size`, `must_run_alone`, `appending_write`, `size_changing`,
    // `may_dispatch`, `dispatch`, `optimize_queue`, `process_queue`, `may_quit`,
    // `enqueue_op`, `truncate_sync`, `truncate_to_logical_size`), the constructor, the drop
    // and the `FileImpl` operations are in file.rs.
}

impl FileImpl for AppendChallengedPosixFileImpl {
    posix_file_impl_common!();

    #[inline]
    unsafe fn read_dma(
        &self,
        pos: u64,
        buffer: *mut u8,
        len: usize,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.read_dma_pc(pos, buffer, len, MaybePriorityClassRef, intent)
    }
    #[inline]
    unsafe fn read_dma_iov(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.read_dma_iov_pc(pos, iov, MaybePriorityClassRef, intent)
    }
    #[inline]
    unsafe fn write_dma(
        &self,
        pos: u64,
        buffer: *const u8,
        len: usize,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.write_dma_pc(pos, buffer, len, MaybePriorityClassRef, intent)
    }
    #[inline]
    unsafe fn write_dma_iov(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.write_dma_iov_pc(pos, iov, MaybePriorityClassRef, intent)
    }
    #[inline]
    unsafe fn dma_read_bulk(
        &self,
        offset: u64,
        range_size: usize,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        self.dma_read_bulk_pc(offset, range_size, MaybePriorityClassRef, intent)
    }

    #[inline]
    fn flush(&self) -> Future<()> {
        self.flush_impl()
    }
    #[inline]
    fn stat(&self) -> Future<Stat> {
        self.stat_impl()
    }
    #[inline]
    fn truncate(&self, length: u64) -> Future<()> {
        self.truncate_impl(length)
    }
    #[inline]
    fn discard(&self, offset: u64, length: u64) -> Future<()> {
        self.base.discard(offset, length)
    }
    #[inline]
    fn size(&self) -> Future<u64> {
        self.size_impl()
    }
    #[inline]
    fn allocate(&self, position: u64, length: u64) -> Future<()> {
        self.allocate_impl(position, length)
    }
    #[inline]
    fn close(&self) -> Future<()> {
        self.close_impl()
    }
}

/// A block device opened as a file.
pub struct BlockdevFileImpl {
    /// The shared POSIX file state.
    pub(crate) base: PosixFileImpl,
}

// The constructor and the `FileImpl` operations are in file.rs.

impl FileImpl for BlockdevFileImpl {
    posix_file_impl_common!();

    #[inline]
    fn truncate(&self, length: u64) -> Future<()> {
        self.truncate_impl(length)
    }
    #[inline]
    fn discard(&self, offset: u64, length: u64) -> Future<()> {
        self.discard_impl(offset, length)
    }
    #[inline]
    fn size(&self) -> Future<u64> {
        self.size_impl()
    }
    #[inline]
    fn allocate(&self, position: u64, length: u64) -> Future<()> {
        self.allocate_impl(position, length)
    }
    #[inline]
    unsafe fn read_dma(
        &self,
        pos: u64,
        buffer: *mut u8,
        len: usize,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.read_dma_pc(pos, buffer, len, MaybePriorityClassRef, intent)
    }
    #[inline]
    unsafe fn read_dma_iov(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.read_dma_iov_pc(pos, iov, MaybePriorityClassRef, intent)
    }
    #[inline]
    unsafe fn write_dma(
        &self,
        pos: u64,
        buffer: *const u8,
        len: usize,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.write_dma_pc(pos, buffer, len, MaybePriorityClassRef, intent)
    }
    #[inline]
    unsafe fn write_dma_iov(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.write_dma_iov_pc(pos, iov, MaybePriorityClassRef, intent)
    }
    #[inline]
    unsafe fn dma_read_bulk(
        &self,
        offset: u64,
        range_size: usize,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        self.dma_read_bulk_pc(offset, range_size, MaybePriorityClassRef, intent)
    }

    #[inline]
    fn flush(&self) -> Future<()> {
        self.base.flush()
    }
    #[inline]
    fn stat(&self) -> Future<Stat> {
        self.base.stat()
    }
    #[inline]
    fn close(&self) -> Future<()> {
        self.base.close()
    }
}
