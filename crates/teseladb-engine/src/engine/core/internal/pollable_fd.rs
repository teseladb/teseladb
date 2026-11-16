//! File descriptors driven by the reactor.
//!
//! A [`PollableFd`] wraps a non-blocking descriptor (usually a socket) whose operations return
//! futures: the reactor backend (epoll, linux-aio or io_uring) waits for readiness or
//! completes the operation and resolves the future on the shard that started it. Copies of a
//! `PollableFd` share one reference-counted [`PollableFdState`], which the backend extends with
//! its own fields; the state records which poll events were requested, installed and observed,
//! plus a speculation that the next operation will not block. The operations themselves are
//! implemented in the reactor module.
//!
//! [`ReadableEventfd`] and [`WriteableEventfd`] are eventfds where one side is used through the
//! reactor and the other from another thread.

use std::ffi::c_void;

use crate::engine::core::future::{ExceptionPtr, Future};
use crate::engine::core::internal::buffer_allocator::BufferAllocator;
use crate::engine::core::posix::FileDesc;
use crate::engine::core::reactor::intrusive_ptr_release;
use crate::engine::core::reactor_backend::{Iovec, Msghdr};
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::net::api::SocketAddress;
use crate::engine::net::packet::Packet;

/// Poll events that are expected to be ready without waiting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Speculation {
    /// The `EPOLLIN`/`EPOLLOUT` events guessed ready.
    pub events: i32,
}

impl Speculation {
    /// A speculation that the events `epoll_events_guessed` are ready.
    #[inline]
    pub const fn new(epoll_events_guessed: i32) -> Self {
        Speculation { events: epoll_events_guessed }
    }
}

/// The base of the per-backend descriptor states. A backend state is `#[repr(C)]` with this
/// struct as its first field.
///
/// `_this` is the state's address with the provenance of the whole backend state
/// (`Box::into_raw` of the derived type), written once by the backend's
/// `make_pollable_fd_state`. The continuations, the kernel (epoll data, completion user
/// data) and the backend's cast to the derived type keep or use this pointer, never one taken
/// from a reference to the base, which covers only the base.
#[repr(C)]
pub struct PollableFdState {
    /// The root pointer (see the type).
    pub(crate) _this: *mut PollableFdState,
    /// Reference count, managed by [`PollableFdStatePtr`].
    pub(crate) _refs: u32,
    /// The descriptor.
    pub fd: FileDesc,
    /// Whether one consumer waits for both read and write readiness (as `accept()` does).
    pub events_rw: bool,
    /// Which sides were shut down ([`RCV_SHUTDOWN`](crate::engine::core::posix::RCV_SHUTDOWN),
    /// [`SND_SHUTDOWN`](crate::engine::core::posix::SND_SHUTDOWN)); kept here because for UDP
    /// the kernel gives no shutdown indication.
    pub shutdown_mask: u32,
    /// Events wanted by pending readable/writeable promises.
    pub events_requested: i32,
    /// Events installed in epoll.
    pub events_epoll: i32,
    /// Events returned from epoll and not consumed yet.
    pub events_known: i32,
}

impl PollableFdState {
    /// A state for `fd` with the initial speculation `speculate`.
    #[inline]
    pub fn new(fd: FileDesc, speculate: Speculation) -> Self {
        PollableFdState {
            _this: std::ptr::null_mut(),
            _refs: 0,
            fd,
            events_rw: false,
            shutdown_mask: 0,
            events_requested: 0,
            events_epoll: 0,
            events_known: speculate.events,
        }
    }

    /// The state's root pointer (see the type): the base with the provenance of the whole
    /// backend state.
    #[inline]
    pub(crate) fn root(&self) -> *mut PollableFdState {
        self._this
    }

    /// The derived state of the backend that made this one, through the root
    /// pointer. Only the derived type's own fields may be reached through the result while a
    /// reference to the base is live.
    ///
    /// # Safety
    /// The state was made by the backend whose derived state is `T` (`#[repr(C)]`, base
    /// first).
    #[inline]
    pub(crate) unsafe fn derived<T>(&self) -> *mut T {
        self._this.cast::<T>()
    }

