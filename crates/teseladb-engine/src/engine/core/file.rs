//! DMA files: the [`File`] handle, its implementation interface and the POSIX backends.
//!
//! A [`File`] is an uncached, unbuffered file accessed with direct I/O: reads and writes go
//! through the shard's I/O queue ([`crate::engine::core::io_queue`]) to the kernel, with
//! offsets, lengths and buffers aligned as the file reports
//! ([`File::disk_read_dma_alignment`] and friends). Copies of a `File` share one
//! implementation ([`FileImpl`]); [`File::dup`] gives a [`FileHandle`] that can be sent to
//! another shard. The module also provides the open functions, directory listing, the
//! `with_file` helpers, and the POSIX implementations declared in
//! [`crate::engine::core::file_impl`] (regular files, files on append-challenged
//! filesystems such as XFS, and block devices).
//!
//! Variants that differ only by arguments have distinct names: `dma_read` (into a
//! buffer), `dma_read_buf` (returning a buffer), `dma_read_iov`, `dma_write`,
//! `dma_write_iov`. The I/O intent argument is explicit: pass `std::ptr::null_mut()` for no
//! intent, or `pin!(IoIntent::new()).as_ptr()`.

use std::cell::{Cell, UnsafeCell};
use std::collections::{HashMap, VecDeque};
use std::ffi::{CStr, c_void};
use std::fmt;
use std::pin::Pin;
use std::ptr;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::SystemTime;

use crate::engine::core::align::align_up;
use crate::engine::core::do_with::do_with;
use crate::engine::core::file_impl::{
    AppendChallengedPosixFileImpl, BlockdevFileImpl, Op, Opcode, PosixFileHandleImpl,
    PosixFileImpl, PosixFileReal, State,
};
use crate::engine::core::future::{
    ExceptionPtr, Future, Futurize, FuturizeT, Promise, RuntimeError, SystemError, futurize_invoke,
    make_exception_future, make_ready_future,
};
use crate::engine::core::internal::read_state::FileReadState;
use crate::engine::core::internal::uname::kernel_uname;
use crate::engine::core::io_intent::IoIntent;
use crate::engine::core::io_priority_class::internal::MaybePriorityClassRef;
use crate::engine::core::io_queue::IoQueue;
use crate::engine::core::io_queue::internal::PriorityClass;
use crate::engine::core::loop_::do_until;
use crate::engine::core::posix::{Stat, Statfs, throw_system_error_on};
use crate::engine::core::queue::Queue;
use crate::engine::core::reactor::internal_iovecs::sanitize_iovecs;
use crate::engine::core::reactor::{engine, engine_logger, report_exception, yield_};
use crate::engine::core::reactor_backend::{Iovec, errno};
use crate::engine::core::shared_ptr::{
    SharedFromThis, SharedPtr, make_lw_shared, make_shared, make_shared_esft, static_pointer_cast,
};
use crate::engine::core::sstring::Sstring;
use crate::engine::core::stream::{NextFn, Stream, Subscription};
use crate::engine::core::syscall_result::{
    SyscallResult, SyscallResultExtra, wrap_syscall, wrap_syscall_extra,
};
use crate::engine::core::temporary_buffer::{ByteType, TemporaryBuffer};
use crate::engine::coroutine::generator::experimental::Generator;
use crate::engine::util::internal::iovec_utils::iovec_len;
use crate::engine::util::internal::magic::internal::FsMagic;
use crate::engine::util::log::FormatInfo;

pub use crate::engine::core::file_types::internal::linux_abi;
pub use crate::engine::core::file_types::{
    AccessFlags, DirectoryEntryType, FilePermissions, FsType, OpenFlags,
};

/// A directory entry being listed.
#[derive(Clone, Debug)]
pub struct DirectoryEntry {
    /// Name of the file in a directory entry. Never "." or ".."; only the last path
    /// component is included.
    pub name: Sstring,
    /// Type of the directory entry, if known.
    pub type_: Option<DirectoryEntryType>,
}

/// Group details from the system group database
#[derive(Clone, Debug, Default)]
pub struct GroupDetails {
    /// Name of the group.
    pub group_name: Sstring,
    /// Password of the group.
    pub group_passwd: Sstring,
    /// Numeric id of the group (`gid_t`).
    pub group_id: u32,
    /// Names of the members of the group.
    pub group_members: Vec<Sstring>,
}

/// Filesystem object stat information
#[derive(Clone, Copy, Debug)]
pub struct StatData {
    /// ID of device containing file
    pub device_id: u64,
    /// Inode number
    pub inode_number: u64,
    /// File type and mode
    pub mode: u64,
    /// Type of the object.
    pub type_: DirectoryEntryType,
    /// Number of hard links
    pub number_of_links: u64,
    /// User ID of owner
    pub uid: u64,
    /// Group ID of owner
    pub gid: u64,
    /// Device ID (if special file)
    pub rdev: u64,
    /// Total size, in bytes
    pub size: u64,
    /// Block size for filesystem I/O
    pub block_size: u64,
    /// Total size of allocated storage, in bytes
    pub allocated_size: u64,

    /// Time of last content access
    pub time_accessed: SystemTime,
    /// Time of last content modification
    pub time_modified: SystemTime,
    /// Time of last status change (either content or attributes)
    pub time_changed: SystemTime,
}

/// Space information of a filesystem, as returned by `file_system_space()`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SpaceInfo {
    /// Total size of the filesystem, in bytes.
    pub capacity: u64,
    /// Free space, in bytes.
    pub free: u64,
    /// Free space available to a non-privileged process, in bytes.
    pub available: u64,
}

/// Options used to configure an open file.
#[derive(Clone, Copy, Debug)]
pub struct FileOpenOptions {
    /// Allocate this much disk space when extending the file
    pub extent_allocation_size_hint: u64,
    /// Allow the file size not to track the amount of data written until a flush
    pub sloppy_size: bool,
    /// Hint as to what the eventual file size will be
    pub sloppy_size_hint: u64,
    /// File permissions to use when creating a file
    pub create_permissions: FilePermissions,
    /// Hint that user promises (or at least tries hard) not to write behind file size
    pub append_is_unlikely: bool,
}

impl FileOpenOptions {
    /// The largest extent allocation size hint: the filesystem's extent size field is 32-bit.
    pub const MAX_EXTENT_ALLOCATION_SIZE_HINT: u64 = 1 << 31;

    /// Alignment of extent size hints (128 KiB, a best guess): XFS ignores hints that
    /// are not aligned to the logical block size.
    pub const MIN_EXTENT_SIZE_HINT_ALIGNMENT: u32 = 128u32 << 10;
}

impl Default for FileOpenOptions {
    #[inline]
    fn default() -> Self {
        FileOpenOptions {
            extent_allocation_size_hint: 1 << 20,
            sloppy_size: false,
            sloppy_size_hint: 1 << 20,
            create_permissions: FilePermissions::DEFAULT_FILE_PERMISSIONS,
            append_is_unlikely: false,
        }
    }
}

/// A handle that can be transported across shards and used to create a dup(2)-like
/// [`File`] referring to the same underlying file.
//
/// It exists to cross shards, so implementations are `Send`.
pub trait FileHandleImpl: Send {
    /// A new handle to the same file.
    fn clone_handle(&self) -> Box<dyn FileHandleImpl>;
    /// Turns the handle into a file implementation on the current shard.
    fn to_file(self: Box<Self>) -> SharedPtr<dyn FileImpl>;
}

/// The common data of every [`FileImpl`]: the alignments and maximum request lengths
/// that [`File`] reads.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FileImplBase {
    /// Alignment of memory buffers (default 4096).
    pub _memory_dma_alignment: u32,
    /// Alignment of read offsets and lengths (default 4096).
    pub _disk_read_dma_alignment: u32,
    /// Alignment of write offsets and lengths (default 4096).
    pub _disk_write_dma_alignment: u32,
    /// Alignment of overwrite offsets and lengths (default 4096).
    pub _disk_overwrite_dma_alignment: u32,
    /// Recommended maximum read length (default 1 GiB).
    pub _read_max_length: u32,
    /// Recommended maximum write length (default 1 GiB).
    pub _write_max_length: u32,
}

impl Default for FileImplBase {
    #[inline]
    fn default() -> Self {
        FileImplBase {
            _memory_dma_alignment: 4096,
            _disk_read_dma_alignment: 4096,
            _disk_write_dma_alignment: 4096,
            _disk_overwrite_dma_alignment: 4096,
            _read_max_length: 1u32 << 30,
            _write_max_length: 1u32 << 30,
        }
    }
}

/// The interface behind a [`File`].
///
/// Buffers are raw pointers: they must stay valid until the returned future resolves.
/// The implementation object must outlive its pending operations.
///
/// Copies of a [`File`] share their implementation, and the operations re-enter it (the queue
/// of an append-challenged file runs operations from inside another one), so every
/// operation takes `&self` and the implementations keep their changing state in cells.
///
/// # Safety (the operations that take raw pointers)
/// `buffer` (or each iovec) is valid for the reads or writes of `len` bytes the operation
/// does, and is neither freed nor touched by anything else until the returned future
/// resolves; `intent` is null or points to a live, pinned `IoIntent` that outlives the
/// requests attached to it; `argp` and `arg` are what the kernel expects for `cmd` and `op`
/// (the pointed-to value stays valid until the future resolves).
pub trait FileImpl: 'static {
    /// The common alignments and lengths.
    fn file_impl_base(&self) -> &FileImplBase;

    /// Writes `len` bytes from `buffer` at `pos`; resolves with the bytes written.
    ///
    /// # Safety
    /// See [`FileImpl`].
    unsafe fn write_dma(
        &self,
        pos: u64,
        buffer: *const u8,
        len: usize,
        intent: *mut IoIntent,
    ) -> Future<usize>;
    /// Writes the buffers of `iov` at `pos`; resolves with the bytes written.
    ///
    /// # Safety
    /// See [`FileImpl`].
    unsafe fn write_dma_iov(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        intent: *mut IoIntent,
    ) -> Future<usize>;
    /// Reads `len` bytes at `pos` into `buffer`; resolves with the bytes read.
    ///
    /// # Safety
    /// See [`FileImpl`].
    unsafe fn read_dma(
        &self,
        pos: u64,
        buffer: *mut u8,
        len: usize,
        intent: *mut IoIntent,
    ) -> Future<usize>;
    /// Reads at `pos` into the buffers of `iov`; resolves with the bytes read.
    ///
    /// # Safety
    /// See [`FileImpl`].
    unsafe fn read_dma_iov(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        intent: *mut IoIntent,
    ) -> Future<usize>;
    /// Reads `range_size` bytes at `offset`, with no alignment requirement, into a new
    /// buffer; it may be shorter at end of file.
    ///
    /// # Safety
    /// See [`FileImpl`].
    unsafe fn dma_read_bulk(
        &self,
        offset: u64,
        range_size: usize,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>>;

    /// Makes written data durable.
    fn flush(&self) -> Future<()>;
    /// The file's `stat(2)` information.
    fn stat(&self) -> Future<Stat>;
    /// Sets the file length.
    fn truncate(&self, length: u64) -> Future<()>;
    /// Discards (deallocates) a range of the file.
    fn discard(&self, offset: u64, length: u64) -> Future<()>;
    /// Runs `ioctl(cmd, argp)` in the syscall thread; resolves with its result.
    ///
    /// # Safety
    /// See [`FileImpl`].
    unsafe fn ioctl(&self, _cmd: u64, _argp: *mut c_void) -> Future<i32> {
        make_exception_future(ExceptionPtr::from(RuntimeError(
            "this file type does not support ioctl".into(),
        )))
    }
    /// Runs `ioctl(cmd, argp)` on the reactor thread, for commands that do not block.
    ///
    /// # Safety
    /// See [`FileImpl`].
    unsafe fn ioctl_short(&self, _cmd: u64, _argp: *mut c_void) -> Future<i32> {
        make_exception_future(ExceptionPtr::from(RuntimeError(
            "this file type does not support ioctl_short".into(),
        )))
    }
    /// Runs `fcntl(op, arg)` in the syscall thread; resolves with its result.
    ///
    /// # Safety
    /// See [`FileImpl`].
    unsafe fn fcntl(&self, _op: i32, _arg: usize) -> Future<i32> {
        make_exception_future(ExceptionPtr::from(RuntimeError(
            "this file type does not support fcntl".into(),
        )))
    }
    /// Runs `fcntl(op, arg)` on the reactor thread, for operations that do not block.
    ///
    /// # Safety
    /// See [`FileImpl`].
    unsafe fn fcntl_short(&self, _op: i32, _arg: usize) -> Future<i32> {
        make_exception_future(ExceptionPtr::from(RuntimeError(
            "this file type does not support fcntl_short".into(),
        )))
    }
    /// Allocates disk space for a range of the file.
    fn allocate(&self, position: u64, length: u64) -> Future<()>;
    /// The file size.
    fn size(&self) -> Future<u64>;
    /// Closes the file.
    fn close(&self) -> Future<()>;
    /// A handle to the file for another shard.
    ///
    /// # Errors
    /// The default fails: the file type cannot be duplicated.
    fn dup(&self) -> Result<Box<dyn FileHandleImpl>, ExceptionPtr> {
        Err(ExceptionPtr::from(RuntimeError("this file type cannot be duplicated".into())))
    }
    /// Lists the directory, calling `next` for each entry.
    fn list_directory(&self, next: NextFn<DirectoryEntry>) -> Subscription<DirectoryEntry>;
    // Buffered generators cannot be used yet (an open issue of the generator), so the
    // default lists through a subscription.
    /// Lists the directory as a generator of entries.
    fn experimental_list_directory(&self) -> Generator<DirectoryEntry> {
        make_list_directory_fallback_generator(self as *const Self)
    }
}

