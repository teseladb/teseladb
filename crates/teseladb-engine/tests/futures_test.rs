//! Tests of futures and promises: states and values, `finally`, `then_wrapped`, forwarding,
//! loops (`do_until`, `repeat`, `parallel_for_each`), `when_all`, `when_any`,
//! `when_all_succeed`, `map_reduce`, `do_with`, shared futures, timeouts, gates, cross-shard
//! futures and exceptional futures. A thrown integer is an [`IntException`]; flags shared
//! between continuations are `Rc<Cell<bool>>`.

#![cfg(target_os = "linux")]
#![allow(unreachable_code)]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/expected_exception.rs"]
mod expected_exception;

use std::cell::Cell;
use std::collections::{LinkedList, VecDeque};
use std::fmt;
use std::rc::Rc;
use std::time::Duration;

use expected_exception::ExpectedException;
use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, FutureState, NestedException, Promise, RuntimeError,
    make_exception_future, make_exception_ptr, make_ready_future,
};
use teseladb::engine::core::gate::Gate;
use teseladb::engine::core::loop_::internal::iterator_range_estimate_vector_capacity;
use teseladb::engine::core::loop_::{StopIteration, do_until, parallel_for_each, repeat};
use teseladb::engine::core::map_reduce::{Reducer, map_reduce, map_reduce_initial};
use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::shared_future::{SharedFuture, SharedPromise};
use teseladb::engine::core::shared_ptr::{LwSharedPtr, make_lw_shared};
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::stream::{Stream, Subscription};
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::thread::async_;
use teseladb::engine::core::when_all::{when_all_iter, when_all_succeed, when_all_succeed_iter};
use teseladb::engine::core::when_any::{WhenAnyResult, when_any, when_any_iter};
use teseladb::engine::util::later::now;
use teseladb::engine::util::noncopyable_function::NoncopyableFunction;
use teseladb::engine_test_case;

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A thrown integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IntException(i32);

impl fmt::Display for IntException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for IntException {}

fn int_of(e: &ExceptionPtr) -> Option<i32> {
    e.downcast_ref::<IntException>().map(|v| v.0)
}

fn flag() -> Rc<Cell<bool>> {
    Rc::new(Cell::new(false))
}

// Gates, shared futures and shared promises: moves cannot fail; the default and copy
// constructions are checked to exist.
const _: () = {
    fn checks() {
        let _: fn() -> Gate = Gate::new;
        let _: fn() -> SharedFuture<()> = SharedFuture::<()>::default;
        let _: fn(&SharedFuture<()>) -> SharedFuture<()> = SharedFuture::<()>::clone;
        let _: fn() -> SharedPromise<()> = SharedPromise::<()>::new;
    }
    let _ = checks;
};

engine_test_case!(test_self_move, {
    // Moving a state onto itself or swapping it with itself would alias one object, which
    // the borrow rules forbid; what remains is the value surviving set() and get().
    let mut s1: FutureState<(Box<i32>,)> = FutureState::new();
    s1.set((Box::new(42),));
    let s1b = std::mem::replace(&mut s1, FutureState::new()); // no crash, but the value of s1 is not defined.
    s1 = s1b;
    drop(s1);

    let mut s2: FutureState<Box<i32>> = FutureState::new();
    s2.set(Box::new(42));
    assert_eq!(*s2.get().expect("value"), 42);

    let mut p1 = Promise::<Box<i32>>::new();
    p1.set_value(Box::new(42));
    let p1b = std::mem::replace(&mut p1, Promise::new()); // no crash, but the value of p1 is not defined.
    p1 = p1b;
    drop(p1);

    let mut p2 = Promise::<Box<i32>>::new();
    p2.set_value(Box::new(42));
    assert_eq!(*p2.get_future().get().expect("value"), 42);

    let mut f1 = make_ready_future::<Box<i32>>(Box::new(42));
    let f1b = std::mem::replace(&mut f1, make_ready_future(Box::new(0))); // no crash, but the value of f1 is not defined.
    f1 = f1b;
    drop(f1);

    let f2 = make_ready_future::<Box<i32>>(Box::new(42));
    assert_eq!(*f2.get().expect("value"), 42);

    make_ready_future(())
});

fn get_empty_subscription(func: impl FnMut(i32) -> Future<()> + 'static) -> Subscription<i32> {
    let mut s = std::pin::pin!(Stream::<i32>::new());
    let ret = s.as_mut().listen_with(Box::new(func));
    s.as_mut().close();
    ret
}

/// The stream behind a lightweight shared pointer: it lives in the shared block, which
/// does not move.
///
/// # Safety
/// The stream is used from this shard only, and no other reference to it is live while
/// the returned one is.
// A mutable reference from a shared handle.
#[allow(clippy::mut_from_ref)]
unsafe fn stream_of(s: &LwSharedPtr<Stream<i32>>) -> std::pin::Pin<&mut Stream<i32>> {
    // SAFETY: the caller's contract; the shared block never moves.
    unsafe { std::pin::Pin::new_unchecked(s.get_mut_unchecked()) }
}

/// A container whose constructor from a vector exists only to check
/// that a future's value may be built from a range.
#[derive(Default)]
struct IntContainer;

impl<T: Into<i32>> From<Vec<T>> for IntContainer {
    fn from(_: Vec<T>) -> Self {
        IntContainer
    }
}

engine_test_case!(test_future_value_constructible_from_range, {
    // verify that the type a future's value is constructible from a range
    type VectorType = Vec<IntContainer>;
    let _ = make_ready_future::<VectorType>(VectorType::new());
    make_ready_future(())
});

engine_test_case!(test_stream, {
    let mut sub = get_empty_subscription(|_x: i32| make_ready_future(()));
    sub.done()
});

engine_test_case!(test_stream_drop_sub, {
    let s = make_lw_shared(Stream::<i32>::new());
    let expected = 42;
    let mut ret: Option<Future<()>>;
    {
        // SAFETY: the stream is used from this shard only, one call at a time.
        let mut sub = unsafe { stream_of(&s) }.listen_with(Box::new(move |actual: i32| {
            assert_eq!(expected, actual);
            make_ready_future(())
        }));
        ret = Some(sub.done());
        // It is ok to drop the subscription when we only want the competition future.
    }
    let ret = ret.take().expect("set above");
    let s2 = s.clone();
    // SAFETY: as above; `s2` keeps the stream alive until the produce resolves.
    unsafe { stream_of(&s).produce(expected) }.then(move |()| {
        // SAFETY: as above.
        unsafe { stream_of(&s2) }.close();
        ret
    })
});

engine_test_case!(test_reference, {
    let mut a = 42;
    // A future of a reference: a future of a pointer to the referenced value.
    let orig: Future<*mut i32> = make_ready_future(&mut a as *mut i32);
    let fut: Future<*mut i32> = orig;
    let r = fut.get().expect("value");
    // SAFETY: `a` is alive.
    unsafe { *r = 43 };
    assert_eq!(a, 43);
    make_ready_future(())
});

engine_test_case!(test_set_future_state_with_tuple, {
    let mut s1: FutureState<(i32,)> = FutureState::new();
    let mut p1 = Promise::<i32>::new();
    let v1: (i32,) = (42,);
    s1.set(v1);
    p1.set_value(v1.0);
    make_ready_future(())
});

engine_test_case!(test_set_value_make_exception_in_copy, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        /// A value whose copy fails with 42. Setting a promise's value from a reference
        /// copies it, and the copy's failure fails
        /// the promise.
        struct ThrowInCopy;
        impl ThrowInCopy {
            fn try_clone(&self) -> Result<ThrowInCopy, ExceptionPtr> {
                Err(make_exception_ptr(IntException(42)))
            }
        }
        let mut p1 = Promise::<ThrowInCopy>::new();
        let v = ThrowInCopy;
        match v.try_clone() {
            Ok(c) => p1.set_value(c),
            Err(e) => p1.set_exception(e),
        }
        assert!(p1.get_future().get().err().and_then(|e| int_of(&e)) == Some(42));
        Ok(())
    })
});

engine_test_case!(test_set_exception_in_constructor, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        /// A value whose constructor fails with 42.
        struct ThrowInConstructor;
        impl ThrowInConstructor {
            fn new() -> Result<Self, ExceptionPtr> {
                Err(make_exception_ptr(IntException(42)))
            }
        }
        // Making the ready future constructs the value; its failure
        // makes the future failed.
        let f: Future<ThrowInConstructor> = match ThrowInConstructor::new() {
            Ok(v) => make_ready_future(v),
            Err(e) => make_exception_future(e),
        };
        assert!(f.failed());
        assert!(f.get().err().and_then(|e| int_of(&e)) == Some(42));
        Ok(())
    })
});

engine_test_case!(test_finally_is_called_on_success_and_failure, {
    let finally1 = flag();
    let finally2 = flag();
    let (f1a, f2a, f1b, f2b) =
        (finally1.clone(), finally2.clone(), finally1.clone(), finally2.clone());

    make_ready_future(())
        .then(|()| {})
        .finally(move || {
            f1a.set(true);
        })
        .then(|()| -> Result<(), ExceptionPtr> { Err(make_exception_ptr(RuntimeError::new(""))) })
        .finally(move || {
            f2a.set(true);
        })
        .then_wrapped(move |f: Future<()>| {
            assert!(f1b.get());
            assert!(f2b.get());

            // Should be failed.
            assert!(f.get().is_err());
        })
});

engine_test_case!(test_get_on_promise, {
    let mut p = Promise::<u32>::new();
    p.set_value(10);
    assert_eq!(10u32, p.get_future().get().expect("value"));
    make_ready_future(())
});

/// An exception class with a controlled what() overload
#[derive(Debug, Clone)]
struct TestException {
    what: Sstring,
}

impl TestException {
    fn new(what: &str) -> Self {
        TestException { what: Sstring::from(what) }
    }
}

impl fmt::Display for TestException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.what)
    }
}

impl std::error::Error for TestException {}

engine_test_case!(test_get_on_exceptional_promise, {
    let mut p = Promise::<()>::new();
    p.set_exception(make_exception_ptr(TestException::new("test")));
    assert!(matches!(p.get_future().get(), Err(ref e) if e.is::<TestException>()));
    make_ready_future(())
});

fn check_finally_exception(ex: ExceptionPtr) {
    // The exception prints with the Rust names of its types: a `NestedException` that
    // wraps the `TestException` of the cleanup and the one of the body.
    assert_eq!(
        format!("{}", ex),
        format!(
            "{}: {} (bar) (while cleaning up after {} (foo))",
            std::any::type_name::<NestedException>(),
            std::any::type_name::<TestException>(),
            std::any::type_name::<TestException>()
        )
    );
    // convert to the concrete type nested_exception
    if let Some(ex) = ex.downcast_ref::<NestedException>() {
        match ex.inner.downcast_ref::<TestException>() {
            Some(inner) => assert_eq!(inner.what.as_str(), "bar"),
            None => panic!("inner is not a test_exception"),
        }
        match ex.rethrow_nested() {
            Err(outer) => match outer.downcast_ref::<TestException>() {
                Some(outer) => assert_eq!(outer.what.as_str(), "foo"),
                None => panic!("outer is not a test_exception"),
            },
            Ok(never) => match never {},
        }
    }
}

engine_test_case!(test_finally_exception, {
    make_ready_future(())
        .then(|()| -> Result<(), ExceptionPtr> {
            Err(make_exception_ptr(TestException::new("foo")))
        })
        .finally(|| -> Result<(), ExceptionPtr> {
            Err(make_exception_ptr(TestException::new("bar")))
        })
        .handle_exception(check_finally_exception)
});

engine_test_case!(test_finally_exceptional_future, {
    make_ready_future(())
        .then(|()| -> Result<(), ExceptionPtr> {
            Err(make_exception_ptr(TestException::new("foo")))
        })
        .finally(|| make_exception_future::<()>(make_exception_ptr(TestException::new("bar"))))
        .handle_exception(check_finally_exception)
});

