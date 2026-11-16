//! Tests of the close and stop guards (`DeferredClose`, `DeferredStop`, `with_closeable`,
//! `with_stoppable`) and of moving gates and gate holders. A guard borrows what it closes
//! mutably, so the tests reach the gate through a raw pointer (`gp`) while the guard lives.

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

#[path = "common/expected_exception.rs"]
mod expected_exception;

use std::cell::Cell;
use std::rc::Rc;

use expected_exception::ExpectedException;
use std::pin::Pin;
use teseladb::engine::core::do_with::do_with;
use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::gate::{Gate, GateClosedException, with_gate};
use teseladb::engine::core::loop_::parallel_for_each;
use teseladb::engine::core::shared_ptr::make_shared;
use teseladb::engine::core::thread::async_ as thread_async;
use teseladb::engine::util::closeable::{
    DeferredClose, DeferredStop, Stoppable, with_closeable, with_stoppable,
};
use teseladb::engine_test_case;

/// Increments `count` in the background, inside the gate `g`.
fn count_in_gate(g: *mut Gate, count: &Rc<Cell<i32>>) {
    let c = count.clone();
    // SAFETY: the gate stays alive and in place until the background continuation, which
    // close() waits for, completes.
    let _ = with_gate(unsafe { Pin::new_unchecked(&*g) }, move || c.set(c.get() + 1));
}

engine_test_case!(deferred_close_test, {
    do_with((Gate::new(), Rc::new(Cell::new(0)), 42), |x| {
        let gp: *mut Gate = &mut x.0;
        let count = x.1.clone();
        let expected = x.2;
        let c2 = count.clone();
        thread_async(move || {
            // SAFETY: do_with keeps the gate alive and in place across the thread; the gate
            // is only reached through shared references.
            let mut gate_ref: &Gate = unsafe { &*gp };
            let _close_gate = DeferredClose::new(&mut gate_ref);
            for _ in 0..expected {
                count_in_gate(gp, &count);
            }
        })
        .then(move |()| {
            // destroying close_gate should invoke g.close()
            // and wait for all background continuations to complete
            // SAFETY: as above.
            assert!(unsafe { &*gp }.is_closed());
            assert_eq!(c2.get(), expected);
        })
    })
});

engine_test_case!(move_deferred_close_test, {
    do_with(Gate::new(), |g: &mut Gate| {
        let gp: *mut Gate = g;
        thread_async(move || {
            // SAFETY: do_with keeps the gate alive and in place across the thread; the gate
            // is only reached through shared references.
            let mut gate_ref: &Gate = unsafe { &*gp };
            let _close_gate = make_shared(DeferredClose::new(&mut gate_ref));
            // g.close() should not be called when deferred_close is moved away
            // SAFETY: do_with keeps the gate alive and in place across the thread, and the gate is only reached through shared references.
            assert!(!unsafe { &*gp }.is_closed());
        })
        .then(move |()| {
            // Without this guarantee, gate::close() would run into an
            // assert failure when leaving previous continuation, if gate::close()
            // is called twice, so this test only verifies the behavior with the
            // release build.
            // SAFETY: as above.
            assert!(unsafe { &*gp }.is_closed());
        })
    })
});

engine_test_case!(close_now_test, {
    do_with((Gate::new(), Rc::new(Cell::new(0)), 42), |x| {
        let gp: *mut Gate = &mut x.0;
        let count = x.1.clone();
        let expected = x.2;
        thread_async(move || {
            // SAFETY: do_with keeps the gate alive and in place across the thread; the gate
            // is only reached through shared references.
            let mut gate_ref: &Gate = unsafe { &*gp };
            let mut close_gate = DeferredClose::new(&mut gate_ref);
            for _ in 0..expected {
                count_in_gate(gp, &count);
            }

            close_gate.close_now();
            // SAFETY: do_with keeps the gate alive and in place across the thread, and the gate is only reached through shared references.
            assert!(unsafe { &*gp }.is_closed());
            assert_eq!(count.get(), expected);
            // gate must not be double-closed.
        })
    })
});

engine_test_case!(cancel_deferred_close_test, {
    let g = Gate::new();
    {
        let mut gate_ref: &Gate = &g;
        let mut close_gate = DeferredClose::new(&mut gate_ref);
        close_gate.cancel();
    }
    g.check().expect("should not throw"); // should not throw
    make_ready_future(())
});

engine_test_case!(with_closeable_test, {
    let count = Rc::new(Cell::new(0));
    let expected = 42;
    let c = count.clone();
    with_closeable(Gate::new(), move |g: &mut Gate| {
        let gp: *mut Gate = g;
        for _ in 0..expected {
            count_in_gate(gp, &c);
        }
        make_ready_future(17)
    })
    .then(move |res: i32| {
        // res should be returned by the function called
        // by with_closeable.
        assert_eq!(res, 17);

        // closing the gate should wait for
        // all background continuations to complete
        assert_eq!(count.get(), expected);
    })
});