impl dyn FileImpl {
    /// The implementation of `f`, if it is initialized.
    #[inline]
    pub fn get_file_impl(f: &mut File) -> Option<*mut dyn FileImpl> {
        f._file_impl.as_ref().and_then(|p| p.get()).map(|p| p.as_ptr())
    }
}

/// A data file on persistent storage.
///
/// Files are uncached and unbuffered: data must be cached at the application layer,
/// since neither the runtime nor the OS caches it.
///
/// Data is transferred using direct memory access (DMA).  This imposes
/// restrictions on file offsets and data pointers.  The former must be aligned
/// on a 4096 byte boundary, while a 512 byte boundary suffices for the latter.
#[derive(Clone, Default)]
pub struct File {
    _file_impl: Option<SharedPtr<dyn FileImpl>>,
}

impl File {
    /// An uninitialized file.
    ///
    /// Useful for declaring a variable and assigning it later. An uninitialized file
    /// must not be used: its operations panic.
    ///
    /// [`File::is_initialized`] tells whether a file is initialized; assigning `File::new()`
    /// resets it.
    #[inline]
    pub const fn new() -> Self {
        File { _file_impl: None }
    }

    /// A file over the implementation `impl_`.
    #[inline]
    pub fn from_impl(impl_: SharedPtr<dyn FileImpl>) -> Self {
        File { _file_impl: Some(impl_) }
    }

    /// Checks whether the file object was initialized.
    ///
    /// Returns false if the file is uninitialized (default constructed), true if it
    /// refers to an actual file.
    #[inline]
    pub fn is_initialized(&self) -> bool {
        self._file_impl.as_ref().is_some_and(|p| p.is_some())
    }

    /// The implementation.
    ///
    /// # Panics
    /// On an uninitialized file.
    #[inline]
    fn impl_(&self) -> &dyn FileImpl {
        match self._file_impl.as_ref().and_then(|p| p.get()) {
            // SAFETY: the shared pointer keeps the implementation alive while `self` is; the
            // implementation is only reached by `&` (its state is interior).
            Some(p) => unsafe { &*p.as_ptr() },
            None => panic!("use of an uninitialized teseladb::file"),
        }
    }

    // Direct I/O requires the buffer, offset and length to be aligned. 4096 used to be
    // necessary; 512 is usually enough now, but the BLKSSZGET ioctl is needed to be sure
    // on a given filesystem, and 4096 is always safe. Reading outside page boundaries
    // would also leave pages around with overlapping ranges, which are very hard to
    // cache.

    /// Alignment requirement for file offsets (for reads).
    #[inline]
    pub fn disk_read_dma_alignment(&self) -> u64 {
        self.impl_().file_impl_base()._disk_read_dma_alignment as u64
    }

    /// Alignment requirement for file offsets (for writes).
    #[inline]
    pub fn disk_write_dma_alignment(&self) -> u64 {
        self.impl_().file_impl_base()._disk_write_dma_alignment as u64
    }

    /// Alignment requirement for file offsets (for overwrites).
    ///
    /// Specifies the minimum alignment for disk offsets for
    /// overwrites (writes to a location that was previously written).
    /// This can be smaller than [`File::disk_write_dma_alignment`], allowing
    /// a reduction in disk bandwidth used.
    #[inline]
    pub fn disk_overwrite_dma_alignment(&self) -> u64 {
        self.impl_().file_impl_base()._disk_overwrite_dma_alignment as u64
    }

    /// Alignment requirement for data buffers.
    #[inline]
    pub fn memory_dma_alignment(&self) -> u64 {
        self.impl_().file_impl_base()._memory_dma_alignment as u64
    }

    /// Recommended limit for read request size. Submitting a larger request causes no
    /// error, but may result in poor latencies for this and any other concurrent
    /// request.
    #[inline]
    pub fn disk_read_max_length(&self) -> usize {
        self.impl_().file_impl_base()._read_max_length as usize
    }

    /// Recommended limit for write request size. Submitting a larger request causes no
    /// error, but may result in poor latencies for this and any other concurrent
    /// request.
    #[inline]
    pub fn disk_write_max_length(&self) -> usize {
        self.impl_().file_impl_base()._write_max_length as usize
    }

    /// Performs a single DMA read of `aligned_len` bytes at offset `aligned_pos` into
    /// `aligned_buffer`, and resolves with the number of bytes actually read.
    ///
    /// The position, the buffer and the length should be aligned; the required alignment
    /// depends on the hardware, and 4 KiB is always safe. `intent` is the I/O intent
    /// ([`IoIntent`]), or null.
    ///
    /// # Errors
    ///
    /// The returned future fails on an I/O error.
    ///
    /// # Safety
    /// `aligned_buffer` is valid for writes of `aligned_len` elements, and is neither read,
    /// written nor freed by anyone else, until the returned future resolves (the kernel
    /// writes into it meanwhile); a non-null `intent` outlives the request. The safe form is
    /// [`File::dma_read_buf`].
    #[inline]
    pub unsafe fn dma_read<CharType: ByteType>(
        &mut self,
        aligned_pos: u64,
        aligned_buffer: *mut CharType,
        aligned_len: usize,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: forwarded from the caller.
        unsafe {
            self.dma_read_impl_ptr(
                aligned_pos,
                aligned_buffer as *mut u8,
                aligned_len,
                MaybePriorityClassRef,
                intent,
            )
        }
    }

