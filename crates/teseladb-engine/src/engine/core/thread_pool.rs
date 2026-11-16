//! The per-shard syscall thread.
//!
//! A [`ThreadPool`] owns one helper thread per shard that runs blocking system calls handed to
//! it through a [`SyscallWorkQueue`], so the reactor thread never blocks. The helper thread
//! blocks all signals, waits on the queue's start eventfd, runs every pending item and pushes
//! it back to the completed ring. When the reactor is about to sleep it enters "interrupt
//! mode", and the helper thread then writes to the reactor's notify eventfd after each
//! completion so the reactor wakes up to collect it.
use std::cell::Cell;
use std::ffi::c_void;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, Ordering, fence};

use crate::engine::core::future::{ExceptionPtr, Future};
use crate::engine::core::posix::{PosixThread, throw_pthread_error};
use crate::engine::core::reactor::{SIG_BLOCK, SigsetT, pthread_self, pthread_sigmask, sigfillset};
use crate::engine::core::reactor_backend::engine_assert;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::syscall_work_queue::{SyscallWorkQueue, WorkItemPtr};
use crate::engine::net::node_ptr::NodePtr;

unsafe extern "C" {
    fn read(fd: i32, buf: *mut c_void, n: usize) -> isize;
    fn write(fd: i32, buf: *const c_void, n: usize) -> isize;
    fn pthread_setname_np(thread: usize, name: *const std::ffi::c_char) -> i32;
}

/// The syscall thread of a shard, which runs blocking system calls.
///
/// The pool is shared by `&` between the reactor thread and its syscall thread: the
/// syscall thread only uses the queues, the eventfds and the atomics, and the counter and
/// the thread handle are `Cell`s that only the reactor thread touches. It lives in the node
/// of a [`ThreadPoolPtr`], whose root pointer the syscall thread keeps; no `&mut` to it is
/// ever formed while the thread runs (the destructor stops and joins it through `&self`).
pub struct ThreadPool {
    /// The reactor's notify eventfd, written to wake the reactor while it sleeps.
    _notify_eventfd: i32,
    _aio_threaded_fallbacks: Cell<u64>,
    /// Queues shared with the syscall thread.
    pub(crate) inter_thread_wq: SyscallWorkQueue,
    _worker_thread: Cell<Option<PosixThread>>,
    _stopped: AtomicBool,
    _main_thread_idle: AtomicBool,
}

/// The owner of a [`ThreadPool`]: the pool in a node that never moves, reached only by shared
/// references. Dropping it stops and joins the syscall thread through `&ThreadPool` before the
/// node is freed.
pub struct ThreadPoolPtr {
    node: NodePtr<ThreadPool>,
}

impl std::ops::Deref for ThreadPoolPtr {
    type Target = ThreadPool;
    #[inline]
    fn deref(&self) -> &ThreadPool {
        // SAFETY: the pool is only ever reached by shared references (its mutable state is
        // in atomics and `Cell`s), so this one overlaps no `&mut`.
        unsafe { self.node.get() }
    }
}

impl Drop for ThreadPoolPtr {
    /// Stops the syscall thread and waits for it to exit.
    fn drop(&mut self) {
        let pool: &ThreadPool = self;
        pool._stopped.store(true, Ordering::Relaxed);
        pool.inter_thread_wq._start_eventfd.signal(1);
        if let Some(t) = pool._worker_thread.take() {
            t.join();
        }
        // The node is freed after this, with the syscall thread joined.
    }
}

/// The pool pointer handed to the worker thread; the pool joins the thread before it dies.
struct PoolPtr(*const ThreadPool);
// SAFETY: the worker only touches the fields designed for it (the queues, the eventfds and
// the atomics), and the pool outlives the thread (it joins it in its destructor).
unsafe impl Send for PoolPtr {}

