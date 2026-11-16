//! Tests of engine threads: joining, several threads over one value, `async_` (immediate,
//! nested), preemption of a busy thread, custom stack sizes and their guard page.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::time::Duration;

use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::loop_::parallel_for_each;
use teseladb::engine::core::semaphore::Semaphore;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::thread::{Thread, ThreadAttributes, async_, async_with_attributes};
use teseladb::engine_test_case;

engine_test_case!(test_thread_1, {
    do_with(Sstring::new(), |x: &mut Sstring| {
        let xp: *mut Sstring = x;
        let t1 = Box::into_raw(Box::new(
            Thread::new(move || -> Result<(), ExceptionPtr> {
                // SAFETY: do_with keeps `x` alive until the join below resolves.
                unsafe { *xp = Sstring::from("abc") };
                Ok(())
            })
            .expect("thread"),
        ));
        // SAFETY: `t1` is freed in the continuation, after the join.
        unsafe { (*t1).join() }.then(move |()| {
            // SAFETY: do_with keeps `x` alive, and the thread that wrote it has been joined.
            assert_eq!(unsafe { &*xp }, "abc");
            // SAFETY: `t1` comes from Box::into_raw above, the thread is joined, and this is its only release.
            drop(unsafe { Box::from_raw(t1) });
        })
    })
});

struct Tmp {
    threads: Vec<Thread>,
    sem1: Semaphore,
    sem2: Semaphore,
    counter: i32,
}

impl Tmp {
    fn thread_fn(&mut self) -> Result<(), ExceptionPtr> {
        self.sem1.wait(1).get()?;
        self.counter += 1;
        self.sem2.signal(1);
        Ok(())
    }
}

engine_test_case!(test_thread_2, {
    do_with(
        Tmp { threads: Vec::new(), sem1: Semaphore::new(0), sem2: Semaphore::new(0), counter: 0 },
        |x: &mut Tmp| {
            let n = 10;
            let xp: *mut Tmp = x;
            for _ in 0..n {
                // SAFETY: do_with keeps `x` alive until every thread is joined.
                x.threads.push(Thread::new(move || unsafe { (*xp).thread_fn() }).expect("thread"));
            }

            assert_eq!(x.counter, 0);
            x.sem1.signal(n as usize);
            x.sem2.wait(n as usize).then(move |()| {
                // SAFETY: as above.
                let x = unsafe { &mut *xp };
                assert_eq!(x.counter, n);
                parallel_for_each(x.threads.iter_mut(), |t: &mut Thread| t.join())
            })
        },
    )
});

fn concat(x: Sstring, y: Sstring) -> Result<Sstring, ExceptionPtr> {
    sleep(Duration::from_millis(10)).get()?;
    Ok(&x + &y)
}

engine_test_case!(test_thread_async, {
    let x = Sstring::from("x");
    let y = Sstring::from("y");
    thread_async(move || concat(x, y)).then(|xy: Sstring| {
        assert_eq!(xy, "xy");
    })
});

engine_test_case!(test_thread_async_immed, {
    async_(|| -> Result<i32, ExceptionPtr> { Ok(3) }).then(|three: i32| {
        assert_eq!(three, 3);
    })
});

engine_test_case!(test_thread_async_nested, {
    async_(|| async_(|| -> Result<i32, ExceptionPtr> { Ok(3) }).get()).then(|three: i32| {
        assert_eq!(three, 3);
    })
});

#[allow(dead_code)]
fn compute(result: &mut f32, done: &std::cell::Cell<bool>, ctr: &mut u64) {
    while !done.get() {
        for _ in 0..10000 {
            *result += 1.0 / (*result + 1.0);
            *ctr += 1;
        }
        Thread::yield_();
    }
}

#[cfg(all(engine_asan_enabled, engine_have_asan_fiber_support))]
mod asan {
    use super::*;
    use std::sync::atomic::{AtomicI32, Ordering};

