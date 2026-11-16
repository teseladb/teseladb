//! Engine threads: stackful coroutines in which futures can be waited for by blocking.
//!
//! Engine threads provide an execution environment where blocking
//! is tolerated; you can issue I/O, and wait for it in the same function,
//! rather than establishing a callback to be called with `Future::then`.
//!
//! Engine threads are not the same as operating system threads:
//!   - engine threads are cooperative; they are never preempted except
//!     at blocking points (see below)
//!   - engine threads always run on the same core they were launched on
//!
//! Like other engine code, engine threads may not issue blocking system calls.
//!
//! An engine thread blocking point is any function that returns a future.
//! You block by calling `get()` on the future; this waits for the future to become
//! available, and in the meanwhile, other engine threads and non-threaded
//! code may execute.
//!
//! An easy way to launch a thread, carry out some computation and return a
//! result from it is [`async_`] (or [`async_with_attributes`]).
//! The result is returned as a future, so that non-threaded code can wait for
//! the thread to terminate and yield a result.
//!
//! Each [`Thread`] owns a [`ThreadContext`]: a heap stack (128 KiB by default) and a saved
//! context. Context switches are a few instructions of assembly on x86_64 and aarch64; with
//! `--cfg engine_asan_enabled` they use `ucontext` and tell AddressSanitizer about the stack
//! switches. A thread runs under its scheduling group and is resumed by the reactor as a
//! task.

use crate::engine::util::noncopyable_function::NoncopyableFunction;
use std::cell::{Cell, UnsafeCell};
#[cfg(any(engine_asan_enabled, engine_thread_stack_guards))]
use std::ffi::c_int;
use std::ffi::c_void;
use std::ptr::{self, NonNull};

use crate::engine::core::future::{
    ExceptionPtr, Future, Futurize, FuturizeT, HostBox, HostedPromise, PromiseHost,
    make_exception_ptr,
};
use crate::engine::core::preempt::need_preempt;
use crate::engine::core::scheduling::{SchedulingGroup, current_scheduling_group};
use crate::engine::core::task::{Task, TaskBase, TaskPtr, schedule};
#[cfg(engine_asan_enabled)]
use crate::engine::core::thread_impl::UContext;
use crate::engine::core::thread_impl::{JmpBufLink, g_current_context, set_g_current_context};

unsafe extern "C" {
    #[cfg(engine_asan_enabled)]
    fn getcontext(ucp: *mut UContext) -> c_int;
    #[cfg(engine_asan_enabled)]
    fn makecontext(ucp: *mut UContext, func: extern "C" fn(c_int, c_int), argc: c_int, ...);
    #[cfg(engine_asan_enabled)]
    fn setcontext(ucp: *const UContext) -> c_int;
    #[cfg(engine_asan_enabled)]
    fn swapcontext(oucp: *mut UContext, ucp: *const UContext) -> c_int;
    fn aligned_alloc(alignment: usize, size: usize) -> *mut c_void;
    fn free(ptr: *mut c_void);
    #[cfg(engine_thread_stack_guards)]
    fn mprotect(addr: *mut c_void, len: usize, prot: c_int) -> c_int;
    #[cfg(engine_thread_stack_guards)]
    fn getpagesize() -> c_int;
}

#[cfg(engine_thread_stack_guards)]
const PROT_READ: c_int = 1;
#[cfg(engine_thread_stack_guards)]
const PROT_WRITE: c_int = 2;

thread_local! {
    /// The link of the main (non-thread) context of this OS thread.
    static G_UNTHREADED_CONTEXT: UnsafeCell<JmpBufLink> = const { UnsafeCell::new(JmpBufLink::new()) };
}

/// Returns the link of the main (non-thread) context of the calling OS thread.
#[inline]
pub fn g_unthreaded_context() -> *mut JmpBufLink {
    G_UNTHREADED_CONTEXT.with(UnsafeCell::get)
}

#[cfg(engine_asan_enabled)]
mod asan {
    use super::*;

    // AddressSanitizer provides two functions as a means of informing it that user context
    // switch has happened. First __sanitizer_start_switch_fiber() needs to be
    // called with a place to store the fake stack pointer and the new stack
    // information as arguments. Then, ucontext switch may be performed after which
    // __sanitizer_finish_switch_fiber() needs to be called with a pointer to the
    // current context fake stack and a place to store stack information of the
    // previous ucontext.
    #[cfg(engine_have_asan_fiber_support)]
    unsafe extern "C" {
        pub fn __sanitizer_start_switch_fiber(
            fake_stack_save: *mut *mut c_void,
            stack_bottom: *const c_void,
            stack_size: usize,
        );
        pub fn __sanitizer_finish_switch_fiber(
            fake_stack_save: *mut c_void,
            stack_bottom_old: *mut *const c_void,
            stack_size_old: *mut usize,
        );
    }
    #[cfg(not(engine_have_asan_fiber_support))]
    #[inline]
    pub unsafe fn __sanitizer_start_switch_fiber(_: *mut *mut c_void, _: *const c_void, _: usize) {}
    #[cfg(not(engine_have_asan_fiber_support))]
    #[inline]
    pub unsafe fn __sanitizer_finish_switch_fiber(
        _: *mut c_void,
        _: *mut *const c_void,
        _: *mut usize,
    ) {
    }

    thread_local! {
        pub static G_PREVIOUS_CONTEXT: Cell<*mut JmpBufLink> = const { Cell::new(ptr::null_mut()) };
    }
    #[inline]
    pub fn prev() -> *mut JmpBufLink {
        G_PREVIOUS_CONTEXT.with(Cell::get)
    }
    #[inline]
    pub fn set_prev(p: *mut JmpBufLink) {
        G_PREVIOUS_CONTEXT.with(|c| c.set(p));
    }
}

