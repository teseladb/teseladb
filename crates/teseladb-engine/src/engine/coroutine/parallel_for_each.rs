//! Running a function on every element of a range concurrently, inside a coroutine.
//!
//! [`ParallelForEach::new`] calls the function on every element at once and keeps the
//! futures that are not ready (five inline, the rest on the heap). Awaiting it consumes the
//! futures from the back: when one is pending it registers the coroutine's task on that
//! future and returns `Pending`, and when polled again it continues from the back. It
//! resolves once every future is consumed, failing with one of the errors if any failed.
//! The layout of the awaiter is tuned so that moving it into the coroutine frame is cheap.

use std::mem::{ManuallyDrop, MaybeUninit};
use std::pin::Pin;
use std::ptr::NonNull;
use std::task::{Context, Poll};

use crate::engine::core::coroutine::internal::{CoroutineCore, current_coroutine};
use crate::engine::core::future::{ExceptionPtr, Future, futurize_invoke};
use crate::engine::core::task::TaskPtr;
use crate::engine::util::critical_alloc_section::memory::ScopedCriticalAllocSection;

/// A small vector of futures: five inline, then the heap.
///
/// The words written at construction come first and the inline slots, never written
/// until a future is pushed, last (see [`ParallelForEach`]). The slots are five fields
/// reached by constant indices (a `match` on the length), not an array indexed by the
/// length: with no dynamic index the compiler keeps the object built by
/// [`ParallelForEach::new`] in registers and writes only its initialized fields into the
/// coroutine frame. With an array, the object stayed in a stack temporary and every move
/// copied all of it, the uninitialized slots included, with SIMD loads and stores, whose
/// values are forwarded to the next load only after several cycles. The alignment to 16
/// puts the slots at 16-byte
/// offsets of the awaiter; at 8-byte offsets the compiler went back to the copy.
#[repr(C, align(16))]
struct SmallFutures {
    inline_len: usize,
    /// Dropped by hand in `drop_slow` (a heap part that never allocated owns nothing),
    /// so that the drop of an awaiter whose futures were all consumed is no code.
    heap: ManuallyDrop<Vec<Future<()>>>,
    s0: MaybeUninit<Future<()>>,
    s1: MaybeUninit<Future<()>>,
    s2: MaybeUninit<Future<()>>,
    s3: MaybeUninit<Future<()>>,
    s4: MaybeUninit<Future<()>>,
}

impl SmallFutures {
    const N: usize = 5;

    /// Inline slot `i` (< N).
    #[inline(always)]
    fn slot(&mut self, i: usize) -> &mut MaybeUninit<Future<()>> {
        match i {
            0 => &mut self.s0,
            1 => &mut self.s1,
            2 => &mut self.s2,
            3 => &mut self.s3,
            _ => &mut self.s4,
        }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.inline_len == 0
    }

    #[inline]
    fn back(&mut self) -> &mut Future<()> {
        if !self.heap.is_empty() {
            // SAFETY: just checked that the heap part is not empty.
            return unsafe { self.heap.last_mut().unwrap_unchecked() };
        }
        let i = self.inline_len - 1;
        // SAFETY: callers check non-emptiness; slot inline_len-1 is initialized.
        unsafe { self.slot(i).assume_init_mut() }
    }

    #[inline]
    fn pop_back(&mut self) -> Future<()> {
        if let Some(f) = self.heap.pop() {
            return f;
        }
        self.inline_len -= 1;
        let i = self.inline_len;
        // SAFETY: the slot was initialized and is moved out exactly once.
        unsafe { self.slot(i).assume_init_read() }
    }
}

/// The inline slots that [`ParallelForEach::new`] filled so far, a guard of their own
/// so that a launch that unwinds drops them. It has no address taken (the heap part, which calls take by address, is a
/// separate local), so the compiler keeps it in registers (see `SmallFutures`).
struct Launched {
    len: usize,
    s0: MaybeUninit<Future<()>>,
    s1: MaybeUninit<Future<()>>,
    s2: MaybeUninit<Future<()>>,
    s3: MaybeUninit<Future<()>>,
    s4: MaybeUninit<Future<()>>,
}

impl Drop for Launched {
    #[inline(always)]
    fn drop(&mut self) {
        // Only on unwinding: the normal path never drops the guard.
        while self.len > 0 {
            self.len -= 1;
            let f = match self.len {
                0 => &mut self.s0,
                1 => &mut self.s1,
                2 => &mut self.s2,
                3 => &mut self.s3,
                _ => &mut self.s4,
            };
            // SAFETY: the first `len` slots are initialized; each is dropped once.
            unsafe { f.assume_init_drop() };
        }
    }
}

