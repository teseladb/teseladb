//! The I/O requests the reactor hands to the kernel.
//!
//! [`IoRequest`] is a compact tagged union (`#[repr(C)]`, the [`Operation`] tag first in every
//! variant) describing one read, write, socket or poll operation, built with the `make_*`
//! constructors and submitted by a reactor backend (linux-aio, io_uring or epoll). Large disk
//! requests are split by the I/O queue module into [`Part`]s; [`IoDirectionAndLength`] is the
//! direction and length pair the I/O scheduler accounts for.

use std::ffi::c_void;

use crate::engine::core::reactor_backend::{Iovec, Msghdr};

/// The I/O log, shared with the I/O queue module.
pub use crate::engine::core::io_queue::io_log;

/// The kind of an [`IoRequest`].
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    /// Positioned read into one buffer.
    Read,
    /// Positioned read into an iovec array.
    Readv,
    /// Positioned write from one buffer.
    Write,
    /// Positioned write from an iovec array.
    Writev,
    /// `fdatasync` of a file.
    Fdatasync,
    /// Socket receive into one buffer.
    Recv,
    /// Socket receive of a message.
    Recvmsg,
    /// Socket send from one buffer.
    Send,
    /// Socket send of a message.
    Sendmsg,
    /// Accept a connection.
    Accept,
    /// Connect a socket.
    Connect,
    /// Wait for poll events on a descriptor.
    PollAdd,
    /// Remove a pending poll.
    PollRemove,
    /// Cancel a pending request.
    Cancel,
}

// The upper layers pass untyped pointers, but storing `*mut c_void` here invites silently
// converting other pointer types and chasing very odd bugs. Buffers are stored as `*mut u8`
// and cast back to `*mut c_void` in the accessors.

/// Arguments of a single-buffer positioned read (also used for writes).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ReadOp {
    /// The operation tag.
    pub op: Operation,
    /// Whether the file supports `RWF_NOWAIT` for this request.
    pub nowait_works: bool,
    /// The file descriptor.
    pub fd: i32,
    /// The file offset.
    pub pos: u64,
    /// The buffer.
    pub addr: *mut u8,
    /// The buffer length in bytes.
    pub size: usize,
}

/// Arguments of a vectored positioned read (also used for writes).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ReadvOp {
    /// The operation tag.
    pub op: Operation,
    /// Whether the file supports `RWF_NOWAIT` for this request.
    pub nowait_works: bool,
    /// The file descriptor.
    pub fd: i32,
    /// The file offset.
    pub pos: u64,
    /// The iovec array.
    pub iovec: *mut Iovec,
    /// The number of iovecs.
    pub iov_len: usize,
}

/// Arguments of a single-buffer socket receive (also used for sends).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RecvOp {
    /// The operation tag.
    pub op: Operation,
    /// The socket descriptor.
    pub fd: i32,
    /// The buffer.
    pub addr: *mut u8,
    /// The buffer length in bytes.
    pub size: usize,
    /// `MSG_*` flags.
    pub flags: i32,
}

/// Arguments of a message receive (also used for sends).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct RecvmsgOp {
    /// The operation tag.
    pub op: Operation,
    /// The socket descriptor.
    pub fd: i32,
    /// The message header.
    pub msghdr: *mut Msghdr,
    /// `MSG_*` flags.
    pub flags: i32,
}

/// Arguments of a single-buffer send.
pub type SendOp = RecvOp;
/// Arguments of a message send.
pub type SendmsgOp = RecvmsgOp;
/// Arguments of a single-buffer positioned write.
pub type WriteOp = ReadOp;
/// Arguments of a vectored positioned write.
pub type WritevOp = ReadvOp;

/// Arguments of an `fdatasync`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FdatasyncOp {
    /// The operation tag.
    pub op: Operation,
    /// The file descriptor.
    pub fd: i32,
}

/// Arguments of an accept.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AcceptOp {
    /// The operation tag.
    pub op: Operation,
    /// The listening socket.
    pub fd: i32,
    /// Storage for the peer address.
    pub sockaddr: *mut c_void,
    /// In/out length of the peer address storage.
    pub socklen_ptr: *mut u32,
    /// `accept4` flags.
    pub flags: i32,
}