#[cfg(engine_asan_enabled)]
impl JmpBufLink {
    /// Switches into a new thread for the first time, saving the current context as its link.
    ///
    /// # Safety
    /// `initial_context` must be a context prepared with `makecontext`.
    pub unsafe fn initial_switch_in(
        this: *mut Self,
        initial_context: *mut UContext,
        stack_bottom: *const c_void,
        stack_size: usize,
    ) {
        use asan::*;
        // SAFETY: context switching between valid contexts of this OS thread.
        unsafe {
            let prev = g_current_context();
            set_g_current_context(this);
            (*this).link = prev;
            set_prev(prev);
            __sanitizer_start_switch_fiber(&mut (*prev).fake_stack, stack_bottom, stack_size);
            swapcontext(&mut (*prev).context, initial_context);
            __sanitizer_finish_switch_fiber(
                (*g_current_context()).fake_stack,
                &mut (*asan::prev()).stack_bottom,
                &mut (*asan::prev()).stack_size,
            );
        }
    }

    /// Switches into this context, saving the current one as its link.
    ///
    /// # Safety
    /// `this` must hold a context saved by `switch_out`.
    pub unsafe fn switch_in(this: *mut Self) {
        use asan::*;
        // SAFETY: as above.
        unsafe {
            let prev = g_current_context();
            set_g_current_context(this);
            (*this).link = prev;
            set_prev(prev);
            __sanitizer_start_switch_fiber(
                &mut (*prev).fake_stack,
                (*this).stack_bottom,
                (*this).stack_size,
            );
            swapcontext(&mut (*prev).context, &(*this).context);
            __sanitizer_finish_switch_fiber(
                (*g_current_context()).fake_stack,
                &mut (*asan::prev()).stack_bottom,
                &mut (*asan::prev()).stack_size,
            );
        }
    }

    /// Switches out of this context, back to its link.
    ///
    /// # Safety
    /// Must be called from the thread whose link this is.
    pub unsafe fn switch_out(this: *mut Self) {
        use asan::*;
        // SAFETY: as above.
        unsafe {
            set_g_current_context((*this).link);
            set_prev(this);
            let cur = g_current_context();
            __sanitizer_start_switch_fiber(
                &mut (*this).fake_stack,
                (*cur).stack_bottom,
                (*cur).stack_size,
            );
            swapcontext(&mut (*this).context, &(*cur).context);
            __sanitizer_finish_switch_fiber(
                (*g_current_context()).fake_stack,
                &mut (*asan::prev()).stack_bottom,
                &mut (*asan::prev()).stack_size,
            );
        }
    }

    /// Runs on the new thread right after its first switch in.
    pub fn initial_switch_in_completed(_this: *mut Self) {
        // This is a new thread and it doesn't have the fake stack yet. The sanitizer will
        // create it lazily, for now just pass null.
        // SAFETY: g_previous_context is the context that switched into this thread.
        unsafe {
            asan::__sanitizer_finish_switch_fiber(
                ptr::null_mut(),
                &mut (*asan::prev()).stack_bottom,
                &mut (*asan::prev()).stack_size,
            );
        }
    }

    /// Switches out of a finished thread for the last time; never returns.
    ///
    /// # Safety
    /// Must be the last thing a thread does.
    pub unsafe fn final_switch_out(this: *mut Self) -> ! {
        use asan::*;
        // SAFETY: as above.
        unsafe {
            set_g_current_context((*this).link);
            set_prev(this);
            let cur = g_current_context();
            // Since the thread is about to die we pass null as fake_stack_save argument
            // so that the sanitizer knows it can destroy the fake stack if it exists.
            __sanitizer_start_switch_fiber(ptr::null_mut(), (*cur).stack_bottom, (*cur).stack_size);
            setcontext(&(*cur).context);
            std::hint::unreachable_unchecked()
        }
    }
}

/// The context switch of builds without AddressSanitizer, in place of `setjmp`/`longjmp`
/// (which return twice, and Rust cannot mark a call as `returns_twice`). The callee-saved
/// registers of the running context are pushed on its own stack, its stack pointer is
/// stored, and the other context's registers are popped
/// from its stack; `ret` continues where that context switched out (or, the first time,
/// in `trampoline`).
#[cfg(not(engine_asan_enabled))]
mod context_switch {
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    compile_error!(
        "engine threads need a context switch for this architecture (x86_64 and aarch64 have one)"
    );

    /// The function a new context starts in: it receives the argument stored in its
    /// initial frame and never returns.
    pub type Entry = unsafe extern "C" fn(*mut u8) -> !;

    /// Saves the running context, storing its stack pointer in `*from_sp`, and resumes
    /// the context whose stack pointer is `to_sp`.
    ///
    /// # Safety
    /// `to_sp` must be a stack pointer stored by this function, or built by
    /// [`init_stack`], for a context that is not running.
    #[cfg(target_arch = "x86_64")]
    #[unsafe(naked)]
    pub unsafe extern "C" fn switch(from_sp: *mut *mut u8, to_sp: *mut u8) {
        // System V: rbx, rbp, r12-r15 are callee-saved, and so are the MXCSR control
        // bits and the x87 control word.
        core::arch::naked_asm!(
            "push rbp",
            "push rbx",
            "push r12",
            "push r13",
            "push r14",
            "push r15",
            "sub rsp, 8",
            "stmxcsr dword ptr [rsp]",
            "fnstcw word ptr [rsp + 4]",
            "mov qword ptr [rdi], rsp",
            "mov rsp, rsi",
            "ldmxcsr dword ptr [rsp]",
            "fldcw word ptr [rsp + 4]",
            "add rsp, 8",
            "pop r15",
            "pop r14",
            "pop r13",
            "pop r12",
            "pop rbx",
            "pop rbp",
            "ret",
        )
    }

