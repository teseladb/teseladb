//! A benchmark of context switches between two engine threads that hand control to each
//! other through promises. A program with its own `main` (`harness = false`).

// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::pin::Pin;
use std::time::Duration;

use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::distributed::Distributed;
use teseladb::engine::core::future::{ExceptionPtr, Future, engine_exit, make_ready_future};
use teseladb::engine::core::semaphore::Semaphore;
use teseladb::engine::core::sharded::ShardedService;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::thread::Thread;

/// The state the two threads share. It is boxed so that its address stays fixed while the
/// tester moves into its shard.
struct State {
    switches: u64,
    s1: Semaphore,
    s2: Semaphore,
    done1: bool,
    done2: bool,
}

struct ContextSwitchTester {
    state: Box<State>,
    t1: Thread,
    t2: Thread,
}

impl ContextSwitchTester {
    fn new() -> Self {
        let mut state = Box::new(State {
            switches: 0,
            s1: Semaphore::new(0),
            s2: Semaphore::new(0),
            done1: false,
            done2: false,
        });
        let p1: *mut State = &mut *state;
        let p2 = p1;
        // SAFETY: the threads are joined in measure() before the state is dropped.
        let t1 = Thread::new(move || unsafe { Self::main1(&mut *p1) }).expect("thread");
        // SAFETY: as above.
        let t2 = Thread::new(move || unsafe { Self::main2(&mut *p2) }).expect("thread");
        ContextSwitchTester { state, t1, t2 }
    }

    fn main1(s: &mut State) -> Result<(), ExceptionPtr> {
        while !s.done1 {
            s.s1.wait(1).get()?;
            s.switches += 1;
            s.s2.signal(1);
        }
        s.done2 = true;
        Ok(())
    }

    fn main2(s: &mut State) -> Result<(), ExceptionPtr> {
        while !s.done2 {
            s.s2.wait(1).get()?;
            s.switches += 1;
            s.s1.signal(1);
        }
        Ok(())
    }

    fn begin_measurement(&mut self) {
        self.state.s1.signal(1);
    }

    fn measure(&mut self) -> Future<u64> {
        self.state.done1 = true;
        let this: *mut Self = self;
        // SAFETY: the tester outlives the future that map_reduce0 waits on.
        self.t1
            .join()
            .then(move |()| unsafe { (*this).t2.join() })
            .then(move |()| make_ready_future(unsafe { (*this).state.switches }))
    }
}

impl ShardedService for ContextSwitchTester {
    fn stop(&mut self) -> Future<()> {
        make_ready_future(())
    }
}

pub(crate) fn main() {
    const TEST_TIME: Duration = Duration::from_secs(5);
    let args: Vec<String> = std::env::args().collect();
    let status = AppTemplate::new(Default::default()).run_deprecated(
        args,
        Box::new(|| {
            let dcstp: *mut Distributed<ContextSwitchTester> =
                Box::into_raw(Box::new(Distributed::new()));
            // The distributed object lives in its box, in place, until the last continuation
            // frees it after stop().
            let dcst = move || {
                // SAFETY: the box is not moved nor freed until the last continuation, and each
                // continuation uses the reference for one call only.
                unsafe { Pin::new_unchecked(&mut *dcstp) }
            };
            let _ = dcst()
                .start(ContextSwitchTester::new)
                .then(move |()| {
                    dcst().invoke_on_all(|t: &mut ContextSwitchTester| t.begin_measurement())
                })
                .then(|()| sleep(TEST_TIME))
                .then(move |()| {
                    dcst().map_reduce0(
                        |t: &mut ContextSwitchTester| t.measure(),
                        0u64,
                        |a, b| a + b,
                    )
                })
                .then(|mut switches: u64| {
                    switches /= u64::from(Smp::count());
                    println!(
                        "context switch time: {:5.1} ns",
                        TEST_TIME.as_nanos() as f64 / switches as f64
                    );
                })
                .then(move |()| dcst().stop())
                .then(move |()| {
                    // SAFETY: nothing refers to the distributed object any more.
                    drop(unsafe { Box::from_raw(dcstp) });
                    engine_exit(ExceptionPtr::default());
                });
        }),
    );
    std::process::exit(status);
}
