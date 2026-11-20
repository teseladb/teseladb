//! Engine coroutines: Rust `async` blocks run as tasks of the current shard.
//!
//! A coroutine is an `async` block whose output is `Result<T, ExceptionPtr>`, started with
//! [`make_coroutine`], which returns an engine [`Future<T>`]. The `async` state machine (the
//! frame) and the promise of the result are one heap object ([`internal::CoroutinePromise`])
//! that is an engine [`Task`]: running the task resumes the coroutine (polls the frame).
//! Inside the block, `fut.await` on an engine future goes through [`internal::Awaiter`]
//! (`await_ready`, `await_suspend`, `await_resume`): a suspended awaiter, pinned in the
//! frame, receives the result in its own state, so no extra allocation is made. A body that
//! completes without suspending returns a ready future and frees the frame at once.
//!
//! Rust futures that are not engine futures (a hand-written `select`, a channel) get a
//! `std::task::Waker`. The waker is reference counted and points
//! to the coroutine; waking it queues the coroutine's own task (no allocation), at most
//! once until it runs, and the coroutine memory is only released when no waker, no queued
//! run and no future refers to it any more. A waker must be woken on
//! the shard that runs the coroutine; waking it from another thread aborts.

use std::future::{Future as StdFuture, IntoFuture};
use std::marker::PhantomPinned;
use std::mem::ManuallyDrop;
use std::pin::Pin;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::engine::core::future::internal::{PromiseBaseWithType, PromiseNode};
use crate::engine::core::future::{
    ExceptionFutureMarker, ExceptionPtr, Future, FutureState, ReadyFutureMarker,
};
use crate::engine::core::preempt::need_preempt;
use crate::engine::core::scheduling::{SchedulingGroup, current_scheduling_group};
use crate::engine::core::task::{Task, TaskBase, TaskPtr, schedule};

/// The coroutine object, its awaiter and its waker.
pub mod internal {
    use super::*;

    thread_local! {
        /// Its address identifies the thread a coroutine runs on, for the wakers.
        static THREAD_MARKER: u8 = const { 0 };
    }

    /// The address of this thread's marker.
    #[inline]
    fn thread_marker() -> *const u8 {
        THREAD_MARKER.with(|m| m as *const u8)
    }

    /// The part of a coroutine that does not depend on its types: the task header and the
    /// counts that decide when its memory goes. It is the first field of
    /// [`CoroutinePromise`], so a pointer to it is a pointer to the coroutine.
    #[repr(C)]
    pub struct CoroutineCore {
        /// The coroutine task, first field. The
        /// promises, the run queue and the wakers all hold this one header.
        pub(super) task: TaskBase,
        /// The thread running the coroutine.
        owner: *const u8,
        /// Live clones of the waker.
        wakers: AtomicUsize,
        /// Runs of `task` handed to a promise or to the run queue and not run yet, except
        /// the one a waker queued (that one is [`CoroutineCore::WAKE_QUEUED`]).
        main_pending: u32,
        /// [`CoroutineCore::DONE`], [`CoroutineCore::NODE_LIVE`] and
        /// [`CoroutineCore::WAKE_QUEUED`], in one byte so that the test that decides
        /// whether the memory can go is a single comparison.
        flags: u8,
    }

    impl CoroutineCore {
        /// The frame has completed.
        const DONE: u8 = 1;
        /// The embedded promise node is still referenced.
        const NODE_LIVE: u8 = 2;
        /// A waker queued `task` and that run has not happened yet.
        const WAKE_QUEUED: u8 = 4;

        /// Whether the frame has completed.
        ///
        /// # Safety
        /// `c` must point to a live coroutine.
        #[inline(always)]
        pub(super) unsafe fn done(c: *const CoroutineCore) -> bool {
            // SAFETY: guaranteed by the caller.
            unsafe { (*c).flags & Self::DONE != 0 }
        }

        /// Sets `flag`.
        ///
        /// # Safety
        /// `c` must point to a live coroutine.
        #[inline(always)]
        unsafe fn set(c: *mut CoroutineCore, flag: u8) {
            // SAFETY: guaranteed by the caller.
            unsafe { (*c).flags |= flag };
        }

        /// Clears `flag`.
        ///
        /// # Safety
        /// `c` must point to a live coroutine.
        #[inline(always)]
        unsafe fn clear(c: *mut CoroutineCore, flag: u8) {
            // SAFETY: guaranteed by the caller.
            unsafe { (*c).flags &= !flag };
        }

        /// Marks the frame as completed.
        ///
        /// # Safety
        /// `c` must point to a live coroutine.
        #[inline(always)]
        pub(super) unsafe fn set_done(c: *mut CoroutineCore) {
            // SAFETY: forwarded from the caller.
            unsafe { Self::set(c, Self::DONE) };
        }

        /// Records that the embedded promise node is no longer referenced.
        ///
        /// # Safety
        /// `c` must point to a live coroutine.
        #[inline(always)]
        pub(super) unsafe fn retire_node(c: *mut CoroutineCore) {
            // SAFETY: forwarded from the caller.
            unsafe { Self::clear(c, Self::NODE_LIVE) };
        }

