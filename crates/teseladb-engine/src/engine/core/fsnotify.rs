//! Filesystem modification notifications over Linux inotify.
//!
//! [`Fsnotifier`] owns a non-blocking inotify descriptor polled by this shard's reactor.
//! [`Fsnotifier::create_watch`] registers a path for a set of [`FsnotifierFlags`] and
//! returns an RAII [`FsnotifierWatch`] that removes the watch when dropped;
//! [`Fsnotifier::wait`] resolves with the next batch of [`FsnotifierEvent`]s. Rename events
//! are paired through their [`SequenceNo`].
//!
//! The API is experimental and also re-exported from [`experimental`]. Everything runs on
//! the shard that created the notifier: the notifier and its watches share its state
//! through a non-atomic shared pointer.

use crate::engine::core::future::{ExceptionPtr, Future, RuntimeError, make_exception_future};
use crate::engine::core::internal::pollable_fd::PollableFd;
use crate::engine::core::posix::{FileDesc, throw_system_error_on};
use crate::engine::core::reactor::engine;
use crate::engine::core::reactor_backend::errno;
use crate::engine::core::shared_ptr::{EnableSharedFromThis, SharedPtr, make_shared_esft};
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;

/// The notifier API under its experimental path.
pub mod experimental {
    pub use super::{
        Fsnotifier, FsnotifierEvent, FsnotifierFlags, FsnotifierWatch, SequenceNo, WatchToken,
    };
}

// Linux inotify constants and record layout.
const IN_ACCESS: u32 = 0x0000_0001;
const IN_MODIFY: u32 = 0x0000_0002;
const IN_ATTRIB: u32 = 0x0000_0004;
const IN_CLOSE_WRITE: u32 = 0x0000_0008;
const IN_CLOSE_NOWRITE: u32 = 0x0000_0010;
const IN_CLOSE: u32 = IN_CLOSE_WRITE | IN_CLOSE_NOWRITE;
const IN_OPEN: u32 = 0x0000_0020;
const IN_MOVED_FROM: u32 = 0x0000_0040;
const IN_MOVED_TO: u32 = 0x0000_0080;
const IN_MOVE: u32 = IN_MOVED_FROM | IN_MOVED_TO;
const IN_CREATE: u32 = 0x0000_0100;
const IN_DELETE: u32 = 0x0000_0200;
const IN_DELETE_SELF: u32 = 0x0000_0400;
const IN_MOVE_SELF: u32 = 0x0000_0800;
const IN_IGNORED: u32 = 0x0000_8000;
const IN_ONLYDIR: u32 = 0x0100_0000;
const IN_ONESHOT: u32 = 0x8000_0000;
const IN_NONBLOCK: i32 = 0o4000;
const IN_CLOEXEC: i32 = 0o2000000;
const EINVAL: i32 = 22;

/// The fixed header of an inotify event record; `len` bytes of NUL-padded name follow.
#[repr(C)]
struct InotifyEvent {
    wd: i32,
    mask: u32,
    cookie: u32,
    len: u32,
    name: [u8; 0],
}

unsafe extern "C" {
    fn inotify_rm_watch(fd: i32, wd: i32) -> i32;
}

/// Flags of the events supported by [`Fsnotifier`].
///
/// The flags are bit-identical to the inotify `IN_*` masks, and `&`, `|`, `&=` and `|=`
/// take their intersection or union directly.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FsnotifierFlags(u32);

