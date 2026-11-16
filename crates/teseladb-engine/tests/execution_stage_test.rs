//! Tests of execution stages: calls are batched and run in order, values and errors come
//! back through futures, arguments are moved rather than copied, references pass through,
//! and stages run in the scheduling group they were created for. A stage takes its
//! arguments as one tuple; `stage(a, b)` is `stage.call((a, b))`.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/minstd.rs"]
mod minstd;

use std::cell::Cell;
use std::fmt;
use std::rc::Rc;
use std::time::Duration;

use minstd::UniformIntDistribution;
use teseladb::engine::core::execution_stage::{
    InheritingConcreteExecutionStage, make_execution_stage_default,
};
use teseladb::engine::core::future::{
    ExceptionPtr, Future, InvalidArgument, make_exception_future, make_ready_future,
};
use teseladb::engine::core::reactor::{
    create_scheduling_group_with_shares, destroy_scheduling_group,
};
use teseladb::engine::core::scheduling::{SchedulingGroup, current_scheduling_group};
use teseladb::engine::core::shared_ptr::make_shared;
use teseladb::engine::core::sleep::sleep_steady as sleep;
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::core::thread::{Thread, ThreadAttributes, async_};
use teseladb::engine::testing::random::testing::local_random;
use teseladb::engine::util::defer::defer;
use teseladb::engine_test_case;

fn name(s: &str) -> Sstring {
    Sstring::from(s)
}

/// A thrown integer.
#[derive(Debug, Clone, Copy)]
struct IntException(i32);

impl fmt::Display for IntException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for IntException {}

engine_test_case!(test_create_stage_from_lvalue_function_object, {
    async_(|| -> Result<(), ExceptionPtr> {
        let obj = make_shared(53);
        // A value that cannot fail is `Ok` (Futurize has no impl for plain values).
        let dont_move = move |()| Ok::<i32, ExceptionPtr>(*obj);
        let mut stage = make_execution_stage_default(&name("test"), dont_move.clone())?;
        assert_eq!(stage.call(()).get()?, 53);
        assert!(matches!(dont_move(()), Ok(53)));
        Ok(())
    })
});

engine_test_case!(test_create_stage_from_rvalue_function_object, {
    async_(|| -> Result<(), ExceptionPtr> {
        let obj = Box::new(42);
        let dont_copy = move |()| Ok::<i32, ExceptionPtr>(*obj);
        let mut stage = make_execution_stage_default(&name("test"), dont_copy)?;
        assert_eq!(stage.call(()).get()?, 42);
        Ok(())
    })
});

fn func(_: ()) -> Result<i32, ExceptionPtr> {
    Ok(64)
}

engine_test_case!(test_create_stage_from_function, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut stage = make_execution_stage_default(&name("test"), func)?;
        assert_eq!(stage.call(()).get()?, 64);
        Ok(())
    })
});

fn test_simple_execution_stage<F, R>(
    func: F,
    verify: impl Fn(i32, Future<i32>),
) -> Result<(), ExceptionPtr>
where
    F: FnMut(i32) -> R + 'static,
    R: teseladb::engine::core::future::Futurize<Output = i32>,
{
    let mut stage = make_execution_stage_default(&name("test"), func)?;

    let dist = UniformIntDistribution::new(0, 100_000);
    let vs: Vec<i32> =
        (0..1_000).map(|_| dist.sample_fn(&mut || u64::from(local_random())) as i32).collect();

    let mut fs: Vec<Future<i32>> = Vec::new();
    for &v in &vs {
        fs.push(stage.call(v));
    }

    for (i, f) in fs.into_iter().enumerate() {
        verify(vs[i], f);
    }
    Ok(())
}

fn verify_doubled(input: i32, result: Future<i32>) {
    if input % 2 != 0 {
        assert_eq!(input * 2, result.get().expect("value"));
    } else {
        let e = result.get().expect_err("int");
        assert!(e.downcast_ref::<IntException>().is_some_and(|v| v.0 == input));
    }
}

engine_test_case!(test_simple_stage_returning_int, {
    async_(|| {
        test_simple_execution_stage(
            |x: i32| -> Result<i32, ExceptionPtr> {
                if x % 2 != 0 { Ok(x * 2) } else { Err(ExceptionPtr::new(IntException(x))) }
            },
            verify_doubled,
        )
    })
});

