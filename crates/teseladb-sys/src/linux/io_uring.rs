//! io_uring (012 FR-001): `io_uring_setup(2)`, `io_uring_enter(2)` and `io_uring_register(2)`,
//! the mapping of the submission and completion queues, and the registration of buffers, files and
//! an `eventfd`.
//!
//! [`Ring`] is safe: every pointer an operation hands to the kernel points to memory the ring
//! keeps alive until the operation's completion is taken ([`Ring::complete`]). Paths are owned by
//! the ring while the operation is in flight, registered buffers belong to the ring and cannot be
//! borrowed while the kernel uses them, and a descriptor to close is given up by its owner.
//!
//! The network operations of 012 FR-024 and FR-026 live in [`net`]: multishot `accept` and `recv`
//! that complete several times per submission ([`Completion::more`]), the provided buffer rings the
//! kernel picks a `recv`'s buffer from ([`Completion::buffer`]), `send` and `sendmsg`, and
//! `IORING_OP_SEND_ZC`, whose buffer the kernel keeps until its notification
//! ([`Completion::notification`]).

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU32, Ordering, fence};

use crate::aligned::AlignedBuf;

pub mod net;

use net::{BufRing, MsgHdr};
pub use net::{Part, SendData};

const SYS_IO_URING_SETUP: i64 = 425;
const SYS_IO_URING_ENTER: i64 = 426;
const SYS_IO_URING_REGISTER: i64 = 427;

const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_SQES: i64 = 0x1000_0000;
const IORING_FEAT_SINGLE_MMAP: u32 = 1;
const IORING_ENTER_GETEVENTS: u32 = 1;
const IORING_ENTER_SQ_WAKEUP: u32 = 2;
const IORING_SETUP_SQPOLL: u32 = 2;
const IORING_SQ_NEED_WAKEUP: u32 = 1;
/// How long the kernel's polling thread of a ring spins without work before it sleeps
/// (012 FR-025): the next submission after that wakes it with one `io_uring_enter`.
pub const SQPOLL_IDLE_MS: u32 = 1000;

const IORING_REGISTER_BUFFERS: u32 = 0;
const IORING_REGISTER_FILES: u32 = 2;
const IORING_REGISTER_EVENTFD: u32 = 4;
const IORING_REGISTER_FILES_UPDATE: u32 = 6;
const IORING_REGISTER_PROBE: u32 = 8;
const IORING_REGISTER_PBUF_RING: u32 = 22;
const IO_URING_OP_SUPPORTED: u16 = 1;

const IOSQE_FIXED_FILE: u8 = 1;
const IOSQE_BUFFER_SELECT: u8 = 1 << 5;
/// `ioprio` of an `IORING_OP_ACCEPT` that accepts until cancelled.
const IORING_ACCEPT_MULTISHOT: u16 = 1;
/// `ioprio` of an `IORING_OP_RECV` that receives until the end of the stream or an error.
const IORING_RECV_MULTISHOT: u16 = 1 << 1;
/// `ioprio` of an `IORING_OP_SEND_ZC` that sends from a registered buffer.
const IORING_RECVSEND_FIXED_BUF: u16 = 1 << 2;
/// The completion carries a provided buffer's identifier in its upper 16 bits.
const IORING_CQE_F_BUFFER: u32 = 1;
/// More completions of the same operation follow.
const IORING_CQE_F_MORE: u32 = 1 << 1;
/// The notification of an `IORING_OP_SEND_ZC`: the kernel no longer uses the buffer.
const IORING_CQE_F_NOTIF: u32 = 1 << 3;
const IORING_CQE_BUFFER_SHIFT: u32 = 16;
const SOCK_CLOEXEC: u32 = 0o2_000_000;
/// No `SIGPIPE` on a closed connection, and the whole buffer goes out before the completion.
const MSG_NOSIGNAL: u32 = 0x4000;
const MSG_WAITALL: u32 = 0x100;
const IORING_FSYNC_DATASYNC: u32 = 1;
const AT_FDCWD: i32 = -100;
const AT_REMOVEDIR: u32 = 0x200;
const O_CLOEXEC: i32 = 0o2_000_000;

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const MAP_POPULATE: i32 = 0x8000;
const EINTR: i32 = 4;
const EBUSY: i32 = 16;
const EAGAIN: i32 = 11;

/// Operation codes of `enum io_uring_op` that TeselaDB uses.
pub mod opcode {
    /// `IORING_OP_NOP`.
    pub const NOP: u8 = 0;
    /// `IORING_OP_FSYNC`.
    pub const FSYNC: u8 = 3;
    /// `IORING_OP_SENDMSG`.
    pub const SENDMSG: u8 = 9;
    /// `IORING_OP_ACCEPT`.
    pub const ACCEPT: u8 = 13;
    /// `IORING_OP_ASYNC_CANCEL`.
    pub const ASYNC_CANCEL: u8 = 14;
    /// `IORING_OP_SEND`.
    pub const SEND: u8 = 26;
    /// `IORING_OP_RECV`.
    pub const RECV: u8 = 27;
    /// `IORING_OP_SEND_ZC`.
    pub const SEND_ZC: u8 = 47;
    /// `IORING_OP_READ_FIXED`.
    pub const READ_FIXED: u8 = 4;
    /// `IORING_OP_WRITE_FIXED`.
    pub const WRITE_FIXED: u8 = 5;
    /// `IORING_OP_FALLOCATE`.
    pub const FALLOCATE: u8 = 17;
    /// `IORING_OP_OPENAT`.
    pub const OPENAT: u8 = 18;
    /// `IORING_OP_CLOSE`.
    pub const CLOSE: u8 = 19;
    /// `IORING_OP_RENAMEAT`.
    pub const RENAMEAT: u8 = 35;
    /// `IORING_OP_UNLINKAT`.
    pub const UNLINKAT: u8 = 36;
    /// `IORING_OP_MKDIRAT`.
    pub const MKDIRAT: u8 = 37;
    /// `IORING_OP_LINKAT`.
    pub const LINKAT: u8 = 39;
    /// `IORING_OP_FTRUNCATE`.
    pub const FTRUNCATE: u8 = 55;
}

unsafe extern "C" {
    fn syscall(num: i64, ...) -> i64;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct CqringOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Params {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: SqringOffsets,
    cq_off: CqringOffsets,
}

/// `struct io_uring_sqe`.
#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Sqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    op_flags: u32,
    user_data: u64,
    buf_index: u16,
    personality: u16,
    file_index: i32,
    addr3: u64,
    pad: u64,
}

/// `struct io_uring_cqe`.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawCqe {
    user_data: u64,
    res: i32,
    flags: u32,
}