    /// The first code a new context runs: calls the entry (`rbx`) with its argument
    /// (`r12`), both popped from the initial frame.
    #[cfg(target_arch = "x86_64")]
    #[unsafe(naked)]
    unsafe extern "C" fn trampoline() {
        core::arch::naked_asm!("mov rdi, r12", "call rbx", "ud2")
    }

    /// Size of the frame `switch` keeps on a suspended stack.
    #[cfg(target_arch = "x86_64")]
    const FRAME: usize = 64;

    /// Writes the initial frame of a new context at the top of `stack_top` (16-byte
    /// aligned) and returns its stack pointer.
    ///
    /// # Safety
    /// `stack_top` must be the 16-byte aligned end of a writable stack of at least 4 KiB.
    #[cfg(target_arch = "x86_64")]
    pub unsafe fn init_stack(stack_top: *mut u8, entry: Entry, arg: *mut u8) -> *mut u8 {
        // SAFETY: guaranteed by the caller; the frame is the layout `switch` pops:
        // [mxcsr, x87 cw] r15 r14 r13 r12 rbx rbp, then the return address. After the
        // `ret` the stack pointer is `stack_top`, 16-byte aligned, as `call` needs.
        unsafe {
            let sp = stack_top.sub(FRAME).cast::<u64>();
            sp.write(0x1F80 | (0x037F << 32)); // default MXCSR, default x87 control word
            sp.add(1).write(0); // r15
            sp.add(2).write(0); // r14
            sp.add(3).write(0); // r13
            sp.add(4).write(arg as u64); // r12
            sp.add(5).write(entry as usize as u64); // rbx
            sp.add(6).write(0); // rbp: end of the frame chain
            sp.add(7).write(trampoline as *const () as usize as u64); // return address
            sp.cast::<u8>()
        }
    }

    /// Saves the running context, storing its stack pointer in `*from_sp`, and resumes
    /// the context whose stack pointer is `to_sp`.
    ///
    /// # Safety
    /// As the x86_64 version.
    #[cfg(target_arch = "aarch64")]
    #[unsafe(naked)]
    pub unsafe extern "C" fn switch(from_sp: *mut *mut u8, to_sp: *mut u8) {
        // AAPCS64: x19-x28, the frame pointer x29, the link register x30 and the low
        // halves of v8-v15 (d8-d15) are callee-saved; x18 is the platform register and is
        // left alone.
        core::arch::naked_asm!(
            "sub sp, sp, #0xb0",
            "stp d8, d9, [sp, #0x00]",
            "stp d10, d11, [sp, #0x10]",
            "stp d12, d13, [sp, #0x20]",
            "stp d14, d15, [sp, #0x30]",
            "stp x19, x20, [sp, #0x40]",
            "stp x21, x22, [sp, #0x50]",
            "stp x23, x24, [sp, #0x60]",
            "stp x25, x26, [sp, #0x70]",
            "stp x27, x28, [sp, #0x80]",
            "stp x29, x30, [sp, #0x90]",
            "mov x9, sp",
            "str x9, [x0]",
            "mov sp, x1",
            "ldp d8, d9, [sp, #0x00]",
            "ldp d10, d11, [sp, #0x10]",
            "ldp d12, d13, [sp, #0x20]",
            "ldp d14, d15, [sp, #0x30]",
            "ldp x19, x20, [sp, #0x40]",
            "ldp x21, x22, [sp, #0x50]",
            "ldp x23, x24, [sp, #0x60]",
            "ldp x25, x26, [sp, #0x70]",
            "ldp x27, x28, [sp, #0x80]",
            "ldp x29, x30, [sp, #0x90]",
            "add sp, sp, #0xb0",
            "ret",
        )
    }

    /// The first code a new context runs: calls the entry (`x20`) with its argument
    /// (`x19`), both loaded from the initial frame.
    #[cfg(target_arch = "aarch64")]
    #[unsafe(naked)]
    unsafe extern "C" fn trampoline() {
        core::arch::naked_asm!("mov x0, x19", "blr x20", "brk #0x1")
    }

    /// Size of the frame `switch` keeps on a suspended stack.
    #[cfg(target_arch = "aarch64")]
    const FRAME: usize = 0xb0;

    /// Writes the initial frame of a new context at the top of `stack_top` (16-byte
    /// aligned) and returns its stack pointer.
    ///
    /// # Safety
    /// As the x86_64 version.
    #[cfg(target_arch = "aarch64")]
    pub unsafe fn init_stack(stack_top: *mut u8, entry: Entry, arg: *mut u8) -> *mut u8 {
        // SAFETY: guaranteed by the caller; the frame is the layout `switch` loads (all
        // registers zero but x19 = arg, x20 = entry, x30 = trampoline). After it the
        // stack pointer is `stack_top`, 16-byte aligned.
        unsafe {
            let sp = stack_top.sub(FRAME);
            ptr::write_bytes(sp, 0, FRAME);
            let w = sp.cast::<u64>();
            w.add(0x40 / 8).write(arg as u64); // x19
            w.add(0x48 / 8).write(entry as usize as u64); // x20
            w.add(0x98 / 8).write(trampoline as *const () as usize as u64); // x30
            sp
        }
    }

    #[cfg(target_arch = "aarch64")]
    use std::ptr;
}

#[cfg(not(engine_asan_enabled))]
impl JmpBufLink {
    /// Switches into a new thread for the first time, saving the current context as its link;
    /// its initial context is the stack pointer built by `init_stack`.
    ///
    /// # Safety
    /// `this` must be the link of a new thread whose `sp` is its initial frame; no
    /// reference to either link may be live.
    #[inline]
    pub unsafe fn initial_switch_in(this: *mut Self) {
        // SAFETY: guaranteed by the caller.
        unsafe { Self::switch_in(this) };
    }

