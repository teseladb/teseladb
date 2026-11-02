//! The global objects are built once per program and shared by its calls (016 SC-004, D-119):
//! a call sees the same globals as a fresh one, whatever the calls before it did.

use teseladb_bson::{Document, Value};
use teseladb_js::{JsCtx, JsError, Limits, Program};

fn call(p: &Program) -> Result<Value, JsError> {
    p.call(None, &[], &mut JsCtx::default())
}

#[test]
fn a_global_set_by_one_call_is_not_seen_by_the_next() {
    let p = Program::compile(
        "function(){ globalThis.n = (globalThis.n || 0) + 1; var k = typeof seen; seen = 1; \
         return n * 10 + (k == 'undefined' ? 0 : 5) }",
    )
    .unwrap();
    for _ in 0..3 {
        assert_eq!(call(&p), Ok(Value::Double(10.0)));
    }
}

#[test]
fn a_changed_builtin_does_not_reach_the_next_call() {
    let p = Program::compile(
        "function(){ var before = Array.prototype.length; \
         try { Array.prototype.push.call(Array.prototype, 5) } catch (e) {} \
         try { Object.prototype.leak = 1 } catch (e) {} \
         try { delete Math.PI } catch (e) {} \
         return [before, Object.prototype.leak === undefined, typeof Math.PI] }",
    )
    .unwrap();
    let first = call(&p).unwrap();
    for _ in 0..3 {
        assert_eq!(call(&p).unwrap(), first);
    }
}

#[test]
fn the_document_and_scope_of_each_call_are_its_own() {
    let p =
        Program::compile("this.a == obj.a && (typeof z == 'undefined' || z == this.a)").unwrap();
    for a in 0..4 {
        let doc = Document::new().with("a", a);
        let scope = Document::new().with("z", a);
        let mut ctx = JsCtx { scope: (a % 2 == 0).then_some(&scope), ..JsCtx::default() };
        assert_eq!(p.call(Some(&doc), &[], &mut ctx), Ok(Value::Boolean(true)));
    }
}

#[test]
fn a_call_that_ran_out_of_memory_leaves_the_next_one_working() {
    let p = Program::compile(
        "function(n){ var a = []; for (var i = 0; i < n; i++) a.push('x' + i); return a.length }",
    )
    .unwrap();
    let mut small =
        JsCtx { limits: Limits { heap: 4 << 20, ..Limits::default() }, ..JsCtx::default() };
    assert_eq!(p.call(None, &[Value::Int32(10_000_000)], &mut small), Err(JsError::OutOfMemory));
    assert_eq!(p.call(None, &[Value::Int32(3)], &mut small), Ok(Value::Double(3.0)));
    assert_eq!(p.call(None, &[Value::Int32(3)], &mut JsCtx::default()), Ok(Value::Double(3.0)));
}

#[test]
fn a_heap_too_small_for_the_globals_is_out_of_memory_every_time() {
    let p = Program::compile("function(){ return 1 }").unwrap();
    let mut tiny = JsCtx { limits: Limits { heap: 1024, ..Limits::default() }, ..JsCtx::default() };
    assert_eq!(p.call(None, &[], &mut tiny), Err(JsError::OutOfMemory));
    assert_eq!(p.call(None, &[], &mut tiny), Err(JsError::OutOfMemory));
    assert_eq!(call(&p), Ok(Value::Double(1.0)));
}

#[test]
fn clones_of_a_program_share_its_globals_safely() {
    let p =
        Program::compile("function(){ x = (typeof x == 'number' ? x : 0) + 1; return x }").unwrap();
    let q = p.clone();
    assert_eq!(call(&p), Ok(Value::Double(1.0)));
    assert_eq!(call(&q), Ok(Value::Double(1.0)));
    drop(p);
    assert_eq!(call(&q), Ok(Value::Double(1.0)));
}