/// Arguments of a connect.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ConnectOp {
    /// The operation tag.
    pub op: Operation,
    /// The socket.
    pub fd: i32,
    /// The address to connect to.
    pub sockaddr: *mut c_void,
    /// The address length.
    pub socklen: u32,
}

/// Arguments of a poll wait.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PollAddOp {
    /// The operation tag.
    pub op: Operation,
    /// The descriptor.
    pub fd: i32,
    /// The poll events to wait for.
    pub events: i32,
}

/// Arguments of a poll removal (also the layout of a cancellation).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PollRemoveOp {
    /// The operation tag.
    pub op: Operation,
    /// The descriptor.
    pub fd: i32,
    /// The user data of the request to remove or cancel.
    pub addr: *mut u8,
}

/// Arguments of a cancellation.
pub type CancelOp = PollRemoveOp;

#[repr(C)]
#[derive(Clone, Copy)]
union Ops {
    _read: ReadOp,
    _readv: ReadvOp,
    _recv: RecvOp,
    _recvmsg: RecvmsgOp,
    _fdatasync: FdatasyncOp,
    _accept: AcceptOp,
    _connect: ConnectOp,
    _poll_add: PollAddOp,
    _poll_remove: PollRemoveOp,
}

/// A tagged union of the operations the reactor hands to the kernel.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoRequest {
    u: Ops,
}

macro_rules! accessor {
    ($name:ident, $field:ident, $ty:ty, $($op:path),+) => {
        /// The request's arguments as this variant; the opcode must match (checked in debug
        /// builds).
        #[inline]
        pub fn $name(&self) -> &$ty {
            debug_assert!(matches!(self.opcode(), $($op)|+));
            // SAFETY: the active member is the one the opcode names (checked in debug builds);
            // all members begin with the operation tag.
            unsafe { &self.u.$field }
        }
    };
}

impl IoRequest {
    /// A read of `size` bytes at `pos` of `fd` into `address`.
    #[inline]
    pub fn make_read(
        fd: i32,
        pos: u64,
        address: *mut c_void,
        size: usize,
        nowait_works: bool,
    ) -> IoRequest {
        IoRequest {
            u: Ops {
                _read: ReadOp {
                    op: Operation::Read,
                    nowait_works,
                    fd,
                    pos,
                    addr: address.cast(),
                    size,
                },
            },
        }
    }

    /// A vectored read at `pos` of `fd` into the buffers of `iov` (which must outlive the
    /// request).
    #[inline]
    pub fn make_readv(fd: i32, pos: u64, iov: &mut Vec<Iovec>, nowait_works: bool) -> IoRequest {
        IoRequest {
            u: Ops {
                _readv: ReadvOp {
                    op: Operation::Readv,
                    nowait_works,
                    fd,
                    pos,
                    iovec: iov.as_mut_ptr(),
                    iov_len: iov.len(),
                },
            },
        }
    }

    /// A receive of up to `size` bytes from socket `fd` into `address`.
    #[inline]
    pub fn make_recv(fd: i32, address: *mut c_void, size: usize, flags: i32) -> IoRequest {
        IoRequest {
            u: Ops { _recv: RecvOp { op: Operation::Recv, fd, addr: address.cast(), size, flags } },
        }
    }

    /// A message receive from socket `fd`.
    #[inline]
    pub fn make_recvmsg(fd: i32, msg: *mut Msghdr, flags: i32) -> IoRequest {
        IoRequest {
            u: Ops { _recvmsg: RecvmsgOp { op: Operation::Recvmsg, fd, msghdr: msg, flags } },
        }
    }

    /// A send of `size` bytes from `address` on socket `fd`.
    #[inline]
    pub fn make_send(fd: i32, address: *const c_void, size: usize, flags: i32) -> IoRequest {
        IoRequest {
            u: Ops {
                _recv: RecvOp {
                    op: Operation::Send,
                    fd,
                    addr: address.cast_mut().cast(),
                    size,
                    flags,
                },
            },
        }
    }