    /// Switches into this context, saving the current one as its link.
    ///
    /// # Safety
    /// `this` must hold a context saved by `switch_out` (or an initial frame); no
    /// reference to either link may be live.
    #[inline]
    pub unsafe fn switch_in(this: *mut Self) {
        // SAFETY: guaranteed by the caller; the running context is `prev`, whose stack
        // pointer is saved for its own switch_in.
        unsafe {
            let prev = g_current_context();
            set_g_current_context(this);
            (*this).link = prev;
            context_switch::switch(ptr::addr_of_mut!((*prev).sp), (*this).sp);
        }
    }

    /// Switches out of this context, back to its link.
    ///
    /// # Safety
    /// Must be called from the thread whose link this is; no reference to either link
    /// may be live.
    #[inline]
    pub unsafe fn switch_out(this: *mut Self) {
        // SAFETY: guaranteed by the caller; the context switched back to is the one that
        // switched into this thread.
        unsafe {
            let to = (*this).link;
            set_g_current_context(to);
            context_switch::switch(ptr::addr_of_mut!((*this).sp), (*to).sp);
        }
    }

    /// Runs on the new thread right after its first switch in.
    #[inline]
    pub fn initial_switch_in_completed(_this: *mut Self) {}

    /// Switches out of a finished thread for the last time; never returns.
    ///
    /// # Safety
    /// Must be the last thing a thread does.
    #[inline]
    pub unsafe fn final_switch_out(this: *mut Self) -> ! {
        // SAFETY: as in switch_out; this context is never resumed.
        unsafe {
            Self::switch_out(this);
            std::hint::unreachable_unchecked()
        }
    }
}

// Both AddressSanitizer and optimizations can increase the stack used by a
// function. When both are used, we need more than 128 KiB.
#[cfg(engine_asan_enabled)]
const BASE_STACK_SIZE: usize = 256 * 1024;
#[cfg(not(engine_asan_enabled))]
const BASE_STACK_SIZE: usize = 128 * 1024;

fn get_stack_size(attr: &ThreadAttributes) -> usize {
    #[cfg(all(not(debug_assertions), engine_asan_enabled))]
    {
        BASE_STACK_SIZE.max(attr.stack_size)
    }
    #[cfg(not(all(not(debug_assertions), engine_asan_enabled)))]
    {
        if attr.stack_size != 0 { attr.stack_size } else { BASE_STACK_SIZE }
    }
}

/// Attributes controlling the behavior of a thread.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadAttributes {
    /// The scheduling group the thread runs in (the current one if unset).
    pub sched_group: Option<SchedulingGroup>,
    /// Stack size in bytes; 0 selects the default (128 KiB, 256 KiB with AddressSanitizer).
    pub stack_size: usize,
}

/// The owned stack memory of a thread, freed on drop.
struct StackHolder {
    ptr: NonNull<u8>,
    /// The id of the stack registration with Valgrind.
    valgrind_id: i32,
}

impl StackHolder {
    #[inline]
    fn get(&self) -> *mut u8 {
        self.ptr.as_ptr()
    }
}

impl Drop for StackHolder {
    /// Deregisters the stack and frees it.
    fn drop(&mut self) {
        valgrind_stack_deregister(self.valgrind_id);
        // SAFETY: the stack was allocated with aligned_alloc in make_stack.
        unsafe { free(self.ptr.as_ptr() as *mut c_void) };
    }
}

/// Registers a stack with Valgrind. The client request is not issued, which is what it
/// amounts to outside Valgrind: the id is always 0.
#[inline]
fn valgrind_stack_register(_start: *mut u8, _end: *mut u8) -> i32 {
    0
}

/// Deregisters a stack from Valgrind; no effect (see [`valgrind_stack_register`]).
#[inline]
fn valgrind_stack_deregister(_id: i32) {}

/// The function a thread runs; returning `Err` fails the thread. Small closures (such as
/// the one capturing a pointer in [`async_with_attributes`]) are stored inline, without an
/// allocation.
pub type ThreadFunc = NoncopyableFunction<fn() -> Result<(), ExceptionPtr>>;

/// A [`ThreadFunc`] that runs `func` (which the thread calls once).
#[inline]
pub fn make_thread_func<F>(func: F) -> ThreadFunc
where
    F: FnOnce() -> Result<(), ExceptionPtr> + 'static,
{
    // noncopyable_function calls through `operator()`, so the one-shot closure is kept
    // in an Option (no extra space for a closure that holds a reference or a box).
    let mut func = Some(func);
    ThreadFunc::new(move || match func.take() {
        Some(f) => f(),
        None => Ok(()),
    })
}

/// The state of an engine thread, kept apart from [`Thread`] because a `Thread` is movable
/// and pointers to this state are captured. It is also the task the reactor runs to resume
/// the thread.
///
/// Its code runs on two stacks (the thread's and the reactor's), so every member that is
/// used across a context switch takes the context as a raw pointer: no reference to it
/// stays live in a suspended frame while the other stack changes it.
#[repr(C)]
pub struct ThreadContext {
    // First field: the task header (see `Task`).
    task: TaskBase,
    stack: StackHolder,
    func: ThreadFunc,
    context: JmpBufLink,
    done: HostedPromise<()>,
    joined: bool,
    /// The function has returned and `done` is set: the thread will not run on its stack
    /// again (it switches out for the last time right after setting it).
    finished: bool,
    /// The [`Thread`] that owned the context was dropped while the thread still ran: the
    /// context frees itself once the thread has switched out for the last time.
    orphaned: bool,
    /// Links in the per-OS-thread list of all thread contexts.
    all_prev: *mut ThreadContext,
    all_next: *mut ThreadContext,
}

