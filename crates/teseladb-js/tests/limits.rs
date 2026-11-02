//! The limits of a call (016 FR-007, FR-008): steps, heap, call depth and the deadline end it
//! with their own error and the next call works again; the interpreter sees no clock, files or
//! network.

use std::cell::Cell;

use teseladb_bson::Value;
use teseladb_js::{JsCtx, JsError, Limits, Program};

fn run_with(src: &str, ctx: &mut JsCtx<'_>) -> Result<Value, JsError> {
    Program::compile(src)?.call(None, &[], ctx)
}

fn run(src: &str) -> Result<Value, JsError> {
    run_with(src, &mut JsCtx::default())
}

#[test]
fn an_endless_loop_stops_at_the_deadline() {
    let reads = Cell::new(0u32);
    let deadline = || {
        reads.set(reads.get() + 1);
        reads.get() > 3
    };
    let mut ctx = JsCtx { deadline: Some(&deadline), ..JsCtx::default() };
    assert_eq!(run_with("function(){ while(true){} }", &mut ctx), Err(JsError::Deadline));
    assert_eq!(reads.get(), 4);
}

#[test]
fn an_endless_loop_stops_at_the_step_limit() {
    let mut ctx =
        JsCtx { limits: Limits { steps: 10_000, ..Limits::default() }, ..JsCtx::default() };
    assert_eq!(run_with("function(){ for(;;){} }", &mut ctx), Err(JsError::Steps));
}

#[test]
fn deep_recursion_is_too_much_recursion_and_can_be_caught() {
    let e = run("function(){ function f(n){ return f(n + 1) } return f(0) }").unwrap_err();
    assert!(matches!(e, JsError::Thrown { .. }), "{e:?}");
    assert_eq!(e.message(), "too much recursion");
    let caught = run(
        "function(){ function f(n){ return f(n + 1) } try { f(0) } catch (e) { return e instanceof InternalError } }",
    );
    assert_eq!(caught, Ok(Value::Boolean(true)));
}

#[test]
fn huge_allocations_run_out_of_memory() {
    let e =
        run("function(){ var a = []; while(true){ a.push(new Array(1e6).fill(1)) } }").unwrap_err();
    assert_eq!(e, JsError::OutOfMemory);
    assert_eq!(e.message(), "uncaught exception: out of memory");
    // A string that keeps doubling passes the longest string SpiderMonkey makes: a catchable
    // error, as on a `refrouter` 9.0.4 (measured on 02-11).
    let e = run("function(){ var s = 'x'; while(true){ s = s + s } }").unwrap_err();
    assert_eq!(e.message(), "allocation size overflow");
    let caught = run(
        "function(){ var s = 'x'; try { while(true){ s = s.concat(s) } } catch (e) { return e.name } }",
    );
    assert_eq!(caught, Ok(Value::String("InternalError".into())));
    let e = run("function(){ var s = 'x'; while(true){ s = s.repeat(2) } }").unwrap_err();
    assert_eq!(
        e.message(),
        "repeat count must be less than infinity and not overflow maximum string size"
    );
}

#[test]
fn garbage_is_freed_so_a_long_loop_of_small_objects_fits() {
    let r = run(
        "function(){ let n = 0; for (let i = 0; i < 300000; i++) { const o = {a: i, b: [i]}; n += o.b[0] } return n }",
    );
    assert_eq!(r, Ok(Value::Double(44_999_850_000.0)));
}

#[test]
fn nothing_outside_the_arguments_is_reachable() {
    for name in [
        "require",
        "process",
        "load",
        "db",
        "fetch",
        "XMLHttpRequest",
        "Reflect",
        "setTimeout",
        "quit",
    ] {
        let r = run(&format!("function(){{ return typeof {name} }}"));
        assert_eq!(r, Ok(Value::String("undefined".into())), "{name}");
    }
    // The clock is the cluster time the runtime gives.
    let mut ctx = JsCtx { now_ms: 1_700_000_000_000, ..JsCtx::default() };
    assert_eq!(run_with("function(){ return Date.now() }", &mut ctx), Ok(Value::Double(1.7e12)));
    // `Math.random` draws from the seed: the same seed gives the same numbers.
    let a =
        run_with("function(){ return Math.random() }", &mut JsCtx { seed: 9, ..JsCtx::default() });
    let b =
        run_with("function(){ return Math.random() }", &mut JsCtx { seed: 9, ..JsCtx::default() });
    assert_eq!(a, b);
    assert!(matches!(a, Ok(Value::Double(x)) if (0.0..1.0).contains(&x)));
}

#[test]
fn a_symbol_cannot_be_returned() {
    let e = run("function(){ return Symbol('a') }").unwrap_err();
    assert_eq!(e.message(), "unable to convert JavaScript property to ref element __returnValue");
}

#[test]
fn deeply_nested_source_is_refused_without_overflowing() {
    let src = format!("function(){{ return {}1{} }}", "(".repeat(5000), ")".repeat(5000));
    assert!(matches!(Program::compile(&src), Err(JsError::Compile { .. })));
    let src = format!("function(){{ return {}1{} }}", "[".repeat(5000), "]".repeat(5000));
    assert!(matches!(Program::compile(&src), Err(JsError::Compile { .. })));
}

/// Recursion whose frames take much native stack (`for…in`, a shell function per level) ends as
/// `too much recursion` before the thread's stack runs out (measured on a `refrouter` 9.0.4 on 02-11
/// with `Object.extend` over an object that contains itself).
#[test]
fn deep_recursion_through_heavy_frames_is_too_much_recursion() {
    let e = run(
        "function(){ const s = {a: 1}; s.self = s; function f(o){ for (const k in o) { if (typeof o[k] == 'object') f(o[k]) } } f(s) }",
    )
    .unwrap_err();
    assert_eq!(e.message(), "too much recursion");
    let shell = run(
        "function(){ const s = {a: 1}; s.self = s; try { Object.extend({}, s, true) } catch (e) { return e.name + ':' + e.message } }",
    );
    assert_eq!(shell, Ok(Value::String("InternalError:too much recursion".into())));
}