/// `struct io_uring_files_update`.
#[repr(C)]
struct FilesUpdate {
    offset: u32,
    resv: u32,
    fds: u64,
}

/// A file an operation acts on: a plain descriptor or a slot of the registered table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A descriptor the caller keeps open until the completion.
    Fd(RawFd),
    /// Slot of the table registered with [`Ring::register_files`].
    Fixed(u32),
}

/// An operation for the ring.
#[derive(Debug)]
pub enum Op {
    /// Does nothing (`IORING_OP_NOP`).
    Nop,
    /// Reads `len` bytes at `offset` into registered buffer `buf` from byte `at`.
    ReadFixed {
        /// File.
        target: Target,
        /// Registered buffer.
        buf: u16,
        /// First byte of the buffer.
        at: u32,
        /// Bytes.
        len: u32,
        /// File offset.
        offset: u64,
    },
    /// Writes `len` bytes of registered buffer `buf` from byte `at` at `offset`.
    WriteFixed {
        /// File.
        target: Target,
        /// Registered buffer.
        buf: u16,
        /// First byte of the buffer.
        at: u32,
        /// Bytes.
        len: u32,
        /// File offset.
        offset: u64,
    },
    /// `fsync`, or `fdatasync` with `data_only`.
    Fsync {
        /// File or directory.
        target: Target,
        /// Only the data and what is needed to read it back.
        data_only: bool,
    },
    /// `fallocate(fd, 0, 0, len)`.
    Fallocate {
        /// File.
        target: Target,
        /// Bytes to reserve from the start.
        len: u64,
    },
    /// `ftruncate(fd, len)`.
    Ftruncate {
        /// File.
        target: Target,
        /// New length.
        len: u64,
    },
    /// `openat(AT_FDCWD, path, flags | O_CLOEXEC, mode)`; the result is the new descriptor.
    OpenAt {
        /// Path.
        path: CString,
        /// `open(2)` flags.
        flags: i32,
        /// Mode of a created file.
        mode: u32,
    },
    /// Closes a descriptor its owner gave up.
    Close {
        /// Descriptor.
        fd: OwnedFd,
    },
    /// `renameat(AT_FDCWD, from, AT_FDCWD, to)`.
    RenameAt {
        /// Source.
        from: CString,
        /// Destination.
        to: CString,
    },
    /// `linkat(AT_FDCWD, from, AT_FDCWD, to, 0)`: a hard link.
    LinkAt {
        /// Existing name.
        from: CString,
        /// New name.
        to: CString,
    },
    /// `unlinkat`, or `rmdir` with `dir`.
    UnlinkAt {
        /// Path.
        path: CString,
        /// Removes an empty directory.
        dir: bool,
    },
    /// `mkdirat(AT_FDCWD, path, mode)`.
    MkdirAt {
        /// Path.
        path: CString,
        /// Mode.
        mode: u32,
    },
    /// Multishot `accept4(fd, NULL, NULL, SOCK_CLOEXEC)` on a listening socket: one completion
    /// per connection, with its descriptor in [`Completion::fd`], until [`Op::Cancel`] or an
    /// error ends it (a completion without [`Completion::more`]).
    AcceptMulti {
        /// Listening socket, kept open by the caller until the last completion.
        fd: RawFd,
    },
    /// Multishot `recv(fd, …)` into the buffers of provided buffer ring `group`
    /// ([`Ring::register_buf_ring`]): one completion per arrival, with the buffer in
    /// [`Completion::buffer`], until the end of the stream (a result of 0), an error, or
    /// [`Op::Cancel`]. With every buffer of the group taken it ends with `ENOBUFS`.
    RecvMulti {
        /// Connected socket, kept open by the caller until the last completion.
        fd: RawFd,
        /// Provided buffer ring.
        group: u16,
    },
    /// `send(fd, data, MSG_NOSIGNAL | MSG_WAITALL)`.
    Send {
        /// Connected socket.
        fd: RawFd,
        /// Bytes, owned by the ring until the completion.
        data: Part,
    },
    /// `sendmsg(fd, {parts}, MSG_NOSIGNAL | MSG_WAITALL)`: the parts go out in order in one call.
    SendMsg {
        /// Connected socket.
        fd: RawFd,
        /// Bytes, owned by the ring until the completion.
        parts: Vec<Part>,
    },
    /// `IORING_OP_SEND_ZC`: the kernel sends straight from `data` without copying it to the
    /// socket. The first completion has the bytes sent and [`Completion::more`]; the second,
    /// [`Completion::notification`], says the kernel no longer reads the buffer, which the ring
    /// keeps (or a registered buffer stays taken) until then.
    SendZc {
        /// Connected socket.
        fd: RawFd,
        /// Bytes.
        data: SendData,
    },
    /// `IORING_OP_ASYNC_CANCEL` of operation `id`: it ends with `ECANCELED`.
    Cancel {
        /// Identifier [`Ring::push`] returned.
        id: u64,
    },
}

/// A completion: the identifier [`Ring::push`] returned and the result (`res` of the CQE, a
/// count or a new descriptor, or the error).
#[derive(Debug)]
pub struct Completion {
    /// Identifier of the operation.
    pub id: u64,
    /// Its result.
    pub result: io::Result<u32>,
    /// The descriptor an [`Op::OpenAt`] opened or an [`Op::AcceptMulti`] accepted, owned by
    /// whoever takes it.
    pub fd: Option<OwnedFd>,
    /// More completions of this operation follow (`IORING_CQE_F_MORE`): a multishot operation
    /// still running, or an [`Op::SendZc`] whose notification is to come.
    pub more: bool,
    /// The provided buffer an [`Op::RecvMulti`] filled with `result` bytes: read it with
    /// [`Ring::provided`] and give it back with [`Ring::recycle`].
    pub buffer: Option<u16>,
    /// The notification of an [`Op::SendZc`]: the kernel no longer reads its buffer.
    pub notification: bool,
}

/// What the kernel may still read or write for an operation in flight.
enum Keep {
    Nothing,
    /// An open: its result is a new descriptor.
    Open {
        _alive: CString,
    },
    Buffer(u16),
    /// Paths the kernel reads.
    Paths {
        _alive: Vec<CString>,
    },
    /// A multishot accept: each result is a new descriptor.
    Accept,
    /// A multishot receive into provided buffer ring `group`.
    Recv {
        group: u16,
    },
    /// Bytes the kernel reads, with the `msghdr` and the `iovec`s of a `sendmsg`.
    Send {
        _parts: Vec<Part>,
        _iov: Vec<[u64; 2]>,
        _msg: Option<Box<MsgHdr>>,
    },
    /// An operation to cancel: done when its own completion comes.
    Cancel,
}