        /// A core for the coroutine type `C`, owned by this thread.
        #[inline]
        pub(super) fn new<C: Task>(sg: SchedulingGroup) -> Self {
            Self {
                task: TaskBase::new::<C>(sg),
                owner: thread_marker(),
                wakers: AtomicUsize::new(0),
                main_pending: 0,
                flags: Self::NODE_LIVE,
            }
        }

        /// Accounts for a run of `task` that has started: the one a waker queued, if there
        /// is one, or else one of the hand-outs. Every run does the same (resume the frame
        /// unless it is done, then free it if nothing refers to it), so which queued entry is
        /// charged to which does not matter, only that each one is charged once.
        ///
        /// # Safety
        /// `c` must point to a live coroutine whose `task` is running.
        #[inline(always)]
        pub(super) unsafe fn start_run(c: *mut CoroutineCore) {
            // SAFETY: guaranteed by the caller.
            unsafe {
                if (*c).flags & Self::WAKE_QUEUED != 0 {
                    Self::clear(c, Self::WAKE_QUEUED);
                } else {
                    (*c).main_pending -= 1;
                }
            }
        }

        /// The coroutine task, counted as handed out: the caller gives it to a promise
        /// (`set_coroutine`) or to the run queue.
        ///
        /// # Safety
        /// `core` must point to a live coroutine, derived from the pointer to the whole
        /// object.
        #[inline]
        pub(crate) unsafe fn hand_out_task(core: NonNull<CoroutineCore>) -> TaskPtr {
            let c = core.as_ptr();
            // SAFETY: guaranteed by the caller; `task` is the first field of the coroutine.
            unsafe {
                (*c).main_pending += 1;
                TaskPtr::from_raw(ptr::addr_of_mut!((*c).task))
            }
        }

        /// Gives back a hand-out of [`CoroutineCore::hand_out_task`] that will never run
        /// (an awaiter dropped while waiting took the task back from its promise), and
        /// frees the coroutine if that was the last thing keeping it.
        ///
        /// # Safety
        /// `core` must point to a live coroutine, derived from the pointer to the whole
        /// object, with one hand-out taken back and no reference to it live.
        #[inline]
        pub(crate) unsafe fn give_back_task(core: NonNull<CoroutineCore>) {
            let c = core.as_ptr();
            // SAFETY: guaranteed by the caller; the count includes the hand-out.
            unsafe {
                (*c).main_pending -= 1;
                Self::maybe_free(c);
            }
        }

        /// Whether nothing refers to the coroutine any more, so its memory can go.
        ///
        /// # Safety
        /// `c` must point to a live coroutine, with no reference to it live.
        #[inline(always)]
        pub(super) unsafe fn unreferenced(c: *const CoroutineCore) -> bool {
            // SAFETY: guaranteed by the caller.
            unsafe {
                // Done, its node retired and no wake queued: this decides almost every
                // call, so it comes first, as one comparison of the flags byte.
                if (*c).flags != Self::DONE || (*c).main_pending != 0 {
                    return false;
                }
                // Last, the clones of the waker, with acquire ordering against the release
                // decrement of the last clone dropped. Arc::drop loads relaxed and fences
                // only when the count drops to zero, because there the zero is rare; here it
                // is the common case, and on aarch64 a relaxed load plus `dmb ishld` costs
                // about 1 ns more than the `ldar` (measured).
                (*c).wakers.load(Ordering::Acquire) == 0
            }
        }

        /// Frees the coroutine if nothing refers to it any more, for the callers that do
        /// not know its type: through its own `run_and_dispose`, as one more counted run,
        /// which finds the frame done and frees it with its type (the `delete` of the
        /// frame needs no hook of its own).
        ///
        /// # Safety
        /// `c` must point to a live coroutine, with no reference to it live.
        #[inline(always)]
        pub(super) unsafe fn maybe_free(c: *mut CoroutineCore) {
            // SAFETY: guaranteed by the caller; a done, unreferenced coroutine only frees
            // itself when it runs.
            unsafe {
                if Self::unreferenced(c) {
                    (*c).main_pending += 1;
                    TaskPtr::from_raw(ptr::addr_of_mut!((*c).task)).run_and_dispose();
                }
            }
        }

        /// The `wake_by_ref` of the waker: queues the coroutine task unless a wake is queued
        /// already or the coroutine has completed.
        ///
        /// # Safety
        /// `c` must point to a live coroutine.
        unsafe fn wake(c: *mut CoroutineCore) {
            // SAFETY: guaranteed by the caller.
            unsafe {
                if (*c).owner != thread_marker() {
                    // A engine coroutine runs on its shard only; there is no cross-shard
                    // wake in engine.
                    std::process::abort();
                }
                if (*c).flags & (Self::DONE | Self::WAKE_QUEUED) != 0 {
                    return;
                }
                Self::set(c, Self::WAKE_QUEUED);
                schedule(TaskPtr::from_raw(ptr::addr_of_mut!((*c).task)));
            }
        }
    }

    /// A coroutine: its core, the promise of its result and its frame (`Fut`). A coroutine
    /// without a value has `T = ()`.
    ///
    /// The promise is an embedded promise node, so a coroutine costs one allocation; the
    /// allocation is released
    /// once the frame has completed and the returned future has let go of the node.
    #[repr(C)]
    pub struct CoroutinePromise<T: 'static, Fut>
    where
        Fut: StdFuture<Output = Result<T, ExceptionPtr>> + 'static,
    {
        /// First field: the task header and the counts.
        pub(super) core: CoroutineCore,
        pub(super) promise: PromiseNode<T>,
        /// The frame, live while the core is not [`CoroutineCore::DONE`]: it is dropped in
        /// place when the body completes or the coroutine is destroyed, which sets the flag.
        pub(super) frame: ManuallyDrop<Fut>,
    }

