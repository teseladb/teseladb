//! Native stacks of their own for calls that nest deep (016 A016-3-18, 023 FR-018): a client's
//! commands run in engine threads whose stacks are small and come from the shard's memory, while
//! server-side JavaScript must reach the 51 000 nested calls of the reference router, which take
//! a stack of hundreds of MiB. [`run`] runs such a call on a stack of its own instead: address
//! space reserved with `mmap` (`MAP_NORESERVE`), which the kernel commits a page at a time as the
//! call reaches it, with an inaccessible guard below.
//!
//! Each thread (each shard) keeps a few idle stacks for the next calls, so a call makes no system
//! call once its shard has as many as it runs at once. A call that went deep leaves its pages
//! committed; when its stack goes back, the pages below the top [`KEEP`] bytes are returned to
//! the kernel (`MADV_DONTNEED`). Whether a call went that deep is read from canary words written
//! below [`KEEP`], so a shallow call makes no system call.
//!
//! A call that parks its engine thread meanwhile keeps its stack: the stacks in use are known, a
//! call made from one of them runs where it is, and another engine thread of the shard takes
//! another stack.
//!
//! The switch is a few instructions: save the frame pointer, move the stack pointer to the new
//! stack's top, call, and come back. A panic of the call is caught on the new stack and resumed on
//! the caller's, so no unwinding crosses the switch.

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::ptr;

// The same declarations as the AF_XDP module's.
unsafe extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn mprotect(addr: *mut u8, len: usize, prot: i32) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

const PROT_NONE: i32 = 0;
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_PRIVATE: i32 = 0x02;
const MAP_ANONYMOUS: i32 = 0x20;
const MAP_NORESERVE: i32 = 0x4000;
const MAP_STACK: i32 = 0x20000;
const MADV_DONTNEED: i32 = 4;

/// The inaccessible bytes below a stack: a call that overruns it faults instead of writing over
/// other memory. Whole pages of 4, 16 and 64 KiB.
pub const GUARD: usize = 64 << 10;
/// The bytes at the top of a stack that stay committed between calls: what an ordinary call uses.
pub const KEEP: usize = 1 << 20;
/// The most idle stacks a thread keeps; past them a stack that comes back is unmapped.
pub const MAX_IDLE: usize = 4;
/// The words below [`KEEP`] that tell whether a call went deeper, one every 4 KiB.
const CANARIES: usize = 16;
const CANARY_STEP: usize = 4 << 10;
const CANARY: u64 = 0x7e5e_1adb_57ac_cafe;

/// A stack of its own: `len` bytes mapped at `base`, the lowest [`GUARD`] of them inaccessible.
struct Stack {
    base: *mut u8,
    len: usize,
}

impl Stack {
    /// A stack with `usable` bytes above its guard; `None` when the address space is not there.
    fn new(usable: usize) -> Option<Stack> {
        // Whole 64 KiB, so that the top is aligned for any page size.
        let usable = usable.max(2 * KEEP).next_multiple_of(GUARD);
        let len = usable + GUARD;
        let flags = MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE | MAP_STACK;
        // SAFETY: a new anonymous mapping that nothing else refers to.
        let base = unsafe { mmap(ptr::null_mut(), len, PROT_READ | PROT_WRITE, flags, -1, 0) };
        if base.is_null() || base as usize == usize::MAX {
            return None;
        }
        let stack = Stack { base, len };
        // SAFETY: the lowest GUARD bytes of the mapping just made.
        if unsafe { mprotect(base, GUARD, PROT_NONE) } != 0 {
            return None;
        }
        stack.arm();
        Some(stack)
    }

    /// The bytes a call may use.
    fn usable(&self) -> usize {
        self.len - GUARD
    }

    /// The end of the stack, where a call starts: 64 KiB aligned.
    fn top(&self) -> *mut u8 {
        // SAFETY: one past the end of the mapping.
        unsafe { self.base.add(self.len) }
    }

    #[cfg(test)]
    fn contains(&self, addr: usize) -> bool {
        let base = self.base as usize;
        addr >= base && addr < base + self.len
    }

    /// The canary words, below the top [`KEEP`] bytes.
    fn canaries(&self) -> impl Iterator<Item = *mut u64> + '_ {
        (1..=CANARIES).map(move |k| {
            // SAFETY: KEEP + CANARIES * CANARY_STEP is far below the usable bytes (at least 2 MiB).
            unsafe { self.top().sub(KEEP + k * CANARY_STEP).cast::<u64>() }
        })
    }

    /// Writes the canaries.
    fn arm(&self) {
        for c in self.canaries() {
            // SAFETY: an aligned word of the writable part of the mapping, used by no call now.
            unsafe { c.write(CANARY) };
        }
    }

    /// Whether a call reached below the top [`KEEP`] bytes.
    fn went_deep(&self) -> bool {
        // SAFETY: as in `arm`; no call runs on the stack now.
        self.canaries().any(|c| unsafe { c.read() } != CANARY)
    }

    /// Gives the pages below the top [`KEEP`] bytes back to the kernel when a call used them.
    fn trim(&self) {
        if !self.went_deep() {
            return;
        }
        // SAFETY: the writable part of the mapping below the kept top; no call runs on it. The
        // pages read as zeros afterwards, and the canaries are written again.
        unsafe {
            madvise(self.base.add(GUARD), self.usable() - KEEP, MADV_DONTNEED);
        }
        self.arm();
    }
}

