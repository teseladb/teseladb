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

#[test]
fn a_catastrophic_regular_expression_stops_at_the_step_limit_and_the_deadline() {
    let steps = Limits { steps: 20_000, ..Limits::default() };
    for src in [
        "function(){ return /(a*)*b/.test('a'.repeat(40)) }",
        "function(){ return /(?:a|a)*?c/.exec('a'.repeat(40)) }",
        "function(){ return /^(a+)+$/.test('a'.repeat(40) + 'b') }",
        "function(){ return 'a'.repeat(40).replace(/(a|aa)+b/g, 'x') }",
    ] {
        let mut ctx = JsCtx { limits: steps, ..JsCtx::default() };
        assert_eq!(run_with(src, &mut ctx), Err(JsError::Steps), "{src}");
    }
    let reads = Cell::new(0u32);
    let deadline = || {
        reads.set(reads.get() + 1);
        reads.get() > 2
    };
    let mut ctx = JsCtx { deadline: Some(&deadline), ..JsCtx::default() };
    assert_eq!(
        run_with("function(){ return /(a*)*b/.test('a'.repeat(60)) }", &mut ctx),
        Err(JsError::Deadline)
    );
}

#[test]
fn a_low_call_depth_limit_is_too_much_recursion_for_calls_and_constructors() {
    let limits = Limits { depth: 8, ..Limits::default() };
    for src in [
        "function(){ function f(n){ return n ? f(n - 1) : 0 } return f(20) }",
        "function(){ function F(n){ if (n) new F(n - 1) } new F(20) }",
        "function(){ class A { constructor(n) { if (n) new A(n - 1) } } new A(20) }",
        "function(){ const f = (n) => n ? [n].map(x => f(x - 1)) : 0; return f(20) }",
    ] {
        let mut ctx = JsCtx { limits, ..JsCtx::default() };
        let e = run_with(src, &mut ctx).unwrap_err();
        assert_eq!(e.message(), "too much recursion", "{src}");
    }
}

#[test]
fn errors_made_while_the_heap_is_full_end_the_call_out_of_memory() {
    let limits = Limits { heap: 64 * 1024, ..Limits::default() };
    for src in [
        "function(){ var a = []; while (true) { try { a.push(new Array(100).fill(1)); null.x } catch (e) { a.push(e) } } }",
        "function(){ var a = []; while (true) { try { a.push('x'.repeat(100)); eval('(') } catch (e) { a.push(e) } } }",
        "function(){ var a = []; while (true) { try { a.push({}); +Symbol() } catch (e) { a.push(e) } } }",
        "function(){ var a = []; while (true) { try { a.push([1, 2, 3]); new (() => 1) } catch (e) { a.push(e) } } }",
    ] {
        let mut ctx = JsCtx { limits, ..JsCtx::default() };
        assert_eq!(run_with(src, &mut ctx), Err(JsError::OutOfMemory), "{src}");
    }
}

/// The stack a thread that runs commands has, and what it gives a call.
const BIG_STACK: usize = 300 << 20;

