//! Tests of `Gate` and `NamedGate`: entering, leaving and closing, waiting for the holders on
//! close, and the gate-closed error (with the gate's name for a named gate).

#![cfg(target_os = "linux")]
// This file calls unsafe engine, libc or FFI interfaces directly; every `unsafe` block
// states why it is sound in a `// SAFETY:` comment.
#![allow(unsafe_code)]

use std::pin::Pin;

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{
    ExceptionPtr, Future, make_exception_future, make_ready_future,
};
use teseladb::engine::core::gate::{
    Gate, GateClosedException, NamedGate, NamedGateClosedException, try_with_gate, with_gate,
};
use teseladb::engine::core::sstring::Sstring;
use teseladb::engine_test_case;

// Default construction and moves cannot fail: `Gate::new()` returns the gate and moves
// are bitwise.

/// The future of a call that may fail: ready, or failed with its error.
fn futurize(r: Result<(), ExceptionPtr>) -> Future<()> {
    match r {
        Ok(()) => make_ready_future(()),
        Err(e) => make_exception_future(e),
    }
}

/// The `what()` of a gate-closed exception, whichever of the two types carries it.
fn closed_what(e: &ExceptionPtr) -> Option<String> {
    if let Some(g) = e.downcast_ref::<GateClosedException>() {
        return Some(g.what().to_string());
    }
    e.downcast_ref::<NamedGateClosedException>().map(|g| g.what().to_string())
}

fn check_gate_closed_exception<F: FnOnce() -> Future<()> + 'static>(func: F) -> Future<()> {
    make_coroutine(async move {
        match func().await {
            Ok(()) => panic!("func was expected to throw gate_closed_exception"),
            Err(e) => match closed_what(&e) {
                Some(what) => assert_eq!(what, "gate closed"),
                None => panic!("unexpected exception: {}", e),
            },
        }
        Ok(())
    })
}

engine_test_case!(basic_gate_test, {
    make_coroutine(async move {
        let g = std::pin::pin!(Gate::new());
        assert_eq!(g.get_count(), 0);
        assert!(!g.is_closed());
        assert!(g.check().is_ok());
        assert_eq!(g.get_count(), 0);

        assert!(g.enter().is_ok());
        assert_eq!(g.get_count(), 1);

        let mut gh0 = g.as_ref().try_hold();
        assert!(gh0.is_some());
        assert_eq!(g.get_count(), 2);

        let mut gh1 = g.as_ref().hold()?;
        assert_eq!(g.get_count(), 3);

        assert!(!g.is_closed());
        let f = g.close();
        assert!(!f.available());

        g.leave();
        assert_eq!(g.get_count(), 2);

        gh0.as_mut().expect("holder").release();
        assert_eq!(g.get_count(), 1);

        gh1.release();
        assert_eq!(g.get_count(), 0);

        assert!(f.await.is_ok());
        assert!(g.is_closed());
        Ok(())
    })
});

engine_test_case!(gate_closed_test, {
    make_coroutine(async move {
        let g = std::pin::pin!(Gate::new());
        assert!(!g.is_closed());
        assert!(g.check().is_ok());
        assert!(g.enter().is_ok());

        let mut gh0 = g.as_ref().try_hold();
        assert!(gh0.is_some());

        let mut gh1 = g.as_ref().hold()?;

        assert!(!g.is_closed());
        let f = g.close();
        assert!(!f.available());

        g.leave();
        gh0.as_mut().expect("holder").release();
        gh1.release();

        assert!(f.await.is_ok());
        assert!(g.is_closed());

        assert!(g.as_ref().try_hold().is_none());

        let gp: *const Gate = &*g;
        // SAFETY: `g` is pinned in this coroutine and outlives each
        // awaited check.
        check_gate_closed_exception(move || futurize(unsafe { (*gp).check() })).await?;
        // SAFETY: `g` is pinned in this coroutine and outlives each awaited check.
        check_gate_closed_exception(move || futurize(unsafe { (*gp).enter() })).await?;
        check_gate_closed_exception(move || {
            // SAFETY: `g` is pinned in this coroutine and outlives each awaited check, so the pin stays valid.
            futurize(unsafe { Pin::new_unchecked(&*gp) }.hold().map(drop))
        })
        .await?;
        check_gate_closed_exception(move || {
            // SAFETY: `g` is pinned in this coroutine and outlives each awaited check, so the pin stays valid.
            match with_gate(unsafe { Pin::new_unchecked(&*gp) }, || make_ready_future(())) {
                Ok(f) => f,
                Err(e) => make_exception_future(e),
            }
        })
        .await?;
        check_gate_closed_exception(move || {
            // SAFETY: `g` is pinned in this coroutine and outlives each awaited check, so the pin stays valid.
            try_with_gate(unsafe { Pin::new_unchecked(&*gp) }, || make_ready_future(()))
        })
        .await?;
        Ok(())
    })
});