impl Drop for Stack {
    fn drop(&mut self) {
        // SAFETY: the mapping this stack made; no call runs on it.
        unsafe { munmap(self.base, self.len) };
    }
}

/// A thread's stacks: the idle ones and the address ranges of those in use.
#[derive(Default)]
struct Pool {
    idle: Vec<Stack>,
    busy: Vec<(usize, usize)>,
}

thread_local! {
    static POOL: RefCell<Pool> = RefCell::new(Pool::default());
}

/// An address on the running stack.
#[inline(never)]
fn stack_address() -> usize {
    let marker = 0u8;
    std::hint::black_box(ptr::addr_of!(marker)) as usize
}

/// Runs `call` once: with `Some(bytes)` on a stack of its own of at least `size` bytes, or with
/// `None` where the caller runs, when the caller already is on such a stack or none could be
/// made. A panic of `call` comes back here.
pub fn run(size: usize, call: &mut dyn FnMut(Option<usize>)) {
    let here = stack_address();
    let nested =
        POOL.with(|p| p.borrow().busy.iter().any(|&(base, len)| here >= base && here < base + len));
    if nested {
        call(None);
        return;
    }
    let idle = POOL.with(|p| p.borrow_mut().idle.pop()).filter(|s| s.usable() >= size);
    let Some(stack) = idle.or_else(|| Stack::new(size)) else {
        call(None);
        return;
    };
    let range = (stack.base as usize, stack.len);
    POOL.with(|p| p.borrow_mut().busy.push(range));
    let usable = stack.usable();
    let outcome = on_stack(stack.top(), &mut || call(Some(usable)));
    POOL.with(|p| {
        let mut p = p.borrow_mut();
        if let Some(i) = p.busy.iter().position(|r| *r == range) {
            p.busy.swap_remove(i);
        }
    });
    stack.trim();
    POOL.with(|p| {
        let mut p = p.borrow_mut();
        if p.idle.len() < MAX_IDLE {
            p.idle.push(stack);
        }
    });
    if let Err(panic) = outcome {
        resume_unwind(panic);
    }
}

/// The idle stacks of this thread.
#[must_use]
pub fn idle_here() -> usize {
    POOL.with(|p| p.borrow().idle.len())
}

/// Whether this code runs on a stack of [`run`].
#[must_use]
pub fn on_own_stack() -> bool {
    let here = stack_address();
    POOL.with(|p| p.borrow().busy.iter().any(|&(base, len)| here >= base && here < base + len))
}

/// The bytes left below this code when it runs on a stack of [`run`]: from here down to the guard.
/// A call made from such a stack runs where it is, and this is the room it has there.
#[must_use]
pub fn room_here() -> Option<usize> {
    let here = stack_address();
    POOL.with(|p| {
        p.borrow()
            .busy
            .iter()
            .find(|&&(base, len)| here >= base && here < base + len)
            .map(|&(base, _)| here - (base + GUARD))
    })
}

/// Runs `f` with the stack pointer at `top`, catching its panic there.
fn on_stack(top: *mut u8, f: &mut dyn FnMut()) -> std::thread::Result<()> {
    let mut outcome: std::thread::Result<()> = Ok(());
    let mut caught = || outcome = catch_unwind(AssertUnwindSafe(&mut *f));
    let mut entry: &mut dyn FnMut() = &mut caught;
    // SAFETY: `top` is the aligned end of a mapped, writable stack no other code runs on;
    // `enter` receives the pointer to `entry`, alive until the switch returns, and returns
    // normally since the panic is caught, so the switch back always runs.
    unsafe { switch_and_call(ptr::addr_of_mut!(entry).cast(), enter, top) };
    outcome
}

/// The first function on the new stack: calls the closure behind `data`.
unsafe extern "C" fn enter(data: *mut u8) {
    // SAFETY: `data` is the `&mut &mut dyn FnMut()` of `on_stack`, alive across the call.
    let f = unsafe { &mut *data.cast::<&mut dyn FnMut()>() };
    f();
}

