//! The language (016 FR-002): each program is a function called with no arguments, and its result
//! comes back as BSON. The expected values are those of RefDb 9.0.4's engine for the same code.

use teseladb_bson::{Document, Value};
use teseladb_js::{JsCtx, JsError, Program};

fn call_with(src: &str, this: Option<&Document>, args: &[Value]) -> Result<Value, JsError> {
    let program = Program::compile(src)?;
    program.call(this, args, &mut JsCtx::default())
}

fn run(src: &str) -> Value {
    call_with(src, None, &[]).unwrap_or_else(|e| panic!("{src}: {e:?}"))
}

fn num(src: &str) -> f64 {
    match run(src) {
        Value::Double(x) => x,
        Value::Int32(n) => f64::from(n),
        other => panic!("{src}: {other:?}"),
    }
}

fn text(src: &str) -> String {
    match run(src) {
        Value::String(s) => s,
        other => panic!("{src}: {other:?}"),
    }
}

#[test]
fn arithmetic_and_precedence() {
    assert_eq!(num("function(){ return 1 + 2 * 3 - 4 / 2 }"), 5.0);
    assert_eq!(num("function(){ return 2 ** 3 ** 2 }"), 512.0);
    assert_eq!(num("function(){ return 7 % 3 + (-7 % 3) }"), 0.0);
    assert_eq!(num("function(){ return (5 & 3) | (1 << 4) ^ (256 >> 2) }"), 81.0);
    assert_eq!(num("function(){ return -1 >>> 28 }"), 15.0);
    assert_eq!(text("function(){ return 1 + '2' + 3 }"), "123");
    assert_eq!(num("function(){ return '6' * '7' }"), 42.0);
    assert_eq!(
        text("function(){ return typeof (0/0) + ' ' + (0.1 + 0.2) }"),
        "number 0.30000000000000004"
    );
    assert_eq!(
        text("function(){ return String(1e21) + ' ' + 1e-7 + ' ' + 123.456 + ' ' + -0 }"),
        "1e+21 1e-7 123.456 0"
    );
}

#[test]
fn variables_scopes_and_closures() {
    assert_eq!(num("function(){ var a = 1; { let a = 2; } return a }"), 1.0);
    assert_eq!(
        num(
            "function(){ function mk(n){ return function(){ return ++n } } var f = mk(10); f(); return f() }"
        ),
        12.0
    );
    assert_eq!(
        num(
            "function(){ const fs = []; for (let i = 0; i < 3; i++) fs.push(() => i); return fs[0]() + fs[2]() }"
        ),
        2.0
    );
    assert_eq!(num("function(){ return h(); function h(){ return 4 } }"), 4.0);
    assert_eq!(
        num("function(){ let x = 5; x += 2; x *= 3; x -= 1; x /= 4; x %= 3; return x }"),
        2.0
    );
    assert_eq!(
        num(
            "function(){ let a = null; a ??= 3; let b = 0; b ||= 4; let c = 1; c &&= 6; return a + b + c }"
        ),
        13.0
    );
    let e = call_with("function(){ const c = 1; c = 2 }", None, &[]).unwrap_err();
    assert!(e.message().contains("invalid assignment to const"), "{e:?}");
    let e = call_with("function(){ return y }", None, &[]).unwrap_err();
    assert_eq!(e.message(), "y is not defined");
}

#[test]
fn control_flow() {
    assert_eq!(
        num(
            "function(){ let s = 0; for (let i = 0; i < 10; i++) { if (i % 2) continue; if (i > 6) break; s += i } return s }"
        ),
        12.0
    );
    assert_eq!(
        num("function(){ let i = 0; while (i < 5) i++; do { i += 10 } while (i < 30); return i }"),
        35.0
    );
    assert_eq!(
        text(
            "function(){ let r = ''; for (const k in {a:1, b:2}) r += k; for (const v of [3, 4]) r += v; for (const c of 'xy') r += c; return r }"
        ),
        "ab34xy"
    );
    assert_eq!(
        text(
            "function(){ switch (3) { case 1: return 'a'; case 3: case 4: return 'b'; default: return 'c' } }"
        ),
        "b"
    );
    assert_eq!(
        text(
            "function(){ let r = ''; switch ('z') { case 'y': r += 1; default: r += 2; case 'x': r += 3 } return r }"
        ),
        "23"
    );
    assert_eq!(
        text(
            "function(){ try { throw new TypeError('t') } catch (e) { return e.name + ':' + e.message } }"
        ),
        "TypeError:t"
    );
    assert_eq!(num("function(){ let n = 0; try { n = 1; return n } finally { n = 2 } }"), 1.0);
    assert_eq!(
        num(
            "function(){ outer: for (let i = 0; i < 3; i++) { for (let j = 0; j < 3; j++) { if (j == 1) continue outer; if (i == 2) break outer } } return 7 }"
        ),
        7.0
    );
    assert_eq!(text("function(){ return true ? 'y' : 'n' }"), "y");
    assert_eq!(
        text(
            "function(){ try { null.x } catch (e) { return e instanceof TypeError ? 'te' : 'other' } }"
        ),
        "te"
    );
}

