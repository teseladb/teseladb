#![cfg(target_os = "linux")]
//! While the failure action of the allocation failure injector runs, it is out of its slot
//! (so a nested `run_with_callback` cannot replace the function being called). A failure
//! injected inside the action itself, with no action of its own installed, fails with
//! `BadAlloc`, the default action, and not with a call to the empty slot.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use teseladb::engine::core::future::BadAlloc;
use teseladb::engine::util::alloc_failure_injector::memory::local_failure_injector;
use teseladb::engine::util::noncopyable_function::NoncopyableFunction;

#[test]
fn a_failure_inside_the_failure_action_is_a_bad_alloc() {
    let inj = local_failure_injector();
    let actions = Rc::new(Cell::new(0));
    let a = actions.clone();
    let r = catch_unwind(AssertUnwindSafe(|| {
        inj.run_with_callback(
            // The action reaches an allocation point with a failure scheduled.
            NoncopyableFunction::<fn()>::new(move || {
                a.set(a.get() + 1);
                let inj = local_failure_injector();
                inj.fail_after(0);
                inj.on_alloc_point();
            }),
            NoncopyableFunction::<fn()>::new(|| {
                let inj = local_failure_injector();
                inj.fail_after(0);
                inj.on_alloc_point();
            }),
        );
    }));
    let payload = r.expect_err("the nested failure must unwind");
    let message = payload.downcast_ref::<&str>().map(|s| s.to_string());
    assert!(payload.downcast_ref::<BadAlloc>().is_some(), "not a BadAlloc: {message:?}");
    assert_eq!(actions.get(), 1);
    assert!(inj.failed());

    // The default action is back after the run.
    inj.fail_after(0);
    let r = catch_unwind(AssertUnwindSafe(|| inj.on_alloc_point()));
    assert!(r.expect_err("a failure").downcast_ref::<BadAlloc>().is_some());
}