    /// Reads `len` bytes at offset `pos` and resolves with a buffer holding them.
    ///
    /// Neither `pos` nor `len` needs to be aligned. The buffer may be shorter than `len`
    /// when the end of the file is reached. `intent` is the I/O intent ([`IoIntent`]), or
    /// null.
    ///
    /// # Errors
    ///
    /// The returned future fails on an I/O error.
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`]: `intent` (or `argp`/`arg`) as
    /// described there.
    #[inline]
    pub unsafe fn dma_read_buf<CharType: ByteType>(
        &mut self,
        pos: u64,
        len: usize,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<CharType>> {
        // SAFETY: forwarded from the caller.
        unsafe { self.dma_read_impl(pos, len, MaybePriorityClassRef, intent) }
            .then(cast_buffer::<CharType>)
    }

    /// Reads exactly `len` bytes at offset `pos` and resolves with a buffer holding them.
    ///
    /// `intent` is the I/O intent ([`IoIntent`]), or null.
    ///
    /// # Errors
    ///
    /// The returned future fails with [`EofError`] if the end of the file comes before
    /// `len` bytes, or with a system error on an I/O error.
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`]: `intent` (or `argp`/`arg`) as
    /// described there.
    #[inline]
    pub unsafe fn dma_read_exactly<CharType: ByteType>(
        &mut self,
        pos: u64,
        len: usize,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<CharType>> {
        // SAFETY: forwarded from the caller.
        unsafe { self.dma_read_exactly_impl(pos, len, MaybePriorityClassRef, intent) }
            .then(cast_buffer::<CharType>)
    }

    /// Performs a DMA read into the buffers of `iov`.
    ///
    /// `pos` must be aligned to [`File::disk_read_dma_alignment`]; the buffer addresses
    /// must be aligned. `intent` is the I/O intent ([`IoIntent`]), or null.
    ///
    /// Resolves with the number of bytes actually read; a short read may happen at end
    /// of file or on an I/O error.
    ///
    /// For this variant, [`File::disk_read_max_length`] applies to the sum of the buffer
    /// sizes.
    ///
    /// # Safety
    /// Every iovec describes memory valid for writes, used by no one else until the returned
    /// future resolves; a non-null `intent` outlives the request.
    #[inline]
    pub unsafe fn dma_read_iov(
        &mut self,
        pos: u64,
        iov: Vec<Iovec>,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: forwarded from the caller.
        unsafe { self.dma_read_impl_iov(pos, iov, MaybePriorityClassRef, intent) }
    }

    /// Performs a DMA write of `len` bytes from `buffer`.
    ///
    /// `pos` and `len` must be aligned to [`File::disk_write_dma_alignment`] and `buffer`
    /// to [`File::memory_dma_alignment`]; the buffer must exist until the future
    /// resolves. `intent` is the I/O intent ([`IoIntent`]), or null.
    ///
    /// Resolves with the number of bytes actually written; a short write may happen on
    /// an I/O error.
    ///
    /// # Safety
    /// `buffer` is valid for reads of `len` elements and is not written or freed until the
    /// returned future resolves; a non-null `intent` outlives the request.
    #[inline]
    pub unsafe fn dma_write<CharType: ByteType>(
        &mut self,
        pos: u64,
        buffer: *const CharType,
        len: usize,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: forwarded from the caller.
        unsafe {
            self.dma_write_impl_ptr(pos, buffer as *const u8, len, MaybePriorityClassRef, intent)
        }
    }

    /// Performs a DMA write from the buffers of `iov`.
    ///
    /// `pos` must be aligned to [`File::disk_write_dma_alignment`]; the buffer addresses
    /// must be aligned. `intent` is the I/O intent ([`IoIntent`]), or null.
    ///
    /// Resolves with the number of bytes actually written; a short write may happen on
    /// an I/O error.
    ///
    /// For this variant, [`File::disk_write_max_length`] applies to the sum of the buffer
    /// sizes.
    ///
    /// # Safety
    /// Every iovec describes memory valid for reads, not written or freed until the returned
    /// future resolves; a non-null `intent` outlives the request.
    #[inline]
    pub unsafe fn dma_write_iov(
        &mut self,
        pos: u64,
        iov: Vec<Iovec>,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: forwarded from the caller.
        unsafe { self.dma_write_impl_iov(pos, iov, MaybePriorityClassRef, intent) }
    }

    // `flush`, `stat`, `truncate`, `allocate`, `discard`, the ioctl and fcntl calls, the
    // lifetime hints, `size`, `close` and `list_directory` are defined further below,
    // with their documentation.

    /// Reads a bulk of data that contains the `range_size` bytes starting at `offset`,
    /// extended to the DMA alignment (4 KiB) or to the end of the file, and resolves with
    /// a buffer holding it.
    ///
    /// `intent` is the I/O intent ([`IoIntent`]), or null.
    ///
    /// # Errors
    ///
    /// The returned future fails with a system error on an I/O error, or with
    /// [`EofError`] when `offset` is beyond the end of the file.
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`]: `intent` (or `argp`/`arg`) as
    /// described there.
    #[inline]
    pub unsafe fn dma_read_bulk<CharType: ByteType>(
        &mut self,
        offset: u64,
        range_size: usize,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<CharType>> {
        // SAFETY: forwarded from the caller.
        unsafe { self.dma_read_bulk_impl(offset, range_size, MaybePriorityClassRef, intent) }
            .then(cast_buffer::<CharType>)
    }
}

/// Reinterprets a byte buffer as a buffer of another one-byte type, without copying.
#[inline]
fn cast_buffer<CharType: ByteType>(t: TemporaryBuffer<u8>) -> Future<TemporaryBuffer<CharType>> {
    make_ready_future(t.retype::<CharType>())
}

/// The error of reading past end of file with [`File::dma_read_exactly`].
#[derive(Clone, Copy, Debug, Default)]
pub struct EofError;

impl fmt::Display for EofError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("end of file")
    }
}

impl std::error::Error for EofError {}

/// Runs `func` on the file produced by `file_fut` and closes the file afterwards.
///
/// The future resolves with `func`'s result, or fails if `file_fut`, `func` or closing
/// the file failed.
#[inline]
pub fn with_file<Func, R>(file_fut: Future<File>, func: Func) -> FuturizeT<R>
where
    Func: FnOnce(&mut File) -> R + 'static,
    R: Futurize,
{
    file_fut.then(move |f: File| {
        do_with(f, move |f: &mut File| {
            let fp = f as *mut File;
            futurize_invoke(move || func(f)).finally(move || {
                // SAFETY: do_with keeps the file alive until this future resolves.
                unsafe { (*fp).close() }
            })
        })
    })
}

/// Runs `func` on the file produced by `file_fut`, closing the file only if `func` fails.
///
/// - If `func` fails with an error E, the file is closed and the future fails with E.
/// - If `func` returns a value V, the file is not closed and the future resolves with V.
///
/// When `func` succeeds it is responsible for the file: it can close it, return it or
/// store it somewhere.
///
/// The future fails if `file_fut` failed, or with a nested error if closing the file
/// after a failure failed.
#[inline]
pub fn with_file_close_on_failure<Func, R>(file_fut: Future<File>, func: Func) -> FuturizeT<R>
where
    Func: FnOnce(&mut File) -> R + 'static,
    R: Futurize,
{
    file_fut.then(move |f: File| {
        do_with(f, move |f: &mut File| {
            let fp = f as *mut File;
            futurize_invoke(move || func(f)).then_wrapped(move |ret: FuturizeT<R>| {
                if !ret.failed() {
                    return ret;
                }
                ret.finally(move || {
                    // If f.close() fails, return that as a nested error.
                    // SAFETY: do_with keeps the file alive until this future resolves.
                    unsafe { (*fp).close() }
                })
            })
        })
    })
}

/// A shard-transportable handle to a file.
///
/// To read a file from several shards, [`File::dup`] creates a handle that can be
/// sent to another shard and turned into a [`File`] there. This is more efficient than
/// opening the file again.
pub struct FileHandle {
    _impl: Box<dyn FileHandleImpl>,
}

impl FileHandle {
    /// A handle over the implementation `impl_`.
    #[inline]
    fn from_impl(impl_: Box<dyn FileHandleImpl>) -> Self {
        FileHandle { _impl: impl_ }
    }
}

/// The error a cancelled I/O request resolves its future with (see
/// [`IoIntent`]).
#[derive(Clone, Copy, Debug, Default)]
pub struct CancelledError;

impl CancelledError {
    /// The error message.
    #[inline]
    pub const fn what(&self) -> &'static str {
        "cancelled"
    }
}

impl fmt::Display for CancelledError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.what())
    }
}

impl std::error::Error for CancelledError {}

// Definitions from the XFS and Linux headers used below.

/// The XFS direct-I/O attributes (`struct dioattr`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Dioattr {
    /// Memory buffer alignment.
    pub d_mem: u32,
    /// Minimum transfer size.
    pub d_miniosz: u32,
    /// Maximum transfer size.
    pub d_maxiosz: u32,
}

/// The `XFS_IOC_DIOINFO` ioctl: reads the direct-I/O attributes.
const XFS_IOC_DIOINFO: u64 = ioc_read(b'X' as u64, 30, std::mem::size_of::<Dioattr>() as u64);
/// The `BLKBSZGET` ioctl: the block device's block size.
const BLKBSZGET: u64 = ioc_read(0x12, 112, std::mem::size_of::<usize>() as u64);
/// The `BLKGETSIZE64` ioctl: the block device's size in bytes.
const BLKGETSIZE64: u64 = ioc_read(0x12, 114, std::mem::size_of::<usize>() as u64);
/// The `BLKDISCARD` ioctl: discards a range of a block device.
const BLKDISCARD: u64 = (0x12 << 8) | 119;
/// The major device number of Linux MD (software RAID) devices.
const MD_MAJOR: u32 = 9;

const FALLOC_FL_KEEP_SIZE: i32 = 0x01;
const FALLOC_FL_PUNCH_HOLE: i32 = 0x02;
const FALLOC_FL_ZERO_RANGE: i32 = 0x10;
const F_GET_RW_HINT: i32 = 1024 + 11;
const F_SET_RW_HINT: i32 = 1024 + 12;
use crate::engine::core::internal::linux_abi::{O_ACCMODE, O_RDONLY};
const SEEK_END: i32 = 2;
const EINVAL: i32 = 22;
const EOPNOTSUPP: i32 = 95;
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFBLK: u32 = 0o060000;
const DT_FIFO: u8 = 1;
const DT_CHR: u8 = 2;
const DT_DIR: u8 = 4;
const DT_BLK: u8 = 6;
const DT_REG: u8 = 8;
const DT_LNK: u8 = 10;
const DT_SOCK: u8 = 12;

/// The `_IOR(type, nr, size)` ioctl request number encoding.
const fn ioc_read(type_: u64, nr: u64, size: u64) -> u64 {
    (2 << 30) | (size << 16) | (type_ << 8) | nr
}

/// Whether the mode is a block device.
#[inline]
const fn s_isblk(m: u32) -> bool {
    m & S_IFMT == S_IFBLK
}

/// Whether the mode is a directory.
#[inline]
const fn s_isdir(m: u32) -> bool {
    m & S_IFMT == S_IFDIR
}

/// The major number of a device id.
#[inline]
const fn major(dev: u64) -> u32 {
    (((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff)) as u32
}

unsafe extern "C" {
    #[link_name = "ioctl"]
    fn c_ioctl(fd: i32, request: u64, ...) -> i32;
    #[link_name = "fcntl"]
    fn c_fcntl(fd: i32, cmd: i32, ...) -> i32;
    #[link_name = "fstat"]
    fn c_fstat(fd: i32, buf: *mut Stat) -> i32;
    #[link_name = "ftruncate"]
    fn c_ftruncate(fd: i32, length: i64) -> i32;
    #[link_name = "fallocate"]
    fn c_fallocate(fd: i32, mode: i32, offset: i64, len: i64) -> i32;
    #[link_name = "lseek"]
    fn c_lseek(fd: i32, offset: i64, whence: i32) -> i64;
    #[link_name = "close"]
    fn c_close(fd: i32) -> i32;
}

/// Filesystem information shared with the reactor.
pub mod internal {
    use super::Dioattr;

    /// What the file layer knows about a filesystem.
    #[derive(Clone, Copy, Debug, Default)]
    pub struct FsInfo {
        /// Block size of the filesystem.
        pub block_size: u32,
        /// Whether appending writes serialize with other AIO on this filesystem.
        pub append_challenged: bool,
        /// How many size-changing operations may run at once.
        pub append_concurrency: u32,
        /// Whether fsync blocks other operations.
        pub fsync_is_exclusive: bool,
        /// Whether `RWF_NOWAIT` works.
        pub nowait_works: bool,
        /// The XFS direct-IO attributes, if known.
        pub dioinfo: Option<Dioattr>,
    }
}

use self::linux_abi::LinuxDirent64;

/// The reactor's syscall thread pool, created when the reactor is constructed.
#[inline]
fn thread_pool() -> &'static crate::engine::core::thread_pool::ThreadPool {
    // SAFETY: on a reactor thread; the pool lives as long as the reactor (the rest of the
    // thread's life once it runs) and is shared by `&` with its syscall thread.
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).thread_pool() }
}

impl Clone for FileHandle {
    /// A new handle to the same file.
    #[inline]
    fn clone(&self) -> Self {
        FileHandle { _impl: self._impl.clone_handle() }
    }
}

impl FileHandle {
    /// A file on this shard over a copy of the handle.
    #[inline]
    pub fn to_file(&self) -> File {
        self.clone().into_file()
    }

    /// A file on this shard, consuming the handle.
    #[inline]
    pub fn into_file(self) -> File {
        File::from_impl(self._impl.to_file())
    }
}

impl PosixFileImpl {
    /// The state of a newly opened file over `fd`.
    pub(crate) fn new(
        fd: i32,
        f: OpenFlags,
        _options: FileOpenOptions,
        device_id: u64,
        nowait_works: bool,
    ) -> Self {
        let mut me = PosixFileImpl {
            _base: FileImplBase::default(),
            _refcount: Cell::new(ptr::null_mut()),
            _nowait_works: nowait_works,
            _device_id: device_id,
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
            _io_queue: unsafe { (*engine()).get_io_queue(device_id) },
            _open_flags: f,
            _fd: Cell::new(fd),
        };
        me.configure_io_lengths();
        me
    }

    /// The state of a newly opened file over `fd`, configured from the filesystem
    /// information `fsi`.
    pub(crate) fn with_fs_info(
        fd: i32,
        f: OpenFlags,
        options: FileOpenOptions,
        device_id: u64,
        fsi: &internal::FsInfo,
    ) -> Self {
        let mut me = Self::new(fd, f, options, device_id, fsi.nowait_works);
        me.configure_dma_alignment(fsi);
        me
    }
}

impl Drop for PosixFileImpl {
    /// Closes the descriptor if it is still open, when this is its last reference.
    fn drop(&mut self) {
        if !self._refcount.get().is_null()
            // SAFETY: a non-null refcount is shared by the dups of this fd and freed by the last one.
            && unsafe { (*self._refcount.get()).fetch_sub(1, Ordering::Relaxed) } != 1
        {
            return;
        }
        if !self._refcount.get().is_null() {
            // SAFETY: we were the last holder of the counter allocated by `dup()`.
            drop(unsafe { Box::from_raw(self._refcount.get()) });
        }
        if self._fd.get() != -1 {
            // close() can block on NFS.
            // SAFETY: closing the descriptor we own.
            unsafe { c_close(self._fd.get()) };
        }
    }
}

/// Whether the running kernel (5.12 or later) lets XFS overwrites use the minimum I/O
/// size alignment instead of the write alignment.
static XFS_WITH_RELAXED_OVERWRITE_ALIGNMENT: LazyLock<bool> =
    LazyLock::new(|| kernel_uname().whitelisted(&["5.12"]));

impl PosixFileImpl {
    /// Sets the DMA alignments from the filesystem information `fsi`.
    fn configure_dma_alignment(&mut self, fsi: &internal::FsInfo) {
        if let Some(da) = fsi.dioinfo.as_ref() {
            self._base._memory_dma_alignment = da.d_mem;
            self._base._disk_read_dma_alignment = da.d_miniosz;
            // XFS wants at least the block size for writes.
            // FIXME: really read the block size.
            self._base._disk_write_dma_alignment = da.d_miniosz.max(fsi.block_size);
            self._base._disk_overwrite_dma_alignment = if *XFS_WITH_RELAXED_OVERWRITE_ALIGNMENT {
                da.d_miniosz
            } else {
                self._base._disk_write_dma_alignment
            };
        }
    }

    /// Sets the maximum read and write lengths from the I/O queue's request limits.
    fn configure_io_lengths(&mut self) {
        // SAFETY: the io queue of a device lives as long as the reactor.
        let limits = unsafe { (*self._io_queue).get_request_limits() };
        self._base._read_max_length =
            (self._base._read_max_length as usize).min(limits.max_read) as u32;
        self._base._write_max_length =
            (self._base._write_max_length as usize).min(limits.max_write) as u32;
    }

    /// A handle sharing the descriptor and its reference count.
    pub(crate) fn dup(&self) -> Result<Box<dyn FileHandleImpl>, ExceptionPtr> {
        if self._refcount.get().is_null() {
            self._refcount.set(Box::into_raw(Box::new(AtomicU32::new(1))));
        }
        let ret = Box::new(PosixFileHandleImpl::new(
            self._fd.get(),
            self._open_flags,
            self._refcount.get(),
            self._device_id,
            self._base._memory_dma_alignment,
            self._base._disk_read_dma_alignment,
            self._base._disk_write_dma_alignment,
            self._base._disk_overwrite_dma_alignment,
            self._nowait_works,
        ));
        // SAFETY: the counter was allocated above or by an earlier dup and is alive.
        unsafe { (*self._refcount.get()).fetch_add(1, Ordering::Relaxed) };
        Ok(ret)
    }

    /// The state of a file over a descriptor shared through a handle.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_dup(
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
        let mut me = PosixFileImpl {
            _base: FileImplBase::default(),
            _refcount: Cell::new(refcount),
            _nowait_works: nowait_works,
            _device_id: device_id,
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
            _io_queue: unsafe { (*engine()).get_io_queue(device_id) },
            _open_flags: f,
            _fd: Cell::new(fd),
        };
        me._base._memory_dma_alignment = memory_dma_alignment;
        me._base._disk_read_dma_alignment = disk_read_dma_alignment;
        me._base._disk_write_dma_alignment = disk_write_dma_alignment;
        me._base._disk_overwrite_dma_alignment = disk_overwrite_dma_alignment;
        me.configure_io_lengths();
        me
    }

    /// Makes written data durable with `fdatasync`, unless the file is opened with
    /// [`OpenFlags::DSYNC`].
    pub(crate) fn flush(&self) -> Future<()> {
        if (self._open_flags & OpenFlags::DSYNC) != OpenFlags::from_bits(0) {
            // With OpenFlags::DSYNC every write is already synced to the disk, so
            // there is nothing to do.
            return make_ready_future(());
        }
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        unsafe { (*engine()).fdatasync(self._fd.get()) }
    }

    /// The file's `fstat` information.
    pub(crate) fn stat(&self) -> Future<Stat> {
        let fd = self._fd.get();
        thread_pool()
            .submit::<SyscallResultExtra<Stat>, _>(move || {
                // SAFETY: `Stat` is plain data; fstat fills it.
                let mut st: Stat = unsafe { std::mem::zeroed() };
                // SAFETY: a valid stat buffer.
                let ret = unsafe { c_fstat(fd, &mut st) };
                wrap_syscall_extra(ret as i64, st)
            })
            .then(|ret: SyscallResultExtra<Stat>| -> Result<Stat, ExceptionPtr> {
                ret.throw_if_error()?;
                Ok(ret.extra)
            })
    }

    /// Truncates the file to `length`.
    pub(crate) fn truncate(&self, length: u64) -> Future<()> {
        let fd = self._fd.get();
        thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                // SAFETY: plain system call on our descriptor.
                wrap_syscall::<i32>(unsafe { c_ftruncate(fd, length as i64) })
            })
            .then(|sr: SyscallResult<i32>| sr.throw_if_error())
    }

    /// Runs `ioctl` in the syscall thread.
    pub(crate) fn ioctl(&self, cmd: u64, argp: *mut c_void) -> Future<i32> {
        let fd = self._fd.get();
        let argp = argp as usize;
        thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                // SAFETY: the caller keeps `argp` valid for the command until the future resolves.
                wrap_syscall::<i32>(unsafe { c_ioctl(fd, cmd, argp as *mut c_void) })
            })
            .then(|sr: SyscallResult<i32>| -> Result<i32, ExceptionPtr> {
                sr.throw_if_error()?;
                // Some ioctls return a positive integer.
                Ok(sr.result)
            })
    }

    /// Runs `ioctl` on the reactor thread.
    pub(crate) fn ioctl_short(&self, cmd: u64, argp: *mut c_void) -> Future<i32> {
        // SAFETY: the caller keeps `argp` valid for the command.
        let ret = unsafe { c_ioctl(self._fd.get(), cmd, argp) };
        if ret == -1 {
            return make_exception_future(ExceptionPtr::from(SystemError::new(
                errno(),
                "ioctl failed",
            )));
        }
        make_ready_future(ret)
    }

    /// Runs `fcntl` in the syscall thread.
    pub(crate) fn fcntl(&self, op: i32, arg: usize) -> Future<i32> {
        let fd = self._fd.get();
        thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                // SAFETY: the caller keeps `arg` valid for the operation until the future resolves.
                wrap_syscall::<i32>(unsafe { c_fcntl(fd, op, arg) })
            })
            .then(|sr: SyscallResult<i32>| -> Result<i32, ExceptionPtr> {
                sr.throw_if_error()?;
                // Some fcntls return a positive integer.
                Ok(sr.result)
            })
    }

    /// Runs `fcntl` on the reactor thread.
    pub(crate) fn fcntl_short(&self, op: i32, arg: usize) -> Future<i32> {
        // SAFETY: the caller keeps `arg` valid for the operation.
        let ret = unsafe { c_fcntl(self._fd.get(), op, arg) };
        if ret == -1 {
            return make_exception_future(ExceptionPtr::from(SystemError::new(
                errno(),
                "fcntl failed",
            )));
        }
        make_ready_future(ret)
    }

    /// Discards a range with `fallocate(FALLOC_FL_PUNCH_HOLE)`.
    pub(crate) fn discard(&self, offset: u64, length: u64) -> Future<()> {
        let fd = self._fd.get();
        thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                // SAFETY: plain system call on our descriptor.
                wrap_syscall::<i32>(unsafe {
                    c_fallocate(
                        fd,
                        FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE,
                        offset as i64,
                        length as i64,
                    )
                })
            })
            .then(|sr: SyscallResult<i32>| sr.throw_if_error())
    }

    /// Allocates a range with `fallocate(FALLOC_FL_ZERO_RANGE)`; succeeds without doing
    /// anything where that is not supported.
    pub(crate) fn allocate(&self, position: u64, length: u64) -> Future<()> {
        // FALLOC_FL_ZERO_RANGE is fairly new, so a missing implementation is not an error.
        static SUPPORTED: AtomicBool = AtomicBool::new(true);
        if !SUPPORTED.load(Ordering::Relaxed) {
            return make_ready_future(());
        }
        let fd = self._fd.get();
        thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                // SAFETY: plain system call on our descriptor.
                let mut ret = unsafe {
                    c_fallocate(
                        fd,
                        FALLOC_FL_ZERO_RANGE | FALLOC_FL_KEEP_SIZE,
                        position as i64,
                        length as i64,
                    )
                };
                if ret == -1 && errno() == EOPNOTSUPP {
                    ret = 0;
                    SUPPORTED.store(false, Ordering::Relaxed); // Racy, but harmless.  At most we issue an extra call or two.
                }
                wrap_syscall::<i32>(ret)
            })
            .then(|sr: SyscallResult<i32>| sr.throw_if_error())
    }

    /// The file size, from `fstat`.
    pub(crate) fn size(&self) -> Future<u64> {
        // SAFETY: plain system call on our descriptor.
        let r = unsafe { c_lseek(self._fd.get(), 0, SEEK_END) };
        if r == -1 {
            return make_exception_future(ExceptionPtr::from(SystemError::from_errno(errno())));
        }
        make_ready_future(r as u64)
    }

    /// Closes the descriptor in the syscall thread when this is its last reference.
    pub(crate) fn close(&self) -> Future<()> {
        if self._fd.get() == -1 {
            engine_logger().warn(
                FormatInfo::new("double close() detected, contact support"),
                format_args!("double close() detected, contact support"),
            );
            return make_ready_future(());
        }
        let fd = self._fd.get();
        self._fd.set(-1); // Prevent a concurrent close (which is illegal) from closing another file's fd
        if !self._refcount.get().is_null()
            // SAFETY: a non-null refcount is shared by the dups of this fd.
            && unsafe { (*self._refcount.get()).fetch_sub(1, Ordering::Relaxed) } != 1
        {
            self._refcount.set(ptr::null_mut());
            return make_ready_future(());
        }
        if !self._refcount.get().is_null() {
            // SAFETY: we were the last holder of the counter.
            drop(unsafe { Box::from_raw(self._refcount.get()) });
        }
        self._refcount.set(ptr::null_mut());
        let closed = if (self._open_flags & OpenFlags::RO) != OpenFlags::from_bits(0) {
            // SAFETY: closing the descriptor we owned.
            futurize_invoke(move || make_ready_future(wrap_syscall::<i32>(unsafe { c_close(fd) })))
        } else {
            thread_pool().submit::<SyscallResult<i32>, _>(move || {
                // SAFETY: closing the descriptor we owned.
                wrap_syscall::<i32>(unsafe { c_close(fd) })
            })
        };
        closed.then(|sr: SyscallResult<i32>| {
            if let Err(e) = sr.throw_if_error() {
                report_exception("close() syscall failed", &e);
            }
        })
    }
}

impl BlockdevFileImpl {
    /// The block device size, from the `BLKGETSIZE64` ioctl.
    pub(crate) fn size_impl(&self) -> Future<u64> {
        let fd = self.base._fd.get();
        thread_pool()
            .submit::<SyscallResultExtra<u64>, _>(move || {
                let mut size: u64 = 0;
                // SAFETY: BLKGETSIZE64 writes a u64.
                let ret = unsafe { c_ioctl(fd, BLKGETSIZE64, &mut size as *mut u64) };
                wrap_syscall_extra(ret as i64, size)
            })
            .then(|ret: SyscallResultExtra<u64>| -> Result<u64, ExceptionPtr> {
                ret.throw_if_error()?;
                Ok(ret.extra)
            })
    }
}

/// The entry type of a dirent, if the filesystem reports it.
fn dirent_type(de: &LinuxDirent64) -> Option<DirectoryEntryType> {
    match de.d_type {
        DT_BLK => Some(DirectoryEntryType::BlockDevice),
        DT_CHR => Some(DirectoryEntryType::CharDevice),
        DT_DIR => Some(DirectoryEntryType::Directory),
        DT_FIFO => Some(DirectoryEntryType::Fifo),
        DT_REG => Some(DirectoryEntryType::Regular),
        DT_LNK => Some(DirectoryEntryType::Link),
        DT_SOCK => Some(DirectoryEntryType::Socket),
        // Unknown type: ignore it.
        _ => None,
    }
}

/// The entry name of a dirent, unless it is `.` or `..`.
///
/// # Safety
/// `de` must point at a complete dirent returned by getdents64.
#[inline]
unsafe fn dirent_name(de: *const LinuxDirent64) -> Sstring {
    // SAFETY: getdents64 NUL-terminates d_name inside the record.
    let name = unsafe { CStr::from_ptr((&raw const (*de).d_name) as *const std::ffi::c_char) };
    Sstring::from_bytes(name.to_bytes())
}

#[inline]
fn is_dot_or_dotdot(name: &Sstring) -> bool {
    let b = name.as_bytes();
    b == b"." || b == b".."
}

/// A generator listing the directory `fd` with `getdents64`.
fn make_list_directory_generator(fd: i32) -> Generator<DirectoryEntry> {
    Generator::new(async move |y| {
        let mut buf = TemporaryBuffer::<u8>::new(8192);

        loop {
            // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
            let size =
                unsafe { (*engine()).read_directory(fd, buf.get_write(), buf.size()) }.await?;
            if size == 0 {
                return Ok(());
            }

            let base = buf.get();
            let mut off = 0usize;
            while off < size {
                // SAFETY: getdents64 wrote `size` bytes of whole records into `buf`.
                let de = unsafe { base.add(off) } as *const LinuxDirent64;
                // SAFETY: as above.
                off += unsafe { (*de).d_reclen } as usize;
                // SAFETY: as above.
                let name = unsafe { dirent_name(de) };
                if is_dot_or_dotdot(&name) {
                    continue;
                }
                // SAFETY: as above.
                let type_ = dirent_type(unsafe { &*de });
                // Entries are yielded one at a time (see the issue tracker, #1677).
                let ret = DirectoryEntry { name, type_ };
                y.yield_value(ret).await;
            }
        }
    })
}

impl PosixFileImpl {
    /// Lists the directory as a generator, reading it with `getdents64`.
    pub(crate) fn experimental_list_directory(&self) -> Generator<DirectoryEntry> {
        // Buffered generators cannot be used yet (an open issue of the generator).
        // TODO:
        // Keep 8 entries. The sizeof(directory_entry) is 24 bytes, the name itself
        // is allocated out of this buffer, so the buffer would grow up to ~200 bytes
        make_list_directory_generator(self._fd.get())
    }

    /// Lists the directory, calling `next` for each entry; the subscription ends after
    /// the last entry or with the first error.
    pub(crate) fn list_directory(
        &self,
        next: NextFn<DirectoryEntry>,
    ) -> Subscription<DirectoryEntry> {
        const BUFFER_SIZE: usize = 8192;
        struct Work {
            s: Stream<DirectoryEntry>,
            current: u32,
            total: u32,
            eof: bool,
            #[allow(dead_code)]
            error: i32,
            buffer: [u8; BUFFER_SIZE],
        }

        // While it would be natural to use fdopendir()/readdir(),
        // our syscall thread pool doesn't support malloc(), which is
        // required for this to work.  So resort to using getdents()
        // instead.

        let w = make_lw_shared(Work {
            s: Stream::new(),
            current: 0,
            total: 0,
            eof: false,
            error: 0,
            buffer: [0; BUFFER_SIZE],
        });
        // SAFETY: the work lives in its shared allocation, which never moves (so the stream
        // stays pinned); nothing else borrows it now.
        let ret = unsafe { Pin::new_unchecked(&mut (*w.get()).s) }.listen_with(next);
        let this = self as *const PosixFileImpl;
        // List the directory asynchronously in the background.
        // Caller synchronizes using the returned subscription.
        let w1 = w.clone();
        let w2 = w.clone();
        // SAFETY: as above.
        let started = unsafe { Pin::new_unchecked(&mut (*w.get()).s) }.started();
        let _ = started
            .then(move |()| {
                let we = w1.clone();
                let eofcond = move || we.eof;
                let wb = w1.clone();
                do_until(eofcond, move || -> Future<()> {
                    let wp = wb.get();
                    // SAFETY: the shared work is alive while `wb` is; the reactor is single-threaded.
                    let w = unsafe { &mut *wp };
                    if w.current == w.total {
                        let wr = wb.clone();
                        // SAFETY: the file outlives the listing, which runs while its subscription
                        // is alive.
                        let fd = unsafe { (*this)._fd.get() };
                        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
                        return unsafe {
                            (*engine()).read_directory(fd, w.buffer.as_mut_ptr(), BUFFER_SIZE)
                        }
                        .then(move |size: usize| {
                            // SAFETY: as above.
                            let w = unsafe { &mut *wr.get() };
                            if size == 0 {
                                w.eof = true;
                            } else {
                                w.current = 0;
                                w.total = size as u32;
                            }
                        });
                    }
                    // SAFETY: `current` is the offset of a whole record inside the `total` bytes read.
                    let de = unsafe { w.buffer.as_ptr().add(w.current as usize) }
                        as *const LinuxDirent64;
                    // SAFETY: as above.
                    w.current += unsafe { (*de).d_reclen } as u32;
                    // SAFETY: as above.
                    let name = unsafe { dirent_name(de) };
                    if is_dot_or_dotdot(&name) {
                        return make_ready_future(());
                    }
                    // SAFETY: as above.
                    let type_ = dirent_type(unsafe { &*de });
                    // SAFETY: the stream is pinned in the shared work, which `wb` keeps alive until
                    // do_until() is done with the returned future.
                    unsafe { Pin::new_unchecked(&mut w.s).produce(DirectoryEntry { name, type_ }) }
                })
            })
            .then(move |()| {
                // SAFETY: as above.
                unsafe { Pin::new_unchecked(&mut (*w2.get()).s) }.close();
            })
            .handle_exception(|_ignored: ExceptionPtr| {});
        ret
    }

    /// Queues a write of `len` bytes from `buffer` at `pos` on the shard's I/O queue.
    #[inline]
    pub(crate) fn do_write_dma(
        &self,
        pos: u64,
        buffer: *const u8,
        len: usize,
        io_priority_class: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        let req = crate::engine::core::internal::io_request::IoRequest::make_write(
            self._fd.get(),
            pos,
            buffer as *const c_void,
            len,
            self._nowait_works,
        );
        // SAFETY: the io queue of a device lives as long as the reactor; `_io_queue` is its
        // root pointer (`get_io_queue`), with no reference to the queue live.
        unsafe {
            IoQueue::submit_io_write(
                self._io_queue,
                PriorityClass::from_maybe(io_priority_class),
                len,
                req,
                intent,
                Vec::new(),
            )
        }
    }

    /// Queues a vectored write at `pos` on the shard's I/O queue.
    #[inline]
    pub(crate) fn do_write_dma_iov(
        &self,
        pos: u64,
        mut iov: Vec<Iovec>,
        io_priority_class: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        let len = sanitize_iovecs(&mut iov, self._base._disk_write_dma_alignment as usize);
        let req = crate::engine::core::internal::io_request::IoRequest::make_writev(
            self._fd.get(),
            pos,
            &mut iov,
            self._nowait_works,
        );
        // SAFETY: the io queue of a device lives as long as the reactor; `_io_queue` is its
        // root pointer (`get_io_queue`), with no reference to the queue live.
        unsafe {
            IoQueue::submit_io_write(
                self._io_queue,
                PriorityClass::from_maybe(io_priority_class),
                len,
                req,
                intent,
                iov,
            )
        }
    }

    /// Queues a read of `len` bytes into `buffer` at `pos` on the shard's I/O queue.
    #[inline]
    pub(crate) fn do_read_dma(
        &self,
        pos: u64,
        buffer: *mut u8,
        len: usize,
        io_priority_class: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        let req = crate::engine::core::internal::io_request::IoRequest::make_read(
            self._fd.get(),
            pos,
            buffer as *mut c_void,
            len,
            self._nowait_works,
        );
        // SAFETY: the io queue of a device lives as long as the reactor; `_io_queue` is its
        // root pointer (`get_io_queue`), with no reference to the queue live.
        unsafe {
            IoQueue::submit_io_read(
                self._io_queue,
                PriorityClass::from_maybe(io_priority_class),
                len,
                req,
                intent,
                Vec::new(),
            )
        }
    }

    /// Queues a vectored read at `pos` on the shard's I/O queue.
    #[inline]
    pub(crate) fn do_read_dma_iov(
        &self,
        pos: u64,
        mut iov: Vec<Iovec>,
        io_priority_class: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        let len = sanitize_iovecs(&mut iov, self._base._disk_read_dma_alignment as usize);
        let req = crate::engine::core::internal::io_request::IoRequest::make_readv(
            self._fd.get(),
            pos,
            &mut iov,
            self._nowait_works,
        );
        // SAFETY: the io queue of a device lives as long as the reactor; `_io_queue` is its
        // root pointer (`get_io_queue`), with no reference to the queue live.
        unsafe {
            IoQueue::submit_io_read(
                self._io_queue,
                PriorityClass::from_maybe(io_priority_class),
                len,
                req,
                intent,
                iov,
            )
        }
    }
}

impl PosixFileReal {
    /// Writes `len` bytes from `buffer` at `pos`.
    #[inline]
    pub(crate) fn write_dma_pc(
        &self,
        pos: u64,
        buffer: *const u8,
        len: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.base.do_write_dma(pos, buffer, len, pc, intent)
    }

    /// Writes the buffers of `iov` at `pos`.
    #[inline]
    pub(crate) fn write_dma_iov_pc(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.base.do_write_dma_iov(pos, iov, pc, intent)
    }

    /// Reads `len` bytes at `pos` into `buffer`.
    #[inline]
    pub(crate) fn read_dma_pc(
        &self,
        pos: u64,
        buffer: *mut u8,
        len: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.base.do_read_dma(pos, buffer, len, pc, intent)
    }

    /// Reads at `pos` into the buffers of `iov`.
    #[inline]
    pub(crate) fn read_dma_iov_pc(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.base.do_read_dma_iov(pos, iov, pc, intent)
    }

    /// Reads `range_size` bytes at `offset` into a new buffer, with no alignment
    /// requirement.
    #[inline]
    pub(crate) fn dma_read_bulk_pc(
        &self,
        offset: u64,
        range_size: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        PosixFileImpl::do_dma_read_bulk(self as *const Self, offset, range_size, pc, intent)
    }
}

impl PosixFileImpl {
    /// Reads `range_size` bytes at `offset` with no alignment requirement: reads the
    /// aligned range covering it, retries short reads with `read_maybe_eof`, and trims the
    /// result to the requested range (shorter at end of file).
    ///
    /// `this` is the concrete file, so that `read_dma_one()` reaches its own `read_dma()`
    /// statically.
    pub(crate) fn do_dma_read_bulk<F: FileImpl>(
        this: *const F,
        mut offset: u64,
        mut range_size: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the file outlives its pending operations.
        let base = unsafe { *(*this).file_impl_base() };
        let front = offset & (base._disk_read_dma_alignment as u64 - 1);
        offset -= front;
        range_size += front as usize;

        let rstate = make_lw_shared(FileReadState::<u8>::new(
            offset,
            front,
            range_size,
            base._memory_dma_alignment as usize,
            base._disk_read_dma_alignment as usize,
            intent,
        ));
        // SAFETY: the state lives in its shared allocation and never moves from there.
        unsafe { Pin::new_unchecked(&mut *rstate.get()) }.bind_intent();

        //
        // First, try to read directly into the buffer. Most of the reads will
        // end here.
        //
        // SAFETY: as above; the buffer is owned by the state, which the continuation keeps.
        let (dst, dst_len) =
            unsafe { ((*rstate.get()).buf.get_write(), (*rstate.get()).buf.size()) };
        let read = Self::read_dma_one(this, offset, dst, dst_len, pc, intent);
        read.then(move |size: usize| {
            // SAFETY: the shared state is alive while `rstate` is.
            unsafe { (*rstate.get()).pos = size };

            //
            // If we haven't read all required data at once -
            // start read-copy sequence. We can't continue with direct reads
            // into the previously allocated buffer here since we have to ensure
            // the aligned read length and thus the aligned destination buffer
            // size.
            //
            // The copying will actually take place only if there was a HW glitch.
            // In EOF case or in case of a persistent I/O error the only overhead is
            // an extra allocation.
            //
            let rs1 = rstate.clone();
            let rs2 = rstate.clone();
            do_until(
                move || rs1.done(),
                move || -> Future<()> {
                    // SAFETY: as above.
                    let rs = unsafe { &mut *rs2.get() };
                    let intent = match rs.get_intent() {
                        Ok(i) => i,
                        Err(e) => return make_exception_future(e),
                    };
                    let rs3 = rs2.clone();
                    Self::read_maybe_eof(this, rs.cur_offset(), rs.left_to_read(), pc, intent).then(
                        move |mut buf1: TemporaryBuffer<u8>| {
                            // SAFETY: as above.
                            let rs = unsafe { &mut *rs3.get() };
                            if buf1.size() != 0 {
                                rs.append_new_data(&mut buf1);
                            } else {
                                rs.eof = true;
                            }

                            make_ready_future(())
                        },
                    )
                },
            )
            .then(move |()| {
                // SAFETY: as above.
                let rs = unsafe { &mut *rstate.get() };
                //
                // If we are here we are promised to have read some bytes beyond
                // "front" so we may trim straight away.
                //
                rs.trim_buf_before_ret();
                make_ready_future(std::mem::take(&mut rs.buf))
            })
        })
    }

    /// Reads `len` bytes at `pos` where a previous short read stopped, and checks for end of
    /// file; resolves with the data, or an empty buffer if `pos` is at or beyond end of file.
    ///
    /// Short reads due to I/O errors always end at an address aligned to the hardware block
    /// size, so a new read from the next position gets an error (other than `EINVAL`). A
    /// short read at end of file makes that read return either a zero-length success (file
    /// size aligned to the block size) or `EINVAL` (file size not aligned), which both mean
    /// end of file.
    ///
    /// # Errors
    /// The I/O error of the read.
    ///
    /// Try to read from the given position where the previous short read has
    /// stopped. Check the EOF condition (see file_impl.rs).
    fn read_maybe_eof<F: FileImpl>(
        this: *const F,
        pos: u64,
        len: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: the file outlives its pending operations.
        let base = unsafe { *(*this).file_impl_base() };
        //
        // We have to allocate a new aligned buffer to make sure we don't get
        // an EINVAL error due to unaligned destination buffer.
        //
        let mut buf = TemporaryBuffer::<u8>::aligned(
            base._memory_dma_alignment as usize,
            align_up(len, base._disk_read_dma_alignment as usize),
        );

        // try to read a single bulk from the given position
        let dst = buf.get_write();
        let buf_size = buf.size();
        Self::read_dma_one(this, pos, dst, buf_size, pc, intent).then_wrapped(
            move |f: Future<usize>| -> Result<TemporaryBuffer<u8>, ExceptionPtr> {
                match f.get() {
                    Ok(size) => {
                        buf.trim(size);

                        Ok(buf)
                    }
                    Err(e) => {
                        //
                        // TODO: implement a non-trowing file_impl::dma_read() interface to
                        //       avoid the exceptions throwing in a good flow completely.
                        //       Otherwise for users that don't want to care about the
                        //       underlying file size and preventing the attempts to read
                        //       bytes beyond EOF there will always be at least one
                        //       exception throwing at the file end for files with unaligned
                        //       length.
                        //
                        if e.downcast_ref::<SystemError>().is_some_and(|se| se.code() == EINVAL) {
                            buf.trim(0);
                            Ok(buf)
                        } else {
                            Err(e)
                        }
                    }
                }
            },
        )
    }

    /// Reads once with the concrete file's `read_dma`.
    #[inline]
    fn read_dma_one<F: FileImpl>(
        this: *const F,
        pos: u64,
        buffer: *mut u8,
        len: usize,
        _pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: the file outlives its pending operations.
        unsafe { (*this).read_dma(pos, buffer, len, intent) }
    }
}

/// Whether the kernel supports `RWF_NOWAIT` on generic block devices (4.13 or later).
static BLOCKDEV_GEN_NOWAIT_WORKS: LazyLock<bool> =
    LazyLock::new(|| kernel_uname().whitelisted(&["4.13"]));
/// Whether the kernel supports `RWF_NOWAIT` on MD devices (5.17 or later).
static BLOCKDEV_MD_NOWAIT_WORKS: LazyLock<bool> =
    LazyLock::new(|| kernel_uname().whitelisted(&["5.17"]));

/// Whether `RWF_NOWAIT` works on the block device `device_id`.
fn blockdev_nowait_works(device_id: u64) -> bool {
    if major(device_id) == MD_MAJOR {
        return *BLOCKDEV_MD_NOWAIT_WORKS;
    }

    *BLOCKDEV_GEN_NOWAIT_WORKS
}

impl BlockdevFileImpl {
    /// A block device file over `fd` with block size `block_size`.
    pub fn new(
        fd: i32,
        f: OpenFlags,
        options: FileOpenOptions,
        device_id: u64,
        _block_size: usize,
    ) -> Self {
        // FIXME -- configure file_impl::_..._dma_alignment's from block_size
        BlockdevFileImpl {
            base: PosixFileImpl::new(fd, f, options, device_id, blockdev_nowait_works(device_id)),
        }
    }

    /// Truncation is not supported on a block device: does nothing.
    pub(crate) fn truncate_impl(&self, _length: u64) -> Future<()> {
        make_ready_future(())
    }

    /// Discards a range of the device with the `BLKDISCARD` ioctl.
    pub(crate) fn discard_impl(&self, offset: u64, length: u64) -> Future<()> {
        let fd = self.base._fd.get();
        thread_pool()
            .submit::<SyscallResult<i32>, _>(move || {
                let mut range: [u64; 2] = [offset, length];
                // SAFETY: BLKDISCARD reads two u64.
                wrap_syscall::<i32>(unsafe { c_ioctl(fd, BLKDISCARD, &mut range as *mut [u64; 2]) })
            })
            .then(|sr: SyscallResult<i32>| sr.throw_if_error())
    }

    /// Allocation does nothing on a block device: the space always exists.
    pub(crate) fn allocate_impl(&self, _position: u64, _length: u64) -> Future<()> {
        // nothing to do for block device
        make_ready_future(())
    }

    /// Writes `len` bytes from `buffer` at `pos`.
    #[inline]
    pub(crate) fn write_dma_pc(
        &self,
        pos: u64,
        buffer: *const u8,
        len: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.base.do_write_dma(pos, buffer, len, pc, intent)
    }

    /// Writes the buffers of `iov` at `pos`.
    #[inline]
    pub(crate) fn write_dma_iov_pc(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.base.do_write_dma_iov(pos, iov, pc, intent)
    }

    /// Reads `len` bytes at `pos` into `buffer`.
    #[inline]
    pub(crate) fn read_dma_pc(
        &self,
        pos: u64,
        buffer: *mut u8,
        len: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.base.do_read_dma(pos, buffer, len, pc, intent)
    }

    /// Reads at `pos` into the buffers of `iov`.
    #[inline]
    pub(crate) fn read_dma_iov_pc(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        self.base.do_read_dma_iov(pos, iov, pc, intent)
    }

    /// Reads `range_size` bytes at `offset` into a new buffer.
    #[inline]
    pub(crate) fn dma_read_bulk_pc(
        &self,
        offset: u64,
        range_size: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        PosixFileImpl::do_dma_read_bulk(self as *const Self, offset, range_size, pc, intent)
    }
}

impl AppendChallengedPosixFileImpl {
    /// A file over `fd` on an append-challenged filesystem described by `fsi`; it starts
    /// with the logical and committed sizes of the file.
    pub fn new(
        fd: i32,
        f: OpenFlags,
        options: FileOpenOptions,
        fsi: &internal::FsInfo,
        device_id: u64,
    ) -> Result<Self, ExceptionPtr> {
        let base = PosixFileImpl::with_fs_info(fd, f, options, device_id, fsi);
        let sloppy_size_hint =
            align_up::<u64>(options.sloppy_size_hint, base._base._disk_write_dma_alignment as u64);
        // SAFETY: plain system call on the descriptor `base` owns.
        let r = unsafe { c_lseek(fd, 0, SEEK_END) };
        throw_system_error_on(r == -1, "")?;
        Ok(AppendChallengedPosixFileImpl {
            base,
            _esft: Default::default(),
            _committed_size: Cell::new(r as u64),
            _logical_size: Cell::new(r as u64),
            _q: UnsafeCell::new(VecDeque::new()),
            _max_size_changing_ops: fsi.append_concurrency,
            _current_non_size_changing_ops: Cell::new(0),
            _current_size_changing_ops: Cell::new(0),
            _fsync_is_exclusive: fsi.fsync_is_exclusive,
            _closing_state: Cell::new(State::Open),
            _sloppy_size: options.sloppy_size,
            _sloppy_size_hint: sloppy_size_hint,
            _completed: UnsafeCell::new(Promise::new()),
        })
    }
}

impl Drop for AppendChallengedPosixFileImpl {
    /// Asserts that the file was closed and its queue is empty.
    fn drop(&mut self) {
        // If the file has not been closed we risk having running tasks
        // that will try to access freed memory.
        //
        // It is safe to destory it if nothing is queued.
        // Note that posix_file_impl::~posix_file_impl auto-closes the file descriptor.
        crate::engine::core::reactor_backend::engine_assert!(
            self._q.get_mut().is_empty()
                && (self._logical_size.get() == self._committed_size.get()
                    || self._closing_state.get() == State::Closed)
        );
    }
}

impl AppendChallengedPosixFileImpl {
    /// Whether `candidate` must run with no other operation in flight.
    #[inline]
    pub(crate) fn must_run_alone(&self, candidate: &Op) -> bool {
        // checks if candidate is a non-write, size-changing operation.
        (candidate.type_ == Opcode::Truncate)
            || (candidate.type_ == Opcode::Allocate)
            || (candidate.type_ == Opcode::Flush && (self._fsync_is_exclusive || self._sloppy_size))
    }

    /// Whether `candidate` is a write past the committed size.
    #[inline]
    pub(crate) fn appending_write(&self, candidate: &Op) -> bool {
        (candidate.type_ == Opcode::Write)
            && (candidate.pos + candidate.len as u64 > self._committed_size.get())
    }

    /// Whether `candidate` may change the file size.
    #[inline]
    pub(crate) fn size_changing(&self, candidate: &Op) -> bool {
        self.appending_write(candidate) || self.must_run_alone(candidate)
    }

    /// Whether `candidate` can be dispatched now, given the operations in flight.
    #[inline]
    pub(crate) fn may_dispatch(&self, candidate: &Op) -> bool {
        if self.size_changing(candidate) {
            self._current_size_changing_ops.get() == 0
                && self._current_non_size_changing_ops.get() == 0
        } else {
            self._current_size_changing_ops.get() == 0
        }
    }

    /// Starts `candidate` and accounts it in flight; its completion updates the committed
    /// size and processes the queue again.
    pub(crate) fn dispatch(&self, candidate: Op) {
        let op_counter: *const Cell<u32> = if self.size_changing(&candidate) {
            &self._current_size_changing_ops
        } else {
            &self._current_non_size_changing_ops
        };
        // SAFETY: the counter is a field of this file.
        unsafe { (*op_counter).set((*op_counter).get() + 1) };
        // SAFETY: the file was created by `make_shared_esft` (see `make_file_impl`).
        let me = unsafe { SharedFromThis::shared_from_this(self as *const Self) };
        // FIXME: future is discarded
        let _ = candidate.run.run().then(move |()| {
            // SAFETY: `me` keeps the file, and so the counter, alive; the file is reached by
            // `&` only.
            unsafe {
                (*op_counter).set((*op_counter).get() - 1);
                (*me.get().unwrap_unchecked().as_ptr()).process_queue();
            }
        });
    }

    /// Truncates the file to `length` synchronously, in the syscall thread.
    pub(crate) fn truncate_sync(&self, length: u64) -> i32 {
        // SAFETY: plain system call on our descriptor.
        let r = unsafe { c_ftruncate(self.base._fd.get(), length as i64) };
        if r != -1 {
            self._committed_size.set(length);
        }
        r
    }

    /// Truncates the file to its logical size.
    ///
    /// # Errors
    /// The system error of the truncation.
    pub(crate) fn truncate_to_logical_size(&self) -> Result<(), ExceptionPtr> {
        let r = self.truncate_sync(self._logical_size.get());
        if r == -1 {
            return Err(ExceptionPtr::from(SystemError::new(errno(), "truncate")));
        }
        Ok(())
    }

    // If we have a bunch of size-extending writes in the queue,
    // issue an ftruncate() extending the file size, so they can
    // be issued concurrently.
    /// When nothing is in flight and several size-extending writes are queued, issues one
    /// truncation that extends the file to their final size, so that they can then run
    /// concurrently.
    pub(crate) fn optimize_queue(&self) {
        if self._current_non_size_changing_ops.get() != 0
            || self._current_size_changing_ops.get() != 0
        {
            // Can't issue an ftruncate() if something is going on
            return;
        }
        let mut speculative_size = self._committed_size.get();
        let mut n_appending_writes: u32 = 0;
        // SAFETY: a short shared access to the queue; nothing runs an operation meanwhile.
        for op in unsafe { (*self._q.get()).iter() } {
            // stop calculating speculative size after a non-write, size-changing
            // operation is found to prevent an useless truncate from being issued.
            if self.must_run_alone(op) {
                break;
            }

            if self.appending_write(op) {
                speculative_size = speculative_size.max(op.pos + op.len as u64);
                n_appending_writes += 1;
            }
        }
        if n_appending_writes > self._max_size_changing_ops
            || (n_appending_writes != 0 && self._sloppy_size)
        {
            if self._sloppy_size {
                if self._committed_size.get() == 0 {
                    speculative_size = speculative_size.max(self._sloppy_size_hint);
                } else if speculative_size < 2 * self._committed_size.get() {
                    speculative_size = align_up::<u64>(
                        2 * self._committed_size.get(),
                        self.base._base._disk_write_dma_alignment as u64,
                    );
                }
            }
            // We're all alone, so issuing the ftruncate() in the reactor
            // thread won't block us.
            //
            // Issuing it in the syscall thread is too slow; this can happen
            // every several ops, and the syscall thread latency can be very
            // high.

            self.truncate_sync(speculative_size);
            // If we failed, the next write will pick it up.
        }
    }

    /// Dispatches the queued operations that may run now, in order.
    pub(crate) fn process_queue(&self) {
        self.optimize_queue();
        // Each access to the queue ends before dispatch() runs the operation, which may
        // re-enter the file and queue another one.
        loop {
            // SAFETY: a short access to the queue.
            let candidate = unsafe {
                let q = &mut *self._q.get();
                match q.front() {
                    Some(front) if self.may_dispatch(front) => q.pop_front(),
                    _ => None,
                }
            };
            let Some(candidate) = candidate else { break };
            self.dispatch(candidate);
        }
        if self.may_quit() {
            // SAFETY: a short access to the promise; setting a value runs no continuation.
            unsafe { (*self._completed.get()).set_value(()) };
            self._closing_state.set(State::Closing); // prevents _completed to be signaled again in case of recursion
        }
    }

    /// Appends `op` to the queue and processes it.
    #[inline]
    pub(crate) fn enqueue_op(&self, op: Op) {
        // SAFETY: a short access to the queue.
        unsafe { (*self._q.get()).push_back(op) };
        self.process_queue();
    }

    /// Whether closing may finish: closing and no operation queued or in flight.
    #[inline]
    pub(crate) fn may_quit(&self) -> bool {
        // SAFETY: a short shared access to the queue.
        self._closing_state.get() == State::Draining
            && unsafe { (*self._q.get()).is_empty() }
            && self._current_non_size_changing_ops.get() == 0
            && self._current_size_changing_ops.get() == 0
    }

    /// Records that the kernel's file size reached `size`.
    #[inline]
    pub(crate) fn commit_size(&self, size: u64) {
        self._committed_size.set(size.max(self._committed_size.get()));
        self._logical_size.set(size.max(self._logical_size.get()));
    }

    /// Reads `len` bytes at `pos` into `buffer`; a read at or past the logical size
    /// resolves with 0 without I/O, and a read across it is shortened to the logical size
    /// rounded up to the read alignment.
    pub(crate) fn read_dma_pc(
        &self,
        pos: u64,
        buffer: *mut u8,
        mut len: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        if pos >= self._logical_size.get() {
            // yield() avoids tail recursion
            return yield_().then(|()| make_ready_future(0usize));
        }
        len = ((pos + len as u64).min(align_up::<u64>(
            self._logical_size.get(),
            self.base._base._disk_read_dma_alignment as u64,
        )) - pos) as usize;
        let this = self as *const Self;
        self.enqueue::<usize, _, _>(Opcode::Read, pos, len, intent, move |iref| {
            match iref.retrieve() {
                // SAFETY: the file outlives its queued operations (it drains them on close).
                Ok(i) => unsafe { (*this).base.do_read_dma(pos, buffer, len, pc, i) },
                Err(e) => make_exception_future(e),
            }
        })
    }

    /// Reads at `pos` into the buffers of `iov`, shortened at the logical size as
    /// [`AppendChallengedPosixFileImpl::read_dma`].
    pub(crate) fn read_dma_iov_pc(
        &self,
        pos: u64,
        mut iov: Vec<Iovec>,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        if pos >= self._logical_size.get() {
            // yield() avoids tail recursion
            return yield_().then(|()| make_ready_future(0usize));
        }
        let mut len: usize = 0;
        let mut i = 0usize;
        let aligned_logical_size = align_up::<u64>(
            self._logical_size.get(),
            self.base._base._disk_read_dma_alignment as u64,
        );
        while i != iov.len() && pos + len as u64 + iov[i].iov_len as u64 <= aligned_logical_size {
            len += iov[i].iov_len;
            i += 1;
        }
        if i != iov.len() {
            let last_len = pos + len as u64 + iov[i].iov_len as u64 - aligned_logical_size;
            if last_len != 0 {
                iov[i].iov_len = last_len as usize;
                i += 1;
            }
            iov.truncate(i);
        }
        let this = self as *const Self;
        self.enqueue::<usize, _, _>(Opcode::Read, pos, len, intent, move |iref| {
            match iref.retrieve() {
                // SAFETY: the file outlives its queued operations.
                Ok(i) => unsafe { (*this).base.do_read_dma_iov(pos, iov, pc, i) },
                Err(e) => make_exception_future(e),
            }
        })
    }

    /// Queues a write of `len` bytes from `buffer` at `pos`, extending the logical size.
    pub(crate) fn write_dma_pc(
        &self,
        pos: u64,
        buffer: *const u8,
        len: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        let this = self as *const Self;
        self.enqueue::<usize, _, _>(Opcode::Write, pos, len, intent, move |iref| {
            let i = match iref.retrieve() {
                Ok(i) => i,
                Err(e) => return make_exception_future(e),
            };
            // SAFETY: the file outlives its queued operations.
            unsafe { (*this).base.do_write_dma(pos, buffer, len, pc, i) }.then(move |ret: usize| {
                // SAFETY: as above.
                unsafe { (*this).commit_size(pos + ret as u64) };
                make_ready_future(ret)
            })
        })
    }

    /// Queues a write of the buffers of `iov` at `pos`, extending the logical size.
    pub(crate) fn write_dma_iov_pc(
        &self,
        pos: u64,
        iov: Vec<Iovec>,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        let len = iovec_len(&iov);
        let this = self as *const Self;
        self.enqueue::<usize, _, _>(Opcode::Write, pos, len, intent, move |iref| {
            let i = match iref.retrieve() {
                Ok(i) => i,
                Err(e) => return make_exception_future(e),
            };
            // SAFETY: the file outlives its queued operations.
            unsafe { (*this).base.do_write_dma_iov(pos, iov, pc, i) }.then(move |ret: usize| {
                // SAFETY: as above.
                unsafe { (*this).commit_size(pos + ret as u64) };
                make_ready_future(ret)
            })
        })
    }

    /// Reads `range_size` bytes at `offset` into a new buffer.
    #[inline]
    pub(crate) fn dma_read_bulk_pc(
        &self,
        offset: u64,
        range_size: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        PosixFileImpl::do_dma_read_bulk(self as *const Self, offset, range_size, pc, intent)
    }

    /// Makes written data durable. When the logical size is ahead of the committed one, or
    /// fsync blocks other operations, the flush is queued as an operation that first
    /// truncates the file to its logical size.
    pub(crate) fn flush_impl(&self) -> Future<()> {
        if (!self._sloppy_size || self._logical_size.get() == self._committed_size.get())
            && !self._fsync_is_exclusive
        {
            // FIXME: determine if flush can block concurrent reads or writes
            self.base.flush()
        } else {
            let this = self as *const Self;
            self.enqueue::<(), _, _>(Opcode::Flush, 0, 0, ptr::null_mut(), move |_| -> Future<()> {
                // SAFETY: the file outlives its queued operations.
                let me = unsafe { &*this };
                if me._logical_size.get() != me._committed_size.get() {
                    // We're all alone, so can truncate in reactor thread
                    if let Err(e) = me.truncate_to_logical_size() {
                        return make_exception_future(e);
                    }
                }
                me.base.flush()
            })
        }
    }

    /// Queues an allocation of a range of the file.
    pub(crate) fn allocate_impl(&self, position: u64, length: u64) -> Future<()> {
        let this = self as *const Self;
        self.enqueue::<(), _, _>(
            Opcode::Allocate,
            position,
            length as usize,
            ptr::null_mut(),
            move |_| {
                // SAFETY: the file outlives its queued operations.
                unsafe { (*this).base.allocate(position, length) }
            },
        )
    }

    /// The file's `stat` information, with the logical size.
    pub(crate) fn stat_impl(&self) -> Future<Stat> {
        let this = self as *const Self;
        // FIXME: can this conflict with anything?
        self.base.stat().then(move |mut stat: Stat| {
            // SAFETY: the file outlives its pending operations.
            stat.st_size = unsafe { (*this)._logical_size.get() } as i64;
            make_ready_future(stat)
        })
    }

    /// Queues a truncation to `length`, which becomes the logical size.
    pub(crate) fn truncate_impl(&self, length: u64) -> Future<()> {
        let this = self as *const Self;
        self.enqueue::<(), _, _>(Opcode::Truncate, length, 0, ptr::null_mut(), move |_| {
            // SAFETY: the file outlives its queued operations.
            unsafe { (*this).base.truncate(length) }.then(move |()| {
                // SAFETY: as above.
                unsafe {
                    (*this)._committed_size.set(length);
                    (*this)._logical_size.set(length);
                }
            })
        })
    }

    /// The logical size of the file.
    #[inline]
    pub(crate) fn size_impl(&self) -> Future<u64> {
        make_ready_future(self._logical_size.get())
    }

    /// Drains the queued operations, truncates the file to its logical size if needed and
    /// closes the descriptor.
    pub(crate) fn close_impl(&self) -> Future<()> {
        // Caller should have drained all pending I/O
        self._closing_state.set(State::Draining);
        self.process_queue();
        let this = self as *const Self;
        // SAFETY: a short access to the promise.
        let completed = unsafe { (*self._completed.get()).get_future() };
        completed
            .then(move |()| -> Result<(), ExceptionPtr> {
                // SAFETY: the caller keeps the file alive until close() resolves.
                let me = unsafe { &*this };
                if me._logical_size.get() != me._committed_size.get() {
                    me.truncate_to_logical_size()?;
                }
                Ok(())
            })
            .then_wrapped(move |f: Future<()>| {
                if f.failed() {
                    report_exception(
                        "Closing append_challenged_posix_file_impl failed.",
                        &f.get_exception(),
                    );
                }
                // SAFETY: as above.
                unsafe { (*this).base.close() }.finally(move || {
                    // SAFETY: as above.
                    unsafe { (*this)._closing_state.set(State::Closed) };
                })
            })
    }
}

impl Drop for PosixFileHandleImpl {
    /// Closes the descriptor when this was its last reference.
    fn drop(&mut self) {
        if !self._refcount.is_null()
            // SAFETY: a non-null refcount is shared by the dups of this fd.
            && unsafe { (*self._refcount).fetch_sub(1, Ordering::Relaxed) } == 1
        {
            // SAFETY: we were the last holder of the descriptor and the counter.
            unsafe {
                c_close(self._fd);
                drop(Box::from_raw(self._refcount));
            }
        }
    }
}

impl FileHandleImpl for PosixFileHandleImpl {
    /// A new handle sharing the descriptor and its reference count.
    fn clone_handle(&self) -> Box<dyn FileHandleImpl> {
        let ret = Box::new(PosixFileHandleImpl::new(
            self._fd,
            self._open_flags,
            self._refcount,
            self._device_id,
            self._memory_dma_alignment,
            self._disk_read_dma_alignment,
            self._disk_write_dma_alignment,
            self._disk_overwrite_dma_alignment,
            self._nowait_works,
        ));
        if !self._refcount.is_null() {
            // SAFETY: the counter is alive while a holder exists.
            unsafe { (*self._refcount).fetch_add(1, Ordering::Relaxed) };
        }
        ret
    }

    /// A file implementation on the current shard over the shared descriptor.
    fn to_file(mut self: Box<Self>) -> SharedPtr<dyn FileImpl> {
        let ret = make_shared(PosixFileReal::from_dup(
            self._fd,
            self._open_flags,
            self._refcount,
            self._device_id,
            self._memory_dma_alignment,
            self._disk_read_dma_alignment,
            self._disk_write_dma_alignment,
            self._disk_overwrite_dma_alignment,
            self._nowait_works,
        ));
        self._fd = -1;
        self._refcount = ptr::null_mut();
        upcast(ret)
    }
}

/// Upcasts a concrete shared file implementation to the trait object.
#[inline]
pub(crate) fn upcast<T: FileImpl>(p: SharedPtr<T>) -> SharedPtr<dyn FileImpl> {
    // SAFETY: the cast keeps the pointer into the same object.
    unsafe { static_pointer_cast(&p, |x: *mut T| x as *mut dyn FileImpl) }
}

// Some kernels can append to xfs filesystems, some cannot; determine
// from kernel version.
/// How many size-changing operations XFS lets run at once on the running kernel.
fn xfs_concurrency_from_kernel_version() -> u32 {
    // try to see if this is a mainline kernel with xfs append fixed (3.15+)
    // or a RHEL kernel with the backported fix (3.10.0-325.el7+)
    if kernel_uname().whitelisted(&["3.15", "3.10.0-325.el7"]) {
        // Can append, but not concurrently
        return 1;
    }
    // Cannot append at all; need ftrucnate().
    0
}

/// The XFS concurrency of the running kernel, computed once.
static XFS_CONCURRENCY: LazyLock<u32> = LazyLock::new(xfs_concurrency_from_kernel_version);

thread_local! {
    /// Filesystem information cached per device, per thread.
    static S_FSTYPE: UnsafeCell<HashMap<u64, internal::FsInfo>> = UnsafeCell::new(HashMap::new());
}

/// Makes the file implementation for the open descriptor `fd`: a block device file, a
/// file on an append-challenged filesystem, or a regular one, configured from the
/// filesystem of `st` (cached per device).
pub fn make_file_impl(
    fd: i32,
    options: FileOpenOptions,
    flags: i32,
    st: Stat,
) -> Future<SharedPtr<dyn FileImpl>> {
    if s_isblk(st.st_mode) {
        let mut block_size: usize = 0;
        // SAFETY: BLKBSZGET writes a size_t.
        let ret = unsafe { c_ioctl(fd, BLKBSZGET, &mut block_size as *mut usize) };
        if ret == -1 {
            return make_exception_future(ExceptionPtr::from(SystemError::new(
                errno(),
                "ioctl(BLKBSZGET) failed",
            )));
        }
        return make_ready_future(upcast(make_shared(BlockdevFileImpl::new(
            fd,
            OpenFlags::from_bits(flags),
            options,
            st.st_rdev,
            block_size,
        ))));
    }

    if s_isdir(st.st_mode) {
        // Directories don't care about block size, so we need not
        // query it here. Just provide something reasonable.
        let fsi = internal::FsInfo { block_size: 4096, nowait_works: false, ..Default::default() };
        return make_ready_future(upcast(make_shared(PosixFileReal::new(
            fd,
            OpenFlags::from_bits(flags),
            options,
            &fsi,
            st.st_dev,
        ))));
    }

    let st_dev = st.st_dev;

    // SAFETY: the map is thread-local and not borrowed across calls.
    let found = S_FSTYPE.with(|m| unsafe { (*m.get()).get(&st_dev).copied() });
    let Some(fsi) = found else {
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        return unsafe { (*engine()).fstatfs(fd) }.then(move |sfs: Statfs| {
            let mut fsi = internal::FsInfo { block_size: sfs.f_bsize as u32, ..Default::default() };
            match sfs.f_type as u64 {
                FsMagic::XFS => {
                    let mut da = Dioattr::default();
                    // SAFETY: XFS_IOC_DIOINFO writes a dioattr.
                    if unsafe { c_ioctl(fd, XFS_IOC_DIOINFO, &mut da as *mut Dioattr) } == 0 {
                        fsi.dioinfo = Some(da);
                    }

                    fsi.append_challenged = true;
                    fsi.append_concurrency = *XFS_CONCURRENCY;
                    fsi.fsync_is_exclusive = true;
                    fsi.nowait_works = kernel_uname().whitelisted(&["4.13"]);
                }
                FsMagic::NFS => {
                    fsi.append_challenged = false;
                    fsi.append_concurrency = 0;
                    fsi.fsync_is_exclusive = false;
                    fsi.nowait_works = kernel_uname().whitelisted(&["4.13"]);
                }
                FsMagic::EXT4 => {
                    fsi.append_challenged = true;
                    fsi.append_concurrency = 0;
                    fsi.fsync_is_exclusive = false;
                    fsi.nowait_works = kernel_uname().whitelisted(&["5.5"]);
                }
                FsMagic::BTRFS => {
                    fsi.append_challenged = true;
                    fsi.append_concurrency = 0;
                    fsi.fsync_is_exclusive = true;
                    fsi.nowait_works = kernel_uname().whitelisted(&["5.9"]);
                }
                FsMagic::TMPFS | FsMagic::FUSE => {
                    fsi.append_challenged = false;
                    fsi.append_concurrency = 999;
                    fsi.fsync_is_exclusive = false;
                    fsi.nowait_works = false;
                }
                _ => {
                    fsi.append_challenged = true;
                    fsi.append_concurrency = 0;
                    fsi.fsync_is_exclusive = true;
                    fsi.nowait_works = false;
                }
            }
            // SAFETY: as above.
            S_FSTYPE.with(|m| unsafe { (*m.get()).insert(st.st_dev, fsi) });
            make_file_impl(fd, options, flags, st)
        });
    };

    if !fsi.append_challenged || options.append_is_unlikely || ((flags & O_ACCMODE) == O_RDONLY) {
        return make_ready_future(upcast(make_shared(PosixFileReal::new(
            fd,
            OpenFlags::from_bits(flags),
            options,
            &fsi,
            st_dev,
        ))));
    }
    match AppendChallengedPosixFileImpl::new(fd, OpenFlags::from_bits(flags), options, &fsi, st_dev)
    {
        Ok(f) => make_ready_future(upcast(make_shared_esft(f))),
        Err(e) => make_exception_future(e),
    }
}

impl File {
    /// A file on this shard from a [`FileHandle`] obtained from another shard.
    #[inline]
    pub fn from_handle(handle: FileHandle) -> Self {
        handle.into_file()
    }

    /// Gets the file size.
    pub fn size(&self) -> Future<u64> {
        self.impl_().size()
    }

    /// Closes the file.
    ///
    /// Flushes any pending operations and release any resources associated with
    /// the file (except for stable storage). Resets the file object back to
    /// uninitialized state as if by assigning file() to it.
    ///
    ///
    /// `close()` never fails: it reports errors and swallows them. To ensure the data
    /// reaches stable storage, call [`File::flush`] before closing.
    pub fn close(&mut self) -> Future<()> {
        let Some(f) = self._file_impl.take() else {
            panic!("use of an uninitialized teseladb::file");
        };
        // SAFETY: as in `impl_`.
        let fut = unsafe { f.get_mut_unchecked() }.close();
        fut.handle_exception(move |ex: ExceptionPtr| {
            let _f = f;
            report_exception("Closing the file failed unexpectedly", &ex);
        })
    }

    /// Returns a directory listing, given that this file object is a directory.
    pub fn list_directory<Next: FnMut(DirectoryEntry) -> Future<()> + 'static>(
        &mut self,
        next: Next,
    ) -> Subscription<DirectoryEntry> {
        self.impl_().list_directory(Box::new(next))
    }

    /// Returns a directory listing, given that this file object is a directory.
    /// Lists the directory as a generator of entries. Buffered generators cannot be
    /// used yet (an open issue of the generator).
    pub fn experimental_list_directory(&mut self) -> Generator<DirectoryEntry> {
        self.impl_().experimental_list_directory()
    }

    /// Generic ioctl syscall support for special file handling.
    ///
    /// This interface is useful for many non-standard operations on teseladb::file.
    /// The examples can be - querying device or file system capabilities,
    /// configuring special performance or access modes on devices etc.
    /// Refer ioctl(2) man page for more details.
    ///
    /// `cmd`: ioctl command to be executed
    /// `argp`: pointer to the buffer which holds the argument
    ///
    /// Returns a future containing the return value if any, or an exceptional future
    ///         if the operation has failed.
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`]: `intent` (or `argp`/`arg`) as
    /// described there.
    pub unsafe fn ioctl(&mut self, cmd: u64, argp: *mut c_void) -> Future<i32> {
        // SAFETY: forwarded from the caller.
        unsafe { self.impl_().ioctl(cmd, argp) }
    }

    /// Performs a short ioctl syscall on teseladb::file
    ///
    /// This is similar to generic `ioctl`; the difference is, here user indicates
    /// that this operation is a short one, and does not involve any i/o or locking.
    /// The `file` module will process this differently from the normal [`File::ioctl`].
    /// Use this method only if the user is sure that the operation does not involve any
    /// blocking operation. If unsure, use the default [`File::ioctl`] method.
    /// Refer ioctl(2) man page for more details on ioctl operation.
    ///
    /// `cmd`: ioctl command to be executed
    /// `argp`: pointer to the buffer which holds the argument
    ///
    /// Returns a future containing the return value if any, or an exceptional future
    ///         if the operation has failed.
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`]: `intent` (or `argp`/`arg`) as
    /// described there.
    pub unsafe fn ioctl_short(&mut self, cmd: u64, argp: *mut c_void) -> Future<i32> {
        // SAFETY: forwarded from the caller.
        unsafe { self.impl_().ioctl_short(cmd, argp) }
    }

    /// Generic fcntl syscall support for special file handling.
    ///
    /// fcntl performs the operation specified by 'op' field on the file.
    /// Some of the use cases can be - setting file status flags, advisory record locking,
    /// managing signals, managing file leases or write hints etc.
    /// Refer fcntl(2) man page for more details.
    ///
    /// `op`: the operation to be executed
    /// `arg` is the optional argument (0 when unused).
    /// Returns a future containing the return value if any, or an exceptional future
    ///         if the operation has failed
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`]: `intent` (or `argp`/`arg`) as
    /// described there.
    pub unsafe fn fcntl(&mut self, op: i32, arg: usize) -> Future<i32> {
        // SAFETY: forwarded from the caller.
        unsafe { self.impl_().fcntl(op, arg) }
    }

    /// Performs a 'short' fcntl syscall on teseladb::file
    ///
    /// This is similar to generic `fcntl`; the difference is, here user indicates
    /// that this operation is a short one, and does not involve any i/o or locking.
    /// The `file` module will process this differently from normal [`File::fcntl`].
    /// Use this only if the user is sure that the operation does not involve any
    /// blocking operation. If unsure, use the default [`File::fcntl`] method.
    /// Refer fcntl(2) man page for more details on fcntl operation.
    ///
    /// `op`: the operation to be executed
    /// `arg`: the optional argument
    /// Returns a future containing the return value if any, or an exceptional future
    ///         if the operation has failed
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`]: `intent` (or `argp`/`arg`) as
    /// described there.
    pub unsafe fn fcntl_short(&mut self, op: i32, arg: usize) -> Future<i32> {
        // SAFETY: forwarded from the caller.
        unsafe { self.impl_().fcntl_short(op, arg) }
    }

    /// Sets the inode's write lifetime hint with the `fcntl` operation `op`.
    fn set_lifetime_hint_impl(&mut self, op: i32, hint: u64) -> Future<()> {
        let this = self as *mut File;
        do_with(hint, move |arg: &mut u64| {
            let argp = arg as *mut u64 as usize;
            // SAFETY: the caller keeps the file alive until the future resolves; `argp` is the
            // address of the `do_with` value, which lives until the future resolves.
            unsafe { (*this).impl_().fcntl(op, argp) }.then_wrapped(|f: Future<i32>| {
                // Need to handle return value differently from that of fcntl
                if f.failed() {
                    return make_exception_future(f.get_exception());
                }
                make_ready_future(())
            })
        })
    }

    /// Set a lifetime hint for the inode corresponding to teseladb::file
    ///
    /// Write lifetime  hints  can be used to inform the kernel about the relative
    /// expected lifetime of writes on a given inode or via open file descriptor.
    /// An application may use the different hint values to separate writes into different
    /// write classes, so that multiple users or applications running on a single storage back-end
    /// can aggregate their I/O  patterns in a consistent manner.
    /// Refer fcntl(2) man page for more details on write lifetime hints.
    ///
    /// `hint`: the hint value of the stream
    /// Returns future indicating success or failure
    pub fn set_inode_lifetime_hint(&mut self, hint: u64) -> Future<()> {
        self.set_lifetime_hint_impl(F_SET_RW_HINT, hint)
    }

    /// Reads the inode's write lifetime hint with the `fcntl` operation `op`.
    fn get_lifetime_hint_impl(&mut self, op: i32) -> Future<u64> {
        let this = self as *mut File;
        do_with(0u64, move |arg: &mut u64| {
            let argp = arg as *mut u64;
            // SAFETY: the caller keeps the file alive until the future resolves; `argp` is the
            // address of the `do_with` value, which lives until the future resolves.
            unsafe { (*this).impl_().fcntl(op, argp as usize) }.then_wrapped(
                move |f: Future<i32>| {
                    // Need to handle return value differently from that of fcntl
                    if f.failed() {
                        return make_exception_future(f.get_exception());
                    }
                    // SAFETY: do_with keeps `arg` alive until this future resolves.
                    make_ready_future(unsafe { *argp })
                },
            )
        })
    }

    /// Get the lifetime hint of the inode of teseladb::file which was set by
    /// [`File::set_inode_lifetime_hint`].
    ///
    /// Write lifetime  hints  can be used to inform the kernel about the relative
    /// expected lifetime of writes on a given inode or via open file descriptor.
    /// An application may use the different hint values to separate writes into different
    /// write classes, so that multiple users or applications running on a single storage back-end
    /// can aggregate their I/O  patterns in a consistent manner.
    /// Refer fcntl(2) man page for more details on write lifetime hints.
    ///
    /// Returns the hint value of the inode
    pub fn get_inode_lifetime_hint(&mut self) -> Future<u64> {
        self.get_lifetime_hint_impl(F_GET_RW_HINT)
    }

    /// Reads `range_size` bytes at `offset` with priority `pc`; see [`FileImpl::dma_read_bulk`].
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`].
    #[inline]
    pub(crate) unsafe fn dma_read_bulk_impl(
        &mut self,
        offset: u64,
        range_size: usize,
        _pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: forwarded from the caller.
        unsafe { self.impl_().dma_read_bulk(offset, range_size, intent) }
    }

    /// Discard unneeded data from the file.
    ///
    /// The discard operation tells the file system that a range of offsets
    /// (which be aligned) is no longer needed and can be reused.
    pub fn discard(&mut self, offset: u64, length: u64) -> Future<()> {
        self.impl_().discard(offset, length)
    }

    /// Preallocate disk blocks for a specified byte range.
    ///
    /// Requests the file system to allocate disk blocks to
    /// back the specified range (`length` bytes starting at
    /// `position`).  The range may be outside the current file
    /// size; the blocks can then be used when appending to the
    /// file.
    ///
    /// `position`: beginning of the range at which to allocate
    ///                 blocks.
    /// `length`: length of range to allocate.
    /// Returns future that becomes ready when the operation completes.
    pub fn allocate(&mut self, position: u64, length: u64) -> Future<()> {
        self.impl_().allocate(position, length)
    }

    /// Truncates the file to a specified length.
    pub fn truncate(&mut self, length: u64) -> Future<()> {
        self.impl_().truncate(length)
    }

    /// Returns `stat` information about the file.
    pub fn stat(&mut self) -> Future<Stat> {
        self.impl_().stat()
    }

    /// Causes any previously written data to be made stable on persistent storage.
    ///
    /// Prior to a flush, written data may or may not survive a power failure.  After
    /// a flush, data is guaranteed to be on disk.
    pub fn flush(&mut self) -> Future<()> {
        self.impl_().flush()
    }

    /// Writes the buffers of `iov` at `pos` with priority `pc`.
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`].
    #[inline]
    unsafe fn dma_write_impl_iov(
        &mut self,
        pos: u64,
        iov: Vec<Iovec>,
        _pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: forwarded from the caller.
        unsafe { self.impl_().write_dma_iov(pos, iov, intent) }
    }

    /// Writes `len` bytes from `buffer` at `pos` with priority `pc`.
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`].
    #[inline]
    pub(crate) unsafe fn dma_write_impl_ptr(
        &mut self,
        pos: u64,
        buffer: *const u8,
        len: usize,
        _pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: forwarded from the caller.
        unsafe { self.impl_().write_dma(pos, buffer, len, intent) }
    }

    /// Reads at `pos` into the buffers of `iov` with priority `pc`.
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`].
    #[inline]
    unsafe fn dma_read_impl_iov(
        &mut self,
        pos: u64,
        iov: Vec<Iovec>,
        _pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: forwarded from the caller.
        unsafe { self.impl_().read_dma_iov(pos, iov, intent) }
    }

    /// Reads exactly `len` bytes at `pos`, failing with [`EofError`] when the file ends
    /// first.
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`].
    unsafe fn dma_read_exactly_impl(
        &mut self,
        pos: u64,
        len: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: forwarded from the caller.
        unsafe { self.dma_read_impl(pos, len, pc, intent) }.then(
            move |buf: TemporaryBuffer<u8>| -> Result<TemporaryBuffer<u8>, ExceptionPtr> {
                if buf.size() < len {
                    return Err(ExceptionPtr::from(EofError));
                }

                Ok(buf)
            },
        )
    }

    /// Reads `len` bytes at `pos` with no alignment requirement into a new buffer.
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`].
    unsafe fn dma_read_impl(
        &mut self,
        pos: u64,
        len: usize,
        pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: forwarded from the caller.
        unsafe { self.dma_read_bulk_impl(pos, len, pc, intent) }.then(
            move |mut buf: TemporaryBuffer<u8>| {
                if len < buf.size() {
                    buf.trim(len);
                }

                make_ready_future(buf)
            },
        )
    }

    /// Reads `aligned_len` bytes at `aligned_pos` into `aligned_buffer`.
    ///
    /// # Safety
    /// As the raw-pointer operations of [`FileImpl`].
    #[inline]
    unsafe fn dma_read_impl_ptr(
        &mut self,
        aligned_pos: u64,
        aligned_buffer: *mut u8,
        aligned_len: usize,
        _pc: MaybePriorityClassRef,
        intent: *mut IoIntent,
    ) -> Future<usize> {
        // SAFETY: forwarded from the caller.
        unsafe { self.impl_().read_dma(aligned_pos, aligned_buffer, aligned_len, intent) }
    }

    /// Creates a handle that can be transported across shards.
    ///
    /// Creates a handle that can be transported across shards, and then
    /// used to create a new shard-local [`File`] that refers to
    /// the same on-disk file.
    ///
    /// Use it on read-only files.
    ///
    pub fn dup(&mut self) -> Result<FileHandle, ExceptionPtr> {
        Ok(FileHandle::from_impl(self.impl_().dup()?))
    }
}

/// The default directory generator: runs [`FileImpl::list_directory`] and yields its
/// entries.
fn make_list_directory_fallback_generator<T: FileImpl + ?Sized>(
    me: *const T,
) -> Generator<DirectoryEntry> {
    Generator::new(async move |y| {
        let mut ents: Queue<Option<DirectoryEntry>> = Queue::new(16);
        let ents_p = &mut ents as *mut Queue<Option<DirectoryEntry>>;
        let next = Box::new(move |de: DirectoryEntry| {
            // SAFETY: `ents` lives in the pinned coroutine frame until the end of the body,
            // after `done` (which waits for the lister) is awaited.
            unsafe { (*ents_p).push_eventually(Some(de)) }
        });
        // SAFETY: the implementation outlives the generator.
        let mut lister = unsafe { (*me).list_directory(next) };
        let done = lister.done().finally(move || {
            // SAFETY: as above.
            unsafe { (*ents_p).push_eventually(None) }
        });

        loop {
            // SAFETY: as above.
            let de = unsafe { (*ents_p).pop_eventually() }.await?;
            let Some(de) = de else {
                break;
            };
            y.yield_value(de).await;
        }

        done.await?;
        drop(lister);
        Ok(())
    })
}

/// Opens the file `name` for DMA with `flags` and default options.
#[inline]
pub fn open_file_dma(name: &str, flags: OpenFlags) -> Future<File> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).open_file_dma(name, flags, FileOpenOptions::default()) }
}

/// Opens the file `name` for DMA with `flags` and `options`.
#[inline]
pub fn open_file_dma_with_options(
    name: &str,
    flags: OpenFlags,
    options: FileOpenOptions,
) -> Future<File> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).open_file_dma(name, flags, options) }
}

/// Opens the directory `name`, for listing.
#[inline]
pub fn open_directory(name: &str) -> Future<File> {
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
    unsafe { (*engine()).open_directory(name) }
}

/// A file can be closed by the generic close helpers.
impl crate::engine::util::closeable::Closeable for File {
    #[inline]
    fn close(&mut self) -> Future<()> {
        File::close(self)
    }
}
