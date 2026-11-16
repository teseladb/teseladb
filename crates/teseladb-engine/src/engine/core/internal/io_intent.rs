//! Intrusive bookkeeping behind I/O cancellation intents.
//!
//! An I/O intent (see `core::io_intent`) lets a caller cancel a group of requests. Two
//! intrusive structures, which allocate nothing, tie requests to their intent:
//!
//! * [`CancellableQueue`] and its [`Link`]s chain the requests of one intent that are still
//!   queued in an I/O queue, so cancelling the intent removes and fails them all. A link is a
//!   single word: the first link of a queue points back at the queue, the others form the
//!   singly-linked rest list.
//! * [`IntentReference`] is a "safe" reference from an in-flight operation to its intent: the
//!   intent keeps a list of its references and, when cancelled, marks each one so that
//!   [`IntentReference::retrieve`] reports the cancellation instead of returning a dangling
//!   pointer.
//!
//! These structures hold raw pointers into each other; the `# Safety` sections state the
//! address-stability rules their callers keep.

use crate::engine::core::future::ExceptionPtr;
use std::marker::PhantomPinned;
use std::pin::Pin;

use crate::engine::core::io_intent::IoIntent;
use crate::engine::core::reactor_backend::engine_assert;

/// The single word of a [`Link`]: the queue pointer (first link) or the next link (rest list).
#[repr(C)]
#[derive(Clone, Copy)]
union LinkU {
    _ref: *mut CancellableQueue,
    _next: *mut Link,
}

/// A link embedded in a queued request. While the request is the first of its queue the link
/// points back at the queue; otherwise it is the hook of the queue's rest list.
#[repr(C)]
pub struct Link {
    u: LinkU,
}

const _: () = assert!(
    std::mem::size_of::<Link>() == std::mem::size_of::<usize>(),
    "cancellable_queue::link size is too big"
);

impl Default for Link {
    /// An unqueued link.
    fn default() -> Self {
        Link { u: LinkU { _ref: std::ptr::null_mut() } }
    }
}

impl Link {
    /// Appends the link's request to queue `cq`.
    ///
    /// # Safety
    /// `this` is the `_intent` link of a live `QueuedIoRequest`, derived (with
    /// `addr_of_mut!`) from the pointer to the whole request, which neither moves nor is
    /// freed while it is queued; `cq` points to a live queue that does not move while it has
    /// links (see `CancellableQueue::push_back`).
    #[inline]
    pub unsafe fn enqueue(this: *mut Link, cq: *mut CancellableQueue) {
        // SAFETY: forwarded from the caller.
        unsafe { CancellableQueue::push_back(cq, this) };
    }

    /// Removes the link's request from its queue if it is the queue's first one (called when
    /// the request is dispatched).
    ///
    /// # Safety
    /// `this` is a live link, either unqueued or the first link of its queue. The requests of
    /// one cancellable queue sit in the same fair queue class and are dispatched in queue
    /// order, so a link in the rest list (whose word holds the list hook, not the queue
    /// pointer) is never dispatched before the first one.
    #[inline]
    pub unsafe fn maybe_dequeue(this: *mut Link) {
        // SAFETY: both members are pointers; by the contract above the link is unqueued (null
        // `_ref`) or first (`_ref` is its queue).
        let r = unsafe { (*this).u._ref };
        if !r.is_null() {
            // SAFETY: a first link points at its live queue, through the pointer the queue
            // stored itself (`push_back`, `pop_front`, `relocated`).
            unsafe { CancellableQueue::pop_front(r) };
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        // SAFETY: reading the pointer member. A link dropped while it is in the rest list of a
        // queue with a successor would trip the assertion through the hook member; the queue
        // always unlinks a link before its request dies.
        engine_assert!(unsafe { self.u._ref }.is_null());
    }
}

/// The tracker of a cancellable sub-queue of requests.
///
/// The queue holds requests that sit in the same I/O queue for dispatching (there can be other
/// requests as well) and ties them together for cancellation. That I/O queue is the fair
/// queue's priority class queue. Beware: if requests from different I/O queues are put in the
/// same cancellable queue, dispatch order no longer matches queue order and the invariants of
/// [`Link::maybe_dequeue`] break.
///
/// The queue is referenced by its first link; whoever moves a queue that has links calls
/// `relocated` on it at its new place.
///
/// The links and the queue point at each other: every pointer stored here is the pointer the
/// caller passed (to the whole request's link, or to the queue inside its intent), never one
/// taken from a reference, so later accesses to those objects through their owners keep it
/// valid.
pub struct CancellableQueue {
    _first: *mut Link,
    // The rest list: an intrusive singly-linked list with a cached tail.
    _rest_head: *mut Link,
    _rest_tail: *mut Link,
}

impl Default for CancellableQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellableQueue {
    /// An empty queue.
    pub const fn new() -> Self {
        CancellableQueue {
            _first: std::ptr::null_mut(),
            _rest_head: std::ptr::null_mut(),
            _rest_tail: std::ptr::null_mut(),
        }
    }