impl Keep {
    /// An operation that only ends when cancelled or when its socket does.
    fn multishot(&self) -> bool {
        matches!(self, Keep::Accept | Keep::Recv { .. })
    }
}

/// The operations the running kernel supports (`IORING_REGISTER_PROBE`).
#[derive(Clone, Debug)]
pub struct Probe {
    supported: Vec<bool>,
}

impl Probe {
    /// A probe that lists `ops` as supported (for tests of what depends on it).
    pub fn with(ops: &[u8]) -> Probe {
        let mut supported = vec![false; 256];
        for op in ops {
            supported[usize::from(*op)] = true;
        }
        Probe { supported }
    }

    /// Whether operation `op` is supported.
    pub fn supports(&self, op: u8) -> bool {
        self.supported.get(usize::from(op)).copied().unwrap_or(false)
    }
}

/// A shared mapping of the ring.
struct Map {
    ptr: NonNull<u8>,
    len: usize,
}

impl Map {
    fn new(fd: RawFd, len: usize, offset: i64) -> io::Result<Map> {
        // SAFETY: a fresh shared mapping of the ring's descriptor; the kernel checks `offset` and
        // `len` against the ring it created.
        let p = unsafe {
            mmap(
                ptr::null_mut(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED | MAP_POPULATE,
                fd,
                offset,
            )
        };
        if p as isize == -1 {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(p).ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(Map { ptr, len })
    }

    /// Pointer to the field at byte `off`, which the kernel's offsets give.
    fn at<T>(&self, off: u32) -> *mut T {
        debug_assert!(off as usize + size_of::<T>() <= self.len);
        // SAFETY: `off` comes from the kernel's offsets for this mapping, inside `len`.
        unsafe { self.ptr.as_ptr().add(off as usize).cast() }
    }
}

impl Drop for Map {
    fn drop(&mut self) {
        // SAFETY: the mapping was created in `new` with this length and nothing points into it
        // after the ring is gone.
        unsafe { munmap(self.ptr.as_ptr(), self.len) };
    }
}

/// An io_uring instance with its queues mapped.
pub struct Ring {
    sq_head: *const AtomicU32,
    sq_tail: *const AtomicU32,
    /// The kernel's flags of the submission queue: whether its polling thread sleeps.
    sq_flags: *const AtomicU32,
    /// Whether a kernel thread polls the submission queue (`IORING_SETUP_SQPOLL`, 012 FR-025).
    sqpoll: bool,
    sq_mask: u32,
    sq_entries: u32,
    sqes: *mut Sqe,
    cq_head: *const AtomicU32,
    cq_tail: *const AtomicU32,
    cq_mask: u32,
    cqes: *const RawCqe,
    /// Entries written to the submission queue that the kernel has not taken yet.
    unsubmitted: u32,
    next_id: u64,
    in_flight: HashMap<u64, Keep>,
    buffers: Vec<AlignedBuf>,
    busy: Vec<bool>,
    files: u32,
    _sqe_map: Map,
    _sq_map: Map,
    fd: OwnedFd,
    /// Provided buffer rings by group; after `fd`, so the ring is gone before their memory.
    buf_rings: HashMap<u16, BufRing>,
}

// SAFETY: the raw pointers point into mappings the ring owns; the ring is used by one thread at a
// time (`&mut self` for everything that touches the queues) and may move between threads.
unsafe impl Send for Ring {}

fn cvt(rc: i64) -> io::Result<u32> {
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(rc as u32) }
}

impl Ring {
    /// A ring with at least `entries` submission slots (rounded up to a power of two by the
    /// kernel) and twice as many completion slots.
    pub fn new(entries: u32) -> io::Result<Ring> {
        Ring::with_sqpoll(entries, false)
    }

    /// [`Ring::new`], with `sqpoll` a kernel thread that takes the submissions from the queue
    /// without system calls (`IORING_SETUP_SQPOLL`, 012 FR-025); it sleeps after
    /// [`SQPOLL_IDLE_MS`] without work and the next [`Ring::submit`] wakes it.
    pub fn with_sqpoll(entries: u32, sqpoll: bool) -> io::Result<Ring> {
        let mut p = Params::default();
        if sqpoll {
            p.flags |= IORING_SETUP_SQPOLL;
            p.sq_thread_idle = SQPOLL_IDLE_MS;
        }
        // SAFETY: `p` is a valid `io_uring_params` that lives during the call.
        let fd = cvt(unsafe { syscall(SYS_IO_URING_SETUP, entries, &mut p as *mut Params) })?;
        // SAFETY: the call has just created the descriptor and nobody else owns it.
        let fd = unsafe { OwnedFd::from_raw_fd(fd as RawFd) };
        let raw = fd.as_raw_fd();
        let sq_len = p.sq_off.array as usize + p.sq_entries as usize * size_of::<u32>();
        let cq_len = p.cq_off.cqes as usize + p.cq_entries as usize * size_of::<RawCqe>();
        // One mapping for both queues (Linux 5.4); the executor needs 5.15 anyway (MKDIRAT).
        if p.features & IORING_FEAT_SINGLE_MMAP == 0 {
            return Err(io::Error::other("io_uring without IORING_FEAT_SINGLE_MMAP"));
        }
        let sq_map = Map::new(raw, sq_len.max(cq_len), IORING_OFF_SQ_RING)?;
        let sqe_map = Map::new(raw, p.sq_entries as usize * size_of::<Sqe>(), IORING_OFF_SQES)?;
        let cq = &sq_map;
        // SAFETY: the masks are plain integers the kernel wrote at these offsets.
        let (sq_mask, cq_mask) =
            unsafe { (*sq_map.at::<u32>(p.sq_off.ring_mask), *cq.at::<u32>(p.cq_off.ring_mask)) };
        let array = sq_map.at::<u32>(p.sq_off.array);
        for i in 0..p.sq_entries {
            // SAFETY: the array has `sq_entries` slots; each submission slot `i` always uses
            // entry `i`, so the indirection is the identity.
            unsafe { array.add(i as usize).write_volatile(i) };
        }
        Ok(Ring {
            sq_head: sq_map.at(p.sq_off.head),
            sq_tail: sq_map.at(p.sq_off.tail),
            sq_flags: sq_map.at(p.sq_off.flags),
            sqpoll,
            sq_mask,
            sq_entries: p.sq_entries,
            sqes: sqe_map.ptr.as_ptr().cast(),
            cq_head: cq.at(p.cq_off.head),
            cq_tail: cq.at(p.cq_off.tail),
            cq_mask,
            cqes: cq.at(p.cq_off.cqes),
            unsubmitted: 0,
            next_id: 1,
            in_flight: HashMap::new(),
            buffers: Vec::new(),
            busy: Vec::new(),
            files: 0,
            _sqe_map: sqe_map,
            _sq_map: sq_map,
            fd,
            buf_rings: HashMap::new(),
        })
    }