    impl<T: 'static, Fut> CoroutinePromise<T, Fut>
    where
        Fut: StdFuture<Output = Result<T, ExceptionPtr>> + 'static,
    {
        /// The block of a coroutine, as the thread's pool of blocks takes it.
        const LAYOUT: std::alloc::Layout = std::alloc::Layout::new::<Self>();

        /// The promise node, with the provenance of the whole coroutine.
        ///
        /// # Safety
        /// `this` must point to a live coroutine.
        #[inline(always)]
        unsafe fn node_ptr(this: *mut Self) -> *mut PromiseNode<T> {
            // SAFETY: guaranteed by the caller.
            unsafe { ptr::addr_of_mut!((*this).promise) }
        }

        /// The promise, taking over the writer reference the coroutine holds; called
        /// once, when the coroutine completes.
        ///
        /// # Safety
        /// `this` must point to a live coroutine whose writer reference was not taken.
        #[inline(always)]
        pub(super) unsafe fn take_promise(this: *mut Self) -> PromiseBaseWithType<T> {
            // SAFETY: guaranteed by the caller.
            PromiseBaseWithType::from_node(unsafe { Self::node_ptr(this) })
        }

        /// Resolves the coroutine's result with `value`.
        ///
        /// # Safety
        /// As `CoroutinePromise::take_promise`.
        #[inline(always)]
        pub unsafe fn return_value(this: *mut Self, value: T) {
            // SAFETY: forwarded from the caller.
            unsafe { Self::take_promise(this) }.set_value(value);
        }

        /// Fails the coroutine's result with the error carried by `ce`.
        ///
        /// # Safety
        /// As `CoroutinePromise::take_promise`.
        #[inline]
        pub unsafe fn return_exception(
            this: *mut Self,
            ce: crate::engine::coroutine::exception::coroutine::Exception,
        ) {
            // SAFETY: forwarded from the caller.
            unsafe { Self::take_promise(this) }.set_exception(ce.eptr);
        }

        /// Fails the coroutine's result with `eptr`.
        ///
        /// # Safety
        /// As `CoroutinePromise::take_promise`.
        #[inline]
        pub unsafe fn set_exception(this: *mut Self, eptr: ExceptionPtr) {
            // SAFETY: forwarded from the caller.
            unsafe { Self::take_promise(this) }.set_exception(eptr);
        }

        /// Forwards the outcome of `fut` to the coroutine's result. Deprecated: return the
        /// awaited value instead.
        ///
        /// # Safety
        /// As `CoroutinePromise::take_promise`.
        #[deprecated(
            note = "Forwarding coroutine returns are deprecated as too dangerous; await the future and return its value instead."
        )]
        pub unsafe fn return_future(this: *mut Self, fut: Future<T>) {
            // SAFETY: forwarded from the caller.
            fut.forward_to_base(unsafe { Self::take_promise(this) });
        }

        /// Fails the coroutine's result with the error that escaped the body, `current`.
        ///
        /// # Safety
        /// As `CoroutinePromise::take_promise`.
        #[inline(always)]
        pub unsafe fn unhandled_exception(this: *mut Self, current: ExceptionPtr) {
            // SAFETY: forwarded from the caller.
            unsafe { Self::take_promise(this) }.set_exception(current);
        }

        /// Points the promise at the
        /// node's state and returns the node, whose future reference the caller owns
        /// ([`make_coroutine`] makes the future once the body has run up to its first
        /// suspension).
        ///
        /// # Safety
        /// Called once, right after the object got its final address.
        #[inline]
        pub(super) unsafe fn get_return_object(this: *mut Self) -> *mut PromiseNode<T> {
            // SAFETY: guaranteed by the caller; the node starts with the future reference.
            unsafe {
                let n = Self::node_ptr(this);
                (*n).state = ptr::addr_of_mut!((*n).local_state);
                n
            }
        }

        /// Sets the scheduling group the coroutine's task runs under; returns the previous
        /// one.
        #[inline]
        pub fn set_scheduling_group(&mut self, sg: SchedulingGroup) -> SchedulingGroup {
            self.core.task.set_scheduling_group(sg)
        }

        /// Release hook of the embedded promise node.
        ///
        /// # Safety
        /// `n` must be the `promise` field of a live `CoroutinePromise` of this type,
        /// obtained with `node_ptr`.
        pub(super) unsafe fn release_node(n: *mut PromiseNode<T>) {
            // SAFETY: the node is embedded at a fixed offset of its coroutine, and `n` has
            // the provenance of the whole coroutine.
            unsafe {
                let c = n.byte_sub(std::mem::offset_of!(Self, promise)).cast::<CoroutineCore>();
                CoroutineCore::retire_node(c);
                CoroutineCore::maybe_free(c);
            }
        }

        /// Allocates the coroutine of `frame` and builds it in place, field by field, on
        /// the heap, so the frame
        /// is written once and not built on the stack and copied. The node starts with the
        /// writer reference and, if `with_future`, the future reference.
        ///
        /// The frame is taken out of drop's reach before anything that could unwind (the
        /// allocation failure handler): with a drop of the frame on that path, the
        /// frame's address was taken, so the compiler kept it in a stack temporary and
        /// copied all of it, unwritten bytes included, into the allocation with 16-byte
        /// loads over the narrower stores that built it, which cannot take their data from
        /// the store buffer. Without it, the captures are stored straight into the
        /// allocation. The allocation
        /// failure handler aborts, so nothing is lost.
        #[inline(always)]
        pub(super) fn alloc(with_future: bool, frame: Fut) -> *mut Self {
            let frame = ManuallyDrop::new(frame);
            let sg = current_scheduling_group();
            // From the thread's pool of blocks (an improvement beyond the original, which
            // allocates and frees every coroutine frame): the block the last coroutine of
            // this size gave back serves the next one.
            let p = crate::engine::core::future::pooled_alloc(Self::LAYOUT).cast::<Self>();
            // SAFETY: `p` is the uninitialized allocation of a Self, uniquely owned here;
            // every field is written once before the coroutine is used.
            unsafe {
                ptr::addr_of_mut!((*p).core).write(CoroutineCore::new::<Self>(sg));
                ptr::addr_of_mut!((*p).promise)
                    .write(PromiseNode::new_detached(with_future, Self::release_node));
                ptr::addr_of_mut!((*p).frame).write(frame);
            }
            p
        }

        /// [`CoroutineCore::maybe_free`] for a caller that knows the type: the frame is
        /// deleted directly, not through a counted run (see [`CoroutineCore::maybe_free`]).
        ///
        /// # Safety
        /// `this` must be a live coroutine allocated by [`make_coroutine`] or
        /// [`SuspendedCoroutine::new`], with no reference to it live; it is not used again
        /// if it is freed.
        #[inline(always)]
        pub(super) unsafe fn free_if_unreferenced(this: *mut Self) {
            // SAFETY: guaranteed by the caller; the core is the first field. The fields are
            // dropped one by one, which is the whole drop of the object (the node owns
            // nothing but its state, and the frame is already gone once the coroutine is
            // done, which an unreferenced coroutine is), so the field drops inline here
            // instead of calling the drop glue of the box; the block came from the Box of
            // `alloc`, with this layout.
            unsafe {
                if CoroutineCore::unreferenced(this.cast::<CoroutineCore>()) {
                    ptr::drop_in_place(ptr::addr_of_mut!((*this).core));
                    ptr::drop_in_place(ptr::addr_of_mut!((*this).promise.local_state));
                    crate::engine::core::future::pooled_dealloc(this.cast::<u8>(), Self::LAYOUT);
                }
            }
        }

        /// Resumes the coroutine: polls the frame once; on completion the value goes to the
        /// promise and the frame is destroyed at once.
        ///
        /// # Safety
        /// `this` must be the live heap object created by [`make_coroutine`], not done.
        #[inline(always)]
        pub(super) unsafe fn resume(this: *mut Self) {
            // SAFETY: guaranteed by the caller.
            unsafe {
                let Poll::Ready(result) = Self::poll_frame(this) else { return };
                // When no future refers to the node any more (the awaiting coroutine linked
                // its own state), the coroutine holds the node's future reference while its
                // promise lets go of it: releasing the writer then does not go through the
                // node's release hook, and the node is retired here, with the frame.
                let n = Self::node_ptr(this);
                let held = !(*n).future_ref;
                if held {
                    (*n).future_ref = true;
                }
                match result {
                    Ok(v) => Self::return_value(this, v),
                    Err(e) => Self::unhandled_exception(this, e),
                }
                if held {
                    (*n).future_ref = false;
                    // A forward chain may still hold the writer reference; its release then
                    // goes through release_node as before.
                    if !(*n).writer_ref {
                        CoroutineCore::retire_node(this.cast());
                    }
                }
            }
        }

        /// Polls the frame once; when the body completes, the frame is destroyed, the
        /// coroutine marked done and the result returned (the caller hands it to the
        /// promise, or to the future of [`make_coroutine`]).
        ///
        /// # Safety
        /// As [`CoroutinePromise::resume`].
        #[inline(always)]
        pub(super) unsafe fn poll_frame(this: *mut Self) -> Poll<Result<T, ExceptionPtr>> {
            // SAFETY: guaranteed by the caller; the frame is never moved out of the box,
            // and every field is reached through `this`, so no reference to the whole
            // coroutine is live while the frame runs (it may touch the node).
            unsafe {
                let data = this.cast::<CoroutineCore>() as *const ();
                // The waker handed to poll() does not count: the coroutine outlives the
                // poll. Its clones count (waker_clone).
                let waker =
                    std::mem::ManuallyDrop::new(Waker::from_raw(RawWaker::new(data, &VTABLE)));
                let mut cx = Context::from_waker(&waker);
                // Not done (guaranteed by the caller): the frame is live.
                let frame = ptr::addr_of_mut!((*this).frame);
                let result = match Pin::new_unchecked(&mut **frame).poll(&mut cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(r) => r,
                };
                ManuallyDrop::drop(&mut *frame);
                CoroutineCore::set_done(this.cast());
                Poll::Ready(result)
            }
        }

        /// Ends a coroutine whose body completed in the first poll of [`make_coroutine`]:
        /// nothing has seen its promise node yet (the future is not handed out, so no
        /// awaiter, continuation or forward chain is linked and no task is registered), so
        /// the node is retired as it was built, its state still `future`, and the frame is
        /// freed if nothing else refers to it (a waker clone, a queued wake or a scheduled
        /// hand-out made by the body keep it until they go).
        ///
        /// # Safety
        /// `this` must be the coroutine of [`make_coroutine`] whose first
        /// [`CoroutinePromise::poll_frame`] returned `Ready`; it is not used again.
        #[inline(always)]
        pub(super) unsafe fn finish_first_poll(this: *mut Self) {
            // SAFETY: guaranteed by the caller; the core is the first field. Nothing wrote to
            // the node, whose local state is still `future` and owns nothing, and the frame
            // is gone, so of the fields only the core can own something (the task backtrace,
            // when it is compiled in); then the block is freed (it came from the Box of
            // `alloc`, with this layout). A coroutine kept alive here is freed later by its
            // own run (CoroutineCore::maybe_free), which drops the same fields.
            unsafe {
                let c = this.cast::<CoroutineCore>();
                CoroutineCore::retire_node(c);
                if CoroutineCore::unreferenced(c) {
                    ptr::drop_in_place(ptr::addr_of_mut!((*this).core));
                    crate::engine::core::future::pooled_dealloc(this.cast::<u8>(), Self::LAYOUT);
                }
            }
        }
    }

    // SAFETY: CoroutinePromise is repr(C); its first field is the core, whose first field
    // is the header built by TaskBase::new::<Self>.
    unsafe impl<T: 'static, Fut> Task for CoroutinePromise<T, Fut>
    where
        Fut: StdFuture<Output = Result<T, ExceptionPtr>> + 'static,
    {
        unsafe fn run_and_dispose(this: NonNull<Self>) {
            let p = this.as_ptr();
            // Resume the coroutine of this task.
            // SAFETY: the reactor runs a live coroutine that make_coroutine boxed; this run
            // is a queued wake or one of the `main_pending` hand-outs (CoroutineCore::maybe_free
            // also runs a done coroutine this way, as a counted run, to free it).
            unsafe {
                let c = p.cast::<CoroutineCore>();
                CoroutineCore::start_run(c);
                if !CoroutineCore::done(c) {
                    Self::resume(p);
                }
                Self::free_if_unreferenced(p);
            }
        }
        unsafe fn waiting_task(this: NonNull<Self>) -> Option<TaskPtr> {
            let p = this.as_ptr();
            // SAFETY: the coroutine is live and holds the writer reference on its node until
            // it completes.
            unsafe {
                if CoroutineCore::done(p.cast()) {
                    return None;
                }
                (*PromiseNode::resolve(Self::node_ptr(p))).task
            }
        }
    }

    unsafe fn waker_clone(data: *const ()) -> RawWaker {
        // SAFETY: data is the core of a live coroutine (a waker keeps it alive).
        unsafe { (*(data as *const CoroutineCore)).wakers.fetch_add(1, Ordering::Relaxed) };
        RawWaker::new(data, &VTABLE)
    }
    unsafe fn waker_wake(data: *const ()) {
        // SAFETY: as in waker_clone; wake() consumes this clone.
        unsafe {
            waker_wake_by_ref(data);
            waker_drop(data);
        }
    }
    unsafe fn waker_wake_by_ref(data: *const ()) {
        // SAFETY: as in waker_clone.
        unsafe { CoroutineCore::wake(data as *mut CoroutineCore) };
    }
    unsafe fn waker_drop(data: *const ()) {
        let c = data as *mut CoroutineCore;
        // SAFETY: as in waker_clone; this clone's count is given back.
        unsafe {
            if (*c).wakers.fetch_sub(1, Ordering::Release) == 1 {
                if (*c).owner != thread_marker() {
                    // The last clone went on another thread: freeing the coroutine there
                    // would race with its shard.
                    std::process::abort();
                }
                CoroutineCore::maybe_free(c);
            }
        }
    }

    pub(crate) static VTABLE: RawWakerVTable =
        RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

    /// Extracts the coroutine from the waker the
    /// coroutine was resumed with.
    ///
    /// # Panics
    /// If a engine future is awaited outside a engine coroutine (an `async` block not
    /// started by [`make_coroutine`]).
    #[inline]
    pub(crate) fn current_coroutine(cx: &Context<'_>) -> NonNull<CoroutineCore> {
        let w = cx.waker();
        if !std::ptr::eq(w.vtable(), &VTABLE) {
            not_a_engine_coroutine();
        }
        // SAFETY: our wakers carry the non-null pointer to the coroutine core.
        unsafe { NonNull::new_unchecked(w.data() as *mut CoroutineCore) }
    }

    /// The panic of [`current_coroutine`], out of the awaiter's inlined path.
    #[cold]
    #[inline(never)]
    #[track_caller]
    fn not_a_engine_coroutine() -> ! {
        panic!("engine future awaited outside a engine coroutine")
    }

    /// The awaiter of an engine future inside a coroutine; with `CHECK_PREEMPT` it also
    /// yields when the future is ready but preemption is requested.
    ///
    /// A suspended awaiter detaches its future and points the promise at that future's own
    /// state: the awaiter is pinned (it is `!Unpin` and
    /// polled in place in the frame), so the promise node is not needed once the promise is
    /// done.
    ///
    /// The enclosing `async` code may drop the awaiter while it is pending (a hand-written
    /// `select`, an abandoned inner `async` block); its `Drop` then unlinks the promise
    /// from its state and takes the coroutine task back, so the coroutine is not left
    /// registered with a promise whose future is gone.
    ///
    /// The layout is fixed: the two words that the constructor writes as constants first,
    /// in their own aligned 16 bytes (one store pair), then the future. The awaiter is
    /// built in a stack temporary and moved into the frame in 16-byte pieces, and every
    /// piece of that move must have been written by one store to forward from the store
    /// buffer: the future comes in pieces that match its own (the pointer and the state word
    /// are a pair, see [`Future`]), and nothing the constructor leaves unwritten shares a
    /// piece with something it writes. A state of its own after the future, as the awaiter
    /// had, put the uninitialized value bytes of that state, or the future's value, next to
    /// `suspended` in one piece.
    #[repr(C)]
    pub struct Awaiter<const CHECK_PREEMPT: bool, T: 'static> {
        /// The coroutine suspended on this awaiter (`await_suspend` ran and the result
        /// has not been taken yet).
        suspended: Option<NonNull<CoroutineCore>>,
        /// The promise node pointed at the future's state (null when the coroutine was
        /// scheduled right away by the preemption check), or [`Awaiter::RESUMED`] once
        /// `await_resume` took the result: `future` owns nothing any more, and dropping the
        /// awaiter is no work. That is the usual end of an awaiter, and once the awaiter is
        /// inlined into its coroutine the marker is a known constant there, so the drop
        /// leaves no code.
        node: *mut PromiseNode<T>,
        /// The awaited future. Once `await_suspend` links the promise, the future is
        /// detached (its promise pointer is null) and its state is the one the promise
        /// writes into.
        future: ManuallyDrop<Future<T>>,
        /// The promise points into this object.
        _pin: PhantomPinned,
    }

    // The awaiter of a `future<>` is four words, two aligned 16-byte pieces.
    const _: () = assert!(std::mem::size_of::<Awaiter<true, ()>>() == 32);

    impl<const CHECK_PREEMPT: bool, T: 'static> Drop for Awaiter<CHECK_PREEMPT, T> {
        #[inline(always)]
        fn drop(&mut self) {
            if self.node != Self::RESUMED {
                self.drop_unresumed();
            }
        }
    }

    impl<const CHECK_PREEMPT: bool, T: 'static> Awaiter<CHECK_PREEMPT, T> {
        /// The `node` of an awaiter whose result was taken: never the address of a node
        /// (nodes are aligned to 8).
        const RESUMED: *mut PromiseNode<T> = ptr::without_provenance_mut(1);

        /// The drop of an awaiter whose result was not taken: never polled, or dropped
        /// while its coroutine is suspended on it (see the type notes).
        #[cold]
        #[inline(never)]
        fn drop_unresumed(&mut self) {
            if let Some(c) = self.suspended {
                // Scheduled already (preemption check, or the promise is done): the run of
                // the hand-out gives its count back.
                if !self.node.is_null() && !self.future.available() {
                    // SAFETY: the state is still unavailable, so the writer of the node has
                    // not gone (it breaks the promise into the state when it goes) and the
                    // node is alive. The coroutine is alive: the hand-out of await_suspend is
                    // counted in `main_pending` until it runs or is taken back here, and a
                    // coroutine with a pending hand-out is never freed. Its fields are
                    // reached through the raw pointer.
                    unsafe {
                        if Future::<T>::unlink_awaiter(self.node) {
                            let core = c.as_ptr();
                            // The hand-out will never run: give its count back. When the
                            // awaiter is dropped by its own frame the coroutine is running and
                            // not done, so this does not free it; if the awaiter outlived its
                            // coroutine, this was the last thing keeping it.
                            (*core).main_pending -= 1;
                            CoroutineCore::maybe_free(core);
                        }
                    }
                }
            }
            // SAFETY: dropped once, here: the result was not taken, so the future still
            // owns what it holds (the promise no longer writes into its state). A detached
            // future has a null promise, so its drop only drops its state.
            unsafe { ManuallyDrop::drop(&mut self.future) };
        }

        /// Wraps the future `f`.
        #[inline]
        pub fn new(f: Future<T>) -> Self {
            Self {
                suspended: None,
                node: ptr::null_mut(),
                future: ManuallyDrop::new(f),
                _pin: PhantomPinned,
            }
        }

        /// Returns whether the result can be taken without suspending: the future is
        /// available and, with `CHECK_PREEMPT`, no preemption is requested.
        #[inline(always)]
        pub fn await_ready(&self) -> bool {
            self.future.available() && (!CHECK_PREEMPT || !need_preempt())
        }

        /// Suspends the coroutine `hndl` on this awaiter: links the promise to the
        /// awaiter's state, or, for an available future under preemption, reschedules the
        /// coroutine.
        ///
        /// # Safety
        /// `hndl` must be the live coroutine that awaits this awaiter; `this` must point
        /// to the pinned awaiter, with no reference to it live.
        #[inline(always)]
        pub unsafe fn await_suspend(this: *mut Self, hndl: NonNull<CoroutineCore>) {
            // SAFETY: guaranteed by the caller; the awaiter does not move until it is
            // dropped, and its Drop unlinks the state.
            unsafe {
                let t = CoroutineCore::hand_out_task(hndl);
                (*this).suspended = Some(hndl);
                if !CHECK_PREEMPT || !(*this).future.available() {
                    let f: *mut Future<T> = ptr::addr_of_mut!((*this).future).cast();
                    // The state of the linked future is `invalid` and owns nothing; it
                    // becomes `future`, and link_awaiter points the promise at it.
                    let st = Future::state_ptr(f);
                    debug_assert!(!(*st).valid());
                    st.write(FutureState::new());
                    (*this).node = (*f).link_awaiter(t, st);
                } else {
                    schedule(t);
                }
            }
        }

        /// Takes the result, which must be available. Afterwards the future owns
        /// nothing.
        #[inline(always)]
        pub fn await_resume(&mut self) -> Result<T, ExceptionPtr> {
            // The promise is done with the state (it is available) and with the node; a
            // linked future was detached, so its state is the result.
            let r = self.future.take_available();
            // Set last, after any call taking the result may make, so that the drop that
            // follows sees them as constants.
            self.suspended = None;
            self.node = Self::RESUMED;
            r
        }

        /// Takes the result as a future state, which the caller makes a ready future (used
        /// by `as_future`).
        #[inline(always)]
        pub(crate) fn resume_state(&mut self) -> FutureState<T> {
            // The promise is done with the state (it is available) and with the node.
            let st = self.future.get_available_state_ref();
            // Set last, as in await_resume.
            self.suspended = None;
            self.node = Self::RESUMED;
            st
        }

        /// The poll of the awaiter, ending with `resume` once the result is there.
        #[inline(always)]
        pub(crate) fn poll_with<R>(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            resume: impl FnOnce(&mut Self) -> R,
        ) -> Poll<R> {
            // SAFETY: the awaiter is not moved out of the pin; await_suspend takes it as a
            // raw pointer.
            let me = unsafe { self.get_unchecked_mut() };
            if me.suspended.is_some() {
                // Resumed by the promise (the result is there) or polled for another reason
                // (a join with another future, a waker): the coroutine task is still
                // registered with the promise, so it is not registered again.
                if me.result_available() {
                    return Poll::Ready(resume(me));
                }
                return Poll::Pending;
            }
            if me.await_ready() {
                return Poll::Ready(resume(me));
            }
            let c = current_coroutine(cx);
            // SAFETY: current_coroutine is the live coroutine being polled; `me` is pinned
            // and not used again in this call.
            unsafe { Self::await_suspend(me, c) };
            Poll::Pending
        }

        /// Whether the result can be taken.
        #[inline(always)]
        fn result_available(&self) -> bool {
            self.future.available()
        }
    }

    impl<const CHECK_PREEMPT: bool, T: 'static> StdFuture for Awaiter<CHECK_PREEMPT, T> {
        type Output = Result<T, ExceptionPtr>;
        #[inline(always)]
        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.poll_with(cx, Self::await_resume)
        }
    }
}