engine_test_case!(test_finally_waits_for_inner, {
    let finally = flag();
    let p = make_lw_shared(Promise::<()>::new());
    let (fin1, fin2) = (finally.clone(), finally.clone());
    let p1 = p.clone();
    let f = make_ready_future(())
        .then(|()| {})
        .finally(move || {
            // SAFETY: the promise is used from this shard only, one call at a time.
            unsafe { p1.get_mut_unchecked() }.get_future().then(move |()| {
                fin1.set(true);
            })
        })
        .then(move |()| {
            assert!(fin2.get());
        });
    assert!(!finally.get());
    // SAFETY: as above.
    unsafe { p.get_mut_unchecked() }.set_value(());
    f
});

engine_test_case!(test_finally_is_called_on_success_and_failure__not_ready_to_armed, {
    let finally1 = flag();
    let finally2 = flag();
    let (f1a, f2a, f1b, f2b) =
        (finally1.clone(), finally2.clone(), finally1.clone(), finally2.clone());

    let mut p = Promise::<()>::new();
    let f = p
        .get_future()
        .finally(move || {
            f1a.set(true);
        })
        .then(|()| -> Result<(), ExceptionPtr> { Err(make_exception_ptr(RuntimeError::new(""))) })
        .finally(move || {
            f2a.set(true);
        })
        .then_wrapped(move |f: Future<()>| {
            assert!(f1b.get());
            assert!(f2b.get());
            let _ = f.get(); // silence exceptional future ignored messages
        });

    p.set_value(());
    f
});

engine_test_case!(test_exception_from_finally_fails_the_target, {
    let mut pr = Promise::<()>::new();
    let f = pr
        .get_future()
        .finally(|| -> Result<(), ExceptionPtr> { Err(make_exception_ptr(RuntimeError::new(""))) })
        .then(|()| -> () {
            panic!("must not be reached");
        })
        .then_wrapped(|f: Future<()>| {
            let _ = f.get(); // silence exceptional future ignored messages
        });

    pr.set_value(());
    f
});

engine_test_case!(test_exception_from_finally_fails_the_target_on_already_resolved, {
    make_ready_future(())
        .finally(|| -> Result<(), ExceptionPtr> { Err(make_exception_ptr(RuntimeError::new(""))) })
        .then(|()| -> () {
            panic!("must not be reached");
        })
        .then_wrapped(|f: Future<()>| {
            let _ = f.get(); // silence exceptional future ignored messages
        })
});

engine_test_case!(test_exception_thrown_from_then_wrapped_causes_future_to_fail, {
    make_ready_future(())
        .then_wrapped(|_f: Future<()>| -> Result<(), ExceptionPtr> {
            Err(make_exception_ptr(RuntimeError::new("")))
        })
        .then_wrapped(|f: Future<()>| {
            assert!(f.get().is_err());
        })
});

engine_test_case!(test_exception_thrown_from_then_wrapped_causes_future_to_fail__async_case, {
    let mut p = Promise::<()>::new();

    let f = p
        .get_future()
        .then_wrapped(|_f: Future<()>| -> Result<(), ExceptionPtr> {
            Err(make_exception_ptr(RuntimeError::new("")))
        })
        .then_wrapped(|f: Future<()>| {
            assert!(f.get().is_err());
        });

    p.set_value(());

    f
});

engine_test_case!(test_failing_intermediate_promise_should_fail_the_master_future, {
    let mut p1 = Promise::<()>::new();
    let mut p2 = Promise::<()>::new();

    let inner = p2.get_future();
    let f = p1.get_future().then(move |()| inner).then(|()| -> () {
        panic!("must not be reached");
    });

    p1.set_value(());
    p2.set_exception(make_exception_ptr(RuntimeError::new("boom")));

    f.then_wrapped(|f: Future<()>| {
        assert!(f.get().is_err());
    })
});

engine_test_case!(test_future_forwarding__not_ready_to_unarmed, {
    let mut p1 = Promise::<()>::new();
    let mut p2 = Promise::<()>::new();

    let f1 = p1.get_future();
    let f2 = p2.get_future();

    f1.forward_to(p2);
    assert!(!f2.available());

    let called = f2.then(|()| {});

    p1.set_value(());
    called
});

engine_test_case!(test_future_forwarding__not_ready_to_armed, {
    let mut p1 = Promise::<()>::new();
    let mut p2 = Promise::<()>::new();

    let f1 = p1.get_future();
    let f2 = p2.get_future();

    // `f2.then()` consumes f2 in Rust; its availability is checked first, before the
    // forwarding (both are false here).
    assert!(!f2.available());
    let called = f2.then(|()| {});

    f1.forward_to(p2);

    p1.set_value(());
    called
});

engine_test_case!(test_future_forwarding__ready_to_unarmed, {
    let mut p2 = Promise::<()>::new();

    let f1 = make_ready_future(());
    let f2 = p2.get_future();

    f1.forward_to(p2);
    assert!(f2.available());

    f2.then_wrapped(|f: Future<()>| {
        assert!(!f.failed());
    })
});

engine_test_case!(test_future_forwarding__ready_to_armed, {
    let mut p2 = Promise::<()>::new();

    let f1 = make_ready_future(());
    let f2 = p2.get_future();

    let called = f2.then(|()| {});

    assert!(f1.available());

    f1.forward_to(p2);
    called
});

fn forward_dead_unarmed_promise_with_dead_future_to(p: &mut Promise<()>) {
    let p2 = Promise::<()>::new();
    p.get_future().forward_to(p2);
}

engine_test_case!(test_future_forwarding__ready_to_unarmed_soon_to_be_dead, {
    let mut p1 = Promise::<()>::new();
    forward_dead_unarmed_promise_with_dead_future_to(&mut p1);
    make_ready_future(()).forward_to(p1);
    make_ready_future(())
});

engine_test_case!(test_exception_can_be_thrown_from_do_until_body, {
    do_until(
        || false,
        || -> Future<()> {
            // `futurize`: a body that throws or returns a future.
            Future::from((|| -> Result<Future<()>, ExceptionPtr> {
                return Err(make_exception_ptr(ExpectedException::new()));
                Ok(now())
            })())
        },
    )
    .then_wrapped(|f: Future<()>| match f.get() {
        Ok(()) => panic!("should have failed"),
        Err(e) if e.is::<ExpectedException>() => {
            // expected
        }
        Err(e) => panic!("{}", e),
    })
});

engine_test_case!(test_exception_can_be_thrown_from_do_until_condition, {
    do_until(
        || -> Result<bool, ExceptionPtr> {
            return Err(make_exception_ptr(ExpectedException::new()));
            Ok(false)
        },
        now,
    )
    .then_wrapped(|f: Future<()>| match f.get() {
        Ok(()) => panic!("should have failed"),
        Err(e) if e.is::<ExpectedException>() => {
            // expected
        }
        Err(e) => panic!("{}", e),
    })
});

engine_test_case!(test_bare_value_can_be_returned_from_callback, {
    // A bare value is `Ok(value)`: `Futurize` has no impl for plain values.
    now().then(|()| -> Result<i32, ExceptionPtr> { Ok(3) }).then(|x: i32| {
        assert!(x == 3);
    })
});

engine_test_case!(test_when_all_iterator_range, {
    let mut futures: Vec<Future<usize>> = Vec::new();
    for i in 0..1_000_000usize {
        // Use a mix of available and unavailable futures to exercise
        // both paths in when_all().
        let fut = if (i % 2) == 0 { make_ready_future(()) } else { yield_() };
        futures.push(fut.then(move |()| make_ready_future(i)));
    }
    // Verify the above statement is correct
    assert!(!futures.iter().all(|f| f.available()));
    when_all_iter(futures).then(|ret: Vec<Future<usize>>| {
        assert!(ret.iter().all(|f| f.available()));
        assert!(ret.into_iter().enumerate().all(|(i, f)| f.get().ok() == Some(i)));
    })
});

fn test_iterator_range_estimate<I: Iterator<Item = i32>, C: FromIterator<i32>>(
    into: impl Fn(&C) -> I,
) {
    let container: C = [1, 2, 3].into_iter().collect();
    assert_eq!(iterator_range_estimate_vector_capacity(&into(&container)), 3);
}

#[test]
fn test_iterator_range_estimate_vector_capacity() {
    test_iterator_range_estimate::<_, Vec<i32>>(|c: &Vec<i32>| c.clone().into_iter());
    test_iterator_range_estimate::<_, LinkedList<i32>>(|c: &LinkedList<i32>| c.clone().into_iter());
    // A singly linked list: its iterator knows no length either way.
    test_iterator_range_estimate::<_, VecDeque<i32>>(|c: &VecDeque<i32>| c.clone().into_iter());
    {
        let n = 42;
        let seq = 0..n;
        assert_eq!(iterator_range_estimate_vector_capacity(&seq) as i32, n);
    }
    {
        // for ranges that generate elements on-the-fly, advancing an iterator
        // might actually consume or transform the underlying sequence, in this
        // case, the function under test returns 0.
        let seq = 1..;
        assert_eq!(iterator_range_estimate_vector_capacity(&seq), 0);
    }
}

// helper function for when_any tests
fn when_all_but_one_succeed(futures: Vec<Future<usize>>, leave_out: usize) -> Future<()> {
    let sz = futures.len();
    assert!(sz >= 1);
    assert!(leave_out < sz);
    let mut all_but_one_tmp: Vec<Future<usize>> = Vec::with_capacity(sz - 1);
    for (i, f) in futures.into_iter().enumerate() {
        if i == leave_out {
            continue;
        }
        all_but_one_tmp.push(f);
    }
    when_all_succeed_iter(all_but_one_tmp).then(|_: Vec<usize>| make_ready_future(()))
}

/// Takes the resolved future out of a `when_any` result.
fn take_index(ret: &mut WhenAnyResult<Vec<Future<usize>>>) -> Future<usize> {
    std::mem::replace(&mut ret.futures[ret.index], make_ready_future(ret.index))
}

engine_test_case!(test_when_any_iterator_range_i, {
    let mut futures: Vec<Future<usize>> = Vec::new();
    for i in 0..100usize {
        let fut = yield_();
        futures.push(fut.then(move |()| make_ready_future(i)));
    }

    // Verify the above statement is correct
    assert!(futures.iter().all(|f| !f.available()));

    when_any_iter(futures).then(|mut ret_obj: WhenAnyResult<Vec<Future<usize>>>| {
        assert!(ret_obj.futures[ret_obj.index].available());
        let index = ret_obj.index;
        assert!(take_index(&mut ret_obj).get().ok() == Some(index));
        when_all_but_one_succeed(ret_obj.futures, index)
    })
});

engine_test_case!(test_when_any_iterator_range_ii, {
    let mut futures: Vec<Future<usize>> = Vec::new();
    for i in 0..100usize {
        if i == 42 {
            let fut = make_ready_future(());
            futures.push(fut.then(move |()| make_ready_future(i)));
        } else {
            let fut = sleep(ms(100));
            futures.push(fut.then(move |()| make_ready_future(i)));
        }
    }
    when_any_iter(futures).then(|mut ret_obj: WhenAnyResult<Vec<Future<usize>>>| {
        assert!(ret_obj.futures[ret_obj.index].available());
        let index = ret_obj.index;
        assert!(take_index(&mut ret_obj).get().ok() == Some(index));
        assert!(index == 42);
        when_all_but_one_succeed(ret_obj.futures, index)
    })
});