    fn register(&self, opcode: u32, arg: *const u8, nr: u32) -> io::Result<u32> {
        loop {
            // SAFETY: `arg` points to `nr` elements of the type `opcode` expects, alive during
            // the call; the caller keeps registered memory alive for the ring's life.
            let rc =
                unsafe { syscall(SYS_IO_URING_REGISTER, self.fd.as_raw_fd(), opcode, arg, nr) };
            match cvt(rc) {
                Err(e) if interrupted(&e) => continue,
                r => return r,
            }
        }
    }

    /// The operations the kernel supports.
    pub fn probe(&self) -> io::Result<Probe> {
        const OPS: usize = 256;
        // `struct io_uring_probe` (16 bytes) followed by `OPS` `struct io_uring_probe_op` (8).
        let mut raw = vec![0u64; (16 + OPS * 8) / 8];
        self.register(IORING_REGISTER_PROBE, raw.as_mut_ptr().cast(), OPS as u32)?;
        let bytes: Vec<u8> = raw.iter().flat_map(|w| w.to_ne_bytes()).collect();
        let ops_len = usize::from(bytes[1]);
        let mut supported = vec![false; OPS];
        for i in 0..ops_len.min(OPS) {
            let at = 16 + i * 8;
            let flags = u16::from_ne_bytes([bytes[at + 2], bytes[at + 3]]);
            supported[usize::from(bytes[at])] = flags & IO_URING_OP_SUPPORTED != 0;
        }
        Ok(Probe { supported })
    }

    /// Registers `buffers` (once per ring): fixed reads and writes then refer to them by index.
    pub fn register_buffers(&mut self, buffers: Vec<AlignedBuf>) -> io::Result<()> {
        if !self.buffers.is_empty() {
            return Err(io::Error::from_raw_os_error(EBUSY));
        }
        let iov: Vec<[u64; 2]> = buffers.iter().map(|b| [b.addr(), b.len() as u64]).collect();
        self.register(IORING_REGISTER_BUFFERS, iov.as_ptr().cast(), iov.len() as u32)?;
        self.busy = vec![false; buffers.len()];
        self.buffers = buffers;
        Ok(())
    }

    /// Registered buffer `i`, unless an operation in flight uses it.
    pub fn buffer(&mut self, i: u16) -> Option<&mut [u8]> {
        let i = usize::from(i);
        if *self.busy.get(i)? {
            return None;
        }
        Some(&mut self.buffers[i])
    }

    /// Registered buffers.
    pub fn buffers(&self) -> usize {
        self.buffers.len()
    }

    /// Registers a table of `slots` empty file slots (once per ring).
    pub fn register_files(&mut self, slots: u32) -> io::Result<()> {
        if self.files != 0 {
            return Err(io::Error::from_raw_os_error(EBUSY));
        }
        let fds = vec![-1i32; slots as usize];
        self.register(IORING_REGISTER_FILES, fds.as_ptr().cast(), slots)?;
        self.files = slots;
        Ok(())
    }

    /// Puts `fd` (or nothing, with -1) in slot `slot` of the registered table. The kernel takes its
    /// own reference, so the descriptor may be closed afterwards.
    pub fn set_file(&mut self, slot: u32, fd: RawFd) -> io::Result<()> {
        if slot >= self.files {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        let fds = [fd];
        let up = FilesUpdate { offset: slot, resv: 0, fds: fds.as_ptr() as u64 };
        self.register(IORING_REGISTER_FILES_UPDATE, (&up as *const FilesUpdate).cast(), 1)?;
        Ok(())
    }

    /// Slots of the registered file table.
    pub fn file_slots(&self) -> u32 {
        self.files
    }

    /// Signals every completion on `fd` (an `eventfd`), so that an `epoll` loop wakes up.
    pub fn register_eventfd(&self, fd: RawFd) -> io::Result<()> {
        let fds = [fd];
        self.register(IORING_REGISTER_EVENTFD, fds.as_ptr().cast(), 1).map(|_| ())
    }

    /// Operations pushed and not completed yet.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Free submission slots.
    pub fn space(&self) -> u32 {
        // SAFETY: `sq_head` points into the mapping; the kernel moves it.
        let head = unsafe { (*self.sq_head).load(Ordering::Acquire) };
        // SAFETY: only this thread writes the tail.
        let tail = unsafe { (*self.sq_tail).load(Ordering::Relaxed) };
        self.sq_entries - tail.wrapping_sub(head)
    }

    fn check_target(&self, t: Target) -> Result<(), &'static str> {
        match t {
            Target::Fixed(s) if s >= self.files => Err("file slot out of the registered table"),
            _ => Ok(()),
        }
    }

    fn check_buf(&self, buf: u16, at: u32, len: u32) -> Result<(), &'static str> {
        let b = self.buffers.get(usize::from(buf)).ok_or("buffer not registered")?;
        if self.busy[usize::from(buf)] {
            return Err("buffer in use by another operation");
        }
        if at as usize + len as usize > b.len() {
            return Err("range outside the buffer");
        }
        Ok(())
    }