engine_test_case!(test_simple_stage_returning_future_int, {
    async_(|| {
        test_simple_execution_stage(
            |x: i32| {
                if x % 2 != 0 {
                    make_ready_future::<i32>(x * 2)
                } else {
                    make_exception_future::<i32>(ExceptionPtr::new(IntException(x)))
                }
            },
            verify_doubled,
        )
    })
});

fn test_execution_stage_avoids_copy<T: Default + 'static>() -> Result<(), ExceptionPtr> {
    let mut stage =
        make_execution_stage_default(&name("test"), |obj: T| Ok::<T, ExceptionPtr>(obj))?;

    let f = stage.call(T::default());
    let obj: T = f.get()?;
    let _ = obj;
    Ok(())
}

engine_test_case!(test_stage_moves_when_cannot_copy, {
    async_(|| {
        /// Not `Clone`: it can only be moved.
        #[derive(Default)]
        struct NoncopyableButMovable;
        test_execution_stage_avoids_copy::<NoncopyableButMovable>()
    })
});

engine_test_case!(test_stage_prefers_move_to_copy, {
    async_(|| {
        #[derive(Default)]
        struct CopyableAndMovable;
        impl Clone for CopyableAndMovable {
            fn clone(&self) -> Self {
                panic!("should not copy");
            }
        }
        test_execution_stage_avoids_copy::<CopyableAndMovable>()
    })
});

engine_test_case!(test_rref_decays_to_value, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut stage = make_execution_stage_default(&name("test"), |vec: Vec<i32>| {
            Ok::<usize, ExceptionPtr>(vec.len())
        })?;

        let mut tmp: Vec<i32> = Vec::new();
        let mut fs: Vec<Future<usize>> = Vec::new();
        for i in 0..100 {
            tmp.resize(i, 0);
            fs.push(stage.call(std::mem::take(&mut tmp)));
            tmp = Vec::new();
        }

        for (i, f) in fs.into_iter().enumerate() {
            assert_eq!(f.get()?, i);
        }
        Ok(())
    })
});

/// A reference the stage passes through unchanged.
#[derive(Clone, Copy)]
struct ReferenceWrapper<T>(*mut T);

impl<T> ReferenceWrapper<T> {
    // A mutable reference from a shared handle.
    #[allow(clippy::mut_from_ref)]
    fn get(&self) -> &mut T {
        // SAFETY: the referent outlives every call of the stage in the tests.
        unsafe { &mut *self.0 }
    }
}

engine_test_case!(test_lref_does_not_decay, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut stage = make_execution_stage_default(&name("test"), |v: ReferenceWrapper<i32>| {
            *v.get() += 1;
        })?;

        let mut value = 0i32;
        let mut fs: Vec<Future<()>> = Vec::new();
        for _ in 0..100 {
            //fs.emplace_back(stage(value)); // should fail to compile
            fs.push(stage.call(ReferenceWrapper(&mut value)));
        }

        for f in fs {
            f.get()?;
        }
        assert_eq!(value, 100);
        Ok(())
    })
});

engine_test_case!(test_explicit_reference_wrapper_is_not_unwrapped, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut stage = make_execution_stage_default(&name("test"), |v: ReferenceWrapper<i32>| {
            *v.get() += 1;
        })?;

        let mut value = 0i32;
        let mut fs: Vec<Future<()>> = Vec::new();
        for _ in 0..100 {
            //fs.emplace_back(stage(value)); // should fail to compile
            fs.push(stage.call(ReferenceWrapper(&mut value)));
        }

        for f in fs {
            f.get()?;
        }
        assert_eq!(value, 100);
        Ok(())
    })
});

engine_test_case!(test_function_is_class_member, {
    async_(|| -> Result<(), ExceptionPtr> {
        struct Foo {
            value: i32,
        }
        impl Foo {
            fn member(&mut self, x: i32) -> i32 {
                std::mem::replace(&mut self.value, x)
            }
        }

        let mut stage =
            make_execution_stage_default(&name("test"), |(this, x): (*mut Foo, i32)| {
                // SAFETY: the object outlives the stage calls.
                Ok::<i32, ExceptionPtr>(unsafe { (*this).member(x) })
            })?;

        let mut object = Foo { value: -1 };
        let mut fs: Vec<Future<i32>> = Vec::new();
        for i in 0..100 {
            fs.push(stage.call((&mut object as *mut Foo, i)));
        }

        for (i, f) in fs.into_iter().enumerate() {
            assert_eq!(f.get()?, i as i32 - 1);
        }
        assert_eq!(object.value, 99);
        Ok(())
    })
});