// SAFETY: hosted_promise returns the `done` field, derived from `this`.
unsafe impl PromiseHost for ThreadContext {
    type Value = ();
    #[inline]
    unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<()> {
        // SAFETY: guaranteed by the caller.
        unsafe { ptr::addr_of_mut!((*this).done) }
    }
}

thread_local! {
    /// The head of the list of all thread contexts of this OS thread.
    static ALL_THREADS: Cell<*mut ThreadContext> = const { Cell::new(ptr::null_mut()) };
}

impl ThreadContext {
    /// Creates the context of a thread running `func` with the attributes `attr`.
    ///
    /// The thread starts running at once (until its first blocking point). The context
    /// is owned by a [`HostBox`], so the node of its `done` promise is embedded.
    ///
    /// # Errors
    /// Fails if the stack cannot be allocated.
    pub fn new(
        attr: ThreadAttributes,
        func: ThreadFunc,
    ) -> Result<HostBox<ThreadContext>, ExceptionPtr> {
        let stack_size = get_stack_size(&attr);
        let ctx = HostBox::new(ThreadContext {
            task: TaskBase::new::<Self>(attr.sched_group.unwrap_or_else(current_scheduling_group)),
            stack: Self::make_stack(stack_size)?,
            func,
            context: JmpBufLink::new(),
            done: HostedPromise::new(),
            joined: false,
            finished: false,
            orphaned: false,
            all_prev: ptr::null_mut(),
            all_next: ptr::null_mut(),
        });
        let p = ctx.as_ptr();
        // SAFETY: the context is live, boxed and reached through `p` only while the new
        // thread runs.
        unsafe {
            Self::setup(p, stack_size)?;
            Self::all_threads_push_front(p);
        }
        Ok(ctx)
    }

    /// Returns the task waiting for the thread to finish.
    ///
    /// # Safety
    /// `this` must point to a live context.
    #[inline]
    pub(crate) unsafe fn waiting_task_of(this: *mut Self) -> Option<TaskPtr> {
        // SAFETY: guaranteed by the caller.
        unsafe { HostedPromise::waiting_task(ptr::addr_of!((*this).done)) }
    }

    /// # Safety
    /// `this` must point to a live context of this OS thread.
    unsafe fn all_threads_push_front(this: *mut Self) {
        ALL_THREADS.with(|head| {
            let h = head.get();
            // SAFETY: list members are live thread contexts of this OS thread.
            unsafe {
                (*this).all_next = h;
                (*this).all_prev = ptr::null_mut();
                if !h.is_null() {
                    (*h).all_prev = this;
                }
            }
            head.set(this);
        });
    }

    fn all_threads_erase(&mut self) {
        let (prev, next) = (self.all_prev, self.all_next);
        // SAFETY: list members are live thread contexts of this OS thread, other than
        // this one.
        unsafe {
            if !next.is_null() {
                (*next).all_prev = prev;
            }
            if prev.is_null() {
                ALL_THREADS.with(|head| head.set(next));
            } else {
                (*prev).all_next = next;
            }
        }
    }

    fn make_stack(stack_size: usize) -> Result<StackHolder, ExceptionPtr> {
        #[cfg(engine_thread_stack_guards)]
        // SAFETY: plain syscall wrapper.
        let page_size = unsafe { getpagesize() } as usize;
        #[cfg(engine_thread_stack_guards)]
        let alignment = page_size;
        #[cfg(not(engine_thread_stack_guards))]
        let alignment = 16; // ABI requirement on x86_64
        // SAFETY: aligned_alloc with a power-of-two alignment.
        let mem = unsafe { aligned_alloc(alignment, stack_size) } as *mut u8;
        let Some(ptr) = NonNull::new(mem) else {
            return Err(make_exception_ptr(std::io::Error::from(std::io::ErrorKind::OutOfMemory)));
        };
        // SAFETY: mem is a block of stack_size bytes.
        let valgrind_id = valgrind_stack_register(mem, unsafe { mem.add(stack_size) });
        let stack = StackHolder { ptr, valgrind_id };
        #[cfg(engine_asan_enabled)]
        // Avoid an AddressSanitizer false positive due to garbage on the stack
        // SAFETY: the block is stack_size bytes long.
        unsafe {
            ptr::write_bytes(stack.get(), 0, stack_size)
        };

        #[cfg(engine_thread_stack_guards)]
        {
            // SAFETY: the first page of the block belongs to us.
            let mp_status = unsafe { mprotect(stack.get() as *mut c_void, page_size, PROT_READ) };
            if mp_status != 0 {
                return Err(make_exception_ptr(std::io::Error::last_os_error()));
            }
        }

        Ok(stack)
    }

    /// Prepares the thread's stack and runs it until it first switches out.
    ///
    /// # Safety
    /// `this` must point to a live, boxed context, with no reference to it live.
    #[cfg(not(engine_asan_enabled))]
    unsafe fn setup(this: *mut Self, stack_size: usize) -> Result<(), ExceptionPtr> {
        // The initial frame is written by hand and every switch is the assembly one.
        // SAFETY: guaranteed by the caller; the stack is ours, stack_size bytes long and
        // 16-byte aligned (make_stack), and the new context runs s_main with `this`.
        unsafe {
            let top = (*this).stack.get().add(stack_size & !15);
            let link = ptr::addr_of_mut!((*this).context);
            (*link).sp = context_switch::init_stack(top, Self::s_main, this.cast::<u8>());
            (*link).thread = this;
            JmpBufLink::initial_switch_in(link);
        }
        Ok(())
    }