engine_test_case!(test_when_any_iterator_range_iii, {
    let mut futures: Vec<Future<usize>> = Vec::new();
    for i in 0..100usize {
        if i == 42 {
            let fut = sleep(ms(5));
            futures.push(fut.then(move |()| make_ready_future(i)));
        } else {
            let fut = sleep(ms(100));
            futures.push(fut.then(move |()| make_ready_future(i)));
        }
    }
    when_any_iter(futures).then(|mut ret_obj: WhenAnyResult<Vec<Future<usize>>>| {
        assert!(ret_obj.futures[ret_obj.index].available());
        let index = ret_obj.index;
        assert!(take_index(&mut ret_obj).get().ok() == Some(index));
        assert!(index == 42);
        when_all_but_one_succeed(ret_obj.futures, index)
    })
});

engine_test_case!(test_when_any_iterator_range_iv, {
    let mut futures: Vec<Future<usize>> = Vec::new();
    for i in 0..100usize {
        if i == 42 {
            let fut = yield_().then(|()| {
                make_exception_future::<()>(make_exception_ptr(RuntimeError::new("test")))
            });
            futures.push(fut.then(move |()| make_ready_future(i)));
        } else {
            let fut = sleep(ms(100));
            futures.push(fut.then(move |()| make_ready_future(i)));
        }
    }
    when_any_iter(futures).then(|mut ret_obj: WhenAnyResult<Vec<Future<usize>>>| {
        assert!(ret_obj.futures[ret_obj.index].available());
        let index = ret_obj.index;
        assert!(matches!(take_index(&mut ret_obj).get(), Err(ref e) if e.is::<RuntimeError>()));
        when_all_but_one_succeed(ret_obj.futures, index)
    })
});

type L33t = (char, i32, i32, char, char, i32, char);

engine_test_case!(test_when_any_variadic_i, {
    let f_int = yield_().then(|()| make_ready_future::<i32>(42));
    let f_string = sleep(ms(100)).then(|()| make_ready_future::<Sstring>(Sstring::from("hello")));
    let f_l33tspeak =
        sleep(ms(100)).then(|()| make_ready_future::<L33t>(('s', 3, 4, 's', 't', 4, 'r')));
    when_any((f_int, f_string, f_l33tspeak)).then(
        |wa_result: WhenAnyResult<(Future<i32>, Future<Sstring>, Future<L33t>)>| {
            assert!(wa_result.index == 0);
            let (one, two, three) = wa_result.futures;
            assert!(one.get().ok() == Some(42));
            when_all_succeed((two, three)).then(|_| make_ready_future(()))
        },
    )
});

engine_test_case!(test_when_any_variadic_ii, {
    /// A copyable value of the variadic `when_any` test.
    #[derive(Clone, Copy)]
    struct Foo {
        bar: i32,
    }
    impl Default for Foo {
        fn default() -> Self {
            Foo { bar: 86 }
        }
    }
    let f_int = sleep(ms(100)).then(|()| make_ready_future::<i32>(42));
    let f_foo = sleep(ms(75)).then(|()| make_ready_future::<Foo>(Foo::default()));
    let f_string = sleep(ms(1)).then(|()| make_ready_future::<Sstring>(Sstring::from("hello")));
    let f_l33tspeak =
        sleep(ms(50)).then(|()| make_ready_future::<L33t>(('s', 3, 4, 's', 't', 4, 'r')));
    type Futures = (Future<i32>, Future<Foo>, Future<Sstring>, Future<L33t>);
    when_any((f_int, f_foo, f_string, f_l33tspeak)).then(|wa_result: WhenAnyResult<Futures>| {
        assert!(wa_result.index == 2);
        let (one, two, three, four) = wa_result.futures;
        assert!(three.get().ok().is_some_and(|s| s == "hello"));
        when_any((one, two, four)).then(
            |wa_nextresult: WhenAnyResult<(Future<i32>, Future<Foo>, Future<L33t>)>| {
                let (one, two, four) = wa_nextresult.futures;
                assert!(wa_nextresult.index == 2);
                assert!(four.get().ok() == Some(('s', 3, 4, 's', 't', 4, 'r')));
                when_any((one, two)).then(|wa_result: WhenAnyResult<(Future<i32>, Future<Foo>)>| {
                    let (one, two) = wa_result.futures;
                    assert!(wa_result.index == 1);
                    assert!(two.get().ok().map(|f| f.bar) == Some(Foo::default().bar));
                    one.then(|x: i32| {
                        assert!(x == 42);
                    })
                })
            },
        )
    })
});

engine_test_case!(test_map_reduce, {
    let square = |x: i64| make_ready_future::<i64>(x * x);
    let n: i64 = 1000;
    map_reduce_initial(0..n, square, 0i64, |a: i64, b: i64| a + b).then(move |result: i64| {
        let m = n - 1; // counting does not include upper bound
        assert_eq!(result, (m * (m + 1) * (2 * m + 1)) / 6);
    })
});

engine_test_case!(test_map_reduce_simple, {
    do_with(0i64, |res: &mut i64| {
        let n: i64 = 10;
        let resp: *mut i64 = res;
        map_reduce(
            0..n,
            |x: i64| make_ready_future(x),
            move |x: i64| {
                // SAFETY: do_with keeps `res` alive until the returned future resolves.
                unsafe { *resp += x };
            },
        )
        .then(move |()| {
            let expected = (n * (n - 1)) / 2;
            // SAFETY: as above.
            assert_eq!(unsafe { *resp }, expected);
        })
    })
});

engine_test_case!(test_map_reduce_tuple, {
    do_with((0i64, 0i64), |res: &mut (i64, i64)| {
        let n: i64 = 10;
        let resp: *mut (i64, i64) = res;
        map_reduce(
            0..n,
            |x: i64| make_ready_future((x, -x)),
            move |t: (i64, i64)| {
                // SAFETY: do_with keeps the results alive until the returned future resolves.
                unsafe {
                    (*resp).0 += t.0;
                    (*resp).1 += t.1;
                }
            },
        )
        .then(move |()| {
            let expected = (n * (n - 1)) / 2;
            // SAFETY: as above.
            unsafe {
                assert_eq!((*resp).0, expected);
                assert_eq!((*resp).1, -expected);
            }
        })
    })
});

/// The `map` of the `map_reduce*_lifetime` tests: it must outlive every mapped future.
#[derive(Clone, Default)]
struct LifetimeMap {
    destroyed: Rc<Cell<bool>>,
}

impl Drop for LifetimeMap {
    fn drop(&mut self) {
        self.destroyed.set(true);
    }
}

impl LifetimeMap {
    fn call(&self, x: i64) -> Future<i64> {
        let destroyed = self.destroyed.clone();
        yield_().then(move |()| {
            assert!(!destroyed.get());
            make_ready_future(x)
        })
    }
}

engine_test_case!(test_map_reduce_lifetime, {
    /// A reducer that records when it is dropped.
    struct Reduce {
        res: *mut i64,
        destroyed: Rc<Cell<bool>>,
    }
    impl Drop for Reduce {
        fn drop(&mut self) {
            self.destroyed.set(true);
        }
    }
    impl Reducer<i64> for Reduce {
        type Result = ();
        fn call(&mut self, x: i64) -> Future<()> {
            let (destroyed, res) = (self.destroyed.clone(), self.res);
            yield_().then(move |()| {
                assert!(!destroyed.get());
                // SAFETY: do_with keeps `res` alive until the reduction resolves.
                unsafe { *res += x };
            })
        }
        fn finish(self) -> Future<()> {
            make_ready_future(())
        }
    }
    do_with(0i64, |res: &mut i64| {
        let n: i64 = 10;
        let resp: *mut i64 = res;
        let map = LifetimeMap::default();
        map_reduce(0..n, move |x: i64| map.call(x), Reduce { res: resp, destroyed: flag() }).then(
            move |()| {
                let expected = (n * (n - 1)) / 2;
                // SAFETY: as above.
                assert_eq!(unsafe { *resp }, expected);
            },
        )
    })
});

engine_test_case!(test_map_reduce0_lifetime, {
    /// A reducer that records when it is dropped.
    #[derive(Clone, Default)]
    struct Reduce {
        destroyed: Rc<Cell<bool>>,
    }
    impl Drop for Reduce {
        fn drop(&mut self) {
            self.destroyed.set(true);
        }
    }
    let n: i64 = 10;
    let map = LifetimeMap::default();
    let reduce = Reduce::default();
    map_reduce_initial(
        0..n,
        move |x: i64| map.call(x),
        0i64,
        move |res: i64, x: i64| {
            assert!(!reduce.destroyed.get());
            res + x
        },
    )
    .then(move |res: i64| {
        let expected = (n * (n - 1)) / 2;
        assert_eq!(res, expected);
    })
});

engine_test_case!(test_map_reduce1_lifetime, {
    /// A reducer with a `get()` that records when it is dropped.
    struct Reduce {
        res: Rc<Cell<i64>>,
        destroyed: Rc<Cell<bool>>,
    }
    impl Drop for Reduce {
        fn drop(&mut self) {
            println!("~reduce()");
            self.destroyed.set(true);
        }
    }
    impl Reducer<i64> for Reduce {
        type Result = i64;
        fn call(&mut self, x: i64) -> Future<()> {
            let (destroyed, res) = (self.destroyed.clone(), self.res.clone());
            yield_().then(move |()| {
                assert!(!destroyed.get());
                res.set(res.get() + x);
                make_ready_future(())
            })
        }
        fn finish(self) -> Future<i64> {
            let (destroyed, res) = (self.destroyed.clone(), self.res.clone());
            // `get()` runs on the reducer, which the reduction keeps alive until the
            // sleep resolves.
            let keep = self;
            sleep(ms(10)).then(move |()| {
                let _keep = &keep;
                assert!(!destroyed.get());
                make_ready_future(res.get())
            })
        }
    }
    let n: i64 = 10;
    let map = LifetimeMap::default();
    map_reduce(
        0..n,
        move |x: i64| map.call(x),
        Reduce { res: Rc::new(Cell::new(0)), destroyed: flag() },
    )
    .then(move |res: i64| {
        let expected = (n * (n - 1)) / 2;
        assert_eq!(res, expected);
    })
});

engine_test_case!(map_reduce_with_throwing_mapper, {
    make_coroutine(async move {
        let vec: Vec<i32> = vec![1, 2, 3, 4, 5, 6, 7];
        let ret = map_reduce_initial(
            vec,
            // Mapper: identity function, but throws
            |x: i32| -> Future<i32> {
                make_coroutine(async move {
                    if x == 5 {
                        return Err(make_exception_ptr(RuntimeError::new("test")));
                    }
                    Ok(x)
                })
            },
            // Initial value (and accumulator): move-only type
            Box::new(0i32),
            // Reducer: test that it does not act on a moved-from value
            |mut acc: Box<i32>, x: i32| -> Box<i32> {
                *acc += x;
                acc
            },
        )
        .await;
        match ret {
            Ok(_) => panic!("should have thrown"),
            Err(_) => {
                // Exception is expected and uninteresting
            }
        }
        Ok(())
    })
});

engine_test_case!(map_reduce_with_throwing_reducer, {
    make_coroutine(async move {
        let vec: Vec<i32> = vec![1, 2, 3, 4, 5, 6, 7];
        let ret = map_reduce_initial(
            vec,
            // Mapper: square function
            |x: i32| -> Future<i32> { make_coroutine(async move { Ok(x * x) }) },
            // Initial value (and accumulator): move-only type
            Box::new(0i32),
            // Reducer: simple sum, but randomly throws
            |mut acc: Box<i32>, x: i32| -> Result<Box<i32>, ExceptionPtr> {
                if *acc > 14 {
                    return Err(make_exception_ptr(RuntimeError::new(
                        "accumulator overflow, launch missiles",
                    )));
                }
                *acc += x;
                Ok(acc)
            },
        )
        .await;
        match ret {
            Ok(_) => panic!("should have thrown"),
            Err(_) => {
                // Exception is expected and uninteresting
            }
        }
        Ok(())
    })
});

