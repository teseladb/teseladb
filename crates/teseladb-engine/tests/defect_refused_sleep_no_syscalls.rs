//! When the reactor is idle but a poller refuses to let it sleep, the attempt to sleep makes no
//! system call: the task quota timer (and the stall detector's timer) are only disarmed once
//! every poller has agreed to sleep. Before, each refused attempt disarmed and re-armed both
//! timers, thousands of times a second per shard, which on machines with many cores kept the
//! idle reactors busy on a kernel lock.
//!
//! The poller below refuses every sleep and, each time it is asked, checks that the task quota
//! timer of this shard (the only timerfd re-armed periodically with the task quota) is still
//! armed. Runs on one shard with the epoll backend (the program's own `main` adds
//! `--smp=1 --reactor-backend=epoll`): with io_uring, a reactor whose pollers never let it
//! sleep did not run its high resolution timers in this test, a separate defect.

#![allow(unsafe_code)]

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use teseladb::engine::core::future::ExceptionPtr;
use teseladb::engine::core::internal::poll::{Poller, Pollfn};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine_test_case;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Itimerspec {
    it_interval: Timespec,
    it_value: Timespec,
}

unsafe extern "C" {
    fn timerfd_gettime(fd: i32, curr: *mut Itimerspec) -> i32;
}

/// The default task quota, the interval of the task quota timer.
const TASK_QUOTA_NS: i64 = 500_000;

fn periodic_task_quota_timerfds() -> Vec<i32> {
    let mut fds = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd").expect("fd directory") {
        let entry = entry.expect("fd entry");
        let Ok(target) = std::fs::read_link(entry.path()) else { continue };
        if target.to_string_lossy() != "anon_inode:[timerfd]" {
            continue;
        }
        let Ok(fd) = entry.file_name().to_string_lossy().parse::<i32>() else { continue };
        let mut its = Itimerspec::default();
        // SAFETY: `its` is a live itimerspec; a closed or foreign fd only fails the call.
        if unsafe { timerfd_gettime(fd, &mut its) } == 0
            && its.it_interval.tv_sec == 0
            && its.it_interval.tv_nsec == TASK_QUOTA_NS
        {
            fds.push(fd);
        }
    }
    fds
}

fn armed(fd: i32) -> bool {
    let mut its = Itimerspec::default();
    // SAFETY: as above.
    let r = unsafe { timerfd_gettime(fd, &mut its) };
    r == 0 && (its.it_interval.tv_nsec != 0 || its.it_value.tv_sec != 0 || its.it_value.tv_nsec != 0)
}

struct RefusingPollfn {
    quota_timer: i32,
    attempts: Rc<Cell<u64>>,
    disarmed: Rc<Cell<u64>>,
}

impl Pollfn for RefusingPollfn {
    fn poll(&mut self) -> bool {
        false
    }
    fn pure_poll(&mut self) -> bool {
        false
    }
    fn try_enter_interrupt_mode(&mut self) -> bool {
        self.attempts.set(self.attempts.get() + 1);
        if !armed(self.quota_timer) {
            self.disarmed.set(self.disarmed.get() + 1);
        }
        false
    }
    fn exit_interrupt_mode(&mut self) {}
}

engine_test_case!(test_refused_sleep_keeps_the_task_quota_timer_armed, {
    thread_async(move || -> Result<(), ExceptionPtr> {
        let fds = periodic_task_quota_timerfds();
        assert_eq!(fds.len(), 1, "expected the task quota timer of the only shard: {fds:?}");
        let attempts = Rc::new(Cell::new(0u64));
        let disarmed = Rc::new(Cell::new(0u64));
        let poller = Poller::new(RefusingPollfn {
            quota_timer: fds[0],
            attempts: attempts.clone(),
            disarmed: disarmed.clone(),
        });
        // Idle for a while: the reactor tries to sleep on every empty poll after max_poll_time.
        sleep(Duration::from_millis(200)).get()?;
        drop(poller);
        eprintln!("{} refused sleeps, {} with the timer disarmed", attempts.get(), disarmed.get());
        assert!(attempts.get() > 0, "the reactor never tried to sleep");
        assert_eq!(disarmed.get(), 0, "a refused sleep disarmed the task quota timer");
        Ok(())
    })
});

pub(crate) fn main() {
    let mut args: Vec<String> = std::env::args().collect();
    args.push("--smp=1".to_string());
    args.push("--reactor-backend=epoll".to_string());
    std::process::exit(teseladb::engine::testing::entry_point::testing::entry_point(args));
}
