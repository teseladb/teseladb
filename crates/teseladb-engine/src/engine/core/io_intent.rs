//! Cancellation handles for I/O requests.
//!
//! An [`IoIntent`] is passed (as a pointer) with requests queued on the per-shard I/O
//! queue. Each request is linked into the intent's cancellable queue for its (I/O queue,
//! priority class) pair, and [`IoIntent::cancel`], or dropping the intent, fails every
//! request still attached with a cancelled error. Requests issued without an intent run
//! until completion, success or error.
//!
//! Requests and [`IntentReference`]s keep raw links back into the intent, and a Rust move
//! runs no code to re-point them, so an intent is pinned (`std::pin::pin!(IoIntent::new())`)
//! before any request or reference may point at it; it then never moves. The first
//! per-queue entry is stored inline, so the common single-queue case allocates nothing.

use std::marker::PhantomPinned;
use std::pin::Pin;
use std::ptr;

use crate::engine::core::internal::io_intent::{CancellableQueue, IntentRefHook, IntentReference};
use crate::engine::core::io_priority_class::IoPriorityClassId;

/// The cancellable requests of one intent on one (I/O queue, priority class) pair.
pub struct IntentsForQueue {
    /// The id of the IO queue.
    pub qid: u32,
    /// The id of the priority class inside that queue.
    pub cid: IoPriorityClassId,
    /// The cancellable sub-queue of the requests of this intent in that class.
    pub cq: CancellableQueue,
}

impl IntentsForQueue {
    /// An empty entry for queue `qid` and class `cid`.
    #[inline]
    pub const fn new(qid: u32, cid: IoPriorityClassId) -> Self {
        IntentsForQueue { qid, cid, cq: CancellableQueue::new() }
    }

    // Called after the entry was moved by the vector growing: the cancellable queue's first
    // link points back at the queue, so it is re-pointed to the new address.
    //
    // # Safety
    // `this` is the element at its new address, reached through the vector's buffer pointer.
    #[inline]
    unsafe fn relocated(this: *mut Self) {
        // SAFETY: forwarded from the caller.
        unsafe { CancellableQueue::relocated(ptr::addr_of_mut!((*this).cq)) };
    }
}

/// The [`IntentReference`]s bound to an intent: a circular doubly linked auto-unlink list
/// through the `list` sentinel. A null sentinel is the empty list; it is closed on the
/// first `bind`, once the intent is pinned.
///
/// The linked references point at the sentinel, so the list is only changed through a
/// pointer to the whole intent (`bind`, `clear`), never through a reference to the
/// sentinel, and it is `!Unpin` like its intent.
///
/// Invariant: every hook linked in the list is the first field of a live, pinned
/// [`IntentReference`], which unlinks itself when dropped, so walking the list from the
/// sentinel only meets live hooks.
pub struct References {
    list: IntentRefHook,
    _pin: PhantomPinned,
}

impl References {
    /// An empty list (null sentinel).
    #[inline]
    pub const fn new() -> Self {
        References { list: IntentRefHook::new(), _pin: PhantomPinned }
    }

    /// Unlinks every bound reference and marks each one cancelled.
    ///
    /// The emptied list is left with a null sentinel, so the next `bind` closes it again with
    /// the pointer it is given.
    ///
    /// # Safety
    /// `this` points to the live list of a pinned intent.
    pub unsafe fn clear(this: *mut Self) {
        // SAFETY: the contract above and the type's invariant; every access is a raw place.
        unsafe {
            let s = ptr::addr_of_mut!((*this).list);
            if (*s).next.is_null() {
                return;
            }
            let mut p = (*s).next;
            while p != s {
                let r = IntentReference::from_hook(p);
                p = (*p).next;
                // `r` is unlinked before being marked.
                (*r).hook.prev = ptr::null_mut();
                (*r).hook.next = ptr::null_mut();
                (*r).on_cancel();
            }
            (*s).next = ptr::null_mut();
            (*s).prev = ptr::null_mut();
        }
    }

