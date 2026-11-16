//! Tests of `ForeignPtr`: building from shared pointers, copying, getting, releasing and
//! resetting, and destruction on the owner shard when dropped on another one.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::time::Duration;

use teseladb::engine::core::future::{ExceptionPtr, Promise, make_ready_future};
use teseladb::engine::core::internal::run_in_background::run_in_background_func as run_in_background;
use teseladb::engine::core::reactor::engine_logger;
use teseladb::engine::core::shard_id::this_shard_id;
use teseladb::engine::core::sharded::{ForeignPtr, make_foreign};
use teseladb::engine::core::shared_ptr::{make_lw_shared, make_shared};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::thread::async_;
use teseladb::engine_test_case;

engine_test_case!(make_foreign_ptr_from_lw_shared_ptr, {
    let p = make_foreign(make_lw_shared(Sstring::from("foo")));
    assert!(p.size() == 3);
    make_ready_future(())
});

engine_test_case!(make_foreign_ptr_from_shared_ptr, {
    let p = make_foreign(make_shared(Sstring::from("foo")));
    assert!(p.size() == 3);
    make_ready_future(())
});

engine_test_case!(foreign_ptr_copy_test, {
    async_(|| -> Result<(), ExceptionPtr> {
        let ptr = make_foreign(make_shared(Sstring::from("foo")));
        assert!(ptr.size() == 3);
        // SAFETY: `ptr` lives, unmodified, on this thread until get() returns the copy.
        let ptr2 = unsafe { ptr.copy() }.get()?;
        assert!(ptr2.size() == 3);
        Ok(())
    })
});

engine_test_case!(foreign_ptr_get_test, {
    let p = make_foreign(Box::new(Sstring::from("foo")));
    assert_eq!(p.get().map(|s| s as *const Sstring), Some(&*p as *const Sstring));
    make_ready_future(())
});

engine_test_case!(foreign_ptr_release_test, {
    let mut p = make_foreign(Box::new(Sstring::from("foo")));
    let raw_ptr = p.get().map(|s| s as *const Sstring);
    assert!(p.is_some());
    assert!(p.size() == 3);
    // SAFETY: the released box is used and destroyed here, on its owner shard.
    let released_p = unsafe { p.release() }.expect("released");
    assert!(!p.is_some());
    assert!(released_p.size() == 3);
    assert_eq!(raw_ptr, Some(&*released_p as *const Sstring));
    make_ready_future(())
});

engine_test_case!(foreign_ptr_reset_test, {
    let mut fp = make_foreign(Box::new(Sstring::from("foo")));
    assert!(fp.is_some());
    assert!(fp.size() == 3);

    fp.reset(Some(Box::new(Sstring::from("foobar"))));
    assert!(fp.is_some());
    assert!(fp.size() == 6);

    fp.reset(None);
    assert!(!fp.is_some());
    make_ready_future(())
});

struct Dummy {
    cpu: u32,
}

impl Dummy {
    fn new() -> Self {
        Dummy { cpu: this_shard_id() }
    }
}

impl Drop for Dummy {
    fn drop(&mut self) {
        assert_eq!(self.cpu, this_shard_id());
    }
}

engine_test_case!(foreign_ptr_cpu_test, {
    if Smp::count() == 1 {
        eprint!(
            "Skipping multi-cpu foreign_ptr tests. Run with --smp=2 to test multi-cpu delete and reset."
        );
        return make_ready_future(());
    }

    async_(|| -> Result<(), ExceptionPtr> {
        let mut p =
            Smp::submit_to(1, || make_ready_future(make_foreign(Box::new(Dummy::new())))).get()?;

        p.reset(Some(Box::new(Dummy::new())));
        Ok(())
    })
    .then(|()| {
        // Let the foreign pointer's drop take its course. RIP dummy.
        sleep(Duration::from_millis(100))
    })
});

engine_test_case!(foreign_ptr_move_assignment_test, {
    if Smp::count() == 1 {
        eprint!(
            "Skipping multi-cpu foreign_ptr tests. Run with --smp=2 to test multi-cpu delete and reset."
        );
        return make_ready_future(());
    }

    async_(|| -> Result<(), ExceptionPtr> {
        // The assignment drops the value it replaces, which is what this checks.
        #[allow(unused_assignments)]
        let mut p =
            Smp::submit_to(1, || make_ready_future(make_foreign(Box::new(Dummy::new())))).get()?;

        p = ForeignPtr::<Box<Dummy>>::default();
        drop(p);
        Ok(())
    })
    .then(|()| {
        // Let the foreign pointer's drop take its course. RIP dummy.
        sleep(Duration::from_millis(100))
    })
});

/// A value whose drop, on whatever shard it happens, resolves one of the test's promises on shard 0.
struct Deferred {
    done: *mut Vec<Promise<bool>>,
}

impl Drop for Deferred {
    fn drop(&mut self) {
        teseladb::log_info!(engine_logger(), "~deferred");
        let done = self.done as usize;
        let shard = this_shard_id();
        run_in_background(move || {
            Smp::submit_to(0, move || {
                // SAFETY: the vector lives on shard 0 until the test has read both promises.
                let done = unsafe { &mut *(done as *mut Vec<Promise<bool>>) };
                done[shard as usize].set_value(true);
                done[(shard ^ 1) as usize].set_value(false);
            })
        });
    }
}

engine_test_case!(foreign_ptr_destroy_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        if Smp::count() == 1 {
            eprint!(
                "Skipping multi-cpu foreign_ptr tests. Run with --smp=2 to test multi-cpu delete and reset."
            );
            return Ok(());
        }

        let mut done: Vec<Promise<bool>> = (0..Smp::count()).map(|_| Promise::new()).collect();

        let dp = &mut done as *mut Vec<Promise<bool>> as usize;
        let mut val = Smp::submit_to(1, move || {
            make_ready_future(make_foreign(Box::new(Deferred {
                done: dp as *mut Vec<Promise<bool>>,
            })))
        })
        .get()?;
        val.destroy().get()?;
        assert!(done[1].get_future().get()?);
        assert!(!done[0].get_future().get()?);
        Ok(())
    })
});