    /// A message send on socket `fd`.
    #[inline]
    pub fn make_sendmsg(fd: i32, msg: *mut Msghdr, flags: i32) -> IoRequest {
        IoRequest {
            u: Ops { _recvmsg: RecvmsgOp { op: Operation::Sendmsg, fd, msghdr: msg, flags } },
        }
    }

    /// A write of `size` bytes from `address` at `pos` of `fd`.
    #[inline]
    pub fn make_write(
        fd: i32,
        pos: u64,
        address: *const c_void,
        size: usize,
        nowait_works: bool,
    ) -> IoRequest {
        IoRequest {
            u: Ops {
                _read: ReadOp {
                    op: Operation::Write,
                    nowait_works,
                    fd,
                    pos,
                    addr: address.cast_mut().cast(),
                    size,
                },
            },
        }
    }

    /// A vectored write of the buffers of `iov` (which must outlive the request) at `pos` of
    /// `fd`.
    #[inline]
    pub fn make_writev(fd: i32, pos: u64, iov: &mut Vec<Iovec>, nowait_works: bool) -> IoRequest {
        IoRequest {
            u: Ops {
                _readv: ReadvOp {
                    op: Operation::Writev,
                    nowait_works,
                    fd,
                    pos,
                    iovec: iov.as_mut_ptr(),
                    iov_len: iov.len(),
                },
            },
        }
    }

    /// An `fdatasync` of `fd`.
    #[inline]
    pub fn make_fdatasync(fd: i32) -> IoRequest {
        IoRequest { u: Ops { _fdatasync: FdatasyncOp { op: Operation::Fdatasync, fd } } }
    }

    /// An accept on listening socket `fd`.
    #[inline]
    pub fn make_accept(fd: i32, addr: *mut c_void, addrlen: *mut u32, flags: i32) -> IoRequest {
        IoRequest {
            u: Ops {
                _accept: AcceptOp {
                    op: Operation::Accept,
                    fd,
                    sockaddr: addr,
                    socklen_ptr: addrlen,
                    flags,
                },
            },
        }
    }

    /// A connect of socket `fd` to `addr`.
    #[inline]
    pub fn make_connect(fd: i32, addr: *mut c_void, addrlen: u32) -> IoRequest {
        IoRequest {
            u: Ops {
                _connect: ConnectOp {
                    op: Operation::Connect,
                    fd,
                    sockaddr: addr,
                    socklen: addrlen,
                },
            },
        }
    }

    /// A wait for poll `events` on `fd`.
    #[inline]
    pub fn make_poll_add(fd: i32, events: i32) -> IoRequest {
        IoRequest { u: Ops { _poll_add: PollAddOp { op: Operation::PollAdd, fd, events } } }
    }

    /// The removal of the poll request whose user data is `addr`.
    #[inline]
    pub fn make_poll_remove(fd: i32, addr: *mut c_void) -> IoRequest {
        IoRequest {
            u: Ops {
                _poll_remove: PollRemoveOp { op: Operation::PollRemove, fd, addr: addr.cast() },
            },
        }
    }

    /// The cancellation of the request whose user data is `addr`.
    #[inline]
    pub fn make_cancel(fd: i32, addr: *mut c_void) -> IoRequest {
        IoRequest {
            u: Ops { _poll_remove: PollRemoveOp { op: Operation::Cancel, fd, addr: addr.cast() } },
        }
    }

    /// Whether the request reads from a file.
    #[inline]
    pub fn is_read(&self) -> bool {
        matches!(
            self.opcode(),
            Operation::Read | Operation::Readv | Operation::Recvmsg | Operation::Recv
        )
    }

    /// Whether the request writes to a file.
    #[inline]
    pub fn is_write(&self) -> bool {
        matches!(
            self.opcode(),
            Operation::Write | Operation::Writev | Operation::Send | Operation::Sendmsg
        )
    }

    /// The operation tag.
    // All variants are `#[repr(C)]` with the operation as their first member, read here
    // through the `_read` member (chosen arbitrarily): the tag sits at the same offset in all
    // of them.
    #[inline]
    pub fn opcode(&self) -> Operation {
        // SAFETY: every member is `#[repr(C)]` with the operation tag first.
        unsafe { self.u._read.op }
    }