    /// Links `iref` at the back of the list, closing the sentinel first if the list was
    /// never used.
    ///
    /// # Safety
    /// `this` points to the live list of a pinned intent (derived from the intent pointer);
    /// `iref` points to a live, pinned, unlinked reference, derived from the pointer to the
    /// whole reference.
    #[inline]
    pub unsafe fn bind(this: *mut Self, iref: *mut IntentReference) {
        // SAFETY: the contract above; every access is a raw place.
        unsafe {
            let s = ptr::addr_of_mut!((*this).list);
            if (*s).next.is_null() {
                (*s).next = s;
                (*s).prev = s;
            }
            let h = ptr::addr_of_mut!((*iref).hook);
            (*h).prev = (*s).prev;
            (*h).next = s;
            // `list.prev` is the sentinel or a live linked hook
            (*(*s).prev).next = h;
            (*s).prev = h;
        }
    }
}

impl Default for References {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for References {
    /// Marks every still bound reference cancelled.
    fn drop(&mut self) {
        // SAFETY: the list is live; the pointer is only used for this call.
        unsafe { Self::clear(self) };
    }
}

/// A small vector of [`IntentsForQueue`]: the first element lives inline, the others go to
/// the heap. Growing the heap part moves its elements, so their queues' first links are
/// re-pointed to the new addresses afterwards.
///
/// The queues are pointed at by their links, so they are reached through raw pointers (the
/// inline element by `addr_of_mut!`, the others through the vector's buffer pointer), never
/// through references that would outlive the call.
struct SmallIntents {
    _first: IntentsForQueue,
    _has_first: bool,
    _rest: Vec<IntentsForQueue>,
}

impl SmallIntents {
    #[inline]
    const fn new() -> Self {
        SmallIntents { _first: IntentsForQueue::new(0, 0), _has_first: false, _rest: Vec::new() }
    }

    /// Cancels the requests of every element, in order, and removes the elements.
    ///
    /// # Safety
    /// `this` points to the live intents of a pinned intent.
    #[inline]
    unsafe fn clear(this: *mut Self) {
        // SAFETY: the contract above; the elements are reached through raw pointers.
        unsafe {
            if (*this)._has_first {
                CancellableQueue::cancel_all(ptr::addr_of_mut!((*this)._first.cq));
                (*this)._has_first = false;
            }
            let rest = ptr::addr_of_mut!((*this)._rest);
            let base = (*rest).as_mut_ptr();
            for i in 0..(*rest).len() {
                CancellableQueue::cancel_all(ptr::addr_of_mut!((*base.add(i)).cq));
            }
            // the queues are empty now, so dropping them does nothing more
            (*rest).clear();
        }
    }

    /// Appends an empty entry for (`qid`, `cid`) and returns its queue.
    ///
    /// # Safety
    /// As in [`SmallIntents::clear`].
    unsafe fn emplace_back(
        this: *mut Self,
        qid: u32,
        cid: IoPriorityClassId,
    ) -> *mut CancellableQueue {
        // SAFETY: the contract above; the elements are reached through raw pointers.
        unsafe {
            if !(*this)._has_first {
                (*this)._first = IntentsForQueue::new(qid, cid);
                (*this)._has_first = true;
                return ptr::addr_of_mut!((*this)._first.cq);
            }
            let rest = ptr::addr_of_mut!((*this)._rest);
            let old = (*rest).as_ptr();
            (*rest).push(IntentsForQueue::new(qid, cid));
            let base = (*rest).as_mut_ptr();
            let last = (*rest).len() - 1;
            if base.cast_const() != old {
                for i in 0..last {
                    IntentsForQueue::relocated(base.add(i));
                }
            }
            ptr::addr_of_mut!((*base.add(last)).cq)
        }
    }
}

/// A handle confirming the caller's intent to do the I/O.
///
/// When a pointer to an intent is passed with a request queued on the I/O queue
/// ([`crate::engine::core::io_queue`]), the request is attached to the intent and is
/// only processed as long as the intent is alive and [`IoIntent::cancel`] is not called.
///
/// If no intent is provided, the request is processed until its completion, be it
/// success or error. The intent must be pinned before use; see the module docs.
pub struct IoIntent {
    _intents: SmallIntents,
    pub(crate) _refs: References,
    _pin: PhantomPinned,
}

impl Default for IoIntent {
    fn default() -> Self {
        Self::new()
    }
}

impl IoIntent {
    /// An intent with no attached requests or references; pin it before use.
    #[inline]
    pub const fn new() -> Self {
        IoIntent { _intents: SmallIntents::new(), _refs: References::new(), _pin: PhantomPinned }
    }