/// Makes engine futures awaitable in coroutines, with a preemption check.
///
/// Awaiting a engine future is only possible inside a coroutine started by
/// [`make_coroutine`]; elsewhere the first poll panics.
impl<T: 'static> IntoFuture for Future<T> {
    type Output = Result<T, ExceptionPtr>;
    type IntoFuture = internal::Awaiter<true, T>;
    #[inline]
    fn into_future(self) -> Self::IntoFuture {
        internal::Awaiter::new(self)
    }
}

/// Starts a coroutine running `frame` and returns the future of its result.
///
/// The coroutine is allocated (one allocation) and the body runs at once until its first
/// suspension.
///
/// A body that completes without suspending leaves its result in a ready future that
/// owns it, and the frame is freed right away. Only a coroutine that suspends returns a
/// future linked to its promise node.
#[inline(always)]
pub fn make_coroutine<T: 'static, Fut>(frame: Fut) -> Future<T>
where
    Fut: StdFuture<Output = Result<T, ExceptionPtr>> + 'static,
{
    type Cp<T, Fut> = internal::CoroutinePromise<T, Fut>;
    let p = Cp::<T, Fut>::alloc(true, frame);
    // SAFETY: p was just allocated; it frees itself once the frame has completed and
    // nothing refers to it. The node's future reference is the returned future's.
    unsafe {
        let n = Cp::get_return_object(p);
        match Cp::poll_frame(p) {
            Poll::Pending => Future::from_node(n),
            // Completed before the first suspension: the result goes straight into a ready
            // future that owns it, and the promise node, which nothing has seen, is not
            // involved.
            Poll::Ready(result) => {
                Cp::finish_first_poll(p);
                Future::from_state(match result {
                    Ok(v) => FutureState::ready(ReadyFutureMarker, v),
                    Err(e) => FutureState::exception(ExceptionFutureMarker, e),
                })
            }
        }
    }
}