impl FsnotifierFlags {
    /// File was accessed (e.g., read(2), execve(2)).
    pub const ACCESS: FsnotifierFlags = FsnotifierFlags(IN_ACCESS);
    /// Metadata changed, for example permissions, timestamps or extended attributes.
    pub const ATTRIB: FsnotifierFlags = FsnotifierFlags(IN_ATTRIB);
    /// File opened for writing was closed.
    pub const CLOSE_WRITE: FsnotifierFlags = FsnotifierFlags(IN_CLOSE_WRITE);
    /// File or directory not opened for writing was closed.
    pub const CLOSE_NOWRITE: FsnotifierFlags = FsnotifierFlags(IN_CLOSE_NOWRITE);
    /// File or directory created in the watched directory.
    pub const CREATE_CHILD: FsnotifierFlags = FsnotifierFlags(IN_CREATE);
    /// File or directory deleted from the watched directory.
    pub const DELETE_CHILD: FsnotifierFlags = FsnotifierFlags(IN_DELETE);
    /// The watched file or directory was itself deleted (also when it is moved to another
    /// filesystem).
    pub const DELETE_SELF: FsnotifierFlags = FsnotifierFlags(IN_DELETE_SELF);
    /// File was modified (e.g., write(2), truncate(2)).
    pub const MODIFY: FsnotifierFlags = FsnotifierFlags(IN_MODIFY);
    /// The watched file or directory was itself moved.
    pub const MOVE_SELF: FsnotifierFlags = FsnotifierFlags(IN_MOVE_SELF);
    /// Generated for the directory containing the old file name when a file is renamed.
    pub const MOVE_FROM: FsnotifierFlags = FsnotifierFlags(IN_MOVED_FROM);
    /// Generated for the directory containing the new file name when a file is renamed.
    pub const MOVE_TO: FsnotifierFlags = FsnotifierFlags(IN_MOVED_TO);
    /// File was opened.
    pub const OPEN: FsnotifierFlags = FsnotifierFlags(IN_OPEN);
    /// [`FsnotifierFlags::CLOSE_WRITE`] | [`FsnotifierFlags::CLOSE_NOWRITE`].
    pub const CLOSE: FsnotifierFlags = FsnotifierFlags(IN_CLOSE);
    /// [`FsnotifierFlags::MOVE_FROM`] | [`FsnotifierFlags::MOVE_TO`].
    pub const MOVE: FsnotifierFlags = FsnotifierFlags(IN_MOVE);
    /// Listen for a single notification only, after which the token is invalid.
    pub const ONESHOT: FsnotifierFlags = FsnotifierFlags(IN_ONESHOT);
    /// Generated when a watch is removed or the watched file is deleted.
    pub const IGNORED: FsnotifierFlags = FsnotifierFlags(IN_IGNORED);
    /// Watch the path only if it is a directory; otherwise the watch fails with
    /// `ENOTDIR`. This is a race-free way of ensuring the monitored object is a directory.
    pub const ONLYDIR: FsnotifierFlags = FsnotifierFlags(IN_ONLYDIR);

    /// The underlying inotify mask.
    #[inline]
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// The flags with the given inotify mask bits.
    #[inline]
    pub const fn from_bits(bits: u32) -> Self {
        FsnotifierFlags(bits)
    }
}

/// Token of a watch point (the inotify watch descriptor).
pub type WatchToken = i32;
/// Sequence number associating related events.
///
/// It is only used for rename events ([`FsnotifierFlags::MOVE_FROM`] and
/// [`FsnotifierFlags::MOVE_TO`]) and is 0 for other types.
pub type SequenceNo = u32;

/// RAII wrapper around a [`WatchToken`]: the watch of the path is removed when the
/// wrapper is dropped.
pub struct FsnotifierWatch {
    _token: WatchToken,
    _impl: Option<SharedPtr<FsnotifierImpl>>,
}

impl FsnotifierWatch {
    /// The token of this watch point.
    #[inline]
    pub fn token(&self) -> WatchToken {
        self._token
    }
}

impl From<&FsnotifierWatch> for WatchToken {
    /// Cast this watch point to a watch token.
    #[inline]
    fn from(w: &FsnotifierWatch) -> WatchToken {
        w._token
    }
}

/// Filesystem modification notifier.
///
/// A thin wrapper around Linux inotify (see `inotify(7)`), the light-weight filesystem
/// modification watch interface of Linux, usable to log filesystem activity, reload
/// configuration and similar.
///
/// It provides a buffered read of events and RAII handling of the watches.
///
/// This is an experimental API: backward-incompatible changes may still be made. Watches
/// that are re-registered for the same path are not handled specially.
pub struct Fsnotifier {
    _impl: SharedPtr<FsnotifierImpl>,
}

/// One filesystem event, decoded from an inotify event record.
#[derive(Clone, Debug)]
pub struct FsnotifierEvent {
    /// The watch that produced the event.
    pub id: WatchToken,
    /// The event flags.
    pub mask: FsnotifierFlags,
    /// Correlates [`FsnotifierFlags::MOVE_FROM`] with [`FsnotifierFlags::MOVE_TO`].
    pub seq: SequenceNo,
    /// File name inside a watched directory; empty when the kernel gives none.
    pub name: Sstring,
}

impl std::ops::BitOr for FsnotifierFlags {
    type Output = FsnotifierFlags;
    /// The union of two flag sets.
    #[inline]
    fn bitor(self, b: FsnotifierFlags) -> FsnotifierFlags {
        FsnotifierFlags(self.0 | b.0)
    }
}

impl std::ops::BitOrAssign for FsnotifierFlags {
    /// The union of two flag sets, assignment form.
    #[inline]
    fn bitor_assign(&mut self, b: FsnotifierFlags) {
        *self = *self | b;
    }
}