    /// Appends link `il`.
    ///
    /// # Safety
    /// `this` points to a live queue that keeps its address while it has links (or is
    /// `relocated()` at its new one); `il` is a live, unqueued link with the provenance of its
    /// whole request (see [`Link::enqueue`]).
    pub(crate) unsafe fn push_back(this: *mut Self, il: *mut Link) {
        // SAFETY: the contract above; every access is a raw place, so no reference to the
        // queue or to the link outlives this statement block.
        unsafe {
            if (*this)._first.is_null() {
                (*this)._first = il;
                (*il).u._ref = this;
            } else {
                (*il).u._next = std::ptr::null_mut();
                if (*this)._rest_tail.is_null() {
                    (*this)._rest_head = il;
                } else {
                    // the tail is a live linked request
                    (*(*this)._rest_tail).u._next = il;
                }
                (*this)._rest_tail = il;
            }
        }
    }

    /// Removes the first link; the next one (if any) becomes first and points at the queue.
    ///
    /// # Safety
    /// `this` points to a live queue with a first link; its links are live.
    pub(crate) unsafe fn pop_front(this: *mut Self) {
        // SAFETY: the contract above; the links were stored by `push_back` with their own
        // provenance.
        unsafe {
            (*(*this)._first).u._ref = std::ptr::null_mut();
            if (*this)._rest_head.is_null() {
                (*this)._first = std::ptr::null_mut();
            } else {
                (*this)._first = (*this)._rest_head;
                (*this)._rest_head = (*(*this)._first).u._next;
                if (*this)._rest_head.is_null() {
                    (*this)._rest_tail = std::ptr::null_mut();
                }
                (*(*this)._first).u._ref = this;
            }
        }
    }

    /// Takes `o`'s links and re-points the first one at `this` (a move of a queue with links).
    ///
    /// # Safety
    /// `this` and `o` point to two distinct live queues; `this` is empty and keeps its
    /// address while it has links.
    pub unsafe fn take_from(this: *mut Self, o: *mut CancellableQueue) {
        // SAFETY: the contract above.
        unsafe {
            (*this)._first = std::mem::replace(&mut (*o)._first, std::ptr::null_mut());
            (*this)._rest_head = std::mem::replace(&mut (*o)._rest_head, std::ptr::null_mut());
            (*this)._rest_tail = std::mem::replace(&mut (*o)._rest_tail, std::ptr::null_mut());
            if !(*this)._first.is_null() {
                // the first link is alive
                (*(*this)._first).u._ref = this;
            }
        }
    }

    /// Re-points the first link at the queue after it was moved bitwise (for example by a
    /// growing `Vec`).
    ///
    /// # Safety
    /// `this` points to the live queue at its new address, with the provenance its owner
    /// gives to later accesses (a pointer into the vector's buffer, not a reference).
    #[inline]
    pub(crate) unsafe fn relocated(this: *mut Self) {
        // SAFETY: the contract above; the first link is alive.
        unsafe {
            if !(*this)._first.is_null() {
                (*(*this)._first).u._ref = this;
            }
        }
    }

    /// Cancels every queued request; called when the intent is cancelled or dropped.
    ///
    /// # Safety
    /// `this` points to a live queue whose links are those of live queued requests.
    pub(crate) unsafe fn cancel_all(this: *mut Self) {
        // SAFETY: the contract above; every link lives inside a queued request (see
        // io_queue.rs), and `from_cq_link` keeps the provenance of the whole request that the
        // link pointer carries.
        unsafe {
            while !(*this)._first.is_null() {
                crate::engine::core::io_queue::QueuedIoRequest::cancel(
                    crate::engine::core::io_queue::QueuedIoRequest::from_cq_link((*this)._first),
                );
                Self::pop_front(this);
            }
        }
    }
}

impl Drop for CancellableQueue {
    /// Cancels every queued request.
    fn drop(&mut self) {
        if !self._first.is_null() {
            // SAFETY: the queue is live and its links are those of live queued requests; the
            // pointer made here is the only one used on the queue until it is gone (the links'
            // `_ref` are rewritten or cleared through it by `pop_front`).
            unsafe { Self::cancel_all(self) };
        }
    }
}

/// The hook of an [`IntentReference`] in its intent's doubly-linked list (and the sentinel of
/// that list). Null pointers mean unlinked; dropping a reference unlinks it automatically.
#[repr(C)]
pub struct IntentRefHook {
    /// Previous hook in the list.
    pub(crate) prev: *mut IntentRefHook,
    /// Next hook in the list.
    pub(crate) next: *mut IntentRefHook,
}

impl IntentRefHook {
    /// An unlinked hook.
    #[inline]
    pub const fn new() -> Self {
        IntentRefHook { prev: std::ptr::null_mut(), next: std::ptr::null_mut() }
    }