// This test doesn't actually test anything - it just waits for the future
// returned by sleep to complete. However, a bug we had in sleep() caused
// this test to fail the sanitizer in the debug build, so this is a useful
// regression test.
engine_test_case!(test_sleep, { sleep(ms(100)) });

engine_test_case!(test_do_with_1, {
    do_with(1i32, |one: &mut i32| {
        assert_eq!(*one, 1);
        make_ready_future(())
    })
});

engine_test_case!(test_do_with_2, {
    do_with((1i32, 2i64), |(one, two): &mut (i32, i64)| {
        assert_eq!(*one, 1);
        assert_eq!(*two, 2);
        make_ready_future(())
    })
});

engine_test_case!(test_do_with_3, {
    do_with((1i32, 2i64, 3i32), |(one, two, three): &mut (i32, i64, i32)| {
        assert_eq!(*one, 1);
        assert_eq!(*two, 2);
        assert_eq!(*three, 3);
        make_ready_future(())
    })
});

engine_test_case!(test_do_with_4, {
    do_with((1i32, 2i64, 3i32, 4i32), |(one, two, three, four): &mut (i32, i64, i32, i32)| {
        assert_eq!(*one, 1);
        assert_eq!(*two, 2);
        assert_eq!(*three, 3);
        assert_eq!(*four, 4);
        make_ready_future(())
    })
});

engine_test_case!(test_do_with_5, {
    type Func = NoncopyableFunction<dyn FnMut()>;
    do_with(Func::new(|| {}), |_: &mut Func| make_ready_future(()))
});

engine_test_case!(test_do_with_6, {
    let x: i32 = 42;
    do_with((42i32, x), |_: &mut (i32, i32)| make_ready_future(()))
});

engine_test_case!(test_do_with_7, {
    let x: i32 = 42;
    do_with(x, |_: &mut i32| make_ready_future(()))
});

engine_test_case!(test_do_while_stopping_immediately, {
    do_with(0i32, |count: &mut i32| {
        let cp: *mut i32 = count;
        repeat(move || {
            // SAFETY: do_with keeps `count` alive until the returned future resolves.
            unsafe { *cp += 1 };
            make_ready_future(StopIteration::YES)
        })
        .then(move |()| {
            // SAFETY: do_with keeps `count` alive until the returned future resolves; the loop has ended.
            assert!(unsafe { *cp } == 1);
        })
    })
});

engine_test_case!(test_do_while_stopping_after_two_iterations, {
    do_with(0i32, |count: &mut i32| {
        let cp: *mut i32 = count;
        // SAFETY: as in test_do_while_stopping_immediately.
        repeat(move || {
            // SAFETY: do_with keeps `count` alive until the returned future resolves; nothing else references it here.
            unsafe { *cp += 1 };
            // SAFETY: do_with keeps `count` alive until the returned future resolves; nothing else references it here.
            make_ready_future(if unsafe { *cp } == 2 {
                StopIteration::YES
            } else {
                StopIteration::NO
            })
        })
        .then(move |()| {
            // SAFETY: do_with keeps `count` alive until the returned future resolves; the loop has ended.
            assert!(unsafe { *cp } == 2);
        })
    })
});

engine_test_case!(test_do_while_failing_in_the_first_step, {
    repeat(|| -> Result<StopIteration, ExceptionPtr> {
        return Err(make_exception_ptr(ExpectedException::new()));
        Ok(StopIteration::NO)
    })
    .then_wrapped(|f: Future<()>| match f.get() {
        Ok(()) => panic!("should not happen"),
        Err(e) if e.is::<ExpectedException>() => {
            // expected
        }
        Err(e) => panic!("{}", e),
    })
});

engine_test_case!(test_do_while_failing_in_the_second_step, {
    do_with(0i32, |count: &mut i32| {
        let cp: *mut i32 = count;
        // SAFETY: as in test_do_while_stopping_immediately.
        repeat(move || -> Future<StopIteration> {
            // `futurize`: a body that throws or returns a future.
            Future::from((|| -> Result<Future<StopIteration>, ExceptionPtr> {
                // SAFETY: do_with keeps `count` alive until the returned future resolves; nothing else references it here.
                unsafe { *cp += 1 };
                // SAFETY: do_with keeps `count` alive until the returned future resolves; nothing else references it here.
                if unsafe { *cp } > 1 {
                    return Err(make_exception_ptr(ExpectedException::new()));
                }
                Ok(yield_().then(|()| make_ready_future(StopIteration::NO)))
            })())
        })
        .then_wrapped(move |f: Future<()>| match f.get() {
            Ok(()) => panic!("should not happen"),
            Err(e) if e.is::<ExpectedException>() => {
                // SAFETY: do_with keeps `count` alive until the returned future resolves; the loop has ended.
                assert!(unsafe { *cp } == 2);
            }
            Err(e) => panic!("{}", e),
        })
    })
});

engine_test_case!(test_parallel_for_each, {
    async_(|| -> Result<(), ExceptionPtr> {
        // empty
        parallel_for_each(Vec::<i32>::new(), |_: i32| -> Future<()> {
            panic!("should not reach");
        })
        .get()?;

        // immediate result
        let range: Vec<i32> = (1..6).collect();
        let sum = Rc::new(Cell::new(0));
        let s = sum.clone();
        parallel_for_each(range.clone(), move |v: i32| {
            s.set(s.get() + v);
            make_ready_future(())
        })
        .get()?;
        assert_eq!(sum.get(), 15);

        // all suspend
        sum.set(0);
        let s = sum.clone();
        parallel_for_each(range.clone(), move |v: i32| {
            let s = s.clone();
            yield_().then(move |()| {
                s.set(s.get() + v);
            })
        })
        .get()?;
        assert_eq!(sum.get(), 15);

        // throws immediately
        assert!(
            parallel_for_each(range.clone(), |_: i32| -> Future<()> {
                // `futurize`: a body that throws or returns a future.
                let r: Result<Future<()>, ExceptionPtr> = Err(make_exception_ptr(IntException(5)));
                Future::from(r)
            })
            .get()
            .err()
            .and_then(|e| int_of(&e))
                == Some(5)
        );

        // throws after suspension
        assert!(
            parallel_for_each(range.clone(), |_: i32| {
                yield_().then(|()| -> Result<(), ExceptionPtr> {
                    Err(make_exception_ptr(IntException(5)))
                })
            })
            .get()
            .err()
            .and_then(|e| int_of(&e))
                == Some(5)
        );
        Ok(())
    })
});

// The pending futures are kept inline for a loop over at most five elements, in one heap
// vector for a longer one, and inline then on the heap when the iterator announced fewer
// elements than it yields (a filter's lower bound is 0): every element runs once, and a
// failure among pending futures is reported, in each storage.
engine_test_case!(test_parallel_for_each_pending_futures_in_every_storage, {
    async_(|| -> Result<(), ExceptionPtr> {
        fn run<I: IntoIterator<Item = i32>>(items: I, fail_at: Option<i32>) -> (i32, bool) {
            let sum = Rc::new(Cell::new(0));
            let s = sum.clone();
            let r = parallel_for_each(items, move |v: i32| {
                let s = s.clone();
                yield_().then(move |()| -> Result<(), ExceptionPtr> {
                    s.set(s.get() + v);
                    if Some(v) == fail_at {
                        return Err(make_exception_ptr(IntException(v)));
                    }
                    Ok(())
                })
            })
            .get();
            let failed_as_expected = match (r, fail_at) {
                (Ok(()), None) => true,
                (Err(e), Some(v)) => int_of(&e) == Some(v),
                _ => false,
            };
            (sum.get(), failed_as_expected)
        }
        let total = |n: i32| n * (n - 1) / 2;
        for n in [1, 3, 5, 6, 100] {
            assert_eq!(run(0..n, None), (total(n), true), "range of {n}");
            assert_eq!(run(0..n, Some(n / 2)), (total(n), true), "range of {n}, one failure");
            let filtered = (0..n).filter(|_| true);
            assert_eq!(filtered.size_hint().0, 0);
            assert_eq!(run(filtered, None), (total(n), true), "filter of {n}");
            let filtered = (0..n).filter(|_| true);
            assert_eq!(run(filtered, Some(n - 1)), (total(n), true), "filter of {n}, failure");
        }
        Ok(())
    })
});

engine_test_case!(test_parallel_for_each_early_failure, {
    do_with(0i32, |counter: &mut i32| {
        let cp: *mut i32 = counter;
        parallel_for_each(0..11000i32, move |i: i32| {
            // force scheduling
            sleep(ms((i % 31 + 1) as u64)).then(move |()| {
                // SAFETY: do_with keeps `counter` alive until the returned future resolves.
                unsafe { *cp += 1 };
                if i % 1777 == 1337 {
                    return make_exception_future::<()>(make_exception_ptr(IntException(i)));
                }
                make_ready_future(())
            })
        })
        .then_wrapped(move |f: Future<()>| {
            // SAFETY: as above.
            assert_eq!(unsafe { *cp }, 11000);
            assert!(f.failed());
            match f.get() {
                Ok(()) => panic!("wanted an exception"),
                Err(e) => match int_of(&e) {
                    Some(i) => assert!(i % 1777 == 1337),
                    None => panic!("bad exception type"),
                },
            }
        })
    })
});

engine_test_case!(test_parallel_for_each_waits_for_all_fibers_even_if_one_of_them_failed, {
    let can_exit = flag();
    let ce = can_exit.clone();
    parallel_for_each(0..2i32, move |i: i32| {
        let ce = ce.clone();
        yield_().then(move |()| -> Future<()> {
            // `futurize`: a body that throws or returns a future.
            let r: Result<Future<()>, ExceptionPtr> = if i == 1 {
                Err(make_exception_ptr(ExpectedException::new()))
            } else {
                Ok(sleep(ms(300)).then(move |()| {
                    ce.set(true);
                }))
            };
            Future::from(r)
        })
    })
    .then_wrapped(move |f: Future<()>| {
        match f.get() {
            Ok(()) => {}
            Err(_) => {
                // expected
            }
        }
        assert!(can_exit.get());
    })
});

engine_test_case!(test_parallel_for_each_broken_promise, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        // The closure is invoked in place, so `v` is dropped before the future is waited for.
        #[allow(clippy::redundant_closure_call)]
        let fut = (|| {
            let mut v: Vec<Promise<()>> = vec![Promise::new(), Promise::new()];
            let vp: *mut Vec<Promise<()>> = &mut v;
            // SAFETY: the futures are taken now, while `v` is alive; then `v` is dropped.
            let f =
                parallel_for_each(0..2usize, move |i: usize| unsafe { (&mut *vp)[i].get_future() });
            drop(v);
            f
        })();
        assert!(
            matches!(fut.get(), Err(ref e) if e.is::<teseladb::engine::core::future::BrokenPromise>())
        );
        Ok(())
    })
});

engine_test_case!(test_repeat_broken_promise, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let get_fut = || {
            let mut pr = Promise::<StopIteration>::new();
            pr.get_future()
        };

        let mut fut = Some(get_fut());
        let r: Future<()> = repeat(move || fut.take().expect("called once: the future is broken"));

        assert!(
            matches!(r.get(), Err(ref e) if e.is::<teseladb::engine::core::future::BrokenPromise>())
        );
        Ok(())
    })
});

#[cfg(not(engine_shuffle_task_queue))]
engine_test_case!(test_high_priority_task_runs_in_the_middle_of_loops, {
    use teseladb::engine::core::make_task::make_task_ptr;
    use teseladb::engine::core::reactor::engine;
    use teseladb::engine::core::scheduling::current_scheduling_group;

    let counter = Rc::new(Cell::new(0));
    let flag = flag();
    repeat(move || {
        if counter.get() == 1 {
            assert!(flag.get());
            return make_ready_future(StopIteration::YES);
        }
        let f = flag.clone();
        // SAFETY: on this shard's reactor thread; the reactor is reached for this call only.
        unsafe {
            (*engine()).add_high_priority_task(make_task_ptr(
                current_scheduling_group(),
                move || {
                    f.set(true);
                },
            ))
        };
        counter.set(counter.get() + 1);
        make_ready_future(StopIteration::NO)
    })
});