    /// Prepares the thread's stack with `makecontext` (AddressSanitizer builds) and runs it
    /// until it first switches out.
    ///
    /// # Safety
    /// As the other `setup`.
    #[cfg(engine_asan_enabled)]
    unsafe fn setup(this: *mut Self, stack_size: usize) -> Result<(), ExceptionPtr> {
        // use setcontext() for the initial jump, as it allows us
        // to set up a stack, but continue with longjmp() as it's
        // much faster.
        let mut initial_context = UContext::zeroed();
        let q = this as usize as u64;
        // SAFETY: getcontext/makecontext on a context we own, with a stack we own; the
        // new context runs s_main_asan with this (address-stable, boxed) object.
        unsafe {
            let r = getcontext(&mut initial_context);
            if r == -1 {
                return Err(make_exception_ptr(std::io::Error::last_os_error()));
            }
            initial_context.uc_stack.ss_sp = (*this).stack.get() as *mut c_void;
            initial_context.uc_stack.ss_size = stack_size;
            initial_context.uc_link = ptr::null_mut();
            makecontext(
                &mut initial_context,
                Self::s_main_asan,
                2,
                q as u32 as c_int,
                (q >> 32) as u32 as c_int,
            );
            let link = ptr::addr_of_mut!((*this).context);
            (*link).thread = this;
            let bottom = (*this).stack.get() as *const c_void;
            JmpBufLink::initial_switch_in(link, &mut initial_context, bottom, stack_size);
        }
        Ok(())
    }

    /// Switches into this context, saving the current one as its link.
    ///
    /// # Safety
    /// `this` must point to a live context suspended by `switch_out`, with no reference
    /// to it live.
    pub unsafe fn switch_in(this: *mut Self) {
        // SAFETY: local_engine() is this shard's live reactor; no reference to it is live
        // here (the reactor is running this task through raw pointers).
        unsafe { (*crate::engine::core::reactor::local_engine())._current_task = None }; // thread_wake_task is on the stack and will be invalid when we resume
        // SAFETY: guaranteed by the caller; the context was saved by switch_out().
        unsafe { JmpBufLink::switch_in(ptr::addr_of_mut!((*this).context)) };
    }

    /// Switches out of this context, back to its link.
    ///
    /// # Safety
    /// `this` must be the running thread, with no reference to it live.
    pub unsafe fn switch_out(this: *mut Self) {
        // SAFETY: guaranteed by the caller.
        unsafe { JmpBufLink::switch_out(ptr::addr_of_mut!((*this).context)) };
    }

    /// Returns whether the thread should yield: preemption was requested.
    #[inline]
    pub fn should_yield(&self) -> bool {
        need_preempt()
    }

    /// Returns the context as a task pointer.
    ///
    /// # Safety
    /// `this` must point to a live context.
    #[inline]
    unsafe fn task_ptr(this: *mut Self) -> TaskPtr {
        // SAFETY: guaranteed by the caller (so non-null).
        TaskPtr::from_task(unsafe { NonNull::new_unchecked(this) })
    }

    /// Reschedules the thread at the back of its scheduling group's queue and switches out.
    ///
    /// # Safety
    /// As [`ThreadContext::switch_out`].
    pub unsafe fn yield_(this: *mut Self) {
        // SAFETY: guaranteed by the caller.
        unsafe {
            schedule(Self::task_ptr(this));
            Self::switch_out(this);
        }
    }

    /// Schedules the thread to be resumed by the reactor.
    ///
    /// # Safety
    /// `this` must point to a live context.
    pub unsafe fn reschedule(this: *mut Self) {
        // SAFETY: guaranteed by the caller.
        schedule(unsafe { Self::task_ptr(this) });
    }

    /// The entry of the new context; the argument is the context (`init_stack` passes a
    /// pointer, so there is no need to split it in two `int`s as `makecontext` requires).
    #[cfg(not(engine_asan_enabled))]
    unsafe extern "C" fn s_main(q: *mut u8) -> ! {
        // SAFETY: q is the context passed by setup().
        unsafe { Self::main(q.cast::<Self>()) }
    }

    // all parameters MUST be 'int' for makecontext
    #[cfg(engine_asan_enabled)]
    extern "C" fn s_main_asan(lo: c_int, hi: c_int) {
        let q = (lo as u32 as u64) | ((hi as u32 as u64) << 32);
        // SAFETY: q is the address passed by setup().
        unsafe { Self::main(q as usize as *mut ThreadContext) }
    }

    /// Runs the thread's function, resolves its `done` promise, and switches out for the
    /// last time.
    ///
    /// # Safety
    /// `this` must be the live context of the running thread.
    unsafe fn main(this: *mut Self) -> ! {
        // There is no caller of main() in this context. We need to annotate this frame like this so that
        // unwinders don't try to trace back past this frame.
        // SAFETY: a CFI annotation, no code.
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::asm!(".cfi_undefined rip")
        };
        // SAFETY: as above.
        #[cfg(target_arch = "powerpc64")]
        unsafe {
            std::arch::asm!(".cfi_undefined lr")
        };
        // SAFETY: as above.
        #[cfg(target_arch = "aarch64")]
        unsafe {
            std::arch::asm!(".cfi_undefined x30")
        };
        // SAFETY: as above.
        #[cfg(target_arch = "s390x")]
        unsafe {
            std::arch::asm!(".cfi_undefined %r14")
        };
        // On other targets backtraces from engine threads may be broken.
        // SAFETY: guaranteed by the caller; the fields are reached through `this`, and no
        // reference to the context is live across a switch.
        unsafe {
            let link = ptr::addr_of_mut!((*this).context);
            JmpBufLink::initial_switch_in_completed(link);
            if (*this).task.group() != current_scheduling_group() {
                Self::yield_(this);
            }
            let mut func =
                std::mem::replace(&mut *ptr::addr_of_mut!((*this).func), ThreadFunc::empty());
            let done = ptr::addr_of_mut!((*this).done);
            // The `try { _func(); } catch (...) { _done.set_exception(...); }` of the
            // original: a panic of the function is caught here and fails the thread. It
            // must not unwind further: this frame ends the stack (its return address is
            // undefined above), so the unwinder would find no handler and abort.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| func.call()))
                .unwrap_or_else(|payload| Err(ExceptionPtr::from_panic(payload)));
            drop(func);
            match result {
                Err(e) => HostedPromise::set_exception(done, e),
                Ok(()) => HostedPromise::set_value(done, ()),
            }
            *ptr::addr_of_mut!((*this).finished) = true;
            if *ptr::addr_of!((*this).orphaned) {
                // The owner is gone and the stack is still in use: the context runs once
                // more as a task, after this last switch, and frees itself there (see
                // run_and_dispose). It was going to be done with its stack anyway, so
                // this reuses its own task header and allocates nothing.
                schedule(Self::task_ptr(this));
            }
            JmpBufLink::final_switch_out(link)
        }
    }
}