    /// Queues `op` and returns its identifier, or gives it back with the reason when the queue is
    /// full or the operation names a buffer or slot the ring does not have (or a buffer in use).
    /// Nothing reaches the kernel until [`Ring::submit`].
    pub fn push(&mut self, op: Op) -> Result<u64, (Op, &'static str)> {
        if self.space() == 0 {
            return Err((op, "submission queue full"));
        }
        let checked = match &op {
            Op::ReadFixed { target, buf, at, len, .. }
            | Op::WriteFixed { target, buf, at, len, .. } => {
                self.check_target(*target).and_then(|()| self.check_buf(*buf, *at, *len))
            }
            Op::Fsync { target, .. }
            | Op::Fallocate { target, .. }
            | Op::Ftruncate { target, .. } => self.check_target(*target),
            Op::RecvMulti { group, .. } if !self.buf_rings.contains_key(group) => {
                Err("buffer ring not registered")
            }
            Op::SendZc { data: SendData::Fixed { buf, at, len }, .. } => {
                self.check_buf(*buf, *at, *len)
            }
            _ => Ok(()),
        };
        if let Err(why) = checked {
            return Err((op, why));
        }
        let id = self.next_id;
        self.next_id += 1;
        let mut sqe = Sqe { user_data: id, ..Sqe::default() };
        let set_target = |sqe: &mut Sqe, t: Target| match t {
            Target::Fd(fd) => sqe.fd = fd,
            Target::Fixed(slot) => {
                sqe.fd = slot as i32;
                sqe.flags |= IOSQE_FIXED_FILE;
            }
        };
        let keep = match op {
            Op::Nop => Keep::Nothing,
            Op::ReadFixed { target, buf, at, len, offset }
            | Op::WriteFixed { target, buf, at, len, offset } => {
                let write = matches!(op, Op::WriteFixed { .. });
                sqe.opcode = if write { opcode::WRITE_FIXED } else { opcode::READ_FIXED };
                set_target(&mut sqe, target);
                sqe.addr = self.buffers[usize::from(buf)].addr() + u64::from(at);
                sqe.len = len;
                sqe.off = offset;
                sqe.buf_index = buf;
                self.busy[usize::from(buf)] = true;
                Keep::Buffer(buf)
            }
            Op::Fsync { target, data_only } => {
                sqe.opcode = opcode::FSYNC;
                set_target(&mut sqe, target);
                sqe.op_flags = if data_only { IORING_FSYNC_DATASYNC } else { 0 };
                Keep::Nothing
            }
            Op::Fallocate { target, len } => {
                sqe.opcode = opcode::FALLOCATE;
                set_target(&mut sqe, target);
                // The length goes in `addr` and the mode in `len`.
                sqe.addr = len;
                Keep::Nothing
            }
            Op::Ftruncate { target, len } => {
                sqe.opcode = opcode::FTRUNCATE;
                set_target(&mut sqe, target);
                sqe.off = len;
                Keep::Nothing
            }
            Op::OpenAt { path, flags, mode } => {
                sqe.opcode = opcode::OPENAT;
                sqe.fd = AT_FDCWD;
                sqe.addr = path.as_ptr() as u64;
                sqe.len = mode;
                sqe.op_flags = (flags | O_CLOEXEC) as u32;
                Keep::Open { _alive: path }
            }
            Op::Close { fd } => {
                sqe.opcode = opcode::CLOSE;
                sqe.fd = fd.into_raw_fd();
                Keep::Nothing
            }
            Op::RenameAt { from, to } => {
                sqe.opcode = opcode::RENAMEAT;
                sqe.fd = AT_FDCWD;
                sqe.addr = from.as_ptr() as u64;
                sqe.len = AT_FDCWD as u32;
                sqe.off = to.as_ptr() as u64;
                Keep::Paths { _alive: vec![from, to] }
            }
            Op::LinkAt { from, to } => {
                sqe.opcode = opcode::LINKAT;
                sqe.fd = AT_FDCWD;
                sqe.addr = from.as_ptr() as u64;
                sqe.len = AT_FDCWD as u32;
                sqe.off = to.as_ptr() as u64;
                Keep::Paths { _alive: vec![from, to] }
            }
            Op::UnlinkAt { path, dir } => {
                sqe.opcode = opcode::UNLINKAT;
                sqe.fd = AT_FDCWD;
                sqe.addr = path.as_ptr() as u64;
                sqe.op_flags = if dir { AT_REMOVEDIR } else { 0 };
                Keep::Paths { _alive: vec![path] }
            }
            Op::MkdirAt { path, mode } => {
                sqe.opcode = opcode::MKDIRAT;
                sqe.fd = AT_FDCWD;
                sqe.addr = path.as_ptr() as u64;
                sqe.len = mode;
                Keep::Paths { _alive: vec![path] }
            }
            Op::AcceptMulti { fd } => {
                sqe.opcode = opcode::ACCEPT;
                sqe.fd = fd;
                sqe.ioprio = IORING_ACCEPT_MULTISHOT;
                sqe.op_flags = SOCK_CLOEXEC;
                Keep::Accept
            }
            Op::RecvMulti { fd, group } => {
                sqe.opcode = opcode::RECV;
                sqe.fd = fd;
                sqe.ioprio = IORING_RECV_MULTISHOT;
                sqe.flags |= IOSQE_BUFFER_SELECT;
                sqe.buf_index = group;
                Keep::Recv { group }
            }
            Op::Send { fd, data } => {
                let bytes = data.bytes();
                sqe.opcode = opcode::SEND;
                sqe.fd = fd;
                sqe.addr = bytes.as_ptr() as u64;
                sqe.len = len_u32(bytes.len());
                sqe.op_flags = MSG_NOSIGNAL | MSG_WAITALL;
                Keep::Send { _parts: vec![data], _iov: Vec::new(), _msg: None }
            }
            Op::SendMsg { fd, parts } => {
                let iov: Vec<[u64; 2]> = parts
                    .iter()
                    .map(|p| {
                        let b = p.bytes();
                        [b.as_ptr() as u64, b.len() as u64]
                    })
                    .collect();
                let msg = Box::new(MsgHdr::of(&iov));
                sqe.opcode = opcode::SENDMSG;
                sqe.fd = fd;
                sqe.addr = &*msg as *const MsgHdr as u64;
                sqe.len = 1;
                sqe.op_flags = MSG_NOSIGNAL | MSG_WAITALL;
                Keep::Send { _parts: parts, _iov: iov, _msg: Some(msg) }
            }
            Op::SendZc { fd, data } => {
                sqe.opcode = opcode::SEND_ZC;
                sqe.fd = fd;
                sqe.op_flags = MSG_NOSIGNAL | MSG_WAITALL;
                match data {
                    SendData::Owned(part) => {
                        let bytes = part.bytes();
                        sqe.addr = bytes.as_ptr() as u64;
                        sqe.len = len_u32(bytes.len());
                        Keep::Send { _parts: vec![part], _iov: Vec::new(), _msg: None }
                    }
                    SendData::Fixed { buf, at, len } => {
                        sqe.ioprio = IORING_RECVSEND_FIXED_BUF;
                        sqe.addr = self.buffers[usize::from(buf)].addr() + u64::from(at);
                        sqe.len = len;
                        sqe.buf_index = buf;
                        self.busy[usize::from(buf)] = true;
                        Keep::Buffer(buf)
                    }
                }
            }
            Op::Cancel { id } => {
                sqe.opcode = opcode::ASYNC_CANCEL;
                sqe.fd = -1;
                sqe.addr = id;
                Keep::Cancel
            }
        };
        // SAFETY: only this thread writes the tail.
        let tail = unsafe { (*self.sq_tail).load(Ordering::Relaxed) };
        // SAFETY: `space() > 0`, so slot `tail & mask` is not in the kernel's hands; the slot
        // lies inside the SQE mapping.
        unsafe { self.sqes.add((tail & self.sq_mask) as usize).write(sqe) };
        // SAFETY: the release store publishes the entry written above to the kernel.
        unsafe { (*self.sq_tail).store(tail.wrapping_add(1), Ordering::Release) };
        self.unsubmitted += 1;
        self.in_flight.insert(id, keep);
        Ok(id)
    }

