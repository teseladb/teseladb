//! Cross-shard calls: `smp::submit_to` returns the result of the remote function, and an
//! exception it throws reaches the caller with its type. A program with its own `main`
//! (`harness = false`).

use std::cell::Cell;
use std::fmt;

use teseladb::engine::core::app_template::AppTemplate;
use teseladb::engine::core::future::{Future, make_exception_future_from, make_ready_future};
use teseladb::engine::core::reactor::Reactor;
use teseladb::engine::core::smp::Smp;
use teseladb::engine::core::sstring::Sstring;

fn test_smp_call() -> Future<bool> {
    Smp::submit_to(1, || make_ready_future::<i32>(3))
        .then(|ret: i32| make_ready_future::<bool>(ret == 3))
}

#[derive(Debug)]
struct NastyException;

impl fmt::Display for NastyException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("nasty_exception")
    }
}

impl std::error::Error for NastyException {}

fn test_smp_exception() -> Future<bool> {
    println!("1");
    Smp::submit_to(1, || {
        println!("2");
        let x = make_exception_future_from::<i32, _>(NastyException);
        println!("3");
        x
    })
    .then_wrapped(|result: Future<i32>| {
        println!("4");
        match result.get() {
            Ok(_) => make_ready_future::<bool>(false), // expected an exception
            // all is well
            Err(e) if e.is::<NastyException>() => make_ready_future::<bool>(true),
            // incorrect exception type
            Err(_) => make_ready_future::<bool>(false),
        }
    })
}

thread_local! {
    static TESTS: Cell<i32> = const { Cell::new(0) };
    static FAILS: Cell<i32> = const { Cell::new(0) };
}

fn report(msg: Sstring, result: Future<bool>) -> Future<()> {
    result.then(move |result: bool| {
        println!("{}: {}", if result { "PASS" } else { "FAIL" }, msg);
        TESTS.with(|t| t.set(t.get() + 1));
        FAILS.with(|f| f.set(f.get() + i32::from(!result)));
    })
}

pub(crate) fn main() {
    let args: Vec<String> = std::env::args().collect();
    let status = AppTemplate::new(Default::default()).run_deprecated(
        args,
        Box::new(|| {
            let _ = report(Sstring::from("smp call"), test_smp_call())
                .then(|()| report(Sstring::from("smp exception"), test_smp_exception()))
                .then(|()| {
                    let tests = TESTS.with(Cell::get);
                    let fails = FAILS.with(Cell::get);
                    println!("\n{} tests / {} failures", tests, fails);
                    Reactor::exit(if fails != 0 { 1 } else { 0 });
                });
        }),
    );
    std::process::exit(status);
}