engine_test_case!(with_closeable_exception_test, {
    let count = Rc::new(Cell::new(0));
    let expected = 42;
    let c = count.clone();
    with_closeable(Gate::new(), move |g: &mut Gate| -> Result<(), ExceptionPtr> {
        let gp: *mut Gate = g;
        for _ in 0..expected {
            count_in_gate(gp, &c);
        }
        Err(ExceptionPtr::new(ExpectedException::new()))
    })
    .handle_exception_type(move |_: &ExpectedException| {
        // closing the gate should also happen when func throws,
        // waiting for all background continuations to complete
        assert_eq!(count.get(), expected);
        Ok(())
    })
});

/// Counts its stops into its own counter or into an outside one. A Rust move
/// is bitwise, so the self-pointing case keeps the counter by value instead of by address.
struct CountStops {
    count: i32,
    ptr: Option<Rc<Cell<i32>>>,
}

impl CountStops {
    fn new(ptr: Option<Rc<Cell<i32>>>) -> Self {
        if let Some(p) = &ptr {
            p.set(0);
        }
        CountStops { count: 0, ptr }
    }

    fn stopped(&self) -> i32 {
        match &self.ptr {
            Some(p) => p.get(),
            None => self.count,
        }
    }
}

impl Stoppable for CountStops {
    fn stop(&mut self) -> Future<()> {
        match &self.ptr {
            Some(p) => p.set(p.get() + 1),
            None => self.count += 1,
        }
        make_ready_future(())
    }
}

engine_test_case!(cancel_deferred_stop_test, {
    let mut cs = CountStops::new(None);
    {
        let mut stop = DeferredStop::new(&mut cs);
        stop.cancel();
    }
    assert_eq!(cs.stopped(), 0);
    make_ready_future(())
});

engine_test_case!(deferred_stop_test, {
    do_with(CountStops::new(None), |cs: &mut CountStops| {
        let csp: *mut CountStops = cs;
        thread_async(move || {
            // SAFETY: do_with keeps `cs` alive across the thread.
            let _stop_counting = DeferredStop::new(unsafe { &mut *csp });
        })
        .then(move |()| {
            // cs.stop() should be called when stop_counting is destroyed
            // SAFETY: as above.
            assert_eq!(unsafe { &*csp }.stopped(), 1);
        })
    })
});

engine_test_case!(move_deferred_stop_test, {
    do_with(CountStops::new(None), |cs: &mut CountStops| {
        let csp: *mut CountStops = cs;
        thread_async(move || {
            // SAFETY: do_with keeps `cs` alive across the thread.
            let _stop = make_shared(DeferredStop::new(unsafe { &mut *csp }));
        })
        .then(move |()| {
            // cs.stop() should be called once and only once
            // when stop is destroyed
            // SAFETY: as above.
            assert_eq!(unsafe { &*csp }.stopped(), 1);
        })
    })
});

engine_test_case!(stop_now_test, {
    do_with(CountStops::new(None), |cs: &mut CountStops| {
        let csp: *mut CountStops = cs;
        thread_async(move || {
            // SAFETY: do_with keeps `cs` alive across the thread.
            let mut stop_counting = DeferredStop::new(unsafe { &mut *csp });
            stop_counting.stop_now();
            // cs.stop() should not be called again
            // when stop_counting is destroyed
            // SAFETY: do_with keeps `cs` alive; `stop_counting` has already stopped it, so its drop does not touch the object and this shared read overlaps no use of its `&mut`.
            assert_eq!(unsafe { &*csp }.stopped(), 1);
        })
        .then(move |()| {
            // cs.stop() should be called exactly once
            // SAFETY: as above.
            assert_eq!(unsafe { &*csp }.stopped(), 1);
        })
    })
});

engine_test_case!(with_stoppable_test, {
    let stopped = Rc::new(Cell::new(0));
    let s = stopped.clone();
    with_stoppable(CountStops::new(Some(stopped.clone())), |_cs: &mut CountStops| {
        make_ready_future(17)
    })
    .then(move |res: i32| {
        // res should be returned by the function called
        // by with_closeable.
        assert_eq!(res, 17);
        // cs.stop() should be called before count_stops is destroyed
        assert_eq!(s.get(), 1);
    })
});

engine_test_case!(with_stoppable_exception_test, {
    let stopped = Rc::new(Cell::new(0));
    let s = stopped.clone();
    with_stoppable(
        CountStops::new(Some(stopped.clone())),
        |_cs: &mut CountStops| -> Result<(), ExceptionPtr> {
            Err(ExceptionPtr::new(ExpectedException::new()))
        },
    )
    .handle_exception_type(move |_: &ExpectedException| {
        // cs.stop() should be called before count_stops is destroyed
        // also when func throws
        assert_eq!(s.get(), 1);
        Ok(())
    })
});

/// Moves the gate out of `g1`, leaving a fresh gate in its place.
fn move_gate(g1: &mut Gate) -> Gate {
    let mut g2 = Gate::new();
    g2.assign_from(g1);
    g2
}

engine_test_case!(move_open_gate_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut g1 = Gate::new();
        g1.enter()?;
        // move an open gate
        let g2 = move_gate(&mut g1);
        // the state in g1 should be moved into g2
        assert_eq!(g1.get_count(), 0);
        assert_eq!(g2.get_count(), 1);
        g2.leave();
        g2.close().get()?;
        assert!(!g1.is_closed());
        assert!(g2.is_closed());
        Ok(())
    })
});