#[test]
fn objects_arrays_and_destructuring() {
    assert_eq!(
        num("function(){ const o = {a: 1, 'b': 2, ['c' + 1]: 3}; return o.a + o['b'] + o.c1 }"),
        6.0
    );
    assert_eq!(
        num(
            "function(){ const {a, b: {c = 5} = {}, ...rest} = {a: 1, d: 2, e: 3}; return a + c + Object.keys(rest).length }"
        ),
        8.0
    );
    assert_eq!(
        num(
            "function(){ const [x, , y = 4, ...zs] = [1, 2, undefined, 5, 6]; return x + y + zs.length }"
        ),
        7.0
    );
    assert_eq!(
        num(
            "function(){ function f(a, b = a + 1, ...r){ return a + b + r.length } return f(1) + f(1, 1, 9, 9) }"
        ),
        7.0
    );
    assert_eq!(
        num(
            "function(){ const a = [1, 2]; const b = [...a, ...[3]]; return Math.max(...b) + b.length }"
        ),
        6.0
    );
    assert_eq!(
        text(
            "function(){ const o = {x: 1}; const p = {...o, y: 2}; delete p.x; return JSON.stringify(p) + ('y' in p) }"
        ),
        "{\"y\":2}true"
    );
    assert_eq!(
        text(
            "function(){ const o = {n: 2, get d(){ return this.n * 2 }, m(){ return this.n } }; return '' + o.d + o.m() }"
        ),
        "42"
    );
    assert_eq!(
        text("function(){ const o = {a: {b: null}}; return String(o.a?.b?.c) + String(o.z?.()) }"),
        "undefinedundefined"
    );
    assert_eq!(num("function(){ const a = []; a[3] = 1; return a.length }"), 4.0);
}

#[test]
fn classes_and_prototypes() {
    let src = "function(){ class A { constructor(x){ this.x = x } get twice(){ return this.x * 2 } static make(){ return new A(3) } }
                class B extends A { constructor(){ super(5) } twice2(){ return super.twice + 1 } }
                const b = new B(); return [A.make().twice, b.twice2(), b instanceof A] }";
    match run(src) {
        Value::Array(v) => {
            assert_eq!(v, vec![Value::Double(6.0), Value::Double(11.0), Value::Boolean(true)])
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        num(
            "function(){ function P(n){ this.n = n } P.prototype.get = function(){ return this.n }; return new P(9).get() }"
        ),
        9.0
    );
}

#[test]
fn builtins_of_strings_and_arrays() {
    assert_eq!(
        text("function(){ return 'Hello'.toUpperCase().split('').reverse().join('-') }"),
        "O-L-L-E-H"
    );
    assert_eq!(
        text(
            "function(){ return ' a b '.trim().replaceAll(' ', '_').padStart(5, '*') + 'x'.repeat(3) }"
        ),
        "**a_bxxx"
    );
    assert_eq!(
        text(
            "function(){ return 'abcdef'.slice(-3) + 'abcdef'.substring(1, 3) + 'abc'.at(-1) + 'abc'.indexOf('c') }"
        ),
        "defbcc2"
    );
    assert_eq!(text("function(){ return `${1 + 1} and ${'x'}` }"), "2 and x");
    assert_eq!(
        text(
            "function(){ return [3, 1, 10, 2].sort().join() + '|' + [3, 1, 10, 2].sort((a, b) => a - b).join() }"
        ),
        "1,10,2,3|1,2,3,10"
    );
    assert_eq!(
        num(
            "function(){ return [1, 2, 3, 4].filter(x => x % 2 == 0).map(x => x * 10).reduce((a, b) => a + b, 0) }"
        ),
        60.0
    );
    assert_eq!(
        text(
            "function(){ return JSON.stringify([[1, [2]], [3]].flat(2)) + [1, 2].includes(2) + [5, 6].findLast(x => x < 6) }"
        ),
        "[1,2,3]true5"
    );
    assert_eq!(
        text(
            "function(){ const a = [1, 2, 3]; a.splice(1, 1, 'x', 'y'); return a.join('') + a.indexOf('y') }"
        ),
        "1xy32"
    );
    assert_eq!(
        num(
            "function(){ return Array.from({length: 3}, (_, i) => i * 2).reduce((a, b) => a + b) }"
        ),
        6.0
    );
    assert_eq!(
        text("function(){ return JSON.stringify(Object.entries({a: 1, b: [2]})) }"),
        "[[\"a\",1],[\"b\",[2]]]"
    );
    assert_eq!(
        text(
            "function(){ return JSON.stringify(JSON.parse('{\"a\":[1,true,null,\"s\"],\"b\":{\"c\":1.5}}')) }"
        ),
        "{\"a\":[1,true,null,\"s\"],\"b\":{\"c\":1.5}}"
    );
    assert_eq!(
        text("function(){ return JSON.stringify({a: undefined, b: () => 1, c: 'q\"'}, null, 2) }"),
        "{\n  \"c\": \"q\\\"\"\n}"
    );
    assert_eq!(
        text(
            "function(){ const m = new Map([['a', 1]]); m.set('b', 2); const s = new Set([1, 1, 2]); return '' + m.get('b') + m.size + s.size + s.has(2) }"
        ),
        "222true"
    );
    assert_eq!(
        num(
            "function(){ return parseInt('ff', 16) + parseFloat('1.5e1') + Number('  7 ') + (+true) }"
        ),
        278.0
    );
    assert_eq!(
        text(
            "function(){ return (255).toString(16) + (3.14159).toFixed(2) + Number.isInteger(5.0) }"
        ),
        "ff3.14true"
    );
    assert_eq!(
        num("function(){ return Math.floor(Math.abs(-4.7)) + Math.round(2.5) + Math.sqrt(16) }"),
        11.0
    );
}