    accessor!(as_read, _read, ReadOp, Operation::Read);
    accessor!(as_readv, _readv, ReadvOp, Operation::Readv);
    accessor!(as_recv, _recv, RecvOp, Operation::Recv);
    accessor!(as_recvmsg, _recvmsg, RecvmsgOp, Operation::Recvmsg);
    accessor!(as_send, _recv, SendOp, Operation::Send);
    accessor!(as_sendmsg, _recvmsg, SendmsgOp, Operation::Sendmsg);
    accessor!(as_write, _read, WriteOp, Operation::Write);
    accessor!(as_writev, _readv, WritevOp, Operation::Writev);
    accessor!(as_fdatasync, _fdatasync, FdatasyncOp, Operation::Fdatasync);
    accessor!(as_accept, _accept, AcceptOp, Operation::Accept);
    accessor!(as_connect, _connect, ConnectOp, Operation::Connect);
    accessor!(as_poll_add, _poll_add, PollAddOp, Operation::PollAdd);
    accessor!(as_poll_remove, _poll_remove, PollRemoveOp, Operation::PollRemove);
    accessor!(as_cancel, _poll_remove, CancelOp, Operation::Cancel);

    /// The sub-request of a single-buffer read or write covering `len` bytes at offset `pos`
    /// of this one.
    #[inline]
    pub(crate) fn sub_req_buffer(&self, pos: usize, len: usize) -> IoRequest {
        // read_op and write_op share the same layout, so we don't handle
        // them separately
        // SAFETY: called for read and write requests only, whose member is `_read`.
        let op = unsafe { &self.u._read };
        IoRequest {
            u: Ops {
                _read: ReadOp {
                    op: op.op,
                    nowait_works: op.nowait_works,
                    fd: op.fd,
                    pos: op.pos + pos as u64,
                    addr: op.addr.wrapping_add(pos),
                    size: len,
                },
            },
        }
    }

    /// The sub-request of a vectored read or write starting at offset `pos` of this one, over
    /// the buffers of `iov`.
    #[inline]
    pub(crate) fn sub_req_iovec(&self, pos: usize, iov: &mut Vec<Iovec>) -> IoRequest {
        // readv_op and writev_op share the same layout, so we don't handle
        // them separately
        // SAFETY: called for readv and writev requests only, whose member is `_readv`.
        let op = unsafe { &self.u._readv };
        IoRequest {
            u: Ops {
                _readv: ReadvOp {
                    op: op.op,
                    nowait_works: op.nowait_works,
                    fd: op.fd,
                    pos: op.pos + pos as u64,
                    iovec: iov.as_mut_ptr(),
                    iov_len: iov.len(),
                },
            },
        }
    }
}

/// One piece of a split request.
pub struct Part {
    /// The sub-request.
    pub req: IoRequest,
    /// Its length in bytes.
    pub size: usize,
    /// The iovecs the sub-request points to (empty for single-buffer requests).
    pub iovecs: Vec<Iovec>,
}

/// An I/O direction (read or write) and a length, packed in one word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoDirectionAndLength {
    _directed_length: usize, // bit 0 is R/W flag
}

impl IoDirectionAndLength {
    /// The direction index of reads.
    pub const READ_IDX: i32 = 1;
    /// The direction index of writes.
    pub const WRITE_IDX: i32 = 0;

    /// A direction (`READ_IDX` or `WRITE_IDX`) and a length.
    ///
    /// Aborts the process if `idx` is neither.
    #[inline]
    pub fn new(idx: i32, val: usize) -> Self {
        debug_assert!(idx == Self::READ_IDX || idx == Self::WRITE_IDX);
        if idx != Self::READ_IDX && idx != Self::WRITE_IDX {
            crate::engine::util::assert::internal::assert_fail(
                "idx == read_idx || idx == write_idx",
                file!(),
                line!() as i32,
                "",
            );
        }
        IoDirectionAndLength { _directed_length: (val << 1) | idx as usize }
    }

    /// The length.
    #[inline]
    pub fn length(&self) -> usize {
        self._directed_length >> 1
    }

    /// The direction index.
    #[inline]
    pub fn rw_idx(&self) -> i32 {
        (self._directed_length & 0x1) as i32
    }
}
