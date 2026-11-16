//! Tasks built from closures.
//!
//! [`make_task`] wraps a closure in a heap-allocated [`LambdaTask`] that the reactor can queue
//! like any other [`Task`]. The task embeds the promise node of its result, so taking its
//! future ([`LambdaTask::get_future`]) allocates nothing: the task and its result share one
//! allocation, which is freed once the task has run and nothing refers to the node any more.
//! The header and node of a task returning `()` fill one 64-byte allocation size class.
use std::mem::{self, ManuallyDrop};
use std::ptr::{self, NonNull};

use crate::engine::core::future::internal::{PromiseBaseWithType, PromiseNode};
use crate::engine::core::future::{Future, Futurize, futurize_invoke};
use crate::engine::core::scheduling::{SchedulingGroup, current_scheduling_group};
use crate::engine::core::task::{Task, TaskBase, TaskPtr, task_ptr};

/// A task that runs a closure and resolves a future with its result.
///
/// The result's promise node is embedded in the task, so [`get_future`](Self::get_future)
/// allocates nothing. The task's memory outlives its run until the future has let go of the
/// node, as continuations do.
#[repr(C)]
pub struct LambdaTask<Func: FnOnce() -> R, R: Futurize> {
    // First field: the task header (see `Task`).
    base: TaskBase,
    /// The result node, with the writer reference (the task's, until it runs) and, once
    /// `get_future()` was called, the future reference.
    _result: PromiseNode<R::Output>,
    /// The closure, taken when the task runs.
    _func: ManuallyDrop<Func>,
    // `node_live` (the embedded node is still referenced, by the future or a writer) and
    // `done` (`run_and_dispose()` has run) are `_result.holder_node_live` and
    // `_result.holder_done`, in the padding of the node: the task of `yield()` is 64 bytes,
    // like the header and the node alone (see `PromiseNode::holder_node_live`).
}

// The header and the node of a task returning `()` fill one 64-byte size class (the debug
// configurations add fields to both).
#[cfg(not(any(engine_debug_promise, engine_task_backtrace)))]
const _: () = assert!(mem::size_of::<TaskBase>() + mem::size_of::<PromiseNode<()>>() == 64);

impl<Func: FnOnce() -> R + 'static, R: Futurize> LambdaTask<Func, R> {
    /// Allocates a task that runs `func` in scheduling group `sg`, built in place in a new
    /// allocation (the node points into itself, so the task is constructed at its final
    /// address).
    #[inline]
    fn alloc(sg: SchedulingGroup, func: Func) -> *mut Self {
        let mut b = Box::<Self>::new_uninit();
        let p = b.as_mut_ptr();
        // SAFETY: p is the uninitialized allocation of a Self, uniquely owned here; every
        // field is written once before the box is given up as initialized, and the node's
        // state pointer is derived from the pointer to the whole task.
        unsafe {
            ptr::addr_of_mut!((*p).base).write(TaskBase::new::<Self>(sg));
            ptr::addr_of_mut!((*p)._result)
                .write(PromiseNode::new_detached(false, Self::release_node));
            ptr::addr_of_mut!((*p)._func).write(ManuallyDrop::new(func));
            let p = Box::into_raw(b).cast::<Self>();
            let n = Self::node_ptr(p);
            (*n).state = ptr::addr_of_mut!((*n).local_state);
            p
        }
    }

    /// The embedded node, with the provenance of the whole task (so that
    /// [`LambdaTask::release_node`] can step back to it).
    ///
    /// # Safety
    /// `this` must point to a live task.
    #[inline(always)]
    unsafe fn node_ptr(this: *mut Self) -> *mut PromiseNode<R::Output> {
        // SAFETY: guaranteed by the caller.
        unsafe { ptr::addr_of_mut!((*this)._result) }
    }

    /// Release hook of the embedded node: frees the task once it has run too.
    ///
    /// # Safety
    /// `n` must be the `_result` field of a live `LambdaTask` of this type, obtained with
    /// [`LambdaTask::node_ptr`].
    unsafe fn release_node(n: *mut PromiseNode<R::Output>) {
        // SAFETY: the node is embedded at a fixed offset of its task, and `n` has the
        // provenance of the whole task (node_ptr).
        unsafe {
            let me = n.byte_sub(mem::offset_of!(Self, _result)).cast::<Self>();
            (*n).holder_node_live = false;
            if (*n).holder_done {
                drop(Box::from_raw(me));
            }
        }
    }

    /// The future resolved with the closure's result.
    ///
    /// # Safety
    /// `this` must come from [`make_task`] and not have run yet; it is called at most once.
    #[inline]
    pub unsafe fn get_future(this: *mut Self) -> Future<R::Output> {
        // SAFETY: guaranteed by the caller.
        unsafe {
            let n = Self::node_ptr(this);
            // The future must not have been taken yet, and the node must have a state.
            assert!(!(*n).future && !(*n).future_ref && !(*n).state.is_null());
            (*n).future = true;
            (*n).future_ref = true;
            // The task's memory lives as long as the future refers to the node.
            Future::from_node(n)
        }
    }

    /// The task pointer the reactor queues.
    ///
    /// # Safety
    /// `this` must come from [`make_task`] and not have been queued yet.
    #[inline]
    pub unsafe fn as_task_ptr(this: *mut Self) -> TaskPtr {
        // SAFETY: guaranteed by the caller (a live heap task, so non-null).
        task_ptr(unsafe { NonNull::new_unchecked(this) })
    }
}