fn expect_expected(f: Result<impl Sized, ExceptionPtr>) {
    match f {
        Ok(_) => panic!("should have thrown"),
        Err(e) if e.is::<ExpectedException>() => {}
        Err(e) => panic!("{}", e),
    }
}

engine_test_case!(futurize_invoke_val_exception, {
    use teseladb::engine::core::future::futurize_apply;
    futurize_apply(
        |_arg: i32| -> Result<i32, ExceptionPtr> {
            return Err(make_exception_ptr(ExpectedException::new()));
            Ok(_arg)
        },
        (1,),
    )
    .then_wrapped(|f: Future<i32>| expect_expected(f.get()))
});

engine_test_case!(futurize_invoke_val_ok, {
    use teseladb::engine::core::future::futurize_apply;
    futurize_apply(|arg: i32| -> Result<i32, ExceptionPtr> { Ok(arg * 2) }, (2,)).then_wrapped(
        |f: Future<i32>| match f.get() {
            Ok(x) => assert_eq!(x, 4),
            Err(_) => panic!("should not have thrown"),
        },
    )
});

engine_test_case!(futurize_invoke_val_future_exception, {
    use teseladb::engine::core::future::futurize_apply;
    futurize_apply(
        |_a: i32| {
            sleep(ms(100)).then(|()| -> Future<i32> {
                // `futurize`: a body that throws or returns a future.
                Future::from((|| -> Result<Future<i32>, ExceptionPtr> {
                    return Err(make_exception_ptr(ExpectedException::new()));
                    Ok(make_ready_future::<i32>(0))
                })())
            })
        },
        (0,),
    )
    .then_wrapped(|f: Future<i32>| expect_expected(f.get()))
});

engine_test_case!(futurize_invoke_val_future_ok, {
    use teseladb::engine::core::future::futurize_apply;
    futurize_apply(|a: i32| sleep(ms(100)).then(move |()| make_ready_future::<i32>(a * 100)), (2,))
        .then_wrapped(|f: Future<i32>| match f.get() {
            Ok(x) => assert_eq!(x, 200),
            Err(_) => panic!("should not have thrown"),
        })
});

engine_test_case!(futurize_invoke_void_exception, {
    use teseladb::engine::core::future::futurize_apply;
    futurize_apply(
        |_arg: i32| -> Result<(), ExceptionPtr> {
            Err(make_exception_ptr(ExpectedException::new()))
        },
        (0,),
    )
    .then_wrapped(|f: Future<()>| expect_expected(f.get()))
});

engine_test_case!(futurize_invoke_void_ok, {
    use teseladb::engine::core::future::futurize_apply;
    futurize_apply(|_arg: i32| {}, (0,)).then_wrapped(|f: Future<()>| {
        if f.get().is_err() {
            panic!("should not have thrown");
        }
    })
});

engine_test_case!(futurize_invoke_void_future_exception, {
    use teseladb::engine::core::future::futurize_apply;
    futurize_apply(
        |_a: i32| {
            sleep(ms(100)).then(|()| -> Result<(), ExceptionPtr> {
                Err(make_exception_ptr(ExpectedException::new()))
            })
        },
        (0,),
    )
    .then_wrapped(|f: Future<()>| expect_expected(f.get()))
});

engine_test_case!(futurize_invoke_void_future_ok, {
    use teseladb::engine::core::future::futurize_apply;
    let a = Rc::new(Cell::new(1));
    let a1 = a.clone();
    futurize_apply(
        move |a: Rc<Cell<i32>>| {
            sleep(ms(100)).then(move |()| {
                a.set(a.get() * 100);
            })
        },
        (a1,),
    )
    .then_wrapped(move |f: Future<()>| match f.get() {
        Ok(()) => assert_eq!(a.get(), 100),
        Err(_) => panic!("should not have thrown"),
    })
});

engine_test_case!(test_unused_shared_future_is_not_a_broken_future, {
    let mut p = Promise::<()>::new();
    let _s = SharedFuture::<()>::new(p.get_future());
    make_ready_future(())
});

engine_test_case!(test_shared_future_propagates_value_to_all, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut p = Promise::<Rc<i32>>::new(); // shared_ptr<> to check it deals with emptyable types
        let f = SharedFuture::<Rc<i32>>::new(p.get_future());

        let f1 = f.get_future();
        let f2 = f.get_future();

        p.set_value(Rc::new(1));
        assert!(*f1.get()? == 1);
        assert!(*f2.get()? == 1);
        Ok(())
    })
});

fn check_fails_with_expected<T: 'static>(f: Future<T>) {
    match f.get() {
        Ok(_) => panic!("Should have failed"),
        Err(e) if e.is::<ExpectedException>() => {
            // expected
        }
        Err(e) => panic!("{}", e),
    }
}

engine_test_case!(test_shared_future_propagates_value_to_copies, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut p = Promise::<i32>::new();
        let sf1 = SharedFuture::<i32>::new(p.get_future());
        let sf2 = sf1.clone();

        let f1 = sf1.get_future();
        let f2 = sf2.get_future();

        p.set_value(1);

        assert!(f1.get()? == 1);
        assert!(f2.get()? == 1);
        Ok(())
    })
});

engine_test_case!(test_obtaining_future_from_shared_future_after_it_is_resolved, {
    let mut p1 = Promise::<i32>::new();
    let mut p2 = Promise::<i32>::new();
    let sf1 = SharedFuture::<i32>::new(p1.get_future());
    let sf2 = SharedFuture::<i32>::new(p2.get_future());
    p1.set_value(1);
    p2.set_exception(make_exception_ptr(ExpectedException::new()));
    let f1 = sf1.get_future();
    sf2.get_future()
        .then_wrapped(move |f: Future<i32>| {
            check_fails_with_expected(f);
            f1
        })
        .then_wrapped(|f: Future<i32>| {
            assert!(f.get().ok() == Some(1));
        })
});

engine_test_case!(test_valueless_shared_future, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut p = Promise::<()>::new();
        let f = SharedFuture::<()>::new(p.get_future());

        let f1 = f.get_future();
        let f2 = f.get_future();

        p.set_value(());

        f1.get()?;
        f2.get()?;
        Ok(())
    })
});

engine_test_case!(test_shared_future_propagates_errors_to_all, {
    let mut p = Promise::<i32>::new();
    let f = SharedFuture::<i32>::new(p.get_future());

    let f1 = f.get_future();
    let f2 = f.get_future();

    p.set_exception(make_exception_ptr(ExpectedException::new()));

    f1.then_wrapped(move |f: Future<i32>| {
        check_fails_with_expected(f);
        f2
    })
    .then_wrapped(|f: Future<i32>| {
        check_fails_with_expected(f);
    })
});

engine_test_case!(test_ignored_future_warning, {
    // This doesn't warn:
    let mut p = Promise::<()>::new();
    p.set_exception(make_exception_ptr(ExpectedException::new()));
    let f: Future<()> = p.get_future();
    let mut f = f;
    f.ignore_ready_future();

    // And by analogy, neither should this
    let mut p2 = SharedPromise::<()>::new();
    p2.set_exception(make_exception_ptr(ExpectedException::new()));
    let f2: Future<()> = p2.get_shared_future();
    let mut f2 = f2;
    f2.ignore_ready_future();
    make_ready_future(())
});

engine_test_case!(test_futurize_from_tuple, {
    use teseladb::engine::core::future::Futurize;
    let v1: (i32,) = (3,);
    let v2: () = ();
    // `futurize<int>::from_tuple(v1)`: the futurize of a plain `int` is the one of
    // `Result<int, _>` (the library has no `Futurize` for plain values).
    let fut1: Future<i32> = <Result<i32, ExceptionPtr> as Futurize>::from_tuple(v1.0);
    let fut2: Future<()> = <() as Futurize>::from_tuple(v2);
    assert!(fut1.get().ok() == Some(v1.0));
    drop(fut2);
    make_ready_future(())
});

engine_test_case!(test_repeat_until_value, {
    use teseladb::engine::core::loop_::repeat_until_value;
    do_with(0i32, |counter: &mut i32| {
        let cp: *mut i32 = counter;
        repeat_until_value(move || -> Future<Option<i32>> {
            // SAFETY: do_with keeps `counter` alive until the returned future resolves.
            if unsafe { *cp } == 10000 {
                // SAFETY: do_with keeps `counter` alive until the returned future resolves; nothing else references it here.
                make_ready_future::<Option<i32>>(Some(unsafe { *cp }))
            } else {
                // SAFETY: do_with keeps `counter` alive until the returned future resolves; nothing else references it here.
                unsafe { *cp += 1 };
                make_ready_future::<Option<i32>>(None)
            }
        })
        .then(move |result: i32| {
            // SAFETY: do_with keeps `counter` alive until the returned future resolves; the loop has ended.
            assert!(unsafe { *cp } == 10000);
            // SAFETY: do_with keeps `counter` alive until the returned future resolves; the loop has ended.
            assert!(result == unsafe { *cp });
        })
    })
});

engine_test_case!(test_repeat_until_value_implicit_future, {
    use teseladb::engine::core::loop_::repeat_until_value;
    // Same as above, but returning `Option<i32>` instead of a future of it
    do_with(0i32, |counter: &mut i32| {
        let cp: *mut i32 = counter;
        // SAFETY: as in test_repeat_until_value.
        // A bare value is `Ok(value)`: `Futurize` has no impl for plain values.
        repeat_until_value(move || -> Result<Option<i32>, ExceptionPtr> {
            // SAFETY: do_with keeps `counter` alive until the returned future resolves; nothing else references it here.
            if unsafe { *cp } == 10000 {
                // SAFETY: do_with keeps `counter` alive until the returned future resolves; nothing else references it here.
                Ok(Some(unsafe { *cp }))
            } else {
                // SAFETY: do_with keeps `counter` alive until the returned future resolves; nothing else references it here.
                unsafe { *cp += 1 };
                Ok(None)
            }
        })
        .then(move |result: i32| {
            // SAFETY: do_with keeps `counter` alive until the returned future resolves; the loop has ended.
            assert!(unsafe { *cp } == 10000);
            // SAFETY: do_with keeps `counter` alive until the returned future resolves; the loop has ended.
            assert!(result == unsafe { *cp });
        })
    })
});

engine_test_case!(test_repeat_until_value_exception, {
    use teseladb::engine::core::loop_::repeat_until_value;
    repeat_until_value(|| -> Result<Option<i32>, ExceptionPtr> {
        return Err(make_exception_ptr(ExpectedException::new()));
        Ok(Some(43))
    })
    .then_wrapped(|f: Future<i32>| {
        check_fails_with_expected(f);
    })
});

engine_test_case!(test_when_allx, {
    use teseladb::engine::core::when_all::when_all;
    when_all((yield_(), yield_(), make_ready_future(()))).discard_result()
});

/// A noncopyable and nonmovable struct: Rust values always move, so it is only
/// noncopyable; the closure that owns it is called in place.
#[derive(Default)]
struct NonCopyNonMove;