#[test]
fn dates_are_in_utc() {
    assert_eq!(num("function(){ return new Date(2020, 0, 1).getTime() }"), 1_577_836_800_000.0);
    assert_eq!(
        text("function(){ return new Date(3600000).toISOString() }"),
        "1970-01-01T01:00:00.000Z"
    );
    assert_eq!(
        text("function(){ return new Date(3600000).toString() }"),
        "Thu Jan 01 1970 01:00:00 GMT+0000 (UTC)"
    );
    assert_eq!(
        num(
            "function(){ const d = new Date('2021-03-04T05:06:07.890Z'); return d.getFullYear() + d.getMonth() + d.getDate() + d.getHours() + d.getDay() + d.getTimezoneOffset() }"
        ),
        2021.0 + 2.0 + 4.0 + 5.0 + 4.0
    );
    assert_eq!(run("function(){ return new Date(5) }"), Value::DateTime(5));
}

#[test]
fn errors_report_the_thrown_message() {
    let e = call_with("function(){ throw new Error('boom') }", None, &[]).unwrap_err();
    assert_eq!(e.message(), "boom");
    let e = call_with("function(){ throw 'raw' }", None, &[]).unwrap_err();
    assert_eq!(e.message(), "raw");
    let e = call_with("function(){ return undefinedFn() }", None, &[]).unwrap_err();
    assert_eq!(e.message(), "undefinedFn is not defined");
    let e = call_with("function(){ const o = {}; return o.f() }", None, &[]).unwrap_err();
    assert_eq!(e.message(), "o.f is not a function");
    let e = Program::compile("function(){ return  1 + }").unwrap_err();
    assert!(matches!(e, JsError::Compile { .. }), "{e:?}");
}

#[test]
fn this_and_arguments_come_from_the_caller() {
    let d = Document::new().with("a", 2).with("b", "x");
    assert_eq!(
        call_with("function(k){ return this.a * k }", Some(&d), &[Value::Int32(4)]).unwrap(),
        Value::Double(8.0)
    );
    assert_eq!(
        call_with("function(){ return obj.b + arguments.length }", Some(&d), &[Value::Null])
            .unwrap(),
        Value::String("x1".into())
    );
}

#[test]
fn regular_expressions_follow_ecmascript() {
    assert_eq!(
        text(
            "function(){ return /^a(b+)c$/i.test('ABBC') + ' ' + /(\\d+)-(?<y>\\d+)/.exec('x 12-34').groups.y }"
        ),
        "true 34"
    );
    assert_eq!(
        text("function(){ return 'a1b22c333'.replace(/\\d+/g, m => '<' + m.length + '>') }"),
        "a<1>b<2>c<3>"
    );
    assert_eq!(
        text(
            "function(){ return 'x-y_z'.split(/[-_]/).join('|') + ' ' + 'aaa'.match(/a/g).length }"
        ),
        "x|y|z 3"
    );
    assert_eq!(
        text("function(){ return 'John Smith'.replace(/(\\w+)\\s(\\w+)/, '$2, $1') }"),
        "Smith, John"
    );
    assert_eq!(
        text(
            "function(){ return String(/a(?=b)/.exec('acab').index) + /\\bfoo\\b/.test('a foo b') + /(a)\\1/.test('aa') }"
        ),
        "2truetrue"
    );
    assert_eq!(
        text(
            "function(){ const r = /o/g; r.exec('foo'); return r.lastIndex + ' ' + 'xAx'.search(/a/i) }"
        ),
        "2 1"
    );
    let e = Program::compile("function(){ return /(/ }")
        .and_then(|p| p.call(None, &[], &mut JsCtx::default()))
        .unwrap_err();
    assert!(e.message().contains("invalid regular expression"), "{e:?}");
}