// SAFETY: LambdaTask is repr(C) with the header built by TaskBase::new::<Self> first.
unsafe impl<Func: FnOnce() -> R + 'static, R: Futurize> Task for LambdaTask<Func, R> {
    /// Runs the closure, forwards its result to the embedded promise and frees the task
    /// when nothing refers to its node any more.
    unsafe fn run_and_dispose(this: NonNull<Self>) {
        let me = this.as_ptr();
        // SAFETY: the task is live and runs once, so the function is taken once; the fields
        // are reached through the raw pointer, so no reference to the task is live when the
        // promise (which may release the node) is done with or when the task is freed.
        unsafe {
            let f = ManuallyDrop::take(&mut (*me)._func);
            let n = Self::node_ptr(me);
            // `get_future()` was never called: `_state` still points to `_local_state`.
            let no_future = !(*n).future
                && !(*n).future_ref
                && (*n).state == ptr::addr_of_mut!((*n).local_state);
            // When the future no longer refers to the node (an awaiter or a continuation
            // took it over, or it was dropped), the task holds the node's future reference
            // while its promise lets go of it: releasing the writer then does not go
            // through the release hook, and the task is freed below, together with its
            // promise.
            let held = !(*n).future_ref;
            if held {
                (*n).future_ref = true;
            }
            // Run the closure and forward its result to the embedded promise.
            let result = futurize_invoke(f);
            let pr = PromiseBaseWithType::from_node(n);
            if no_future && !result.available() {
                // When the state still points to the local state, the future was never
                // taken. The promise is about to be destroyed, so the future can never be
                // taken and the forwarding request can be ignored.
                drop(result);
                drop(pr);
            } else {
                result.forward_to_base(pr);
            }
            (*n).holder_done = true;
            if held {
                (*n).future_ref = false;
                // A forward chain may still hold the writer reference (the function
                // returned an unavailable future); release_node frees the task when it lets
                // go.
                if (*n).writer_ref {
                    return;
                }
                (*n).holder_node_live = false;
            }
            if !(*n).holder_node_live {
                // Nothing refers to the node any more and `me` is not used after this
                // point (the box came from alloc()).
                drop(Box::from_raw(me));
            }
        }
    }
    /// The task waiting for this one's result, if it has not run yet.
    #[inline]
    unsafe fn waiting_task(this: NonNull<Self>) -> Option<TaskPtr> {
        let me = this.as_ptr();
        // SAFETY: the task is live; it holds the writer reference on its node until it runs.
        unsafe {
            if (*Self::node_ptr(me)).holder_done {
                return None;
            }
            (*PromiseNode::resolve(Self::node_ptr(me))).task
        }
    }
}

/// Creates a task that runs `func` in the current scheduling group.
///
/// The task is not queued; take its future with [`LambdaTask::get_future`] and queue it with
/// [`LambdaTask::as_task_ptr`]. One allocation.
#[inline]
pub fn make_task_current<Func: FnOnce() -> R + 'static, R: Futurize>(
    func: Func,
) -> *mut LambdaTask<Func, R> {
    LambdaTask::alloc(current_scheduling_group(), func)
}

/// Creates a task that runs `func` in scheduling group `sg`; see [`make_task_current`].
#[inline]
pub fn make_task<Func: FnOnce() -> R + 'static, R: Futurize>(
    sg: SchedulingGroup,
    func: Func,
) -> *mut LambdaTask<Func, R> {
    LambdaTask::alloc(sg, func)
}

/// Creates a task that runs `func` in `sg` and returns the pointer to queue, for callers that
/// do not take its future.
#[inline]
pub fn make_task_ptr<Func: FnOnce() -> R + 'static, R: Futurize>(
    sg: SchedulingGroup,
    func: Func,
) -> TaskPtr {
    // SAFETY: a fresh task from make_task.
    unsafe { LambdaTask::as_task_ptr(make_task(sg, func)) }
}