    /// Sets the speculation of the given I/O events.
    ///
    /// If an I/O completed without blocking and without a short read or write, the next I/O
    /// is expected not to block either (not to return `EAGAIN`). The speculation is
    /// invalidated once it is used by [`take_speculation`](Self::take_speculation).
    #[inline]
    pub fn speculate_epoll(&mut self, events: i32) {
        self.events_known |= events;
    }

    /// Checks whether I/O for `events` is speculated to be possible on the descriptor, and
    /// invalidates the speculation if it covers all of `events`.
    ///
    /// Returns `true` if the current speculation includes all of `events`.
    #[inline]
    pub fn take_speculation(&mut self, events: i32) -> bool {
        // invalidate the speculation set by the last speculate_epoll() call,
        if self.events_known & events != 0 {
            self.events_known &= !events;
            return true;
        }
        false
    }
}

/// Takes a reference on the state `fd`.
///
/// # Safety
/// `fd` points to a live state.
#[inline]
pub unsafe fn intrusive_ptr_add_ref(fd: *mut PollableFdState) {
    // SAFETY: the contract above.
    unsafe { (*fd)._refs += 1 };
}

/// A counted reference to a [`PollableFdState`]; the backend frees the state when the last
/// reference goes.
pub struct PollableFdStatePtr {
    p: *mut PollableFdState,
}

impl PollableFdStatePtr {
    /// Adopts a state made by a backend and takes a reference.
    ///
    /// # Safety
    /// `p` is null or points to a live state made by a backend (`Box::into_raw` of the
    /// backend's derived state, whose `PollableFdState` is its first field), with the
    /// provenance of the whole derived state: the backend converts it back.
    #[inline]
    pub unsafe fn new(p: *mut PollableFdState) -> Self {
        if !p.is_null() {
            // SAFETY: the contract above.
            unsafe { intrusive_ptr_add_ref(p) };
        }
        PollableFdStatePtr { p }
    }

    /// The empty pointer.
    #[inline]
    pub const fn null() -> Self {
        PollableFdStatePtr { p: std::ptr::null_mut() }
    }

    /// The state, or null.
    #[inline]
    pub fn get(&self) -> *mut PollableFdState {
        self.p
    }

    /// Whether the pointer holds a state.
    #[inline]
    pub fn is_some(&self) -> bool {
        !self.p.is_null()
    }

    /// Drops the reference (freeing the state if it was the last) and leaves the pointer
    /// empty.
    #[inline]
    pub fn reset(&mut self) {
        let old = std::mem::replace(&mut self.p, std::ptr::null_mut());
        if !old.is_null() {
            // SAFETY: `old` is the root pointer this owner held a reference on; no reference
            // to the state is live while an owner is reset.
            unsafe { intrusive_ptr_release(old) };
        }
    }
}

impl Clone for PollableFdStatePtr {
    #[inline]
    fn clone(&self) -> Self {
        // SAFETY: `self` keeps its state alive (or is null).
        unsafe { PollableFdStatePtr::new(self.p) }
    }
}

impl Drop for PollableFdStatePtr {
    #[inline]
    fn drop(&mut self) {
        self.reset();
    }
}

/// A non-blocking descriptor whose operations are driven by the reactor and return futures.
///
/// Copies share one state; closing one copy releases only its reference.
#[derive(Clone)]
pub struct PollableFd {
    _s: PollableFdStatePtr,
}

/// Whether a shutdown only calls the kernel (`YES`) or also records the shut-down sides in
/// the state (`NO`), so later operations fail with `ECONNABORTED` even where the kernel
/// would only report `EAGAIN` (UDP).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownKernelOnly(pub bool);

impl ShutdownKernelOnly {
    /// Only call the kernel.
    pub const YES: ShutdownKernelOnly = ShutdownKernelOnly(true);
    /// Also record the shut-down sides in the state.
    pub const NO: ShutdownKernelOnly = ShutdownKernelOnly(false);
}

impl Default for PollableFd {
    /// A closed descriptor (no state).
    fn default() -> Self {
        PollableFd { _s: PollableFdStatePtr::null() }
    }
}

impl PollableFd {
    /// Wraps `fd` (which must be non-blocking) in a state made by the current shard's
    /// backend, with no speculation.
    #[inline]
    pub fn from_file_desc(fd: FileDesc) -> Self {
        PollableFd::new(fd, Speculation::default())
    }

    /// Wraps a state made by the reactor backend.
    #[inline]
    pub(crate) fn from_state(s: PollableFdStatePtr) -> Self {
        PollableFd { _s: s }
    }