impl Drop for SmallFutures {
    /// In line: the usual end of the awaiter has every future consumed and no heap part,
    /// and then there is nothing to do; otherwise the futures and the heap part go out of
    /// line.
    #[inline]
    fn drop(&mut self) {
        if !self.is_empty() || self.heap.capacity() != 0 {
            self.drop_slow();
        }
    }
}

impl SmallFutures {
    /// The rest of the drop: the futures still held, then the heap part.
    #[cold]
    #[inline(never)]
    fn drop_slow(&mut self) {
        while !self.is_empty() {
            drop(self.pop_back());
        }
        // SAFETY: dropped once, here, in the drop of the object.
        unsafe { ManuallyDrop::drop(&mut self.heap) };
    }
}

/// Invokes a function on all elements of a range in parallel and waits for all the futures
/// to complete, in a coroutine.
///
/// `ParallelForEach` can be used to launch a function concurrently on all elements of a
/// given range and wait for all of them to complete, by awaiting it.
///
/// If one or more of the function invocations resolve to an error, then one of the errors
/// is returned. All of the futures are waited for, even in the case of errors.
///
/// Example
///
/// ```no_run
/// # use std::cell::Cell;
/// # use std::rc::Rc;
/// # use teseladb::engine::core::coroutine::make_coroutine;
/// # use teseladb::engine::core::future::{make_ready_future, Future};
/// # use teseladb::engine::coroutine::parallel_for_each::ParallelForEach;
/// fn sum_of_squares(v: Vec<i32>) -> Future<i32> {
///     make_coroutine(async move {
///         let sum = Rc::new(Cell::new(0));
///         let s = sum.clone();
///         ParallelForEach::new(v, move |x: i32| {
///             s.set(s.get() + x * x);
///             make_ready_future(())
///         })
///         .await?;
///         Ok(sum.get())
///     })
/// }
/// ```
///
/// Safe for use with closures that return coroutines.
///
/// All invocations of the function run on the current shard; to run a function on all
/// shards in parallel, see `smp::invoke_on_all`.
///
/// The awaiter is built and then moved into the coroutine frame. Its layout is fixed so
/// that the move reads the words written at construction in aligned 16-byte pieces that
/// match those stores (`_ex` and `_waiting_task`, then `_suspended` and the length of the
/// futures), and the inline future slots, which construction leaves unwritten, after them.
/// `_ex` is a nullable `ExceptionPtr`: one word. An `Option<ExceptionPtr>` is two (the
/// pointer is nullable already, so the option needs a tag), and `None` writes only the tag:
/// the 16-byte piece of the move that read the tag and the unwritten word after it could not
/// take its data from the store buffer.
#[must_use = "a ParallelForEach does nothing unless awaited"]
#[repr(C)]
pub struct ParallelForEach<Func> {
    _ex: ExceptionPtr,
    _waiting_task: Option<TaskPtr>,
    /// The coroutine whose task is registered on the back future.
    _suspended: Option<NonNull<CoroutineCore>>,
    _futures: SmallFutures,
    _func: Func,
}

impl<Func> Drop for ParallelForEach<Func> {
    #[inline]
    fn drop(&mut self) {
        // Dropped while the coroutine waits (the enclosing async code let it go): take the
        // task back from the back future, or the hand-out would never be given back and the
        // coroutine would never be freed.
        let Some(c) = self._suspended else { return };
        if !self._futures.is_empty() {
            let back = self._futures.back();
            if !back.available() && back.take_coroutine() {
                // SAFETY: the coroutine is alive: its hand-out is counted until it runs or
                // is given back here.
                unsafe { CoroutineCore::give_back_task(c) };
            }
        }
    }
}

impl<Func> ParallelForEach<Func> {
    // Consume futures in reverse order.
    // Since futures at the front are expected
    // to become ready before futures at the back,
    // therefore it is less likely we will have
    // to wait on them, after the back futures
    // become available.
    //
    // Return true if and only if all futures were consumed.
    fn consume_next(&mut self) -> bool {
        while !self._futures.is_empty() {
            if !self._futures.back().available() {
                return false;
            }
            let fut = self._futures.pop_back();
            if fut.failed() {
                self._ex = fut.get_exception();
            }
        }
        true
    }