impl std::ops::BitAnd for FsnotifierFlags {
    type Output = FsnotifierFlags;
    /// The intersection of two flag sets.
    #[inline]
    fn bitand(self, b: FsnotifierFlags) -> FsnotifierFlags {
        FsnotifierFlags(self.0 & b.0)
    }
}

impl std::ops::BitAndAssign for FsnotifierFlags {
    /// The intersection of two flag sets, assignment form.
    #[inline]
    fn bitand_assign(&mut self, b: FsnotifierFlags) {
        *self = *self & b;
    }
}

/// The state shared by a notifier and its watches: the inotify descriptor, polled by the
/// reactor, and the dummy watch used by [`FsnotifierImpl::shutdown`].
pub struct FsnotifierImpl {
    _esft: EnableSharedFromThis,
    // The raw descriptor is reached through `fd()`.
    _fd: PollableFd,
    _close_dummy: WatchToken,
}

crate::shared_from_this_impl!(FsnotifierImpl { _esft });

impl FsnotifierImpl {
    /// Opens a non-blocking, close-on-exec inotify descriptor.
    fn new() -> Result<Self, ExceptionPtr> {
        Ok(FsnotifierImpl {
            _esft: EnableSharedFromThis::new(),
            _fd: PollableFd::from_file_desc(FileDesc::inotify_init(IN_NONBLOCK | IN_CLOEXEC)?),
            _close_dummy: -1,
        })
    }

    /// The raw inotify descriptor.
    #[inline]
    fn fd(&mut self) -> i32 {
        self._fd.get_file_desc().get()
    }

    /// Whether the descriptor is still open (the notifier was not shut down).
    #[inline]
    pub fn active(&self) -> bool {
        self._fd.is_open()
    }

    /// Removes the watch `token`; does nothing once the notifier is shut down.
    ///
    /// # Errors
    /// A system error when `inotify_rm_watch` fails with anything but `EINVAL` (the watch
    /// was already gone).
    pub fn remove_watch(&mut self, token: WatchToken) -> Result<(), ExceptionPtr> {
        if self.active() {
            // SAFETY: plain system call on our inotify descriptor.
            let res = unsafe { inotify_rm_watch(self.fd(), token) };
            // EINVAL means the watch is already gone; any other error is reported.
            throw_system_error_on(
                res == -1 && errno() != EINVAL,
                "could not remove inotify watch",
            )?;
        }
        Ok(())
    }

    /// Adds a watch of `path` for `events`; resolves with its token, or fails with the
    /// system error of `inotify_add_watch`, or with a runtime error once the notifier is
    /// closed.
    pub fn create_watch(&mut self, path: &Sstring, events: FsnotifierFlags) -> Future<WatchToken> {
        if !self.active() {
            return make_exception_future(ExceptionPtr::from(RuntimeError(
                "attempting to use closed notifier".into(),
            )));
        }
        let fd = self.fd();
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only (see `engine()`).
        unsafe { (*engine()).inotify_add_watch(fd, path.as_str(), events.bits()) }
    }

    /// Reads the next batch of events. `me` is the caller's shared pointer to the impl,
    /// which the continuation keeps alive (and frees through, if it is the last owner).
    pub fn wait(me: SharedPtr<Self>) -> Future<Vec<FsnotifierEvent>> {
        // Align the buffer for the event records.
        let mut buf = TemporaryBuffer::<u8>::aligned(
            std::mem::align_of::<InotifyEvent>().max(std::mem::align_of::<i64>()),
            4096,
        );
        // SAFETY: `buf`'s storage is moved into the continuation below, which keeps it alive
        // and unused until the read resolves.
        // SAFETY: the impl is alive while `me` is; the reborrow lasts this call, and the
        // buffer as in the comment above.
        let f = unsafe { me.get_mut_unchecked()._fd.read_some_chars(buf.get_write(), buf.size()) };
        f.then(move |n: usize| -> Result<Vec<FsnotifierEvent>, ExceptionPtr> {
            let mut p = buf.get();
            // SAFETY: the read filled `n` bytes of `buf`.
            let e = unsafe { buf.get().add(n) };

            let mut events: Vec<FsnotifierEvent> = Vec::new();

            while p < e {
                let ev = p as *const InotifyEvent;
                // SAFETY: the kernel writes whole, aligned inotify_event records.
                let (wd, mask, cookie, len) =
                    unsafe { ((*ev).wd, (*ev).mask, (*ev).cookie, (*ev).len) };
                // SAFETY: the impl is alive while `me` is; the reactor is single-threaded.
                let m = unsafe { me.get_mut_unchecked() };
                if wd == m._close_dummy && m._close_dummy != -1 {
                    m._fd.close();
                } else {
                    let name = if len != 0 {
                        // SAFETY: a non-empty name is NUL-terminated (and padded) inside the record.
                        let c = unsafe {
                            std::ffi::CStr::from_ptr(
                                (&raw const (*ev).name) as *const std::ffi::c_char,
                            )
                        };
                        Sstring::from_bytes(c.to_bytes())
                    } else {
                        Sstring::default()
                    };
                    events.push(FsnotifierEvent {
                        id: wd,
                        mask: FsnotifierFlags(mask),
                        seq: cookie,
                        name,
                    });
                }
                // SAFETY: the next record starts right after this one, at most at `e`.
                p = unsafe { p.add(std::mem::size_of::<InotifyEvent>() + len as usize) };
            }

            Ok(events)
        })
    }