    /// The state, as the pointer the backend made: the copies of a `PollableFd` share it, so it
    /// is never handed out as a reference tied to one copy.
    ///
    /// Invariant: a `PollableFd` that is used has a state; only debug builds check it.
    #[inline]
    pub(crate) fn state_ptr(&self) -> *mut PollableFdState {
        let p = self._s.get();
        debug_assert!(!p.is_null(), "use of an empty or closed pollable_fd");
        p
    }

    /// The state for one call.
    ///
    /// # Safety
    /// The result is used for a single call that does not reach the same state through
    /// another copy of this `pollable_fd` before it returns (the operations only register
    /// with the backend and return a future, whose continuations keep the state pointer, not
    /// this reference). The `pollable_fd` has a state (see [`state_ptr`](Self::state_ptr)).
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub(crate) unsafe fn state_mut(&self) -> &mut PollableFdState {
        // SAFETY: the contract above; the state is kept alive by this copy.
        unsafe { &mut *self.state_ptr() }
    }

    /// Reads at most `size` bytes into `buffer` once data is available; resolves with the
    /// number of bytes read (0 at end of stream).
    ///
    /// # Safety
    /// `buffer` is valid for writes of `size` bytes and used by no one else until the returned future resolves.
    #[inline]
    pub unsafe fn read_some_chars(&mut self, buffer: *mut u8, size: usize) -> Future<usize> {
        // SAFETY: forwarded from the caller; the state is live while `self` is, and passed by
        // its root pointer.
        unsafe { PollableFdState::read_some_chars(self.state_ptr(), buffer, size) }
    }
    /// Reads at most `size` bytes into `buffer`; same as
    /// [`read_some_chars`](Self::read_some_chars).
    ///
    /// # Safety
    /// `buffer` is valid for writes of `size` bytes and used by no one else until the returned future resolves.
    #[inline]
    pub unsafe fn read_some_bytes(&mut self, buffer: *mut u8, size: usize) -> Future<usize> {
        // SAFETY: forwarded from the caller; the state is live while `self` is, and passed by
        // its root pointer.
        unsafe { PollableFdState::read_some_bytes(self.state_ptr(), buffer, size) }
    }
    /// Reads into the buffers of `iov` once data is available; resolves with the number of
    /// bytes read.
    #[inline]
    pub fn read_some_iov(&mut self, iov: &Vec<Iovec>) -> Future<usize> {
        // SAFETY: the state is live while `self` is, and passed by its root pointer.
        unsafe { PollableFdState::read_some_iov(self.state_ptr(), iov) }
    }
    /// Reads some data into a buffer obtained from `ba` once data is available; resolves with
    /// the buffer (empty at end of stream).
    ///
    /// # Safety
    /// `ba` points to a live allocator that outlives the returned future.
    #[inline]
    pub unsafe fn read_some(
        &mut self,
        ba: *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: forwarded from the caller; the state is live while `self` is, and passed by
        // its root pointer.
        unsafe { PollableFdState::read_some(self.state_ptr(), ba) }
    }
    /// Writes all `size` bytes of `buffer`, waiting for writability as needed.
    ///
    /// # Safety
    /// `buffer` is valid for reads of `size` bytes and not written or freed until the returned future resolves.
    #[inline]
    pub unsafe fn write_all_chars(&mut self, buffer: *const u8, size: usize) -> Future<()> {
        // SAFETY: forwarded from the caller; the state is live while `self` is, and passed by
        // its root pointer.
        unsafe { PollableFdState::write_all_chars(self.state_ptr(), buffer, size) }
    }
    /// Writes all `size` bytes of `buffer`; same as [`write_all_chars`](Self::write_all_chars).
    ///
    /// # Safety
    /// `buffer` is valid for reads of `size` bytes and not written or freed until the returned future resolves.
    #[inline]
    pub unsafe fn write_all_bytes(&mut self, buffer: *const u8, size: usize) -> Future<()> {
        // SAFETY: forwarded from the caller; the state is live while `self` is, and passed by
        // its root pointer.
        unsafe { PollableFdState::write_all_bytes(self.state_ptr(), buffer, size) }
    }
    /// Writes some of the packet `p`; resolves with the number of bytes written.
    #[inline]
    pub fn write_some(&mut self, p: &mut Packet) -> Future<usize> {
        // SAFETY: the state is live while `self` is, and passed by its root pointer.
        unsafe { PollableFdState::write_some(self.state_ptr(), p) }
    }
    /// Writes the whole packet `p`.
    #[inline]
    pub fn write_all(&mut self, p: &mut Packet) -> Future<()> {
        // SAFETY: the state is live while `self` is, and passed by its root pointer.
        unsafe { PollableFdState::write_all(self.state_ptr(), p) }
    }
    /// Resolves when the descriptor is readable.
    #[inline]
    pub fn readable(&mut self) -> Future<()> {
        // SAFETY: the state is live while `self` is, and passed by its root pointer.
        unsafe { PollableFdState::readable(self.state_ptr()) }
    }
    /// Resolves when the descriptor is writeable.
    #[inline]
    pub fn writeable(&mut self) -> Future<()> {
        // SAFETY: the state is live while `self` is, and passed by its root pointer.
        unsafe { PollableFdState::writeable(self.state_ptr()) }
    }
    /// Resolves when the descriptor is readable or writeable.
    #[inline]
    pub fn readable_or_writeable(&mut self) -> Future<()> {
        // SAFETY: the state is live while `self` is, and passed by its root pointer.
        unsafe { PollableFdState::readable_or_writeable(self.state_ptr()) }
    }
    /// Accepts a connection on a listening socket; resolves with the new descriptor and the
    /// peer address.
    #[inline]
    pub fn accept(&mut self) -> Future<(PollableFd, SocketAddress)> {
        // SAFETY: the state is live while `self` is, and passed by its root pointer.
        unsafe { PollableFdState::accept(self.state_ptr()) }
    }
    /// Connects the socket to `sa`; resolves when the connection is established.
    #[inline]
    pub fn connect(&mut self, sa: &mut SocketAddress) -> Future<()> {
        // SAFETY: the state is live while `self` is, and passed by its root pointer.
        unsafe { PollableFdState::connect(self.state_ptr(), sa) }
    }
    /// Receives some data into a buffer obtained from `ba`; resolves with the buffer.
    ///
    /// # Safety
    /// `ba` points to a live allocator that outlives the returned future.
    #[inline]
    pub unsafe fn recv_some(
        &mut self,
        ba: *mut dyn BufferAllocator,
    ) -> Future<TemporaryBuffer<u8>> {
        // SAFETY: forwarded from the caller; the state is live while `self` is, and passed by
        // its root pointer.
        unsafe { PollableFdState::recv_some(self.state_ptr(), ba) }
    }
    /// Sends the message `msg`; resolves with the number of bytes sent.
    ///
    /// # Safety
    /// `msg` and the buffers it describes stay valid and unchanged until the returned future resolves.
    #[inline]
    pub unsafe fn sendmsg(&mut self, msg: *mut Msghdr) -> Future<usize> {
        // SAFETY: forwarded from the caller; the state is live while `self` is, and passed by
        // its root pointer.
        unsafe { PollableFdState::sendmsg(self.state_ptr(), msg) }
    }
    /// Receives a message into `msg`; resolves with the number of bytes received.
    ///
    /// # Safety
    /// `msg` and the buffers it describes stay valid, and are used by no one else, until the returned future resolves.
    #[inline]
    pub unsafe fn recvmsg(&mut self, msg: *mut Msghdr) -> Future<usize> {
        // SAFETY: forwarded from the caller; the state is live while `self` is, and passed by
        // its root pointer.
        unsafe { PollableFdState::recvmsg(self.state_ptr(), msg) }
    }
    /// Sends `len` bytes of `buf` to `addr`; resolves with the number of bytes sent.
    ///
    /// # Safety
    /// `buf` is valid for reads of `len` bytes and not written or freed until the returned future resolves.
    #[inline]
    pub unsafe fn sendto(
        &mut self,
        addr: SocketAddress,
        buf: *const c_void,
        len: usize,
    ) -> Future<usize> {
        // SAFETY: forwarded from the caller; the state is live while `self` is, and passed by
        // its root pointer.
        unsafe { PollableFdState::sendto(self.state_ptr(), addr, buf, len) }
    }
    /// The descriptor, as a shared reference (the system calls of a [`FileDesc`] take
    /// `&self`). It must not be held across an operation of another copy of this
    /// `PollableFd` (which reborrows the state).
    #[inline]
    pub fn get_file_desc(&self) -> &FileDesc {
        // SAFETY: the state is live while `self` is; a shared reborrow of its descriptor.
        unsafe { &(*self.state_ptr()).fd }
    }
    /// The descriptor, for callers that hold the `PollableFd` shared and only make calls that
    /// take the descriptor by `&self` (socket options, addresses).
    #[inline]
    pub fn get_file_desc_ref(&self) -> &FileDesc {
        self.get_file_desc()
    }
    /// The descriptor, mutably, to move or replace it.
    ///
    /// # Safety
    /// No other copy of this `pollable_fd` uses the state while the result is live.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn get_file_desc_mut(&mut self) -> &mut FileDesc {
        // SAFETY: the contract above; the state is live while `self` is.
        unsafe { &mut (*self.state_ptr()).fd }
    }
    /// Releases this copy's reference to the state; the descriptor is closed when the last
    /// copy goes.
    #[inline]
    pub fn close(&mut self) {
        self._s.reset();
    }
    /// Whether this copy holds a state.
    #[inline]
    pub fn is_open(&self) -> bool {
        self._s.is_some()
    }
    /// Resolves when the peer has shut down its side of the connection (`EPOLLRDHUP`).
    #[inline]
    pub fn poll_rdhup(&mut self) -> Future<()> {
        // SAFETY: the state is live while `self` is, and passed by its root pointer.
        unsafe { PollableFdState::poll_rdhup(self.state_ptr()) }
    }
    /// The raw descriptor number.
    #[inline]
    pub(crate) fn get_fd(&self) -> i32 {
        // SAFETY: as in state_ptr(); a shared read.
        unsafe { (*self.state_ptr()).fd.get() }
    }
    /// Fails with `ECONNABORTED` if the receive side was shut down.
    // The reactor calls the state's version directly.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn maybe_no_more_recv(
        &mut self,
    ) -> Result<(), crate::engine::core::future::ExceptionPtr> {
        // SAFETY: the shared state is reborrowed for this call only (see `state_mut`).
        unsafe { self.state_mut().maybe_no_more_recv() }
    }
    /// Fails with `ECONNABORTED` if the send side was shut down.
    // The reactor calls the state's version directly.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn maybe_no_more_send(
        &mut self,
    ) -> Result<(), crate::engine::core::future::ExceptionPtr> {
        // SAFETY: the shared state is reborrowed for this call only (see `state_mut`).
        unsafe { self.state_mut().maybe_no_more_send() }
    }
}