    /// The intent pointer the file and I/O queue APIs take. The intent must stay pinned and
    /// alive while requests or references use the pointer; dropping it cancels them.
    #[inline]
    pub fn as_ptr(self: Pin<&mut Self>) -> *mut IoIntent {
        // SAFETY: only the address is taken; the pin keeps it valid.
        unsafe { self.get_unchecked_mut() as *mut IoIntent }
    }

    /// Explicitly cancels all the requests attached to this intent so far. Their futures
    /// resolve with a cancelled error, and the bound references are marked cancelled.
    #[inline]
    pub fn cancel(self: Pin<&mut Self>) {
        // SAFETY: cancelling does not move the intent; the pointer is to the whole intent.
        unsafe { Self::do_cancel(self.get_unchecked_mut()) };
    }

    /// [`IoIntent::cancel`] through a raw pointer to the pinned intent.
    ///
    /// # Safety
    /// `this` points to a live, pinned intent.
    #[inline]
    pub(crate) unsafe fn do_cancel(this: *mut Self) {
        // SAFETY: forwarded from the caller; the parts are reached through `this`.
        unsafe {
            References::clear(ptr::addr_of_mut!((*this)._refs));
            SmallIntents::clear(ptr::addr_of_mut!((*this)._intents));
        }
    }

    /// Returns the cancellable queue for (`qid`, `cid`), creating an empty one on first use.
    #[inline]
    pub fn find_or_create_cancellable_queue(
        self: Pin<&mut Self>,
        qid: u32,
        cid: IoPriorityClassId,
    ) -> &mut CancellableQueue {
        // SAFETY: the queues stay inside the pinned intent; the reference is tied to the pin.
        unsafe {
            &mut *Self::find_or_create_cancellable_queue_raw(self.get_unchecked_mut(), qid, cid)
        }
    }

    /// [`IoIntent::find_or_create_cancellable_queue`] on an intent reached through its raw
    /// pointer: the queue pointer keeps the provenance of the intent pointer, so the links
    /// may store it.
    ///
    /// # Safety
    /// `this` points to a live, pinned intent.
    pub(crate) unsafe fn find_or_create_cancellable_queue_raw(
        this: *mut Self,
        qid: u32,
        cid: IoPriorityClassId,
    ) -> *mut CancellableQueue {
        // SAFETY: forwarded from the caller; the elements are reached through raw pointers
        // and only read here.
        unsafe {
            let intents = ptr::addr_of_mut!((*this)._intents);
            if (*intents)._has_first && (*intents)._first.qid == qid && (*intents)._first.cid == cid
            {
                return ptr::addr_of_mut!((*intents)._first.cq);
            }
            let rest = ptr::addr_of_mut!((*intents)._rest);
            let base = (*rest).as_mut_ptr();
            for i in 0..(*rest).len() {
                let e = base.add(i);
                if (*e).qid == qid && (*e).cid == cid {
                    return ptr::addr_of_mut!((*e).cq);
                }
            }
            SmallIntents::emplace_back(intents, qid, cid)
        }
    }
}

impl Drop for IoIntent {
    /// Cancels like [`IoIntent::cancel`]: first the references are marked cancelled, then
    /// the requests are cancelled.
    fn drop(&mut self) {
        // SAFETY: the intent is live and pinned until here; the pointer is the whole intent.
        unsafe { Self::do_cancel(self) };
    }
}