    /// Hands the queued operations to the kernel in one `io_uring_enter` and, with `wait > 0`,
    /// waits until at least `wait` completions are ready. Returns the operations submitted.
    pub fn submit(&mut self, wait: u32) -> io::Result<u32> {
        let mut flags = if wait > 0 { IORING_ENTER_GETEVENTS } else { 0 };
        if self.sqpoll {
            // The kernel's thread takes what the tail published. Only a sleeping thread needs a
            // wake-up, and only a wait needs the call at all. The fence orders the tail's store
            // before the load of the flags, as the kernel orders its side (012 FR-025).
            fence(Ordering::SeqCst);
            // SAFETY: the flags are a `u32` of the mapped queue the ring owns.
            let sq_flags = unsafe { (*self.sq_flags).load(Ordering::Relaxed) };
            if sq_flags & IORING_SQ_NEED_WAKEUP != 0 && self.unsubmitted > 0 {
                flags |= IORING_ENTER_SQ_WAKEUP;
            }
            let handed = self.unsubmitted;
            self.unsubmitted = 0;
            if flags == 0 {
                return Ok(handed);
            }
            self.enter(0, wait, flags)?;
            return Ok(handed);
        }
        self.enter(self.unsubmitted, wait, flags)
    }

    /// Whether a kernel thread polls this ring's submissions (`--io-uring-sqpoll`).
    pub fn sqpoll(&self) -> bool {
        self.sqpoll
    }

    /// `io_uring_enter` with `to_submit` entries of the queue, waiting for `wait` completions.
    fn enter(&mut self, to_submit: u32, wait: u32, flags: u32) -> io::Result<u32> {
        loop {
            // SAFETY: no pointers (the signal mask is null with size 0).
            let rc = unsafe {
                syscall(
                    SYS_IO_URING_ENTER,
                    self.fd.as_raw_fd(),
                    to_submit,
                    wait,
                    flags,
                    ptr::null::<u8>(),
                    0usize,
                )
            };
            let e = match cvt(rc) {
                Ok(n) => {
                    self.unsubmitted -= n.min(self.unsubmitted);
                    return Ok(n);
                }
                Err(e) => e,
            };
            match enter_failure(&e, wait) {
                EnterFailure::Retry => continue,
                EnterFailure::Later => return Ok(0),
                EnterFailure::Fatal => return Err(e),
            }
        }
    }

    /// Takes the next completion, if there is one.
    pub fn complete(&mut self) -> Option<Completion> {
        // SAFETY: only this thread writes the head.
        let head = unsafe { (*self.cq_head).load(Ordering::Relaxed) };
        // SAFETY: the acquire load makes the entries the kernel wrote before the tail visible.
        let tail = unsafe { (*self.cq_tail).load(Ordering::Acquire) };
        if head == tail {
            return None;
        }
        // SAFETY: entry `head & mask` is between head and tail, written by the kernel.
        let cqe = unsafe { self.cqes.add((head & self.cq_mask) as usize).read() };
        // SAFETY: the release store hands the slot back to the kernel after reading it.
        unsafe { (*self.cq_head).store(head.wrapping_add(1), Ordering::Release) };
        let more = cqe.flags & IORING_CQE_F_MORE != 0;
        let notification = cqe.flags & IORING_CQE_F_NOTIF != 0;
        let mut fd = None;
        let mut buffer = None;
        // An operation with more completions to come keeps what the kernel may still use.
        let keep = if more {
            self.in_flight.get(&cqe.user_data).map(KeepKind::of)
        } else {
            self.in_flight.remove(&cqe.user_data).as_ref().map(KeepKind::of)
        };
        match keep {
            Some(KeepKind::Buffer(b)) if !more => self.busy[usize::from(b)] = false,
            Some(KeepKind::Fd) if cqe.res >= 0 => {
                // SAFETY: the kernel has just opened or accepted this descriptor for the
                // operation and nobody else owns it.
                fd = Some(unsafe { OwnedFd::from_raw_fd(cqe.res) });
            }
            Some(KeepKind::Recv(group)) if cqe.flags & IORING_CQE_F_BUFFER != 0 => {
                let bid = (cqe.flags >> IORING_CQE_BUFFER_SHIFT) as u16;
                if let Some(ring) = self.buf_rings.get_mut(&group) {
                    ring.hand_out(bid, cqe.res.max(0) as u32);
                    buffer = Some(bid);
                }
            }
            _ => {}
        }
        let result = if cqe.res < 0 {
            Err(io::Error::from_raw_os_error(-cqe.res))
        } else {
            Ok(cqe.res as u32)
        };
        Some(Completion { id: cqe.user_data, result, fd, more, buffer, notification })
    }

    /// Registers a provided buffer ring for group `group` with `entries` buffers (a power of two,
    /// at most 32 768) of `buf_len` bytes, all given to the kernel (`IORING_REGISTER_PBUF_RING`,
    /// Linux 5.19). [`Op::RecvMulti`] of that group takes its buffers from it.
    pub fn register_buf_ring(&mut self, group: u16, entries: u16, buf_len: u32) -> io::Result<()> {
        if self.buf_rings.contains_key(&group) {
            return Err(io::Error::from_raw_os_error(EBUSY));
        }
        let ring = BufRing::new(entries, buf_len)?;
        let reg = ring.registration(group);
        self.register(IORING_REGISTER_PBUF_RING, (&reg as *const net::BufReg).cast(), 1)?;
        self.buf_rings.insert(group, ring);
        Ok(())
    }

    /// The `len` bytes a completion of [`Op::RecvMulti`] left in buffer `bid` of `group`, while
    /// the caller holds it (until [`Ring::recycle`]).
    pub fn provided(&self, group: u16, bid: u16) -> Option<&[u8]> {
        self.buf_rings.get(&group)?.held(bid)
    }

    /// Gives buffer `bid` of `group` back to the kernel; false if the caller did not hold it.
    pub fn recycle(&mut self, group: u16, bid: u16) -> bool {
        self.buf_rings.get_mut(&group).is_some_and(|r| r.give_back(bid))
    }

