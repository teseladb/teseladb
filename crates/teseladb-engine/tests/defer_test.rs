//! Tests of `defer`: the deferred action runs once when the guard drops, also after the
//! guard is moved, and never once it is cancelled.

#![cfg(target_os = "linux")]

use std::cell::Cell;

use teseladb::engine::util::defer::defer;

#[test]
fn test_defer_does_not_run_when_canceled() {
    let ran = Cell::new(false);
    {
        let mut d = defer(|| {
            ran.set(true);
        });
        d.cancel();
    }
    assert!(!ran.get());
}

#[test]
fn test_defer_runs() {
    let ran = Cell::new(false);
    {
        let _d = defer(|| {
            ran.set(true);
        });
    }
    assert!(ran.get());
}

#[test]
fn test_defer_runs_once_when_moved() {
    let ran = Cell::new(0);
    {
        let d = defer(|| {
            ran.set(ran.get() + 1);
        });
        {
            let _d2 = d;
        }
        assert_eq!(1, ran.get());
    }
    assert_eq!(1, ran.get());
}

#[test]
fn test_defer_does_not_run_when_moved_after_cancelled() {
    let ran = Cell::new(0);
    {
        let mut d = defer(|| {
            ran.set(ran.get() + 1);
        });
        d.cancel();
        {
            let _d2 = d;
        }
    }
    assert_eq!(0, ran.get());
}