    static FORCE_WRITE: AtomicI32 = AtomicI32::new(0);

    #[inline(never)]
    fn throw_exception() -> Result<(), ExceptionPtr> {
        let mut buf = [0u8; 1024];
        for b in buf.iter_mut() {
            *b = FORCE_WRITE.load(Ordering::Relaxed) as u8;
        }
        std::hint::black_box(&buf);
        Err(ExceptionPtr::new(teseladb::engine::core::future::RuntimeError("1".to_string())))
    }

    #[inline(never)]
    fn use_stack() {
        let mut buf = [0u8; 2 * 1024];
        for b in buf.iter_mut() {
            *b = FORCE_WRITE.load(Ordering::Relaxed) as u8;
        }
        std::hint::black_box(&buf);
    }

    engine_test_case!(test_asan_false_positive, {
        async_(|| {
            if throw_exception().is_err() {
                use_stack();
            }
        })
    });
}

// Two failing checks are expected, and the case passes when exactly those two fail.
engine_test_case!(abc, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut failures = 0;
        for check in [false, false] {
            if !check {
                failures += 1;
            }
        }
        assert_eq!(failures, 2);
        Ok(())
    })
});

engine_test_case!(test_thread_custom_stack_size, {
    let x = Sstring::from("x");
    let y = Sstring::from("y");
    let attr = ThreadAttributes { stack_size: 16384, ..ThreadAttributes::default() };
    async_with_attributes(attr, move || concat(x, y)).then(|xy: Sstring| {
        assert_eq!(xy, "xy");
    })
});

// The test case uses x86_64 specific signal handler info. The test
// fails with detect_stack_use_after_return=1. We could put it behind
// a command line option and fork/exec to run it after removing
// detect_stack_use_after_return=1 from the environment.
#[cfg(all(engine_thread_stack_guards, target_arch = "x86_64", not(engine_asan_enabled)))]
mod stack_guard {
    use super::*;
    use std::cell::Cell;
    use std::ffi::c_void;

    use teseladb::engine::core::future::SystemError;

    thread_local! {
        static STACK_GUARD_BYPASSED: Cell<bool> = const { Cell::new(false) };
        static DEFAULT_OLD_SIGSEGV_HANDLER: Cell<Sigaction> = const { Cell::new(Sigaction::zeroed()) };
    }

    const SIGSEGV: i32 = 11;
    const SA_SIGINFO: i32 = 4;
    const PROT_READ: i32 = 1;
    const PROT_WRITE: i32 = 2;
    /// `REG_ERR` in `gregs` of glibc x86-64.
    const REG_ERR: usize = 19;
    /// Offset of `uc_mcontext.gregs` in `ucontext_t` of glibc x86-64.
    const GREGS_OFFSET: usize = 40;
    /// Offset of `si_addr` in `siginfo_t`.
    const SI_ADDR_OFFSET: usize = 16;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Sigaction {
        sa_sigaction: usize,
        sa_mask: [u64; 16],
        sa_flags: i32,
        sa_restorer: usize,
    }

    impl Sigaction {
        const fn zeroed() -> Self {
            Sigaction { sa_sigaction: 0, sa_mask: [0; 16], sa_flags: 0, sa_restorer: 0 }
        }
    }

    unsafe extern "C" {
        fn sigaction(signum: i32, act: *const Sigaction, oldact: *mut Sigaction) -> i32;
        fn mprotect(addr: *mut c_void, len: usize, prot: i32) -> i32;
        fn getpagesize() -> i32;
    }

    fn get_mprotect_flags(ctx: *mut c_void) -> i32 {
        // SAFETY: the kernel passes a ucontext_t; REG_ERR is within its gregs.
        let err = unsafe { *((ctx as *const u8).add(GREGS_OFFSET) as *const i64).add(REG_ERR) };
        if err & 0x2 != 0 { PROT_READ | PROT_WRITE } else { PROT_READ }
    }

