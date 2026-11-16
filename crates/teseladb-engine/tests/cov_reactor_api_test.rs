//! Tests of small reactor interfaces: the scheduler clock types, IPv4 address hashing, the
//! descriptor-misuse switch, the high resolution timer signal, the task histogram and the
//! backtrace printers, and, on a running reactor, its uptime and statistics, the stop wait,
//! the idle CPU handler, the exit and destroy hooks, preemption, the current task, and a
//! signal handled once.
#![cfg(target_os = "linux")]

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::idle_cpu_handler::{IdleCpuHandler, IdleCpuHandlerResult};
use teseladb::engine::core::reactor::{
    self, BacktraceBuffer, Reactor, SchedDuration, SchedTimePoint, TaskHistogram, engine,
    hash_sockaddr_in, hrtimer_signal, is_abort_on_ebadf_enabled, libc_in_addr, libc_sockaddr_in,
    print_with_backtrace, set_abort_on_ebadf, set_idle_cpu_handler,
};
use teseladb::engine::core::signal::handle_signal;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::util::noncopyable_function::NoncopyableFunction;
use teseladb::engine_test_case;

/// A test case whose body runs in an engine thread, where it can wait on futures with `get()`.
macro_rules! thread_case {
    ($name:ident, $body:block) => {
        engine_test_case!($name, {
            thread_async(move || {
                // A failed assertion is caught on the thread's own stack and fails the test.
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || $body))
                    .map_err(ExceptionPtr::from_panic)
            })
        });
    };
}

fn r() -> &'static mut Reactor {
    // SAFETY: called on a reactor thread; the reference is used for one call at a time.
    unsafe { &mut *engine() }
}

#[test]
fn scheduler_durations_and_time_points() {
    let d = SchedDuration::from_nanos(1_500_000);
    assert_eq!(d.count(), 1_500_000);
    assert_eq!(d.as_millis(), 1);
    assert_eq!(d.to_std(), Duration::from_micros(1500));
    assert_eq!(SchedDuration::from_nanos(-5).to_std(), Duration::ZERO, "negative is zero");
    assert_eq!(SchedDuration::from_std(Duration::MAX), SchedDuration::max());
    assert_eq!(SchedDuration::max().count(), i64::MAX);
    let mut e = d + SchedDuration::from_nanos(500_000);
    assert_eq!(e.as_millis(), 2);
    e -= SchedDuration::from_nanos(1_000_000);
    e += SchedDuration::from_nanos(10);
    assert_eq!((e - SchedDuration::from_nanos(10)).count(), 1_000_000);
    assert_eq!((e * 2).count(), 2_000_020);
    assert!(SchedDuration::default() < e);

    let t0 = SchedTimePoint::now();
    std::thread::sleep(Duration::from_millis(2));
    let t1 = SchedTimePoint::now();
    assert!(t1 - t0 >= SchedDuration::from_nanos(2_000_000));
    let mut t2 = t0 + SchedDuration::from_nanos(7);
    t2 += SchedDuration::from_nanos(3);
    assert_eq!((t2 - t0).count(), 10);
    assert_eq!(t2.time_since_epoch().count() - t0.time_since_epoch().count(), 10);
}

#[test]
fn ipv4_addresses_hash_and_compare_by_address_and_port() {
    let a = libc_sockaddr_in {
        sin_family: 2,
        sin_port: 0x1234,
        sin_addr: libc_in_addr { s_addr: 0x0100_007f },
        sin_zero: [0; 8],
    };
    let mut b = a;
    b.sin_zero = [9; 8];
    assert!(reactor::sockaddr_in_eq(&a, &b), "the padding does not count");
    assert_eq!(hash_sockaddr_in(a), 0x1234 ^ 0x0100_007f);
    b.sin_port = 0x4321;
    assert!(!reactor::sockaddr_in_eq(&a, &b));
    let mut c = a;
    c.sin_addr.s_addr = 1;
    assert!(!reactor::sockaddr_in_eq(&a, &c));
}

#[test]
fn process_wide_switches_and_signals() {
    let before = is_abort_on_ebadf_enabled();
    set_abort_on_ebadf(true);
    assert!(is_abort_on_ebadf_enabled());
    set_abort_on_ebadf(false);
    assert!(!is_abort_on_ebadf_enabled());
    set_abort_on_ebadf(before);
    // The timer uses the first real-time signal, which Linux places after the 32 classic ones.
    let sig = hrtimer_signal();
    assert!((32..=64).contains(&sig), "{sig}");
}

