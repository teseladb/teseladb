//! Low-level state of engine threads: saved contexts and the current-context pointer.
//!
//! An engine thread (see [`crate::engine::core::thread`]) is a stackful coroutine scheduled by
//! the reactor of its shard. Each context, the thread's or the shard's main one, has a
//! [`JmpBufLink`] holding its saved stack pointer and the link to the context that switched
//! into it; [`g_current_context`] is the per-OS-thread pointer to the running one. The switch
//! functions themselves live in the thread module and are re-exported here.

use std::cell::Cell;
use std::ffi::c_void;

use crate::engine::core::preempt::need_preempt;
use crate::engine::core::thread::ThreadContext;

/// Clock used for scheduling threads
pub type ThreadClock = std::time::Instant;

/// The POSIX `stack_t` (signal stack description), with the Linux layout.
#[repr(C)]
pub struct StackT {
    /// Base address of the stack.
    pub ss_sp: *mut c_void,
    /// Flags (`SS_ONSTACK`, `SS_DISABLE`).
    pub ss_flags: i32,
    /// Size of the stack in bytes.
    pub ss_size: usize,
}

/// The POSIX `ucontext_t` with the glibc layout on Linux. The leading members are common to
/// every architecture; the rest (signal mask, machine context, FPU state) is opaque and
/// sized for the largest one (aarch64: 4560 bytes in all).
#[repr(C, align(16))]
pub struct UContext {
    /// Context flags.
    pub uc_flags: u64,
    /// The context resumed when this one returns.
    pub uc_link: *mut UContext,
    /// The stack of the context.
    pub uc_stack: StackT,
    rest: [u8; 4608],
}

impl UContext {
    /// A zeroed context, to be filled by `getcontext()`.
    #[inline]
    pub const fn zeroed() -> Self {
        UContext {
            uc_flags: 0,
            uc_link: std::ptr::null_mut(),
            uc_stack: StackT { ss_sp: std::ptr::null_mut(), ss_flags: 0, ss_size: 0 },
            rest: [0; 4608],
        }
    }
}

/// The saved context of an engine thread (or of the main context) and the link to the
/// context that switched into it.
///
/// With `--cfg engine_asan_enabled` (AddressSanitizer builds) the switch uses `ucontext`
/// and the fields that tell the sanitizer about stack switches; otherwise only the saved
/// stack pointer is kept.
#[repr(C)]
pub struct JmpBufLink {
    /// The saved context (AddressSanitizer builds).
    #[cfg(engine_asan_enabled)]
    pub context: UContext,
    /// The sanitizer's fake stack of the suspended context (AddressSanitizer builds).
    #[cfg(engine_asan_enabled)]
    pub fake_stack: *mut c_void,
    /// The bottom of the context's stack (AddressSanitizer builds).
    #[cfg(engine_asan_enabled)]
    pub stack_bottom: *const c_void,
    /// The size of the context's stack (AddressSanitizer builds).
    #[cfg(engine_asan_enabled)]
    pub stack_size: usize,
    /// The saved stack pointer of the suspended context. The context switch is written in
    /// assembly (see [`crate::engine::core::thread`]): the callee-saved registers are pushed
    /// on the suspended stack and only the stack pointer is kept here (`setjmp`, which
    /// returns twice, cannot be used from Rust).
    #[cfg(not(engine_asan_enabled))]
    pub sp: *mut u8,
    /// The context that switched into this one, resumed when this one switches out.
    pub link: *mut JmpBufLink,
    /// The engine thread of this context; null for the main context.
    pub thread: *mut ThreadContext,
}

impl JmpBufLink {
    /// A link with no saved context.
    pub const fn new() -> Self {
        JmpBufLink {
            #[cfg(engine_asan_enabled)]
            context: UContext::zeroed(),
            #[cfg(engine_asan_enabled)]
            fake_stack: std::ptr::null_mut(),
            #[cfg(engine_asan_enabled)]
            stack_bottom: std::ptr::null(),
            #[cfg(engine_asan_enabled)]
            stack_size: 0,
            #[cfg(not(engine_asan_enabled))]
            sp: std::ptr::null_mut(),
            link: std::ptr::null_mut(),
            thread: std::ptr::null_mut(),
        }
    }
    // The switch operations (initial switch in, switch in, switch out, final switch out)
    // are defined in the thread module.
}

impl Default for JmpBufLink {
    fn default() -> Self {
        Self::new()
    }
}

thread_local! {
    /// The context running on this OS thread.
    pub static G_CURRENT_CONTEXT: Cell<*mut JmpBufLink> = const { Cell::new(std::ptr::null_mut()) };
}

/// Returns the context running on this OS thread (null before [`init`]).
#[inline]
pub fn g_current_context() -> *mut JmpBufLink {
    G_CURRENT_CONTEXT.with(Cell::get)
}

/// Sets the context running on this OS thread.
#[inline]
pub fn set_g_current_context(c: *mut JmpBufLink) {
    G_CURRENT_CONTEXT.with(|g| g.set(c));
}

/// Returns the running engine thread, or null outside one.
///
/// # Panics
/// If the thread machinery of this OS thread was not initialized ([`init`]).
#[inline]
pub fn get() -> *mut ThreadContext {
    let c = g_current_context();
    assert!(!c.is_null(), "thread_impl::init() has not run on this OS thread");
    // SAFETY: a non-null g_current_context points to a live link (the unthreaded context
    // or the running thread's).
    unsafe { (*c).thread }
}

/// Returns whether the running thread should yield: the reactor requested preemption.
#[inline]
pub fn should_yield() -> bool {
    need_preempt()
}

pub use crate::engine::core::thread::thread_impl::{
    init, sched_group, switch_in, switch_out, yield_,
};