    /// Removes the hook from its list (a no-op when unlinked).
    #[inline]
    pub(crate) fn unlink(&mut self) {
        if !self.prev.is_null() {
            // SAFETY: linked neighbours are live hooks.
            unsafe { (*self.prev).next = self.next };
        }
        if !self.next.is_null() {
            // SAFETY: as above.
            unsafe { (*self.next).prev = self.prev };
        }
        self.prev = std::ptr::null_mut();
        self.next = std::ptr::null_mut();
    }
}

impl Default for IntentRefHook {
    fn default() -> Self {
        Self::new()
    }
}

/// A "safe" reference to an intent: the intent can be cancelled or destroyed at any time and
/// this reference is updated not to point at it any longer. [`retrieve`](Self::retrieve)
/// returns the intent, or an error if the intent was cancelled.
///
/// The hook is embedded. The reference is built unlinked with [`new`](Self::new), placed where
/// it will live, and linked there with [`bind`](Self::bind) on a pin, so it never moves while
/// linked and allocates nothing.
#[repr(C)]
pub struct IntentReference {
    /// The hook in the intent's list of references.
    pub(crate) hook: IntentRefHook,
    _intent: *mut IoIntent,
    _pin: PhantomPinned,
}

impl IntentReference {
    /// The tagged value stored once the intent was cancelled.
    const CANCELLED_INTENT: usize = 1;

    /// A reference to `intent` (null for none). The reference is not linked until
    /// [`bind`](Self::bind); nothing may cancel or destroy the intent between the two calls.
    #[inline]
    pub const fn new(intent: *mut IoIntent) -> Self {
        IntentReference { hook: IntentRefHook::new(), _intent: intent, _pin: PhantomPinned }
    }

    /// Links the reference into its intent's list (when it has an intent).
    #[inline]
    pub fn bind(self: Pin<&mut Self>) {
        // SAFETY: the reference is pinned, so it keeps its address while linked; the pointer
        // is to the whole reference (a `!Unpin` type), and the hook stored in the intent's
        // list is derived from it.
        let me: *mut Self = unsafe { self.get_unchecked_mut() };
        // SAFETY: `me` is live; plain reads of its fields.
        let (intent, linked, cancelled) =
            unsafe { ((*me)._intent, !(*me).hook.next.is_null(), (*me).is_cancelled()) };
        if !intent.is_null() && !linked && !cancelled {
            // SAFETY: a non-null, non-cancelled intent is alive and pinned (see `new`); its
            // list is reached through the intent pointer, without a reference to the intent.
            unsafe {
                crate::engine::core::io_intent::References::bind(
                    std::ptr::addr_of_mut!((*intent)._refs),
                    me,
                )
            };
        }
    }

    /// Moves `o` into `self`: `self` takes over `o`'s intent and its place in the intent's
    /// list, after leaving its own list.
    ///
    /// Both references are pinned, so `self` takes `o`'s place in its intent's list and `o` is
    /// left unlinked and null. Two pins cannot be of the same object, so `this != &o` holds.
    pub fn take_from(self: Pin<&mut Self>, o: Pin<&mut IntentReference>) {
        // SAFETY: both references are pinned and stay where they are; the pointers are to the
        // whole references, and every hook stored in a list is derived from them.
        let me: *mut Self = unsafe { self.get_unchecked_mut() };
        // SAFETY: as above.
        let o: *mut Self = unsafe { o.get_unchecked_mut() };
        // SAFETY: `me` and `o` are live and distinct; their linked neighbours are live hooks
        // (the sentinel of the intent's list or other pinned references), reached through the
        // pointers the list stored.
        unsafe {
            (*me)._intent = std::mem::replace(&mut (*o)._intent, std::ptr::null_mut());
            (*me).hook.unlink();
            // swap_nodes with an unlinked `this`: `this` takes `o`'s links.
            let oh = std::ptr::addr_of_mut!((*o).hook);
            if !(*oh).next.is_null() {
                let h = std::ptr::addr_of_mut!((*me).hook);
                (*h).prev = (*oh).prev;
                (*h).next = (*oh).next;
                (*(*h).prev).next = h;
                (*(*h).next).prev = h;
                (*oh).prev = std::ptr::null_mut();
                (*oh).next = std::ptr::null_mut();
            }
        }
    }

    /// Back from the hook to the reference.
    ///
    /// # Safety
    /// `h` must be the hook of an `IntentReference`.
    #[inline]
    pub(crate) unsafe fn from_hook(h: *mut IntentRefHook) -> *mut IntentReference {
        // The hook is the first field of the `repr(C)` reference.
        h as *mut IntentReference
    }

    /// Marks the reference as cancelled; called by the intent.
    #[inline]
    pub(crate) fn on_cancel(&mut self) {
        self._intent = Self::CANCELLED_INTENT as *mut IoIntent;
    }

    /// Whether the intent was cancelled.
    #[inline]
    fn is_cancelled(&self) -> bool {
        self._intent as usize == Self::CANCELLED_INTENT
    }

    /// The intent (null for none).
    ///
    /// # Errors
    ///
    /// Returns the I/O cancellation error if the intent was cancelled.
    #[inline]
    pub fn retrieve(&self) -> Result<*mut IoIntent, ExceptionPtr> {
        if self.is_cancelled() {
            return Err(crate::engine::core::io_queue::DefaultIoExceptionFactory::cancelled());
        }
        Ok(self._intent)
    }
}

impl Drop for IntentReference {
    /// Leaves the intent's list.
    #[inline]
    fn drop(&mut self) {
        self.hook.unlink();
    }
}