/// An eventfd read through the reactor and written by another thread.
pub struct ReadableEventfd {
    /// The reactor-driven descriptor.
    pub(crate) _fd: PollableFd,
}

impl ReadableEventfd {
    /// Creates the eventfd with counter `initial`.
    ///
    /// # Errors
    ///
    /// Returns the system error when the eventfd cannot be created.
    pub fn new(initial: usize) -> Result<Self, ExceptionPtr> {
        Ok(ReadableEventfd { _fd: PollableFd::from_file_desc(Self::try_create_eventfd(initial)?) })
    }
    /// The descriptor other threads write to.
    #[inline]
    pub fn get_write_fd(&self) -> i32 {
        self._fd.get_fd()
    }
    /// Wraps an existing eventfd.
    pub(crate) fn from_file_desc(fd: FileDesc) -> Self {
        ReadableEventfd { _fd: PollableFd::from_file_desc(fd) }
    }
}

/// An eventfd written by the reactor and read (blocking) by another thread.
pub struct WriteableEventfd {
    /// The descriptor.
    pub(crate) _fd: FileDesc,
}

impl WriteableEventfd {
    /// Creates the eventfd with counter `initial`.
    ///
    /// # Errors
    ///
    /// Returns the system error when the eventfd cannot be created.
    pub fn new(initial: usize) -> Result<Self, ExceptionPtr> {
        Ok(WriteableEventfd { _fd: Self::try_create_eventfd(initial)? })
    }
    /// The descriptor the other thread reads from.
    #[inline]
    pub fn get_read_fd(&self) -> i32 {
        self._fd.get()
    }
    /// Wraps an existing eventfd.
    pub(crate) fn from_file_desc(fd: FileDesc) -> Self {
        WriteableEventfd { _fd: fd }
    }
}