/// The handle of a coroutine created suspended whose result nobody awaits: the body of a
/// generator, which is its own task and runs when it is scheduled.
///
/// The handle does not own the coroutine: once the body has completed, the coroutine
/// frees itself and the handle must not be used again (the generator drops it then).
/// Before that, [`SuspendedCoroutine::destroy`] destroys it.
pub(crate) struct SuspendedCoroutine {
    core: NonNull<internal::CoroutineCore>,
    destroy: unsafe fn(*mut internal::CoroutineCore),
}

impl SuspendedCoroutine {
    /// Allocates the coroutine of `frame`, suspended: nothing runs until it is scheduled.
    pub(crate) fn new<Fut>(frame: Fut) -> Self
    where
        Fut: StdFuture<Output = Result<(), ExceptionPtr>> + 'static,
    {
        type Cp<Fut> = internal::CoroutinePromise<(), Fut>;
        // No future: the node starts without a future reference and with a null state,
        // so the result is dropped when the body completes.
        let p = Cp::<Fut>::alloc(false, frame);
        SuspendedCoroutine {
            // SAFETY: the allocation is never null; the core is the first field.
            core: unsafe { NonNull::new_unchecked(p.cast()) },
            destroy: Cp::<Fut>::destroy,
        }
    }

    /// Queues the coroutine to run (start or resume it).
    ///
    /// # Safety
    /// The coroutine must not have completed.
    #[inline]
    pub(crate) unsafe fn schedule(&self) {
        // SAFETY: guaranteed by the caller: a coroutine that has not completed is alive.
        schedule(unsafe { internal::CoroutineCore::hand_out_task(self.core) });
    }