fn check_named_gate_closed_exception<F: FnOnce() -> Future<()> + 'static>(
    func: F,
    name: Sstring,
) -> Future<()> {
    make_coroutine(async move {
        match func().await {
            Ok(()) => panic!("func was expected to throw gate_closed_exception"),
            Err(e) => match closed_what(&e) {
                Some(what) => assert_eq!(what, format!("{} gate closed", name)),
                None => panic!("unexpected exception: {}", e),
            },
        }
        Ok(())
    })
}

fn test_named_gate(g: *mut NamedGate, name: Sstring) -> Future<()> {
    make_coroutine(async move {
        // SAFETY: the caller keeps the gate alive, and in place, until the returned future
        // resolves.
        let gr = unsafe { Pin::new_unchecked(&*g) };
        if !gr.is_closed() {
            assert!(gr.check().is_ok());
            assert!(gr.enter().is_ok());
            let mut gh0 = gr.try_hold();
            assert!(gh0.is_some());
            let mut gh1 = gr.hold()?;
            assert!(!gr.is_closed());
            let f = gr.close();
            assert!(!f.available());
            gr.leave();
            gh0.as_mut().expect("holder").release();
            gh1.release();
            assert!(f.await.is_ok());
            assert!(gr.is_closed());
        }
        assert!(gr.try_hold().is_none());
        // SAFETY: as above.
        check_named_gate_closed_exception(move || futurize(unsafe { (*g).check() }), name.clone())
            .await?;
        // SAFETY: the gate is pinned in this coroutine and outlives each awaited check.
        check_named_gate_closed_exception(move || futurize(unsafe { (*g).enter() }), name.clone())
            .await?;
        check_named_gate_closed_exception(
            // SAFETY: the gate is pinned in this coroutine and outlives each awaited check, so the pin stays valid.
            move || futurize(unsafe { Pin::new_unchecked(&*g) }.hold().map(drop)),
            name.clone(),
        )
        .await?;
        check_named_gate_closed_exception(
            // SAFETY: the gate is pinned in this coroutine and outlives each awaited check, so the pin stays valid.
            move || match with_gate(unsafe { Pin::new_unchecked(&*g) }, || make_ready_future(())) {
                Ok(f) => f,
                Err(e) => make_exception_future(e),
            },
            name.clone(),
        )
        .await?;
        check_named_gate_closed_exception(
            // SAFETY: the gate is pinned in this coroutine and outlives each awaited check, so the pin stays valid.
            move || try_with_gate(unsafe { Pin::new_unchecked(&*g) }, || make_ready_future(())),
            name.clone(),
        )
        .await?;
        Ok(())
    })
}

engine_test_case!(named_gate_closed_test, {
    make_coroutine(async move {
        println!("test_named_gate(default_constructed)");
        let mut default_constructed = NamedGate::named(Sstring::from("named"));
        test_named_gate(&mut default_constructed, Sstring::from("named")).await?;

        // `foo` names the gate as the test expects.
        #[allow(clippy::disallowed_names)]
        let foo = Sstring::from("foo");
        println!("test_named_gate(name_constructed)");
        let name_constructed = NamedGate::named(foo.clone());
        let mut name_constructed = name_constructed;
        test_named_gate(&mut name_constructed, foo.clone()).await?;

        println!("test_named_gate(move_constructed)");
        let mut move_constructed = name_constructed;
        test_named_gate(&mut move_constructed, foo.clone()).await?;

        println!("test_named_gate(move_assigned)");
        let bar = Sstring::from("bar");
        // The assignment drops the value it replaces, which is what this checks.
        #[allow(unused_assignments)]
        let mut move_assigned = NamedGate::named(foo.clone());
        move_assigned = NamedGate::named(bar.clone());
        test_named_gate(&mut move_assigned, bar).await?;
        Ok(())
    })
});