impl Drop for ThreadContext {
    fn drop(&mut self) {
        #[cfg(engine_thread_stack_guards)]
        {
            // SAFETY: getpagesize/mprotect on the first page of our stack.
            let mp_result = unsafe {
                mprotect(
                    self.stack.get() as *mut c_void,
                    getpagesize() as usize,
                    PROT_READ | PROT_WRITE,
                )
            };
            assert!(mp_result == 0);
        }
        self.all_threads_erase();
    }
}

// SAFETY: ThreadContext is repr(C) with the header built by TaskBase::new::<Self> first.
unsafe impl Task for ThreadContext {
    unsafe fn run_and_dispose(this: NonNull<Self>) {
        let p = this.as_ptr();
        // SAFETY: the reactor runs a live context. A finished one is only scheduled by
        // main() when it is orphaned, and then it has switched out for the last time: its
        // stack is unused and the context was released from its HostBox by Thread::drop,
        // so it is taken back once, here. Otherwise it is a suspended thread to resume.
        unsafe {
            if *ptr::addr_of!((*p).finished) {
                drop(HostBox::from_raw(p));
            } else {
                Self::switch_in(p);
            }
        }
    }
    #[inline]
    unsafe fn waiting_task(this: NonNull<Self>) -> Option<TaskPtr> {
        // SAFETY: the context is live.
        unsafe { Self::waiting_task_of(this.as_ptr()) }
    }
}

/// A stateful thread of execution.
///
/// Threads allow using engine APIs in a blocking manner,
/// by calling `get()` on a non-ready future.  When
/// this happens, the thread is put to sleep until the future
/// becomes ready.
#[derive(Default)]
pub struct Thread {
    context: Option<HostBox<ThreadContext>>,
}

thread_local! {
    /// The thread object of the running engine thread.
    static CURRENT: Cell<*mut Thread> = const { Cell::new(ptr::null_mut()) };
}

impl Thread {
    /// Constructs a thread running `func`, which is called immediately (until its first
    /// blocking point), with default attributes.
    ///
    /// # Errors
    /// Fails if the thread's stack cannot be allocated.
    #[inline]
    pub fn new<F>(func: F) -> Result<Self, ExceptionPtr>
    where
        F: FnOnce() -> Result<(), ExceptionPtr> + 'static,
    {
        Self::with_attributes(ThreadAttributes::default(), func)
    }

    /// Constructs a thread running `func` with the attributes `attr`; `func` is called
    /// immediately (until its first blocking point).
    ///
    /// # Errors
    /// Fails if the thread's stack cannot be allocated.
    #[inline]
    pub fn with_attributes<F>(attr: ThreadAttributes, func: F) -> Result<Self, ExceptionPtr>
    where
        F: FnOnce() -> Result<(), ExceptionPtr> + 'static,
    {
        Ok(Thread { context: Some(ThreadContext::new(attr, make_thread_func(func))?) })
    }

    /// Waits for thread execution to terminate.
    ///
    /// Returns a future that resolves when the thread's function has returned (or fails with
    /// its error), and marks the thread object as joined.
    #[inline]
    ///
    /// # Panics
    /// If the object represents no thread (default-constructed or already joined and
    /// moved from).
    pub fn join(&mut self) -> Future<()> {
        let Some(ctx) = self.context.as_ref() else {
            panic!("thread::join() requires an object that represents a thread");
        };
        let p = ctx.as_ptr();
        // SAFETY: the box owns a live context; its fields are reached through `p`.
        unsafe {
            (*p).joined = true;
            HostedPromise::get_future(ptr::addr_of_mut!((*p).done))
        }
    }

    /// Voluntarily defers execution of the current thread.
    ///
    /// Gives other threads and fibers a chance to run on the current CPU.
    /// The current thread will resume execution promptly.
    ///
    /// # Panics
    /// Panics if called outside an engine thread.
    pub fn yield_() {
        let t = crate::engine::core::thread_impl::get();
        assert!(!t.is_null(), "thread::yield() called outside a engine thread");
        // SAFETY: t is the running engine thread.
        unsafe { ThreadContext::yield_(t) };
    }

    /// Checks whether this thread ought to call [`Thread::yield_`] now.
    ///
    /// Useful where we cannot yield immediately because we need to take some cleanup action
    /// first.
    #[inline]
    pub fn should_yield() -> bool {
        need_preempt()
    }

    /// Yields if this thread ought to yield now.
    ///
    /// Useful where code does a long-running computation and does
    /// not want to hog the CPU for more than its share.
    #[inline]
    pub fn maybe_yield() {
        if Self::should_yield() {
            Self::yield_();
        }
    }

    /// Returns whether the caller runs inside an engine thread.
    #[inline]
    pub fn running_in_thread() -> bool {
        !crate::engine::core::thread_impl::get().is_null()
    }
}