    /// Destroys the frame of a coroutine that is not
    /// running, with everything its body holds; the memory goes once no queued run and
    /// no waker refers to it.
    ///
    /// # Safety
    /// The coroutine must not have completed and must not be running.
    #[inline]
    pub(crate) unsafe fn destroy(self) {
        // SAFETY: guaranteed by the caller.
        unsafe { (self.destroy)(self.core.as_ptr()) }
    }
}

impl<T: 'static, Fut> internal::CoroutinePromise<T, Fut>
where
    Fut: StdFuture<Output = Result<T, ExceptionPtr>> + 'static,
{
    /// The `destroy` hook of [`SuspendedCoroutine`].
    ///
    /// # Safety
    /// `c` must be the core of a live coroutine of this type that has not completed and
    /// is not running.
    unsafe fn destroy(c: *mut internal::CoroutineCore) {
        let this = c.cast::<Self>();
        // SAFETY: guaranteed by the caller. Dropping the frame runs the drops of the
        // awaiters it is suspended on, which give their hand-outs back without freeing
        // the coroutine (it is not done yet). Then the writer reference of the node is
        // released, which may free the coroutine: nothing is touched after it.
        unsafe {
            ManuallyDrop::drop(&mut *ptr::addr_of_mut!((*this).frame));
            internal::CoroutineCore::set_done(c);
            drop(Self::take_promise(this));
        }
    }
}