engine_test_case!(test_when_all_functions, {
    use teseladb::engine::core::when_all::when_all;
    let x = NonCopyNonMove;
    let f = move || {
        let _ = &x;
        make_ready_future::<i32>(42)
    };
    when_all((
        f,
        (|| -> Future<()> {
            // `futurize`: a body that throws or returns a future.
            Future::from((|| -> Result<Future<()>, ExceptionPtr> {
                return Err(make_exception_ptr(IntException(42)));
                Ok(make_ready_future(()))
            })())
        }),
        yield_(),
    ))
    .then(|res: (Future<i32>, Future<()>, Future<()>)| {
        let (r0, mut r1, r2) = res;
        assert_eq!(r0.get().ok(), Some(42));
        assert!(r1.available());
        assert!(r1.failed());
        r1.ignore_ready_future();
        assert!(r2.available());
        assert!(!r2.failed());
        make_ready_future(())
    })
});

engine_test_case!(test_when_all_succeed_functions, {
    let x = NonCopyNonMove;
    let f = move || {
        let _ = &x;
        make_ready_future::<i32>(42)
    };
    when_all_succeed((
        f,
        (|| -> Future<()> {
            // `futurize`: a body that throws or returns a future.
            Future::from((|| -> Result<Future<()>, ExceptionPtr> {
                return Err(make_exception_ptr(IntException(42)));
                Ok(make_ready_future(()))
            })())
        }),
        yield_(),
    ))
    .then_wrapped(|mut res| {
        assert!(res.available());
        assert!(res.failed());
        res.ignore_ready_future();
        make_ready_future(())
    })
});

fn check_failed_with<E: 'static, T: 'static>(f: Future<T>) {
    assert!(f.failed());
    match f.get() {
        Ok(_) => panic!("exception expected"),
        Err(e) if e.is::<E>() => {
            // expected
        }
        Err(e) => panic!("wrong exception: {}", e),
    }
}

fn check_timed_out<T: 'static>(f: Future<T>) {
    check_failed_with::<teseladb::engine::core::timed_out_error::TimedOutError, T>(f);
}

engine_test_case!(test_with_timeout_when_it_times_out, {
    use teseladb::engine::core::manual_clock::ManualClock;
    use teseladb::engine::core::with_timeout::with_timeout;
    async_(|| -> Result<(), ExceptionPtr> {
        let mut pr = Promise::<()>::new();
        let f = with_timeout::<ManualClock, ()>(
            ManualClock::now() + Duration::from_secs(2),
            pr.get_future(),
        );

        assert!(!f.available());

        ManualClock::advance(Duration::from_secs(1));
        yield_().get()?;

        assert!(!f.available());

        ManualClock::advance(Duration::from_secs(1));
        yield_().get()?;

        check_timed_out(f);

        pr.set_value(());
        Ok(())
    })
});

engine_test_case!(test_shared_future_get_future_after_timeout, {
    use teseladb::engine::core::manual_clock::ManualClock;
    use teseladb::engine::core::shared_future::SharedFuture as SharedFutureWithClock;
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        // This used to crash because shared_future checked if the list of
        // pending futures was empty to decide if it had already called
        // then_wrapped. If all pending futures timed out, it would call
        // it again.
        let mut pr = Promise::<()>::new();
        let sfut = SharedFutureWithClock::<(), ManualClock>::new(pr.get_future());

        let fut1: Future<()> = sfut.get_future_until(ManualClock::now() + Duration::from_secs(1));

        ManualClock::advance(Duration::from_secs(1));

        check_timed_out(fut1);

        let fut2: Future<()> = sfut.get_future_until(ManualClock::now() + Duration::from_secs(1));
        ManualClock::advance(Duration::from_secs(1));
        check_timed_out(fut2);

        let fut3: Future<()> = sfut.get_future_until(ManualClock::now() + Duration::from_secs(1));
        pr.set_value(());
        fut3.get()?;
        Ok(())
    })
});

engine_test_case!(test_custom_exception_factory_in_with_timeout, {
    use teseladb::engine::core::manual_clock::ManualClock;
    use teseladb::engine::core::with_timeout::{TimeoutExceptionFactory, with_timeout_factory};
    async_(|| -> Result<(), ExceptionPtr> {
        /// The error of the custom timeout factory; it prints `timedout`.
        #[derive(Debug)]
        struct CustomError;
        impl fmt::Display for CustomError {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("timedout")
            }
        }
        impl std::error::Error for CustomError {}
        /// A timeout exception factory that makes [`CustomError`]s.
        struct MyExceptionFactory;
        impl TimeoutExceptionFactory for MyExceptionFactory {
            fn timeout() -> ExceptionPtr {
                make_exception_ptr(CustomError)
            }
        }
        let mut pr = Promise::<()>::new();
        let f = with_timeout_factory::<MyExceptionFactory, ManualClock, ()>(
            ManualClock::now() + Duration::from_secs(1),
            pr.get_future(),
        );
        ManualClock::advance(Duration::from_secs(1));
        yield_().get()?;
        check_failed_with::<CustomError, ()>(f);
        drop(pr);
        Ok(())
    })
});

engine_test_case!(test_with_timeout_when_it_does_not_time_out, {
    use teseladb::engine::core::manual_clock::ManualClock;
    use teseladb::engine::core::with_timeout::with_timeout;
    async_(|| -> Result<(), ExceptionPtr> {
        {
            let mut pr = Promise::<i32>::new();
            let f = with_timeout::<ManualClock, i32>(
                ManualClock::now() + Duration::from_secs(1),
                pr.get_future(),
            );

            pr.set_value(42);

            assert_eq!(f.get()?, 42);
        }

        // Check that timer was indeed cancelled
        ManualClock::advance(Duration::from_secs(1));
        yield_().get()?;
        Ok(())
    })
});

fn check_aborted<T: 'static>(f: Future<T>) {
    check_failed_with::<teseladb::engine::core::abort_source::AbortRequestedException, T>(f);
}

engine_test_case!(test_shared_future_with_timeout, {
    use teseladb::engine::core::manual_clock::ManualClock;
    use teseladb::engine::core::shared_future::SharedPromise as SharedPromiseWithClock;
    async_(|| -> Result<(), ExceptionPtr> {
        let mut pr = SharedPromiseWithClock::<i32, ManualClock>::new();
        let f1 = pr.get_shared_future_until(ManualClock::now() + Duration::from_secs(1));
        let f2 = pr.get_shared_future_until(ManualClock::now() + Duration::from_secs(2));
        let f3 = pr.get_shared_future();

        assert!(!f1.available());
        assert!(!f2.available());
        assert!(!f3.available());

        ManualClock::advance(Duration::from_secs(1));
        yield_().get()?;

        check_timed_out(f1);
        assert!(!f2.available());
        assert!(!f3.available());

        ManualClock::advance(Duration::from_secs(1));
        yield_().get()?;

        check_timed_out(f2);
        assert!(!f3.available());

        pr.set_value(42);

        assert_eq!(42, f3.get()?);
        Ok(())
    })
});

engine_test_case!(test_shared_future_with_abort, {
    use teseladb::engine::core::abort_source::AbortSource;
    use teseladb::engine::core::manual_clock::ManualClock;
    use teseladb::engine::core::shared_future::SharedPromise as SharedPromiseWithClock;
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut as_ = std::pin::pin!(AbortSource::new());
        let mut as2 = std::pin::pin!(AbortSource::new());
        let mut pr = SharedPromiseWithClock::<i32, ManualClock>::new();
        let f1 = pr.get_shared_future_abortable(as_.as_mut());
        let f2 = pr.get_shared_future_abortable(as2.as_mut());
        let f3 = pr.get_shared_future();

        assert!(!f1.available());
        assert!(!f2.available());
        assert!(!f3.available());

        as_.as_mut().request_abort();

        check_aborted(f1);
        assert!(!f2.available());
        assert!(!f3.available());

        as2.as_mut().request_abort();

        check_aborted(f2);
        assert!(!f3.available());

        pr.set_value(42);

        assert_eq!(42, f3.get()?);

        let f4 = pr.get_shared_future_abortable(as_.as_mut());
        assert!(f4.available());
        Ok(())
    })
});

engine_test_case!(test_shared_promise_with_outstanding_future_is_immediately_available, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut pr1 = SharedPromise::<()>::new();
        let f1 = pr1.get_shared_future();
        pr1.set_value(());
        assert!(pr1.available());
        assert!(f1.get().is_ok());

        let mut pr2 = SharedPromise::<()>::new();
        let f2 = pr2.get_shared_future();
        pr2.set_exception(make_exception_ptr(RuntimeError::new("oops")));
        assert!(pr2.available());
        assert!(matches!(f2.get(), Err(ref e) if e.is::<RuntimeError>()));
        Ok(())
    })
});

/// Reads the internal state of a shared future.
struct SharedFutureTester;

impl SharedFutureTester {
    fn has_scheduled_task<T: Clone + 'static>(f: &SharedFuture<T>) -> bool {
        f.has_scheduled_task()
    }
}

engine_test_case!(test_shared_future_task_scheduled_only_if_there_are_waiting_futures, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        {
            // Case 1: promise is eventually satisfied, get_future is not called
            let mut pr1 = Promise::<()>::new();
            let f1 = SharedFuture::<()>::new(pr1.get_future());
            assert!(!SharedFutureTester::has_scheduled_task(&f1));
            pr1.set_value(());
            // get_future was not called, so no task should have been scheduled
            assert!(!SharedFutureTester::has_scheduled_task(&f1));
        }

        {
            // Case 2: promise is eventually satisfied, get_future was called
            let mut pr2 = Promise::<()>::new();
            let f2 = SharedFuture::<()>::new(pr2.get_future());
            let f2f = f2.get_future();
            // get_future was called, so the task is scheduled to happen after promise is resolved
            assert!(SharedFutureTester::has_scheduled_task(&f2));
            pr2.set_value(());
            f2f.get()?;
            // f2f is resolved by shared future's task, so it must have run
            assert!(!SharedFutureTester::has_scheduled_task(&f2));
        }

        {
            // Case 3: shared future is ready from the start
            let f3 = SharedFuture::<()>::new(make_ready_future(()));
            assert!(!SharedFutureTester::has_scheduled_task(&f3));
            let f3f = f3.get_future();
            // Calling get_future on a ready shared future does not schedule the task
            assert!(!SharedFutureTester::has_scheduled_task(&f3));
            assert!(f3f.available());
        }
        Ok(())
    })
});

/// Whether `excp` is a thrown integer; asserts that it satisfies `pred`.
fn int_matches(excp: &ExceptionPtr, pred: impl Fn(i32) -> bool) -> bool {
    match int_of(excp) {
        Some(v) => {
            assert!(pred(v));
            true
        }
        None => false,
    }
}

engine_test_case!(test_when_all_succeed_tuples, {
    // The value-less futures contribute `()` to the unpacked tuple: when_all_succeed over a
    // tuple keeps `()` for each of them.
    when_all_succeed((
        make_ready_future(()),
        make_ready_future::<Sstring>(Sstring::from("hello world")),
        make_ready_future::<i32>(42),
        make_ready_future(()),
        make_ready_future::<(i32, Sstring)>((84, Sstring::from("hi"))),
        make_ready_future::<bool>(true),
    ))
    .then_unpack(|(), msg: Sstring, v: i32, (), t: (i32, Sstring), b: bool| {
        assert_eq!(msg, "hello world");
        assert_eq!(v, 42);
        assert_eq!(t.0, 84);
        assert_eq!(t.1, "hi");
        assert!(b);

        when_all_succeed((
            make_exception_future::<()>(make_exception_ptr(IntException(42))),
            make_ready_future::<Sstring>(Sstring::from("hello world")),
            make_exception_future::<i32>(make_exception_ptr(IntException(43))),
            make_ready_future(()),
        ))
        .then_unpack(|(), _: Sstring, _: i32, ()| -> Result<bool, ExceptionPtr> {
            panic!("shouldn\'t reach");
            Ok(false)
        })
        .handle_exception(|excp: ExceptionPtr| -> Result<bool, ExceptionPtr> {
            Ok(int_matches(&excp, |v| v == 42 || v == 43))
        })
        .then(|ret: bool| {
            assert!(ret);
        })
    })
});