/// Calls `entry(data)` with the stack pointer at `top`, then puts the caller's back.
///
/// # Safety
///
/// `top` must be the 16-byte aligned end of a writable stack that nothing else uses, and `entry`
/// must return.
#[cfg(target_arch = "x86_64")]
#[unsafe(naked)]
unsafe extern "C" fn switch_and_call(
    data: *mut u8,
    entry: unsafe extern "C" fn(*mut u8),
    top: *mut u8,
) {
    // System V: `data` stays in rdi for the call; rbp keeps the caller's stack pointer, and is
    // callee-saved, so it is still there when `entry` returns.
    core::arch::naked_asm!(
        "push rbp",
        "mov rbp, rsp",
        "mov rsp, rdx",
        "call rsi",
        "mov rsp, rbp",
        "pop rbp",
        "ret",
    )
}

/// As the x86-64 version.
///
/// # Safety
///
/// As the x86-64 version.
#[cfg(target_arch = "aarch64")]
#[unsafe(naked)]
unsafe extern "C" fn switch_and_call(
    data: *mut u8,
    entry: unsafe extern "C" fn(*mut u8),
    top: *mut u8,
) {
    // AAPCS64: `data` stays in x0 for the call; x29 keeps the caller's stack pointer, and is
    // callee-saved, so it is still there when `entry` returns.
    core::arch::naked_asm!(
        "stp x29, x30, [sp, #-16]!",
        "mov x29, sp",
        "mov sp, x2",
        "blr x1",
        "mov sp, x29",
        "ldp x29, x30, [sp], #16",
        "ret",
    )
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// Recurses `depth` times with a frame of about 1 KiB each, returning the depth reached.
    #[inline(never)]
    fn deep(depth: usize) -> usize {
        let frame = std::hint::black_box([u8::try_from(depth % 256).unwrap_or(0); 1024]);
        if depth == 0 {
            return usize::from(frame[0]);
        }
        1 + deep(std::hint::black_box(depth - 1))
    }

    #[test]
    fn a_call_runs_on_a_stack_of_its_own_deeper_than_its_thread_has() {
        // A thread of 256 KiB runs 64 MiB of frames on the stack of `run`.
        let t = std::thread::Builder::new()
            .stack_size(256 << 10)
            .spawn(|| {
                let mut got = None;
                run(128 << 20, &mut |size| {
                    assert!(on_own_stack());
                    got = Some((size, deep(64 << 10)));
                });
                assert!(!on_own_stack());
                got
            })
            .unwrap();
        let (size, reached) = t.join().unwrap().expect("it ran");
        assert!(size.unwrap() >= 128 << 20);
        assert_eq!(reached, 64 << 10);
    }

    #[test]
    fn a_nested_call_runs_where_it_is_and_stacks_are_kept_for_the_next() {
        let mut inner = None;
        run(4 << 20, &mut |outer| {
            assert!(outer.is_some());
            run(4 << 20, &mut |size| inner = Some(size));
        });
        assert_eq!(inner, Some(None), "a call from an own stack stays on it");
        let mut room = None;
        run(4 << 20, &mut |size| {
            let usable = size.expect("a stack of its own");
            room = room_here().map(|r| (usable, r));
        });
        let (usable, left) = room.expect("on a stack of its own");
        assert!(left < usable && left > usable - (64 << 10), "{left} of {usable}");
        assert_eq!(room_here(), None, "off the stacks of `run`");
        let idle = idle_here();
        assert!(idle >= 1);
        run(4 << 20, &mut |_| {});
        assert_eq!(idle_here(), idle, "the stack was taken again and given back");
    }

    #[test]
    fn a_panic_comes_back_to_the_caller_and_the_stack_is_given_back() {
        let r = catch_unwind(AssertUnwindSafe(|| {
            run(4 << 20, &mut |_| panic!("a call failed"));
        }));
        assert!(r.is_err());
        assert!(!on_own_stack());
        assert!(idle_here() >= 1);
    }

    #[test]
    fn a_stack_that_went_deep_is_trimmed_and_armed_again() {
        let s = Stack::new(4 << 20).expect("mapped");
        assert!(!s.went_deep());
        // SAFETY: a word of the writable part of the mapping, below the kept top.
        unsafe { s.top().sub(2 * KEEP).cast::<u64>().write(1) };
        // SAFETY: the first canary word, as a deep call would overwrite it.
        unsafe { s.top().sub(KEEP + CANARY_STEP).cast::<u64>().write(0) };
        assert!(s.went_deep());
        s.trim();
        assert!(!s.went_deep());
        // SAFETY: the word written above, whose page went back to the kernel.
        assert_eq!(unsafe { s.top().sub(2 * KEEP).cast::<u64>().read() }, 0);
        assert!(s.contains(s.top() as usize - 1));
        assert!(!s.contains(s.top() as usize));
    }
}
