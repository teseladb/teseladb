#![cfg(target_os = "linux")]
//! Tests of the error and future plumbing that the other tests leave out: identity and
//! printing of error pointers, the errors made from panics, the standard error types and the
//! file-system error with its paths, the helpers of the internal module, future states built
//! and cleared by hand, promises that fail with a built error, and continuations attached to a
//! future held by reference.

use std::any::Any;
use std::path::Path;

use teseladb::engine::core::future::internal::{
    UninitializedWrapper, all_true, get0, is_trivially_move_constructible_and_destructible,
};
use teseladb::engine::core::future::{
    BadAlloc, BadFunctionCall, BrokenPromise, CurrentExceptionFutureMarker, ExceptionFutureMarker,
    ExceptionPtr, FilesystemError, Future, FutureState, NestedException, Promise,
    ReadyFutureMarker, RuntimeError, as_ready_future, current_exception_as_future,
    make_exception_future_ref, make_ready_future,
};
use teseladb::engine::core::thread::async_ as thread_async;
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

fn runtime(msg: &str) -> ExceptionPtr {
    ExceptionPtr::new(RuntimeError(msg.into()))
}

thread_case!(error_pointers_compare_print_and_come_from_panics, {
    let a = runtime("first");
    let same = a.clone();
    assert!(a == same, "clones share the object");
    assert!(a != runtime("first"), "equal text is not the same object");
    assert!(format!("{a:?}").contains("RuntimeError"), "{a:?}");
    let null = ExceptionPtr::default();
    assert!(null.is_null());
    assert_eq!(format!("{null:?}"), "null");
    assert!(ExceptionPtr::null() == null);
    let r: Result<u32, ExceptionPtr> = a.clone().rethrow();
    assert!(r.unwrap_err() == a);

    // A panic that carried an error pointer gives it back; text payloads become runtime
    // errors; anything else is an unknown panic.
    let carried: Box<dyn Any + Send> = Box::new(a.clone());
    assert!(ExceptionPtr::from_panic(carried) == a);
    let owned: Box<dyn Any + Send> = Box::new(String::from("owned text"));
    assert!(ExceptionPtr::from_panic(owned).to_string().contains("owned text"));
    let literal: Box<dyn Any + Send> = Box::new("literal text");
    assert!(ExceptionPtr::from_panic(literal).to_string().contains("literal text"));
    let other: Box<dyn Any + Send> = Box::new(5u8);
    let unknown = ExceptionPtr::from_panic(other);
    assert!(unknown.is::<RuntimeError>());
    assert!(unknown.to_string().contains("unknown panic"));
});

thread_case!(standard_errors_describe_themselves, {
    assert_eq!(BadFunctionCall.to_string(), "call to an empty function");
    assert_eq!(BadAlloc.to_string(), "memory allocation failed");
    assert_eq!(BrokenPromise::default().to_string(), "broken promise");
    assert_eq!(BrokenPromise::new().to_string(), "broken promise");

    let two = FilesystemError::new("rename failed", Path::new("/a"), Some(Path::new("/b")), 2);
    assert_eq!(two.code(), 2);
    assert_eq!(two.as_system_error().code(), 2);
    assert_eq!(two.path1(), Path::new("/a"));
    assert_eq!(two.path2(), Some(Path::new("/b")));
    let text = two.to_string();
    assert!(text.starts_with("filesystem error: rename failed: "), "{text}");
    assert!(text.ends_with("[/a] [/b]"), "{text}");
    let one = FilesystemError::new("open failed", Path::new("/c"), None, 13);
    assert_eq!(one.path2(), None);
    assert!(one.to_string().ends_with("[/c]"));

    let nested = NestedException::new(runtime("inner"), runtime("outer"));
    assert_eq!(nested.what(), "teseladb::nested_exception");
    assert_eq!(nested.to_string(), nested.what());
});

thread_case!(internal_helpers_hold_values, {
    assert_eq!(get0(7), 7);
    let mut w = UninitializedWrapper::<String>::default();
    w.uninitialized_set("abc".to_string());
    // SAFETY: the value was set just above and is taken once.
    unsafe {
        w.uninitialized_get_mut().push('d');
        assert_eq!(w.uninitialized_get(), "abcd");
        assert_eq!(w.uninitialized_take(), "abcd");
    }
    assert!(is_trivially_move_constructible_and_destructible::<u32>());
    assert!(!is_trivially_move_constructible_and_destructible::<String>());
    assert!(all_true(&[]));
    assert!(all_true(&[true, true]));
    assert!(!all_true(&[true, false, true]));

    assert_eq!(as_ready_future(3).get().unwrap(), 3);
    let e = runtime("by reference");
    assert!(make_exception_future_ref::<u32>(&e).get().unwrap_err() == e);
    assert!(current_exception_as_future::<u32>(e.clone()).get().unwrap_err() == e);
});

thread_case!(future_states_hold_values_and_errors, {
    let pending = FutureState::<String>::new();
    assert!(pending.valid() && !pending.available());
    let ready = FutureState::ready(ReadyFutureMarker, "value".to_string());
    assert_eq!(ready.get_ref().unwrap(), "value");
    let mut ready = ready;
    assert_eq!(ready.get0().unwrap(), "value");

    let mut cleared = FutureState::ready(ReadyFutureMarker, "gone".to_string());
    cleared.clear();
    assert!(!cleared.valid());

    let mut assigned = FutureState::<u32>::new();
    assigned.assign(FutureState::ready(ReadyFutureMarker, 9));
    assert_eq!(assigned.get().unwrap(), 9);

    let e = runtime("state error");
    let failed = FutureState::<u32>::exception(ExceptionFutureMarker, e.clone());
    assert!(failed.failed());
    assert!(failed.get_ref().unwrap_err() == e);
    let mut failed = failed;
    // Clearing a failed state reports the error it drops.
    failed.clear();
    assert!(!failed.valid());

    let mut current = FutureState::<u32>::current_exception(CurrentExceptionFutureMarker, e);
    assert!(current.failed());
    current.ignore();
});

thread_case!(promises_fail_with_built_errors_and_know_their_waiter, {
    let mut p = Promise::<u32>::new();
    assert!(p.waiting_task().is_none(), "no future yet");
    let f = p.get_future();
    p.set_exception_from(RuntimeError("built error".into()));
    assert!(f.get().unwrap_err().to_string().contains("built error"));

    let mut p = Promise::<u32>::default();
    let f = p.get_future();
    let e = runtime("current error");
    p.set_to_current_exception(e.clone());
    assert!(f.get().unwrap_err() == e);

    // A continuation waits on the promise until the value arrives.
    let mut p = Promise::<u32>::new();
    let f = p.get_future().then(|v: u32| v + 1);
    assert!(p.waiting_task().is_some());
    p.set_value(1);
    assert_eq!(f.get().unwrap(), 2);
});

thread_case!(continuations_attach_to_futures_held_by_reference, {
    let mut ready = make_ready_future(4u32);
    let doubled = ready.then_wrapped_ref(|f: Future<u32>| f.get().unwrap() * 2);
    assert_eq!(doubled.get().unwrap(), 8);

    let mut p = Promise::<u32>::new();
    let mut pending = p.get_future();
    let tripled = pending.then_wrapped_ref(|f: Future<u32>| f.get().unwrap() * 3);
    p.set_value(5);
    assert_eq!(tripled.get().unwrap(), 15);

    let mut failing = current_exception_as_future::<u32>(runtime("wrapped"));
    let seen = failing.then_wrapped_ref(|f: Future<u32>| f.get().is_err());
    assert!(seen.get().unwrap());
});