engine_test_case!(test_when_all_succeed_vector_overload, {
    use teseladb::engine::core::when_all::when_all_succeed_vec;
    let mut vecs_noexcept: Vec<Future<i32>> = Vec::with_capacity(10);
    for i in 0..10 {
        vecs_noexcept.push(if i % 2 == 0 {
            make_ready_future::<i32>(42)
        } else {
            yield_().then(|()| make_ready_future(42))
        });
    }

    let mut vecs_except: Vec<Future<i32>> = Vec::with_capacity(10);
    for i in 0..10 {
        vecs_except.push(if i % 2 == 0 {
            make_ready_future::<i32>(42)
        } else {
            make_exception_future::<i32>(make_exception_ptr(IntException(43)))
        });
    }

    when_all_succeed_vec(vecs_noexcept)
        .then(move |vals: Vec<i32>| {
            let all = vals.iter().all(|&val| val == 42);
            assert!(all);
            when_all_succeed_vec(vecs_except)
        })
        .then_wrapped(|vals_fut: Future<Vec<i32>>| -> Result<bool, ExceptionPtr> {
            let _vals = vals_fut.get()?;
            panic!("shouldn't reach");
            Ok(false)
        })
        .handle_exception(|excp: ExceptionPtr| -> Result<bool, ExceptionPtr> {
            Ok(int_matches(&excp, |v| v == 43))
        })
        .then(|ret: bool| {
            assert!(ret);
        })
});

engine_test_case!(test_when_all_succeed_vector, {
    use teseladb::engine::core::when_all::when_all_succeed_iter_void;
    let vecs: Vec<Future<()>> = vec![
        make_ready_future(()),
        make_ready_future(()),
        make_ready_future(()),
        make_ready_future(()),
    ];
    when_all_succeed_iter_void(vecs)
        .then(|()| {
            let vecs: Vec<Future<()>> = vec![
                make_ready_future(()),
                make_ready_future(()),
                make_exception_future::<()>(make_exception_ptr(IntException(42))),
                make_exception_future::<()>(make_exception_ptr(IntException(43))),
            ];
            when_all_succeed_iter_void(vecs)
        })
        .then(|()| -> Result<bool, ExceptionPtr> {
            panic!("shouldn\'t reach");
            Ok(false)
        })
        .handle_exception(|excp: ExceptionPtr| -> Result<bool, ExceptionPtr> {
            Ok(int_matches(&excp, |v| v == 42 || v == 43))
        })
        .then(|ret: bool| {
            assert!(ret);
            let vecs: Vec<Future<i32>> = vec![
                make_ready_future::<i32>(1),
                make_ready_future::<i32>(2),
                make_ready_future::<i32>(3),
            ];
            when_all_succeed_iter(vecs)
        })
        .then(|vals: Vec<i32>| {
            assert_eq!(vals.len(), 3usize);
            assert_eq!(vals[0], 1);
            assert_eq!(vals[1], 2);
            assert_eq!(vals[2], 3);

            let vecs: Vec<Future<i32>> = vec![
                make_ready_future::<i32>(1),
                make_ready_future::<i32>(2),
                make_exception_future::<i32>(make_exception_ptr(IntException(42))),
                make_exception_future::<i32>(make_exception_ptr(IntException(43))),
            ];
            when_all_succeed_iter(vecs)
        })
        .then(|_: Vec<i32>| -> Result<bool, ExceptionPtr> {
            panic!("shouldn\'t reach");
            Ok(false)
        })
        .handle_exception(|excp: ExceptionPtr| -> Result<bool, ExceptionPtr> {
            Ok(int_matches(&excp, |v| v == 42 || v == 43))
        })
        .then(|ret: bool| {
            assert!(ret);
        })
});

engine_test_case!(test_futurize_mutable, {
    let mut count = 0;
    repeat(move || {
        count += 1;
        if count == 3 {
            return make_ready_future(StopIteration::YES);
        }
        make_ready_future(StopIteration::NO)
    })
});

engine_test_case!(test_broken_promises, {
    use teseladb::engine::core::future::BrokenPromise;
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let f: Rc<std::cell::RefCell<Option<Future<()>>>> = Rc::new(std::cell::RefCell::new(None));
        let f2: Option<Future<()>>;

        {
            // Broken after attaching a continuation
            let mut p = Promise::<()>::new();
            // then_wrapped consumes the
            // future, so `f` is empty from here on, as the continuation leaves it.
            let fut = p.get_future();
            let fc = f.clone();
            f2 = Some(fut.then_wrapped(move |f3: Future<()>| {
                assert!(f3.failed());
                assert!(matches!(f3.get(), Err(ref e) if e.is::<BrokenPromise>()));
                *fc.borrow_mut() = None;
            }));
        }
        f2.expect("set above").get()?;
        assert!(f.borrow().is_none());

        {
            // Broken before attaching a continuation
            let mut p = Promise::<()>::new();
            *f.borrow_mut() = Some(p.get_future());
        }
        let fut = f.borrow_mut().take().expect("set above");
        let fc = f.clone();
        fut.then_wrapped(move |f3: Future<()>| {
            assert!(f3.failed());
            assert!(matches!(f3.get(), Err(ref e) if e.is::<BrokenPromise>()));
            *fc.borrow_mut() = None;
        })
        .get()?;
        assert!(f.borrow().is_none());

        {
            // Broken before suspending a thread
            let mut p = Promise::<()>::new();
            *f.borrow_mut() = Some(p.get_future());
        }

        let fut = f.borrow_mut().take().expect("set above");
        assert!(matches!(fut.get(), Err(ref e) if e.is::<BrokenPromise>()));
        Ok(())
    })
});

engine_test_case!(test_warn_on_broken_promise_with_no_future, {
    use teseladb::engine::core::reactor::ReactorTest;
    // Example code where we expect a "Exceptional future ignored"
    // warning.
    let mut p = Promise::<()>::new();
    // Intentionally destroy the future
    let _ = p.get_future();

    let pp: *mut Promise<()> = &mut p;
    ReactorTest::with_allow_abandoned_failed_futures(
        1,
        Box::new(move || {
            // SAFETY: the function runs synchronously, inside this call, while `p` is alive
            // and not otherwise borrowed.
            unsafe { (*pp).set_exception(make_exception_ptr(RuntimeError::new("foo"))) };
        }),
    );

    make_ready_future(())
});

engine_test_case!(test_destroy_promise_after_state_take_value, {
    let f: Future<()>;
    let mut p = Some(Box::new(Promise::<()>::new()));
    f = p.as_mut().expect("set").get_future();
    p.as_mut().expect("set").set_value(());
    let g = f.then(|()| {});
    p = None;
    assert!(p.is_none());
    g
});

engine_test_case!(test_exception_future_with_backtrace, {
    use teseladb::engine::core::future::make_exception_future_with_backtrace;
    use teseladb::engine::core::reactor::ReactorTest;
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let counter = Rc::new(Cell::new(0));
        let c = counter.clone();
        let inner = move |return_exception: bool| -> Future<i32> {
            if !return_exception {
                c.set(c.get() + 1);
                make_ready_future::<i32>(c.get())
            } else {
                make_exception_future_with_backtrace::<i32, _>(ExpectedException::new())
            }
        };
        let outer = move |return_exception: bool| {
            inner(return_exception).then(|i: i32| make_ready_future::<i32>(-i))
        };

        assert_eq!(outer(false).get()?, -1);
        assert_eq!(counter.get(), 1);

        assert!(matches!(outer(true).get(), Err(ref e) if e.is::<ExpectedException>()));

        assert_eq!(counter.get(), 1);

        // Example code where we expect a "Exceptional future ignored"
        // warning.
        let _ = outer(true).then_wrapped(|fut: Future<i32>| {
            ReactorTest::with_allow_abandoned_failed_futures(
                1,
                Box::new(move || {
                    let _foo = fut;
                }),
            );
        });
        Ok(())
    })
});

/// A value whose move fails. A Rust move runs no code, so the
/// failing move is the explicit [`ThrowOnMove::try_move`], called where the test
/// moves the argument into the thread.
struct ThrowOnMove {
    i: i32,
}

impl ThrowOnMove {
    fn new(i: i32) -> Self {
        ThrowOnMove { i }
    }

    /// The failing move: always an error.
    fn try_move(&mut self) -> Result<ThrowOnMove, ExceptionPtr> {
        self.i = -1;
        Err(make_exception_ptr(ExpectedException::new()))
    }

    fn value(&self) -> i32 {
        self.i
    }
}

engine_test_case!(test_async_throw_on_move, {
    let mut arg = ThrowOnMove::new(0);
    let fut: Future<()> = match arg.try_move() {
        Ok(t) => async_(move || -> () {
            let _ = t.value();
            panic!("must not be reached");
        }),
        Err(e) => make_exception_future(e),
    };
    fut.handle_exception_type(|_: &ExpectedException| make_ready_future(()))
});

fn func4() -> Future<()> {
    use teseladb::engine::core::reactor::engine_logger;
    use teseladb::engine::util::backtrace::current_backtrace;
    yield_().then(|()| {
        teseladb::log_info!(engine_logger(), "backtrace: {}", current_backtrace());
    })
}

fn func3() -> Result<(), ExceptionPtr> {
    async_(|| func4().get()).get()
}

fn func2() -> Future<()> {
    async_(func3)
}

fn func1() -> Future<()> {
    yield_().then(|()| func2())
}

engine_test_case!(test_backtracing, { thread_async(move || func1().get()) });

engine_test_case!(test_then_unpack, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        make_ready_future::<()>(())
            .then_unpack(|| {
                // Reaching it is the check.
            })
            .get()?;
        make_ready_future::<(i32,)>((1,))
            .then_unpack(|x: i32| {
                assert!(x == 1);
            })
            .get()?;
        make_ready_future::<(i32, i64)>((1, 2))
            .then_unpack(|x: i32, y: i64| {
                assert!(x == 1 && y == 2);
            })
            .get()?;
        make_ready_future::<(Box<i32>,)>((Box::new(42),))
            .then_unpack(|p1: Box<i32>| {
                assert!(*p1 == 42);
            })
            .get()?;
        Ok(())
    })
});

fn test_then_function_f() -> Future<()> {
    make_ready_future(())
}

engine_test_case!(test_then_function, { make_ready_future(()).then(|()| test_then_function_f()) });