impl ThreadPool {
    /// Creates the pool and starts its syscall thread, named `name`.
    ///
    /// The worker refers to the pool, so the pool is heap-allocated and the thread started once
    /// it has its address. `notify_eventfd` is the reactor's notify eventfd, which must outlive
    /// the pool.
    ///
    /// # Errors
    ///
    /// Returns the error raised when the queue's eventfd or the thread cannot be created.
    // The pool is born at its heap address, which the worker keeps.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(notify_eventfd: i32, name: Sstring) -> Result<ThreadPoolPtr, ExceptionPtr> {
        let this = ThreadPoolPtr {
            node: NodePtr::new(ThreadPool {
                _notify_eventfd: notify_eventfd,
                _aio_threaded_fallbacks: Cell::new(0),
                inter_thread_wq: SyscallWorkQueue::new()?,
                _worker_thread: Cell::new(None),
                _stopped: AtomicBool::new(false),
                _main_thread_idle: AtomicBool::new(false),
            }),
        };
        // The node's root pointer: the pool never moves while the thread runs.
        let p = PoolPtr(this.node.as_ptr().cast_const());
        let t = PosixThread::new(Box::new(move || {
            let p = p;
            // SAFETY: see PoolPtr.
            unsafe { (*p.0).work(name) }
        }));
        // A failure drops `this` with no thread to join.
        this._worker_thread.set(Some(t?));
        Ok(this)
    }

    /// Runs `func` on the syscall thread and returns a future for its result.
    ///
    /// The function runs on the syscall thread and its result is handed back, so both are
    /// `Send`.
    #[inline]
    pub fn submit<T: Send + 'static, Func: FnMut() -> T + Send + 'static>(
        &self,
        func: Func,
    ) -> Future<T> {
        self._aio_threaded_fallbacks.set(self._aio_threaded_fallbacks.get() + 1);
        self.inter_thread_wq.submit::<T, Func>(func)
    }

    /// The number of operations submitted to the syscall thread so far.
    #[inline]
    pub fn operation_count(&self) -> u64 {
        self._aio_threaded_fallbacks.get()
    }

    /// Completes the items the syscall thread has finished; returns `true` if there were any.
    /// Called from the reactor's poll loop.
    #[inline]
    pub fn complete(&self) -> bool {
        self.inter_thread_wq.complete() != 0
    }

    /// Asks the syscall thread to wake the reactor after each completion; called before the
    /// reactor sleeps.
    // Before we enter interrupt mode, we must make sure that the syscall thread will properly
    // generate signals to wake us up. This means we need to make sure that all modifications to
    // the pending and completed fields in the inter_thread_wq are visible to all threads.
    //
    // Simple release-acquire won't do because we also need to serialize all writes that happens
    // before the syscall thread loads this value, so we'll need full seq_cst.
    #[inline]
    pub fn enter_interrupt_mode(&self) {
        self._main_thread_idle.store(true, Ordering::SeqCst);
    }

    /// Stops the wake-ups requested by [`enter_interrupt_mode`](Self::enter_interrupt_mode);
    /// called when the reactor wakes.
    // When we exit interrupt mode, however, we can safely used relaxed order. If any reordering
    // takes place, we'll get an extra signal and complete will be called one extra time, which is
    // harmless.
    #[inline]
    pub fn exit_interrupt_mode(&self) {
        self._main_thread_idle.store(false, Ordering::Relaxed);
    }

    fn work(&self, name: Sstring) {
        // The thread name is cut at its first NUL, so the C string has none left.
        let cname = std::ffi::CString::new(name.as_str().split('\0').next().unwrap_or(""))
            .unwrap_or_default();
        // SAFETY: NUL-terminated name.
        unsafe { pthread_setname_np(pthread_self(), cname.as_ptr()) };
        let mut mask = SigsetT::default();
        // SAFETY: valid signal set.
        let r = unsafe {
            sigfillset(&mut mask);
            pthread_sigmask(SIG_BLOCK, &mask, std::ptr::null_mut())
        };
        if throw_pthread_error(r).is_err() {
            // The error cannot be reported from the thread function, so the program aborts.
            std::process::abort();
        }
        let mut tmp_buf: [MaybeUninit<WorkItemPtr>; SyscallWorkQueue::QUEUE_LENGTH] =
            [MaybeUninit::uninit(); SyscallWorkQueue::QUEUE_LENGTH];
        loop {
            let mut count: u64 = 0;
            // SAFETY: reading 8 bytes into a local from the start eventfd.
            let r = unsafe {
                read(
                    self.inter_thread_wq._start_eventfd.get_read_fd(),
                    &mut count as *mut u64 as *mut c_void,
                    8,
                )
            };
            engine_assert!(r == 8);
            if self._stopped.load(Ordering::Relaxed) {
                break;
            }
            let mut end = 0usize;
            self.inter_thread_wq._pending.consume_all(|wi| {
                // SAFETY: the semaphore bounds the queue to QUEUE_LENGTH items.
                unsafe { tmp_buf.get_unchecked_mut(end).write(wi) };
                end += 1;
            });
            for slot in &tmp_buf[..end] {
                // SAFETY: the first `end` slots were written above.
                let wi = unsafe { slot.assume_init() };
                // SAFETY: the item is owned by this thread until it is pushed back.
                unsafe { (*wi.0).process() };
                let pushed = self.inter_thread_wq._completed.push(wi);
                debug_assert!(pushed, "the semaphore bounds the queue");
                // Prevent the following load of _main_thread_idle to be hoisted before the writes to _completed above.
                fence(Ordering::SeqCst);
                if self._main_thread_idle.load(Ordering::Relaxed) {
                    let one: u64 = 1;
                    // SAFETY: the reactor's notify eventfd outlives the pool; writing 8 bytes.
                    let res = unsafe {
                        write(self._notify_eventfd, &one as *const u64 as *const c_void, 8)
                    };
                    engine_assert!(
                        res == 8 /* "write(2) failed on _reactor._notify_eventfd" */
                    );
                }
            }
        }
    }
}