impl Drop for Thread {
    /// Destroys the thread object.
    ///
    /// The thread must have been joined (see [`Thread::join`]).
    ///
    /// Dropping a joined thread that is still running does not free its stack under it:
    /// the context is released and frees itself after the thread's last switch. Dropping
    /// one that was never joined aborts the process, instead of panicking in `Drop`.
    fn drop(&mut self) {
        let Some(ctx) = self.context.take() else {
            return;
        };
        let p = ctx.as_ptr();
        // SAFETY: the box owns a live context; its fields are reached through `p` one at a
        // time and no reference to it is held. If this runs on the thread's own stack (its
        // function dropped the owning `Thread`), `finished` is still false, so the context
        // is only released, never freed under the running code.
        unsafe {
            if !*ptr::addr_of!((*p).joined) {
                // A thread must be joined before its object is dropped.
                eprintln!("teseladb::thread destroyed without join()");
                std::process::abort();
            }
            if *ptr::addr_of!((*p).finished) {
                // Done with its stack: freed now.
                drop(ctx);
            } else {
                // Still suspended: the stack must outlive it. main() sees the flag after
                // the function returns and schedules the context to free itself.
                *ptr::addr_of_mut!((*p).orphaned) = true;
                let _ = HostBox::into_raw(ctx);
            }
        }
    }
}

/// Executes a callable in an engine thread with the attributes `attr`.
///
/// Runs a block of code in a threaded context, which allows it to block (by calling `get()`
/// on futures). Returns whatever `func` returns, as a future; it fails if `func` fails or the
/// thread cannot be created. One allocation holds the function, the thread and the promise.
pub fn async_with_attributes<F, R>(attr: ThreadAttributes, func: F) -> FuturizeT<R>
where
    F: FnOnce() -> R + 'static,
    R: Futurize,
{
    struct Work<F, T: 'static> {
        func: Option<F>,
        pr: HostedPromise<T>,
        th: Thread,
    }

    // SAFETY: hosted_promise returns the `pr` field, derived from `this`.
    unsafe impl<F, T: 'static> PromiseHost for Work<F, T> {
        type Value = T;
        #[inline]
        unsafe fn hosted_promise(this: *mut Self) -> *mut HostedPromise<T> {
            // SAFETY: guaranteed by the caller.
            unsafe { ptr::addr_of_mut!((*this).pr) }
        }
    }

    // The one allocation; the promise node is embedded in it.
    let wp = HostBox::new(Work::<F, R::Output> {
        func: Some(func),
        pr: HostedPromise::new(),
        th: Thread::default(),
    });
    let w: *mut Work<F, R::Output> = wp.as_ptr();
    // SAFETY: the work object is live; its fields are reached through `w`, one at a time.
    let ret = unsafe { HostedPromise::get_future(ptr::addr_of_mut!((*w).pr)) };
    let th = Thread::with_attributes(attr, move || {
        // SAFETY: the work object is kept alive by the continuation chain below until
        // the thread has been joined; its fields are reached through `w`.
        unsafe {
            if let Some(f) = (*ptr::addr_of_mut!((*w).func)).take()
                && let Some(pr) = HostedPromise::take_writer(ptr::addr_of_mut!((*w).pr))
            {
                // futurize::invoke of the original turns a throw into a failed future;
                // here a panic of the function fails the returned future the same way.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| R::invoke(f)))
                    .unwrap_or_else(|payload| {
                        R::make_exception_future(ExceptionPtr::from_panic(payload))
                    })
                    .forward_to_base(pr);
            }
        }
        Ok(())
    });
    // SAFETY: as above.
    let th_slot = unsafe { &mut *ptr::addr_of_mut!((*w).th) };
    match th {
        Ok(th) => *th_slot = th,
        Err(e) => return R::make_exception_future(e),
    }
    th_slot.join().then(move |()| ret).finally(move || drop(wp))
}

/// Executes a callable in an engine thread with default attributes; see
/// [`async_with_attributes`].
#[inline]
pub fn async_<F, R>(func: F) -> FuturizeT<R>
where
    F: FnOnce() -> R + 'static,
    R: Futurize,
{
    async_with_attributes(ThreadAttributes::default(), func)
}
/// Thread operations used by the reactor and by [`crate::engine::core::thread_impl`].
pub mod thread_impl {
    use super::*;

    /// Yields the running engine thread (see [`ThreadContext::yield_`]).
    ///
    /// # Panics
    /// Panics if called outside an engine thread.
    pub fn yield_() {
        let t = crate::engine::core::thread_impl::get();
        assert!(!t.is_null(), "thread_impl::yield() called outside a engine thread");
        // SAFETY: t is the running engine thread.
        unsafe { ThreadContext::yield_(t) };
    }

    /// Resumes the thread `to`.
    ///
    /// # Safety
    /// `to` must be a live thread context waiting to be resumed.
    pub unsafe fn switch_in(to: *mut ThreadContext) {
        // SAFETY: guaranteed by the caller.
        unsafe { ThreadContext::switch_in(to) };
    }

    /// Suspends the running thread `from`, returning to the context that resumed it.
    ///
    /// # Safety
    /// `from` must be the running thread.
    pub unsafe fn switch_out(from: *mut ThreadContext) {
        // SAFETY: guaranteed by the caller.
        unsafe { ThreadContext::switch_out(from) };
    }

    /// Initializes the thread machinery of this OS thread: the main context becomes the
    /// current one. Must run before any engine thread is created on it.
    pub fn init() {
        let u = g_unthreaded_context();
        // SAFETY: u is the thread-local unthreaded context of this OS thread.
        unsafe {
            (*u).link = ptr::null_mut();
            (*u).thread = ptr::null_mut();
        }
        set_g_current_context(u);
    }

    /// Returns the scheduling group of `thread`.
    ///
    /// # Safety
    /// `thread` must point to a live thread context.
    pub unsafe fn sched_group(thread: *const ThreadContext) -> SchedulingGroup {
        // SAFETY: guaranteed by the caller.
        unsafe { (*thread).task.group() }
    }
}