#[test]
fn task_histograms_and_backtrace_printers_write_to_the_terminal() {
    let h = TaskHistogram::default();
    h.print();
    TaskHistogram::new().print();
    // The printers write to standard error; each mode runs to completion.
    for (oneline, immediate) in [(false, false), (true, false), (false, true), (true, true)] {
        print_with_backtrace("cov backtrace", oneline, immediate);
    }
    let mut buf = BacktraceBuffer::new(false);
    buf.reserve(64);
    buf.append("cov: ");
    buf.append_decimal(1234);
    buf.append(" ");
    buf.append_hex(0xbeef);
    buf.append_bytes(b"\n");
    buf.append_backtrace();
    buf.append_backtrace_oneline();
    buf.flush();
    let mut immediate = BacktraceBuffer::new(true);
    immediate.append("immediate\n");
    immediate.append_backtrace_oneline();
    immediate.flush();
}

thread_case!(reactor_clock_statistics_and_settings, {
    let up = r().uptime();
    assert!(up > SchedDuration::default());
    sleep(Duration::from_millis(5)).get().unwrap();
    assert!(r().uptime() - up >= SchedDuration::from_nanos(5_000_000));
    let tasks = r().get_sched_stats().tasks_processed;
    sleep(Duration::from_millis(1)).get().unwrap();
    assert!(r().get_sched_stats().tasks_processed > tasks);
    let stats = r().get_io_stats();
    let _ = (stats.aio_reads, stats.aio_writes, stats.aio_errors, stats.aio_outsizes);
    #[allow(deprecated)]
    let id = r().cpu_id();
    assert_eq!(id, 0);
    let _ = r().alien();
    // Nobody asked the reactor to stop: the wait times out.
    assert!(r().wait_for_stop(Duration::from_millis(10)).get().is_err());
    // Asking for a poll only makes the current task yield sooner.
    r().force_poll();
    sleep(Duration::from_millis(1)).get().unwrap();
    let current = r().current_task();
    r().set_current_task(current);
    assert!(reactor::internal::scheduling_group_count() >= 1);
    assert!(r().file_exists("/").get().unwrap());
    assert!(!r().file_exists("/no/such/cov/path").get().unwrap());
});

thread_case!(idle_cpu_handlers_run_while_the_shard_is_idle, {
    let calls = Rc::new(Cell::new(0u32));
    let c = calls.clone();
    set_idle_cpu_handler(IdleCpuHandler::new(move |work_waiting| {
        c.set(c.get() + 1);
        if work_waiting.call() {
            IdleCpuHandlerResult::InterruptedByHigherPriorityTask
        } else {
            IdleCpuHandlerResult::NoMoreWork
        }
    }));
    sleep(Duration::from_millis(20)).get().unwrap();
    assert!(calls.get() > 0, "the shard was idle while sleeping");
    // Back to a handler that never has work.
    r().set_idle_cpu_handler(IdleCpuHandler::new(|_| IdleCpuHandlerResult::NoMoreWork));
});

thread_case!(exit_and_destroy_hooks_are_accepted, {
    // They run when the test reactor stops, after the test.
    reactor::internal::at_exit(NoncopyableFunction::<fn() -> Future<()>>::new(|| {
        make_ready_future(())
    }));
    reactor::internal::at_destroy(|| {});
    #[allow(deprecated)]
    {
        r().at_exit(NoncopyableFunction::<fn() -> Future<()>>::new(|| make_ready_future(())));
        r().at_destroy(|| {});
    }
});

unsafe extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn getpid() -> i32;
}

const SIGUSR2: i32 = 12;

thread_case!(signals_handled_once_run_their_handler_once, {
    let calls = Rc::new(Cell::new(0u32));
    let c = calls.clone();
    handle_signal(SIGUSR2, NoncopyableFunction::<fn()>::new(move || c.set(c.get() + 1)), true)
        .unwrap();
    for _ in 0..2 {
        // SAFETY: plain libc calls.
        assert_eq!(unsafe { kill(getpid(), SIGUSR2) }, 0);
        let start = std::time::Instant::now();
        while calls.get() == 0 && start.elapsed() < Duration::from_secs(5) {
            sleep(Duration::from_millis(1)).get().unwrap();
        }
        sleep(Duration::from_millis(10)).get().unwrap();
    }
    assert_eq!(calls.get(), 1);
    // A second registration for the same signal replaces the handler.
    let again = Rc::new(Cell::new(0u32));
    let a = again.clone();
    #[allow(deprecated)]
    r().handle_signal(SIGUSR2, NoncopyableFunction::<fn()>::new(move || a.set(a.get() + 1)))
        .unwrap();
    // SAFETY: plain libc calls.
    assert_eq!(unsafe { kill(getpid(), SIGUSR2) }, 0);
    let start = std::time::Instant::now();
    while again.get() == 0 && start.elapsed() < Duration::from_secs(5) {
        sleep(Duration::from_millis(1)).get().unwrap();
    }
    assert_eq!(again.get(), 1);
    assert_eq!(calls.get(), 1);
});