    fn pagealign(ptr: *mut c_void, page_size: usize) -> *mut c_void {
        let pageshift = page_size.trailing_zeros();
        (((ptr as isize) >> pageshift) << pageshift) as *mut c_void
    }

    extern "C" fn bypass_stack_guard(sig: i32, si: *mut c_void, ctx: *mut c_void) {
        assert!(sig == SIGSEGV);
        let flags = get_mprotect_flags(ctx);
        STACK_GUARD_BYPASSED.with(|b| b.set(flags & PROT_WRITE != 0));
        if !STACK_GUARD_BYPASSED.with(Cell::get) {
            return;
        }
        // SAFETY: plain syscall.
        let page_size = unsafe { getpagesize() } as usize;
        // SAFETY: `si` is a siginfo_t; si_addr is at its fixed offset.
        let addr = unsafe { *((si as *const u8).add(SI_ADDR_OFFSET) as *const *mut c_void) };
        // SAFETY: unprotects the guard page that faulted.
        let mp_result =
            unsafe { mprotect(pagealign(addr, page_size), page_size, PROT_READ | PROT_WRITE) };
        assert!(mp_result == 0);
    }

    fn probing_concat(x: Sstring, y: Sstring) -> Result<Sstring, ExceptionPtr> {
        sleep(Duration::from_millis(10)).get()?;
        // Probe the stack by writing to it in intervals of 1024,
        // until we hit a write fault. In order not to ruin anything,
        // the "write" uses data it just read from the address.
        let mem = &x as *const Sstring as *mut u8;
        for i in 0..20isize {
            // SAFETY: deliberately probes the thread stack below `x`; a fault on the
            // guard page is handled by bypass_stack_guard.
            unsafe {
                let p = mem.offset(i * -1024);
                std::ptr::write_volatile(p, std::ptr::read_volatile(p));
            }
            if STACK_GUARD_BYPASSED.with(Cell::get) {
                break;
            }
        }
        Ok(&x + &y)
    }

    // This test will fail with a regular stack size, because we only probe
    // around 10KiB of data, and the stack guard resides after 128'th KiB.
    engine_test_case!(test_thread_custom_stack_size_failure, {
        if teseladb::engine::util::valgrind::running_on_valgrind() {
            return teseladb::engine::core::future::make_ready_future(());
        }

        let x = Sstring::from("x");
        let y = Sstring::from("y");

        // Catch segmentation fault once:
        let mut sa = Sigaction::zeroed();
        sa.sa_sigaction = bypass_stack_guard as usize;
        sa.sa_flags = SA_SIGINFO;
        let mut old = Sigaction::zeroed();
        // SAFETY: valid sigaction structures.
        let ret = unsafe { sigaction(SIGSEGV, &sa, &mut old) };
        if ret != 0 {
            return teseladb::engine::core::future::make_exception_future(ExceptionPtr::new(
                SystemError::from_errno(ret),
            ));
        }
        DEFAULT_OLD_SIGSEGV_HANDLER.with(|h| h.set(old));

        let mut attr = ThreadAttributes::default();
        attr.stack_size = 16384;
        let (x2, y2) = (x.clone(), y.clone());
        async_with_attributes(attr, move || probing_concat(x, y))
            .then(|xy: Sstring| {
                assert_eq!(xy, "xy");
                assert!(STACK_GUARD_BYPASSED.with(Cell::get));
                let old = DEFAULT_OLD_SIGSEGV_HANDLER.with(Cell::get);
                // SAFETY: restores the saved handler.
                let ret = unsafe { sigaction(SIGSEGV, &old, std::ptr::null_mut()) };
                assert!(ret == 0, "{}", SystemError::from_errno(ret));
            })
            .then(move |()| {
                // The same function with a default stack will not trigger
                // a segfault, because its stack is much bigger than 10KiB
                thread_async(move || probing_concat(x2, y2)).then(|xy: Sstring| {
                    assert_eq!(xy, "xy");
                })
            })
    });
}