/// `src`'s value on a thread with [`BIG_STACK`], as a function called inside a program function
/// like the probes of the `refrouter` (`function(){ … (expression) … }`).
fn on_big_stack(srcs: &'static [(&'static str, &'static str)]) {
    std::thread::Builder::new()
        .stack_size(BIG_STACK)
        .spawn(move || {
            for (src, want) in srcs {
                let limits = Limits { stack: 256 << 20, ..Limits::default() };
                let mut ctx = JsCtx { limits, ..JsCtx::default() };
                let body = format!("function(){{ return ({src}) }}");
                let got = run_with(&body, &mut ctx);
                assert_eq!(got, Ok(Value::String((*want).into())), "{src}");
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

/// With the stack of a thread that runs commands, recursion goes as deep as on a `refrouter` 9.0.4
/// (016 A016-3-18, measured on 02-11): 51 000 frames, the self-hosted builtins included, and
/// about 350 calls back from the engine's own code.
#[test]
fn recursion_on_a_large_stack_goes_as_deep_as_on_a_refrouter() {
    const CASES: &[(&str, &str)] = &[
        (
            "(function(){ let n=0; function f(){ n++; f(); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "50998 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return [1].map(f); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "25499 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return [1].flatMap(f); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "17000 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return 'a'.replace(/a/, f); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "10200 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return [1].values().map(f).toArray(); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "10200 too much recursion",
        ),
        (
            "(function(){ let n=0; function* g(){ n++; yield* g(); } try { [...g()] } catch(e) { return n + ' ' + e.message } })()",
            "25499 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(k){ n++; if (k==0) return 0; return 1+f(k-1); } return f(30000) + ' ' + n })()",
            "30000 30001",
        ),
    ];
    on_big_stack(CASES);
}

/// Calls back from the engine's own code take its native stack: a getter or a conversion about
/// 349 times, through `f.call` 175, through two natives 117 (measured on a `refrouter` 9.0.4).
#[test]
fn calls_back_from_the_engine_end_as_on_a_refrouter() {
    const CASES: &[(&str, &str)] = &[
        (
            "(function(){ let n=0; function f(){ n++; return +{valueOf: f}; } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "349 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return Object.defineProperty({}, 'x', {get: f}).x; } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "349 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return f(...[]); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "349 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return f.call(null); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "175 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return f.bind(null)(); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "175 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return String({toString: f}); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "175 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return Function.prototype.call.call(f, null); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "117 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return f.call.call(f.call, f); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "88 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return JSON.parse('1', f); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "117 too much recursion",
        ),
        (
            "(function(){ let n=0; const p = new Proxy({}, {get(){ n++; return p.x; }}); try { p.x } catch(e) { return n + ' ' + e.message } })()",
            "174 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return Object.keys(new Proxy({}, {ownKeys(){ return f() }})); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "117 too much recursion",
        ),
        (
            "(function(){ let n=0; function f(){ n++; return eval('f()'); } try { f() } catch(e) { return n + ' ' + e.message } })()",
            "343 too much recursion",
        ),
    ];
    on_big_stack(CASES);
}

/// The depth of a value `JSON.stringify` writes, of an array converted to a string, of the groups
/// of a regular expression and of the constructs an `eval` parses ends where a `refrouter` 9.0.4
/// ends it (measured on 02-11); an array that is JSON the engine reads without that limit.
#[test]
fn nesting_in_values_patterns_and_eval_ends_as_on_a_refrouter() {
    const CASES: &[(&str, &str)] = &[
        (
            "(function(){ const t = (d) => { let a = []; for (let i = 0; i < d; i++) a = [a]; try { return JSON.stringify(a).length } catch (e) { return e.message } }; return t(346) + ' ' + t(347) })()",
            "694 too much recursion",
        ),
        (
            "(function(){ const t = (d) => { let a = []; for (let i = 0; i < d; i++) a = [a]; try { return String(a).length } catch (e) { return e.message } }; return t(172) + ' ' + t(173) })()",
            "0 too much recursion",
        ),
        (
            "(function(){ const t = (d) => { try { return new RegExp('(?:'.repeat(d) + 'a' + ')'.repeat(d)).test('a') } catch (e) { return e.name + ':' + e.message } }; return t(346) + ' ' + t(347) })()",
            "true Error:regexp too big",
        ),
        (
            "(function(){ const t = (s) => { try { eval(s); return 'ok' } catch (e) { return e.name } }; return [t('('.repeat(69) + '1' + ')'.repeat(69)), t('('.repeat(70) + '1' + ')'.repeat(70)), t('!'.repeat(341) + '1'), t('!'.repeat(342) + '1'), t('{'.repeat(173) + '}'.repeat(173)), t('{'.repeat(174) + '}'.repeat(174)), t('Math.abs('.repeat(86) + '1' + ')'.repeat(86)), t('Math.abs('.repeat(87) + '1' + ')'.repeat(87))].join() })()",
            "ok,InternalError,ok,InternalError,ok,InternalError,ok,InternalError",
        ),
        (
            "(function(){ return eval('['.repeat(20000) + ']'.repeat(20000)).length + ' ' + JSON.parse('['.repeat(20000) + ']'.repeat(20000)).length })()",
            "1 1",
        ),
    ];
    on_big_stack(CASES);
}

/// A program nested deeper than the engine parses fails to compile with `too much recursion` and
/// no place; arrow functions, which it does not check, exhaust its stack (measured on a `refrouter`
/// 9.0.4 on 02-11).
#[test]
fn a_program_nested_too_deep_does_not_compile() {
    let program = |d: usize| format!("function(){{ return {}1{} }}", "(".repeat(d), ")".repeat(d));
    assert!(Program::compile(&program(66)).is_ok());
    let e = Program::compile(&program(67)).unwrap_err();
    assert_eq!(
        e,
        JsError::Compile { message: "too much recursion".into(), line: 0, col: 1 },
        "67 parentheses"
    );
    let unary = |d: usize| format!("function(){{ return {}1 }}", "!".repeat(d));
    assert!(Program::compile(&unary(330)).is_ok());
    assert!(Program::compile(&unary(331)).is_err());
    // A `$where` expression has the same room.
    let script = |d: usize| format!("{}1{}", "(".repeat(d), ")".repeat(d));
    assert!(Program::compile(&script(66)).is_ok());
    assert!(Program::compile(&script(67)).is_err());
    let arrows = |d: usize| format!("function(){{ return {}1 }}", "x=>".repeat(d));
    assert!(Program::compile(&arrows(313)).is_ok());
    assert_eq!(Program::compile(&arrows(314)).unwrap_err(), JsError::StackExhausted);
    let e = run("function(){ return eval('x=>'.repeat(316) + '1') }").unwrap_err();
    assert_eq!(e.message(), "wasm trap: call stack exhausted");
}