    /// Wakes any pending [`FsnotifierImpl::wait`] by adding and then removing a dummy watch
    /// on `/`. `me` as in [`FsnotifierImpl::wait`].
    pub fn shutdown(me: SharedPtr<Self>) {
        // The reactor has no way to shut down a pending non-socket read, so a watch is
        // created on something ubiquitous and then removed; removal produces an event that
        // wakes any waiter, which ignores the event of this dummy watch.
        // SAFETY: the impl is alive while `me` is; the reborrow lasts this call.
        let f = unsafe { me.get_mut_unchecked() }
            .create_watch(&Sstring::from("/"), FsnotifierFlags::DELETE_SELF);
        // FIXME: the future is discarded.
        let _ = f.then(move |t: WatchToken| {
            // SAFETY: the impl is alive while `me` is.
            let m = unsafe { me.get_mut_unchecked() };
            m._close_dummy = t;
            m.remove_watch(t)
        });
    }
}

impl Drop for FsnotifierWatch {
    /// Removes the watch from the notifier.
    fn drop(&mut self) {
        if let Some(i) = self._impl.as_ref() {
            // SAFETY: the impl is alive while we hold it; the reactor is single-threaded.
            if unsafe { i.get_mut_unchecked() }.remove_watch(self._token).is_err() {
                // A drop cannot report the failure; a failed removal is treated as fatal.
                std::process::abort();
            }
        }
    }
}

impl FsnotifierWatch {
    /// Detaches the watch and returns its token. The watch of the path is not removed;
    /// only the internal reference to the notifier is released.
    #[inline]
    pub fn release(&mut self) -> WatchToken {
        self._impl = None;
        self._token
    }

    /// A watch of `token` on the notifier state `impl`.
    #[inline]
    fn new(impl_: SharedPtr<FsnotifierImpl>, token: WatchToken) -> Self {
        FsnotifierWatch { _token: token, _impl: Some(impl_) }
    }
}

impl Fsnotifier {
    /// Creates a notifier with its own inotify descriptor.
    ///
    /// # Errors
    /// The system error of `inotify_init1` when it fails.
    pub fn new() -> Result<Self, ExceptionPtr> {
        Ok(Fsnotifier { _impl: make_shared_esft(FsnotifierImpl::new()?) })
    }

    /// Monitors the events in `events` for the file or directory at `path`.
    ///
    /// The future resolves with the watch once `inotify_add_watch(2)` completes, or fails
    /// with its system error.
    pub fn create_watch(
        &mut self,
        path: &Sstring,
        events: FsnotifierFlags,
    ) -> Future<FsnotifierWatch> {
        let impl_ = self._impl.clone();
        // SAFETY: the impl is alive while we hold it; the reactor is single-threaded.
        unsafe { self._impl.get_mut_unchecked() }.create_watch(path, events).then(
            move |token: WatchToken| {
                crate::engine::core::future::make_ready_future(FsnotifierWatch::new(impl_, token))
            },
        )
    }

    /// Waits for events; the future resolves with the events read when registered events
    /// occur.
    pub fn wait(&self) -> Future<Vec<FsnotifierEvent>> {
        FsnotifierImpl::wait(self._impl.clone())
    }

    /// Shuts the notifier down and wakes any pending [`Fsnotifier::wait`].
    ///
    /// After shutdown all watches are invalid and no new ones can be created.
    pub fn shutdown(&mut self) {
        FsnotifierImpl::shutdown(self._impl.clone());
    }

    /// Whether the notifier is active (not shut down).
    #[inline]
    pub fn active(&self) -> bool {
        self._impl.active()
    }
}