/// Coroutine utilities: awaiting without a preemption check, and lambda wrappers.
#[allow(clippy::module_inception)]
pub mod coroutine {
    use super::*;

    /// Wrapper for a future which turns off checking for preemption
    /// when awaiting it in a coroutine.
    /// Awaiting it will bypass
    /// checking if the task quota is depleted, which means that
    /// a ready future will be handled immediately.
    #[must_use]
    pub struct WithoutPreemptionCheck<T: 'static>(pub Future<T>);

    impl<T: 'static> WithoutPreemptionCheck<T> {
        /// Wraps the future `f`.
        pub fn new(f: Future<T>) -> Self {
            Self(f)
        }
    }

    impl<T: 'static> std::ops::Deref for WithoutPreemptionCheck<T> {
        type Target = Future<T>;
        fn deref(&self) -> &Future<T> {
            &self.0
        }
    }

    impl<T: 'static> std::ops::DerefMut for WithoutPreemptionCheck<T> {
        fn deref_mut(&mut self) -> &mut Future<T> {
            &mut self.0
        }
    }

    /// Makes a closure that returns a coroutine safe to pass, from an outer coroutine, to
    /// functions that accept continuations.
    ///
    /// Such a closure is not a safe parameter to a function that expects a regular engine
    /// continuation if the coroutine it returns refers to the closure's captures. Wrap it
    /// in [`Lambda`]; the returned coroutine must complete within the borrow of the closure.
    ///
    /// The wrapper borrows the function object for the lifetime `'a`, so it cannot outlive
    /// it.
    pub struct Lambda<'a, Func> {
        func: &'a Func,
    }

    impl<'a, Func> Lambda<'a, Func> {
        /// Creates a wrapper around the function object `func`, to be passed to an engine
        /// function that accepts a continuation.
        pub fn new(func: &'a Func) -> Self {
            Self { func }
        }
        /// Calls the wrapped function object. Normally invoked by the engine function the
        /// wrapper was passed to.
        pub fn call<Args, R>(&self, args: Args) -> R
        where
            Func: Fn(Args) -> R,
        {
            (self.func)(args)
        }
    }
}

/// Awaits a future wrapped in [`coroutine::WithoutPreemptionCheck`] without a preemption
/// check.
impl<T: 'static> IntoFuture for coroutine::WithoutPreemptionCheck<T> {
    type Output = Result<T, ExceptionPtr>;
    type IntoFuture = internal::Awaiter<false, T>;
    #[inline]
    fn into_future(self) -> Self::IntoFuture {
        internal::Awaiter::new(self.0)
    }
}
