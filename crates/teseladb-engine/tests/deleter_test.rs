//! Tests of `Deleter::append`: a shared object reachable from two appended deleters is freed
//! only when the last one is dropped.

#![cfg(target_os = "linux")]

use std::cell::Cell;

use teseladb::engine::core::deleter::{Deleter, make_object_deleter};

thread_local! {
    /// How many test objects were dropped (per test thread, since Rust runs tests in parallel).
    static DELETIONS_CALLED: Cell<i32> = const { Cell::new(0) };
}

fn deletions_called() -> i32 {
    DELETIONS_CALLED.with(Cell::get)
}

/// A Rust move never leaves a moved-from object behind, so the drop counts each object
/// once.
struct TestObject {
    has_ref: bool,
}

impl TestObject {
    fn new() -> Self {
        TestObject { has_ref: true }
    }
}

impl Drop for TestObject {
    fn drop(&mut self) {
        if self.has_ref {
            DELETIONS_CALLED.with(|c| c.set(c.get() + 1));
        }
    }
}

#[test]
fn test_deleter_append_does_not_free_shared_object() {
    {
        let tested: Deleter;
        {
            let obj1 = TestObject::new();
            let mut del1 = make_object_deleter(obj1);
            let obj2 = TestObject::new();
            let del2 = make_object_deleter(obj2);
            del1.append(del2);
            tested = del1.share();
            let obj3 = TestObject::new();
            let del3 = make_object_deleter(obj3);
            del1.append(del3);
        }
        // since deleter tested still holds references to first two objects, last objec should be deleted
        assert!(deletions_called() == 1);
        drop(tested);
    }
    assert!(deletions_called() == 3);
}

#[test]
fn test_deleter_append_same_shared_object_twice() {
    DELETIONS_CALLED.with(|c| c.set(0));
    {
        let mut tested = Deleter::default();
        {
            let mut del1 = make_object_deleter(TestObject::new());
            let del2 = del1.share();
            tested.append(del1);
            tested.append(del2);
        }
        assert!(deletions_called() == 0);
    }
    assert!(deletions_called() == 1);
}