engine_test_case!(test_function_is_const_class_member, {
    async_(|| -> Result<(), ExceptionPtr> {
        struct Foo {
            value: i32,
        }
        impl Foo {
            fn member(&self) -> i32 {
                self.value
            }
        }
        let mut stage = make_execution_stage_default(&name("test"), |this: *const Foo| {
            // SAFETY: the object outlives the stage call.
            Ok::<i32, ExceptionPtr>(unsafe { (*this).member() })
        })?;
        let object = Foo { value: 999 };
        assert_eq!(stage.call(&object as *const Foo).get()?, 999);
        Ok(())
    })
});

engine_test_case!(test_stage_stats, {
    async_(|| -> Result<(), ExceptionPtr> {
        let mut stage = make_execution_stage_default(&name("test"), |()| {})?;

        assert_eq!(stage.get_stats().function_calls_enqueued, 0u64);
        assert_eq!(stage.get_stats().function_calls_executed, 0u64);

        let mut fs: Vec<Future<()>> = Vec::new();
        const CALL_COUNT: u64 = 53;
        for _ in 0..CALL_COUNT {
            fs.push(stage.call(()));
        }

        assert_eq!(stage.get_stats().function_calls_enqueued, CALL_COUNT);

        for (i, f) in fs.into_iter().enumerate() {
            f.get()?;
            assert!(stage.get_stats().tasks_scheduled >= 1u64);
            assert!(stage.get_stats().function_calls_executed >= i as u64);
        }
        assert_eq!(stage.get_stats().function_calls_executed, CALL_COUNT);
        Ok(())
    })
});

engine_test_case!(test_unique_stage_names_are_enforced, {
    async_(|| -> Result<(), ExceptionPtr> {
        {
            let mut stage = make_execution_stage_default(&name("test"), |()| {})?;
            let dup = make_execution_stage_default(&name("test"), |()| {});
            assert!(dup.err().expect("an InvalidArgument").is::<InvalidArgument>());
            stage.call(()).get()?;
        }

        let mut stage = make_execution_stage_default(&name("test"), |()| {})?;
        stage.call(()).get()?;
        Ok(())
    })
});

engine_test_case!(test_inheriting_concrete_execution_stage, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let sg1 = create_scheduling_group_with_shares("sg1".into(), 300.0).get()?;
        let _ksg1 = defer(move || {
            let _ = destroy_scheduling_group(sg1).get();
        });
        let sg2 = create_scheduling_group_with_shares("sg2".into(), 100.0).get()?;
        let _ksg2 = defer(move || {
            let _ = destroy_scheduling_group(sg2).get();
        });
        let check_sg = |sg: SchedulingGroup| {
            assert!(current_scheduling_group() == sg);
        };
        let mut es =
            InheritingConcreteExecutionStage::<SchedulingGroup, ()>::new(&name("stage"), check_sg);
        let esp: *mut InheritingConcreteExecutionStage<SchedulingGroup, ()> = &mut es;
        let make_attr = |sg: SchedulingGroup| ThreadAttributes {
            sched_group: Some(sg),
            ..ThreadAttributes::default()
        };
        let done = Rc::new(Cell::new(false));
        let make_test_thread = |sg: SchedulingGroup| {
            let done = done.clone();
            Thread::with_attributes(make_attr(sg), move || -> Result<(), ExceptionPtr> {
                while !done.get() {
                    // SAFETY: the stage outlives both threads, joined below.
                    unsafe { (*esp).call(sg) }.get()?; // will check if executed with same sg
                }
                Ok(())
            })
        };
        let mut th1 = make_test_thread(sg1)?;
        let mut th2 = make_test_thread(sg2)?;
        sleep(Duration::from_millis(10)).get()?;
        done.set(true);
        th1.join().get()?;
        th2.join().get()?;
        Ok(())
    })
});

struct AStruct;

engine_test_case!(test_inheriting_concrete_execution_stage_reference_parameters, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        // mostly a compile test, but take the opportunity to test that passing
        // by reference preserves the address
        let check_ref = |(r, ptr): (ReferenceWrapper<AStruct>, *mut AStruct)| {
            assert_eq!(r.get() as *mut AStruct, ptr);
        };
        let mut es = InheritingConcreteExecutionStage::<
            (ReferenceWrapper<AStruct>, *mut AStruct),
            (),
        >::new(&name("stage"), check_ref);
        let mut obj = AStruct;
        let p: *mut AStruct = &mut obj;
        es.call((ReferenceWrapper(p), p)).get()?;
        Ok(())
    })
});