    /// Buffers of `group` the caller holds, not yet given back.
    pub fn held_buffers(&self, group: u16) -> usize {
        self.buf_rings.get(&group).map_or(0, BufRing::held_count)
    }

    /// Cancels every multishot operation in flight, which would otherwise never end.
    fn cancel_multishot(&mut self) -> io::Result<()> {
        let ids: Vec<u64> =
            self.in_flight.iter().filter(|(_, k)| k.multishot()).map(|(id, _)| *id).collect();
        for id in ids {
            while self.space() == 0 {
                self.submit(0)?;
            }
            // The queue has room, and a cancel names no buffer nor slot.
            let _ = self.push(Op::Cancel { id });
        }
        Ok(())
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // The kernel may still read paths and use buffers of operations in flight: wait for them
        // before the memory goes. A ring that cannot wait any more (an error) stops waiting.
        // Multishot operations only end when cancelled.
        if self.cancel_multishot().is_err() {
            return;
        }
        while !self.in_flight.is_empty() {
            if self.submit(1).is_err() {
                break;
            }
            while self.complete().is_some() {}
        }
    }
}

impl std::fmt::Debug for Ring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ring")
            .field("entries", &self.sq_entries)
            .field("in_flight", &self.in_flight.len())
            .field("buffers", &self.buffers.len())
            .finish()
    }
}

/// What [`Ring::complete`] needs to know of an operation's [`Keep`].
#[derive(Clone, Copy)]
enum KeepKind {
    Buffer(u16),
    /// Its result is a new descriptor.
    Fd,
    Recv(u16),
    Other,
}

impl KeepKind {
    fn of(k: &Keep) -> KeepKind {
        match k {
            Keep::Buffer(b) => KeepKind::Buffer(*b),
            Keep::Open { .. } | Keep::Accept => KeepKind::Fd,
            Keep::Recv { group } => KeepKind::Recv(*group),
            _ => KeepKind::Other,
        }
    }
}