engine_test_case!(move_closing_gate_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut g1 = Gate::new();
        g1.enter()?;
        let fut = g1.close();
        // move a closing gate
        let g2 = move_gate(&mut g1);
        assert_eq!(g1.get_count(), 0);
        assert_eq!(g2.get_count(), 1);
        g2.leave();
        fut.get()?;
        assert!(!g1.is_closed());
        assert!(g2.is_closed());
        Ok(())
    })
});

engine_test_case!(move_closed_gate_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let mut g1 = Gate::new();
        g1.close().get()?;
        // move a closed gate
        let g2 = move_gate(&mut g1);
        assert_eq!(g1.get_count(), 0);
        assert_eq!(g2.get_count(), 0);
        assert!(!g1.is_closed());
        assert!(g2.is_closed());
        Ok(())
    })
});

engine_test_case!(gate_holder_basic_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let g = std::pin::pin!(Gate::new());
        let mut gh = g.as_ref().hold()?;
        let fut = g.close();
        assert!(!fut.available());
        gh.release();
        fut.get()?;
        Ok(())
    })
});

engine_test_case!(gate_holder_closed_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let g = std::pin::pin!(Gate::new());
        g.close().get()?;
        assert!(matches!(g.as_ref().hold(), Err(ref e) if e.is::<GateClosedException>()));
        Ok(())
    })
});

engine_test_case!(gate_holder_move_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let g = std::pin::pin!(Gate::new());
        let gh0 = g.as_ref().hold()?;
        let fut = g.close();
        assert!(!fut.available());
        let mut gh1 = gh0;
        assert!(!fut.available());
        gh1.release();
        fut.get()?;
        Ok(())
    })
});

engine_test_case!(gate_holder_copy_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let g = std::pin::pin!(Gate::new());
        let mut gh0 = g.as_ref().hold()?;
        let mut gh1 = gh0.clone();
        let fut = g.close();
        assert!(!fut.available());
        gh0.release();
        assert!(!fut.available());
        gh1.release();
        fut.get()?;
        Ok(())
    })
});

engine_test_case!(gate_holder_copy_and_move_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let g0 = std::pin::pin!(Gate::new());
        let mut gh00 = g0.as_ref().hold()?;
        let mut gh01 = gh00.clone();
        let fut0 = g0.close();
        assert!(!fut0.available());
        let g1 = std::pin::pin!(Gate::new());
        let gh1 = g1.as_ref().hold()?;
        let fut1 = g1.close();
        assert!(!fut1.available());
        gh01.release();
        assert!(!fut0.available());
        assert!(!fut1.available());
        gh00 = gh1;
        fut0.get()?;
        assert!(!fut1.available());
        gh00.release();
        fut1.get()?;
        Ok(())
    })
});

engine_test_case!(gate_holder_copy_after_close_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let g = std::pin::pin!(Gate::new());
        let mut gh0 = g.as_ref().hold()?;
        let fut = g.close();
        assert!(g.is_closed());
        let mut gh1 = gh0.clone();
        assert!(!fut.available());
        gh0.release();
        assert!(!fut.available());
        gh1.release();
        fut.get()?;
        Ok(())
    })
});

engine_test_case!(gate_holder_parallel_copy_test, {
    const EXPECTED: i32 = 42;
    let count = Rc::new(Cell::new(0));
    let c = count.clone();
    with_closeable(Gate::new(), move |g: &mut Gate| -> Result<i32, ExceptionPtr> {
        // SAFETY: with_closeable keeps the gate in place, in its do_with frame, until it is
        // closed.
        let gh = unsafe { Pin::new_unchecked(&*g) }.hold()?;
        // Copying the gate holder in the closure below should keep it open
        // until all instances complete
        let _ = parallel_for_each(0..EXPECTED, move |_: i32| {
            let _gh = gh.clone();
            c.set(c.get() + 1);
            make_ready_future(())
        });
        Ok(17)
    })
    .then(move |res: i32| {
        // res should be returned by the function called
        // by with_closeable.
        assert_eq!(res, 17);

        // closing the gate should wait for
        // all background continuations to complete
        assert_eq!(count.get(), EXPECTED);
    })
});

engine_test_case!(gate_holder_try_close_test, {
    thread_async(move || -> Result<(), teseladb::engine::core::future::ExceptionPtr> {
        let g = std::pin::pin!(Gate::new());
        let mut gh0 = g.as_ref().try_hold();
        assert!(gh0.is_some());
        let fut = g.close();
        assert!(g.is_closed());
        let failed_gh = g.as_ref().try_hold();
        assert!(failed_gh.is_none());
        // The assignment drops the value it replaces, which is what this checks.
        #[allow(unused_assignments)]
        let mut gh1 = gh0.take();
        assert!(!fut.available());
        gh0 = None;
        assert!(gh0.is_none());
        assert!(!fut.available());
        gh1 = None;
        assert!(gh1.is_none());
        fut.get()?;
        Ok(())
    })
});