    /// Launches `func` on every element of `iter`.
    ///
    /// The loop works on locals (the exception, the length, the five inline slots, the
    /// heap part and the function), and the object is put together from them once at
    /// the end: no part of it has its address taken before it is complete (the heap
    /// part is grown by calls, which take its address), so the compiler keeps it in
    /// registers and its move into the coroutine frame writes only the fields that hold
    /// something (see `SmallFutures`).
    pub fn new<I, R>(iter: I, func: Func) -> Self
    where
        I: IntoIterator,
        Func: FnMut(I::Item) -> R,
        R: crate::engine::core::future::Futurize<Output = ()>,
    {
        let mut func = func;
        let mut ex: Option<ExceptionPtr> = None;
        let mut heap: Vec<Future<()>> = Vec::new();
        let mut l = Launched {
            len: 0,
            s0: MaybeUninit::uninit(),
            s1: MaybeUninit::uninit(),
            s2: MaybeUninit::uninit(),
            s3: MaybeUninit::uninit(),
            s4: MaybeUninit::uninit(),
        };
        let mut it = iter.into_iter();
        while let Some(x) = it.next() {
            let fut = futurize_invoke(|| func(x));
            if fut.available() {
                if fut.failed() {
                    ex = Some(fut.get_exception());
                }
            } else {
                let _cs = ScopedCriticalAllocSection::new();
                if l.len == 0 {
                    // Reserve from the iterator's size hint.
                    let n = it.size_hint().0 + 1;
                    if n > SmallFutures::N {
                        heap.reserve(n - SmallFutures::N);
                    }
                }
                // Keep the pending future: the inline slots first, then the heap, as
                // SmallFutures does (its length stays at N once the slots are full).
                match l.len {
                    0 => l.s0 = MaybeUninit::new(fut),
                    1 => l.s1 = MaybeUninit::new(fut),
                    2 => l.s2 = MaybeUninit::new(fut),
                    3 => l.s3 = MaybeUninit::new(fut),
                    4 => l.s4 = MaybeUninit::new(fut),
                    _ => heap.push(fut),
                }
                if l.len < SmallFutures::N {
                    l.len += 1;
                }
            }
        }
        // The slots pass to the object: the guard must not drop them.
        let l = std::mem::ManuallyDrop::new(l);
        // SAFETY: each slot is read once, out of a guard that is never dropped; the first
        // `len` slots are initialized, and the object owns exactly those from here on.
        let futures = unsafe {
            SmallFutures {
                inline_len: l.len,
                heap: ManuallyDrop::new(heap),
                s0: std::ptr::read(&l.s0),
                s1: std::ptr::read(&l.s1),
                s2: std::ptr::read(&l.s2),
                s3: std::ptr::read(&l.s3),
                s4: std::ptr::read(&l.s4),
            }
        };
        ParallelForEach {
            _ex: ex.unwrap_or_default(),
            _waiting_task: None,
            _suspended: None,
            _futures: futures,
            _func: func,
        }
    }

    /// Returns whether every future was consumed already, so awaiting does not suspend.
    #[inline]
    pub fn await_ready(&self) -> bool {
        if self._futures.is_empty() {
            return self._ex.is_null();
        }
        false
    }

    fn await_resume(&mut self) -> Result<(), ExceptionPtr> {
        if self._ex.is_null() {
            return Ok(());
        }
        Err(std::mem::take(&mut self._ex))
    }
}

impl<Func> Unpin for ParallelForEach<Func> {}

impl<Func> std::future::Future for ParallelForEach<Func> {
    type Output = Result<(), ExceptionPtr>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self._waiting_task.is_none() && self.await_ready() {
            return Poll::Ready(Ok(()));
        }
        if self._suspended.is_some()
            && !self._futures.is_empty()
            && !self._futures.back().available()
        {
            // Polled again before the back future resolved: the task is still registered.
            return Poll::Pending;
        }
        self._suspended = None;
        // Consume from the back; register on the first pending future.
        if self.consume_next() {
            // The coroutine is the task running this poll already, so it is the current
            // task when it resumes.
            return Poll::Ready(self.await_resume());
        }
        let c = current_coroutine(cx);
        // SAFETY: the waker's coroutine is the live coroutine running this poll; its task
        // is handed to the back future's promise right below.
        let task = unsafe { CoroutineCore::hand_out_task(c) };
        self._suspended = Some(c);
        self._waiting_task = Some(task);
        // Register the coroutine's task on the pending future.
        self._futures.back().set_coroutine(task);
        Poll::Pending
    }
}

/// The waiting task, for backtraces.
impl<Func> ParallelForEach<Func> {
    /// Returns the coroutine task waiting on this awaiter.
    pub fn waiting_task(&self) -> Option<TaskPtr> {
        self._waiting_task
    }
}