/// A length for a submission entry: a send of more than 4 GiB is cut to what fits, and the
/// caller sends the rest after the completion says how much went.
fn len_u32(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

/// Whether a call failed only because a signal interrupted it.
fn interrupted(e: &io::Error) -> bool {
    e.raw_os_error() == Some(EINTR)
}

/// What a failed `io_uring_enter` means for its caller.
#[derive(Debug, PartialEq, Eq)]
enum EnterFailure {
    /// Interrupted while waiting: call again.
    Retry,
    /// Nothing waited for, and the call was interrupted or the kernel is short of memory or
    /// completion slots: what is in the queue stays there for the next call.
    Later,
    /// A real error.
    Fatal,
}

fn enter_failure(e: &io::Error, wait: u32) -> EnterFailure {
    let transient = matches!(e.raw_os_error(), Some(EINTR | EAGAIN | EBUSY));
    if transient && wait == 0 {
        EnterFailure::Later
    } else if interrupted(e) {
        EnterFailure::Retry
    } else {
        EnterFailure::Fatal
    }
}

/// A non-blocking `eventfd` for [`Ring::register_eventfd`].
pub fn eventfd() -> io::Result<OwnedFd> {
    super::new_eventfd()
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::ffi::OsStrExt;

    fn cpath(p: &std::path::Path) -> CString {
        CString::new(p.as_os_str().as_bytes()).unwrap()
    }

    fn temp(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("teseladb-uring-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        let _ = std::fs::remove_file(&p);
        p
    }

    fn wait(r: &mut Ring, id: u64) -> Completion {
        r.submit(1).unwrap();
        loop {
            if let Some(c) = r.complete() {
                assert_eq!(c.id, id);
                return c;
            }
            r.submit(1).unwrap();
        }
    }

    fn wait_one(r: &mut Ring, id: u64) -> io::Result<u32> {
        wait(r, id).result
    }

    #[test]
    fn failed_enters_are_retried_left_for_later_or_returned() {
        let e = |code| io::Error::from_raw_os_error(code);
        assert_eq!(enter_failure(&e(EINTR), 1), EnterFailure::Retry);
        assert_eq!(enter_failure(&e(EINTR), 0), EnterFailure::Later);
        assert_eq!(enter_failure(&e(EAGAIN), 0), EnterFailure::Later);
        assert_eq!(enter_failure(&e(EBUSY), 0), EnterFailure::Later);
        assert_eq!(enter_failure(&e(EAGAIN), 1), EnterFailure::Fatal);
        assert_eq!(enter_failure(&e(9), 0), EnterFailure::Fatal);
        assert!(interrupted(&e(EINTR)) && !interrupted(&e(9)));
    }

    #[test]
    fn a_polled_ring_takes_submissions_without_a_call_and_wakes_its_thread_after_idling() {
        // 012 FR-025: the kernel's thread takes the operations; after SQPOLL_IDLE_MS without
        // work it sleeps, and the next submission wakes it.
        let mut r = Ring::with_sqpoll(8, true).unwrap();
        assert!(r.sqpoll() && !Ring::new(2).unwrap().sqpoll());
        for round in 0..2 {
            let id = r.push(Op::Nop).unwrap();
            assert_eq!(r.submit(0).unwrap(), 1);
            assert_eq!(r.unsubmitted, 0);
            let c = loop {
                if let Some(c) = r.complete() {
                    break c;
                }
                r.submit(1).unwrap();
            };
            assert_eq!((c.id, c.result.unwrap()), (id, 0), "round {round}");
            if round == 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    u64::from(SQPOLL_IDLE_MS) + 300,
                ));
                // SAFETY: the flags are a `u32` of the mapped queue the ring owns.
                let slept = unsafe { (*r.sq_flags).load(Ordering::Acquire) };
                assert_ne!(slept & IORING_SQ_NEED_WAKEUP, 0, "the thread sleeps after idling");
            }
        }
    }

    #[test]
    fn a_mapping_the_kernel_refuses_is_an_error() {
        assert!(Map::new(-1, 4096, 0).is_err());
    }

    #[test]
    fn layouts_match_the_kernel_headers() {
        assert_eq!(size_of::<Params>(), 120);
        assert_eq!(size_of::<Sqe>(), 64);
        assert_eq!(size_of::<RawCqe>(), 16);
        assert_eq!(size_of::<FilesUpdate>(), 16);
    }

    #[test]
    fn the_probe_lists_what_teseladb_needs() {
        let r = Ring::new(8).unwrap();
        let p = r.probe().unwrap();
        for op in
            [opcode::NOP, opcode::FSYNC, opcode::READ_FIXED, opcode::WRITE_FIXED, opcode::OPENAT]
        {
            assert!(p.supports(op), "{op}");
        }
        assert!(!p.supports(255));
        assert!(format!("{r:?}").contains("entries"));
    }

    #[test]
    fn file_round_trip_through_registered_buffers_and_files() {
        let dir = temp("rt");
        let mut r = Ring::new(8).unwrap();
        r.register_buffers(vec![AlignedBuf::new(8192)]).unwrap();
        assert_eq!(r.buffers(), 1);
        assert!(r.register_buffers(vec![AlignedBuf::new(4096)]).is_err());
        r.register_files(4).unwrap();
        assert_eq!(r.file_slots(), 4);
        assert!(r.register_files(4).is_err());
        let id = r.push(Op::MkdirAt { path: cpath(&dir), mode: 0o755 }).unwrap();
        wait_one(&mut r, id).unwrap();
        let f = dir.join("f");
        let id = r
            .push(Op::OpenAt { path: cpath(&f), flags: 0o2 | 0o100 | 0o200, mode: 0o644 })
            .unwrap();
        let owned = wait(&mut r, id).fd.unwrap();
        let fd = owned.as_raw_fd();
        r.set_file(1, fd).unwrap();
        assert!(r.set_file(9, fd).is_err());
        r.buffer(0).unwrap()[..5].copy_from_slice(b"hello");
        let id = r
            .push(Op::WriteFixed { target: Target::Fixed(1), buf: 0, at: 0, len: 4096, offset: 0 })
            .unwrap();
        // The buffer cannot be borrowed nor reused while the kernel has it.
        assert!(r.buffer(0).is_none());
        let again = Op::ReadFixed { target: Target::Fixed(1), buf: 0, at: 0, len: 1, offset: 0 };
        assert_eq!(r.push(again).unwrap_err().1, "buffer in use by another operation");
        assert_eq!(r.in_flight(), 1);
        assert_eq!(wait_one(&mut r, id).unwrap(), 4096);
        let id = r.push(Op::Fsync { target: Target::Fd(fd), data_only: true }).unwrap();
        wait_one(&mut r, id).unwrap();
        r.buffer(0).unwrap().fill(0);
        let id = r
            .push(Op::ReadFixed {
                target: Target::Fixed(1),
                buf: 0,
                at: 4096,
                len: 4096,
                offset: 0,
            })
            .unwrap();
        assert_eq!(wait_one(&mut r, id).unwrap(), 4096);
        assert_eq!(&r.buffer(0).unwrap()[4096..4101], b"hello");
        let id = r.push(Op::Fallocate { target: Target::Fd(fd), len: 1 << 16 }).unwrap();
        wait_one(&mut r, id).unwrap();
        assert_eq!(std::fs::metadata(&f).unwrap().len(), 1 << 16);
        if r.probe().unwrap().supports(opcode::FTRUNCATE) {
            let id = r.push(Op::Ftruncate { target: Target::Fixed(1), len: 10 }).unwrap();
            wait_one(&mut r, id).unwrap();
            assert_eq!(std::fs::metadata(&f).unwrap().len(), 10);
        }
        r.set_file(1, -1).unwrap();
        let id = r.push(Op::Close { fd: owned }).unwrap();
        wait_one(&mut r, id).unwrap();
        let g = dir.join("g");
        let id = r.push(Op::RenameAt { from: cpath(&f), to: cpath(&g) }).unwrap();
        wait_one(&mut r, id).unwrap();
        let mut got = String::new();
        std::fs::File::open(&g).unwrap().take(5).read_to_string(&mut got).unwrap();
        assert_eq!(got, "hello");
        {
            let h = dir.join("h");
            let id = r.push(Op::LinkAt { from: cpath(&g), to: cpath(&h) }).unwrap();
            wait_one(&mut r, id).unwrap();
            let mut got = String::new();
            std::fs::File::open(&h).unwrap().take(5).read_to_string(&mut got).unwrap();
            assert_eq!(got, "hello", "a second name of the same file");
            let id = r.push(Op::LinkAt { from: cpath(&g), to: cpath(&h) }).unwrap();
            assert!(wait_one(&mut r, id).is_err(), "the new name must not exist");
            let id = r.push(Op::UnlinkAt { path: cpath(&h), dir: false }).unwrap();
            wait_one(&mut r, id).unwrap();
        }
        let id = r.push(Op::UnlinkAt { path: cpath(&g), dir: false }).unwrap();
        wait_one(&mut r, id).unwrap();
        let id = r.push(Op::UnlinkAt { path: cpath(&dir), dir: true }).unwrap();
        wait_one(&mut r, id).unwrap();
        assert!(!dir.exists());
    }

    #[test]
    fn errors_come_back_with_their_errno_and_bad_requests_are_refused() {
        let e = Ring::new(0).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput, "no ring of 0 entries");
        let mut r = Ring::new(2).unwrap();
        assert!(r.register_eventfd(-1).is_err());
        let missing = temp("missing").join("x");
        let id = r.push(Op::OpenAt { path: cpath(&missing), flags: 0, mode: 0 }).unwrap();
        assert_eq!(wait_one(&mut r, id).unwrap_err().kind(), io::ErrorKind::NotFound);
        let bad = Op::ReadFixed { target: Target::Fd(0), buf: 3, at: 0, len: 1, offset: 0 };
        assert_eq!(r.push(bad).unwrap_err().1, "buffer not registered");
        let bad = Op::Fsync { target: Target::Fixed(0), data_only: false };
        assert_eq!(r.push(bad).unwrap_err().1, "file slot out of the registered table");
        r.register_buffers(vec![AlignedBuf::new(4096)]).unwrap();
        let bad = Op::WriteFixed { target: Target::Fd(0), buf: 0, at: 4000, len: 200, offset: 0 };
        assert_eq!(r.push(bad).unwrap_err().1, "range outside the buffer");
        // A full submission queue gives the operation back.
        let n = r.space();
        for _ in 0..n {
            r.push(Op::Nop).unwrap();
        }
        assert_eq!(r.push(Op::Nop).unwrap_err().1, "submission queue full");
        assert_eq!(r.submit(0).unwrap(), n);
        // Dropping the ring waits for what is in flight.
        drop(r);
    }

    #[test]
    fn completions_signal_the_registered_eventfd() {
        let r = &mut Ring::new(4).unwrap();
        let efd = eventfd().unwrap();
        r.register_eventfd(efd.as_raw_fd()).unwrap();
        let id = r.push(Op::Nop).unwrap();
        wait_one(r, id).unwrap();
        let mut buf = [0u8; 8];
        let n = std::fs::File::from(efd).read(&mut buf).unwrap();
        assert_eq!(n, 8);
        assert!(u64::from_ne_bytes(buf) >= 1);
    }
}