engine_test_case!(test_with_gate, {
    use teseladb::engine::core::gate::{GateClosedException, try_with_gate, with_gate};
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let g = std::pin::pin!(Gate::new());
        let counter = Rc::new(Cell::new(0));
        let gate_closed_errors = Rc::new(Cell::new(0));
        let other_errors = Rc::new(Cell::new(0));
        let reset = || {
            counter.set(0);
            gate_closed_errors.set(0);
            other_errors.set(0);
        };
        let inc = |c: &Rc<Cell<i32>>| c.set(c.get() + 1);

        // test normal operation when gate is opened
        let c = counter.clone();
        // `with_gate` throws (here: returns `Err`) before giving a future when the gate is closed.
        assert!(with_gate(g.as_ref(), move || inc(&c)).and_then(Future::get).is_ok());
        assert_eq!(counter.get(), 1);

        // test that an exception returned by the calling func
        // is propagated to with_gate future
        reset();
        let (c, gce, oe) = (counter.clone(), gate_closed_errors.clone(), other_errors.clone());
        assert!(
            with_gate(g.as_ref(), move || {
                inc(&c);
                make_exception_future::<()>(make_exception_ptr(ExpectedException::new()))
            })?
            .handle_exception_type(move |_: &GateClosedException| {
                inc(&gce);
            })
            .handle_exception(move |_: ExceptionPtr| {
                inc(&oe);
            })
            .get()
            .is_ok()
        );
        assert!(counter.get() != 0);
        assert!(gate_closed_errors.get() == 0);
        assert!(other_errors.get() != 0);

        g.close().get()?;

        // test that with_gate.get() throws when the gate is closed
        reset();
        let c = counter.clone();
        assert!(
            matches!(with_gate(g.as_ref(), move || inc(&c)).and_then(Future::get), Err(ref e) if e.is::<GateClosedException>())
        );
        assert!(counter.get() == 0);

        // test that with_gate throws when the gate is closed
        reset();
        let (c, gce, oe) = (counter.clone(), gate_closed_errors.clone(), other_errors.clone());
        // `with_gate` fails before returning a future when the gate is closed, so no
        // continuation is ever attached.
        let r = with_gate(g.as_ref(), move || inc(&c));
        let _ = (&gce, &oe);
        assert!(matches!(r, Err(ref e) if e.is::<GateClosedException>()));
        assert!(counter.get() == 0);
        assert!(gate_closed_errors.get() == 0);
        assert!(other_errors.get() == 0);

        // test that try_with_gate returns gate_closed_exception when the gate is closed
        reset();
        let (c, gce, oe) = (counter.clone(), gate_closed_errors.clone(), other_errors.clone());
        try_with_gate(g.as_ref(), move || inc(&c))
            .handle_exception_type(move |_: &GateClosedException| {
                inc(&gce);
            })
            .handle_exception(move |_: ExceptionPtr| {
                inc(&oe);
            })
            .get()?;
        assert!(counter.get() == 0);
        assert!(gate_closed_errors.get() != 0);
        assert!(other_errors.get() == 0);
        Ok(())
    })
});

engine_test_case!(test_max_concurrent_for_each, {
    use teseladb::engine::core::loop_::{
        max_concurrent_for_each, max_concurrent_for_each_unchecked,
    };
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        println!("empty range");
        max_concurrent_for_each(Vec::<i32>::new(), 3, |_: i32| -> Future<()> {
            panic!("should not reach");
        })
        .get()?;

        let range: Vec<i32> = (1..8).collect();
        let sum = Rc::new(Cell::new(0));
        let adder = |sum: &Rc<Cell<i32>>| {
            let s = sum.clone();
            move |v: i32| {
                s.set(s.get() + v);
                make_ready_future(())
            }
        };

        println!("iterator");
        sum.set(0);
        // SAFETY: `range` lives, unchanged, until the end of this thread function, and the
        // loop is waited for (get()) right here.
        unsafe { max_concurrent_for_each_unchecked(range.iter().copied(), 3, adder(&sum)) }
            .get()?;
        assert_eq!(sum.get(), 28);

        println!("const iterator");
        sum.set(0);
        // SAFETY: `range` lives, unchanged, until the end of this thread function, and the
        // loop is waited for (get()) right here.
        unsafe { max_concurrent_for_each_unchecked(range.iter().copied(), 3, adder(&sum)) }
            .get()?;
        assert_eq!(sum.get(), 28);

        println!("reverse iterator");
        sum.set(0);
        // SAFETY: `range` lives, unchanged, until the end of this thread function, and the
        // loop is waited for (get()) right here.
        unsafe { max_concurrent_for_each_unchecked(range.iter().rev().copied(), 3, adder(&sum)) }
            .get()?;
        assert_eq!(sum.get(), 28);

        println!("immediate result");
        sum.set(0);
        max_concurrent_for_each(range.clone(), 3, adder(&sum)).get()?;
        assert_eq!(sum.get(), 28);

        println!("suspend");
        sum.set(0);
        let s = sum.clone();
        max_concurrent_for_each(range.clone(), 3, move |v: i32| {
            let s = s.clone();
            yield_().then(move |()| {
                s.set(s.get() + v);
            })
        })
        .get()?;
        assert_eq!(sum.get(), 28);

        println!("throw immediately");
        sum.set(0);
        let s = sum.clone();
        assert!(
            max_concurrent_for_each(range.clone(), 3, move |v: i32| -> Future<()> {
                // `futurize`: a body that throws or returns a future.
                Future::from((|| -> Result<Future<()>, ExceptionPtr> {
                    s.set(s.get() + v);
                    if v == 1 {
                        return Err(make_exception_ptr(IntException(5)));
                    }
                    Ok(make_ready_future(()))
                })())
            })
            .get()
            .err()
            .and_then(|e| int_of(&e))
                == Some(5)
        );
        assert_eq!(sum.get(), 28);

        println!("throw after suspension");
        sum.set(0);
        let s = sum.clone();
        assert!(
            max_concurrent_for_each(range.clone(), 3, move |v: i32| {
                let s = s.clone();
                yield_().then(move |()| -> Result<(), ExceptionPtr> {
                    s.set(s.get() + v);
                    if v == 2 {
                        return Err(make_exception_ptr(IntException(5)));
                    }
                    Ok(())
                })
            })
            .get()
            .err()
            .and_then(|e| int_of(&e))
                == Some(5)
        );

        println!("concurrency higher than vector length");
        sum.set(0);
        max_concurrent_for_each(range.clone(), range.len() + 3, adder(&sum)).get()?;
        assert_eq!(sum.get(), 28);
        Ok(())
    })
});

engine_test_case!(test_for_each_set, {
    use teseladb::engine::core::bitset_iter::bitsets::{Bitset, for_each_set};
    use teseladb::engine::core::loop_::do_for_each;
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut s = Bitset::<32>(0); // std::bitset<32>
        s.0 |= 1 << 4;
        s.0 |= 1 << 0;
        let range = for_each_set(s, 0);
        let res = Rc::new(Cell::new(0u32));
        let r = res.clone();
        do_for_each(range, move |i: i32| {
            r.set(r.get() | (1 << i));
        })
        .get()?;
        assert_eq!(res.get(), 17);
        Ok(())
    })
});

engine_test_case!(test_yield, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let flag = flag();
        let f = flag.clone();
        let one = yield_().then(move |()| {
            f.set(true);
        });
        assert!(!flag.get());
        one.get()?;
        assert!(flag.get());

        #[cfg(not(engine_debug))]
        {
            // same thing, with now(), but for non-DEBUG only, otherwise .then() doesn't
            // use the ready-future fast-path and always schedules a task
            flag.set(false);
            let f = flag.clone();
            let _two = now().then(move |()| {
                f.set(true);
            });
            // now() does not yield
            assert!(flag.get());
        }
        Ok(())
    })
});

// make_exception_future() takes an exception pointer: one made from a value (here with
// make_exception_ptr), or an existing one, passed by value, moved, or cloned from one that
// must stay untouched. The future must fail with the exception the pointer holds, never
// with the pointer itself.
engine_test_case!(test_make_exception_future, {
    // When the pointer is made from a value - like an int or a
    // runtime error - the value is stored in the
    // future:
    let f1: Future<()> = make_exception_future(make_exception_ptr(IntException(3)));
    assert!(f1.failed());
    assert!(f1.get().err().and_then(|e| int_of(&e)) == Some(3));
    let f2: Future<()> = make_exception_future(make_exception_ptr(RuntimeError::new("hello")));
    assert!(f2.failed());
    assert!(matches!(f2.get(), Err(ref e) if e.is::<RuntimeError>()));
    // However, if make_exception_future() is given an existing exception pointer
    // the exception stored in the future will be
    // the one the pointer holds - not the pointer
    // itself.
    let e3 = make_exception_ptr(IntException(3));
    let f3: Future<()> = make_exception_future(e3.clone());
    assert!(f3.failed());
    assert!(f3.get().err().and_then(|e| int_of(&e)) == Some(3)); // expecting the int, not the pointer
    // If make_exception_future() is given a temporary exception pointer,
    // it should also work correctly:
    // A temporary:
    let f4: Future<()> = make_exception_future(make_exception_ptr(IntException(3)));
    assert!(f4.failed());
    assert!(f4.get().err().and_then(|e| int_of(&e)) == Some(3)); // expecting the int, not the pointer
    // A moved pointer:
    let e5 = make_exception_ptr(IntException(3));
    let f5: Future<()> = make_exception_future(e5); // moved
    assert!(f5.failed());
    assert!(f5.get().err().and_then(|e| int_of(&e)) == Some(3)); // expecting the int, not the pointer
    // A clone of a pointer that must stay untouched:
    // a pointer that cannot be moved from can
    // happen when it is captured by a closure that does not own it;
    // cloning it works.
    let e6 = make_exception_ptr(IntException(3)); // never moved from
    let f6: Future<()> = make_exception_future(e6.clone()); // a clone
    assert!(f6.failed());
    assert!(f6.get().err().and_then(|e| int_of(&e)) == Some(3)); // expecting the int, not the pointer
    drop(e3);
    drop(e6);

    make_ready_future(())
});

// Reproduce a use-after-free of a background task.
engine_test_case!(test_run_in_background, {
    use teseladb::engine::core::reactor::engine;
    use teseladb::engine::core::smp::Smp;
    // SAFETY: on this shard's reactor thread; the reactor is reached for this call only.
    unsafe {
        (*engine())
            .run_in_background_fn(|| sleep(ms(1)).then(|()| Smp::invoke_on_all(|| sleep(ms(1)))))
    };
    make_ready_future(())
});

engine_test_case!(test_manual_clock_advance, {
    use teseladb::engine::core::manual_clock::ManualClock;
    use teseladb::engine::core::timer::Timer;
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let expired = flag();
        let e = expired.clone();
        let mut t = std::pin::pin!(Timer::<ManualClock>::with_callback(move || {
            e.set(true);
        }));
        t.as_mut().arm_in(ms(2));
        ManualClock::advance(ms(1));
        assert!(!expired.get());
        ManualClock::advance(ms(1));
        assert!(expired.get());
        Ok(())
    })
});

engine_test_case!(test_ready_future_across_shards, {
    use teseladb::engine::core::reactor::engine_logger;
    use teseladb::engine::core::shard_id::this_shard_id;
    use teseladb::engine::core::smp::Smp;
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        if Smp::count() == 1 {
            teseladb::log_info!(
                engine_logger(),
                "test_ready_future_across_shards requires at least 2 shards"
            );
            return Ok(());
        }
        let other_shard = (this_shard_id() + 1) % Smp::count();
        /// The ready future moved to the other shard (a `Future` is not `Send`).
        struct ReadyFuture(Future<i32>);
        // SAFETY: a ready future holds only its value (an `i32`), with no promise or task
        // linked to it, so moving it to another thread touches nothing shared.
        unsafe impl Send for ReadyFuture {}
        let f1 = ReadyFuture(make_ready_future::<i32>(42));
        assert!(f1.0.available());
        Smp::submit_to(other_shard, move || {
            let f1 = f1;
            assert_eq!(f1.0.get().ok(), Some(42));
        })
        .get()?;
        Ok(())
    })
});

engine_test_case!(test_foreign_promise_set_value, {
    use teseladb::engine::core::reactor::engine_logger;
    use teseladb::engine::core::shard_id::this_shard_id;
    use teseladb::engine::core::smp::Smp;
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        if Smp::count() == 1 {
            teseladb::log_info!(
                engine_logger(),
                "test_foreign_promise_set_value requires at least 2 shards"
            );
            return Ok(());
        }
        let mut pr = Promise::<u32>::new();
        let other_shard = (this_shard_id() + 1) % Smp::count();
        let getter = pr.get_future();
        // The promise crosses to the other shard as an address.
        let prp = &mut pr as *mut Promise<u32> as usize;
        let setter = Smp::submit_to(other_shard, move || {
            // SAFETY: `pr` outlives the call, waited for right below.
            unsafe { (*(prp as *mut Promise<u32>)).set_value(this_shard_id()) };
        });
        setter.get()?;
        assert_eq!(getter.get()?, other_shard);
        Ok(())
    })
});
