//! TeselaDB's server-side JavaScript (016, D-021): an interpreter of ECMAScript written in safe
//! Rust for `$where`, `$function`, `$accumulator` and `mapReduce`. A program is parsed once and
//! called once per document; each call has its own heap and stops at the limits of steps, heap
//! bytes and call depth, or when the caller's deadline passes. The interpreter reads nothing
//! outside its arguments: no files, network, real clock or server state (016 FR-008); the
//! runtime gives it the cluster time for `Date` and a seed for `Math.random`.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

mod annex;
mod arrays;
mod ast;
mod bigint;
mod bigints;
mod bson;
mod builtins;
mod date;
mod decompile;
mod early;
mod generators;
mod heap;
mod interp;
mod iterators;
mod json;
mod lexer;
mod lowering;
mod math;
mod md5;
mod names;
mod number;
mod order;
mod parser;
mod promise;
mod props;
mod proxy;
mod recursion;
mod regex;
mod setmethods;
mod shared;
mod shell;
mod strings;
mod symbols;
mod text;
mod typed;
mod uri;
mod value;
mod weak;

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use teseladb_bson::{Document, Value};

use crate::ast::{Expr, Stmt};
use crate::interp::{Ctrl, Fatal, Interp};
use crate::shared::Globals;
use crate::value::{Kind, Val};

pub use crate::interp::Limits;

/// The frames a call may hold before `too much recursion`, as a `refrouter` 9.0.4 counts them (the
/// program's function and every function it calls, a builtin included; measured on 02-11).
pub const MAX_FRAMES: usize = 51_000;
/// The native stack a call takes by default: well inside the 2 MiB of a spawned thread.
pub const DEFAULT_STACK: usize = 1 << 20;
/// The steps a call may run: a loop that fills the heap one element at a time (two steps a turn,
/// 73 million turns) reaches `out of memory` before them, as on a `refrouter` 9.0.4, which has no
/// limit of steps and stops a loop that never ends after about a minute.
pub const STEPS: u64 = 200_000_000;
/// The heap of a call: the 100 MiB `getJSHeapLimitMB()` gives on a `refrouter` 9.0.4, at the costs
/// measured there (A016-3-22).
pub const HEAP: usize = 100 << 20;
/// The bytes of this process a call may take: room for the largest values that fit in [`HEAP`]
/// (68 million numbers in an array, a string of 2^30 - 2 characters) with what this interpreter
/// spends on them, and a limit that protects the node.
pub const MEMORY: usize = 2 << 30;

impl Default for Limits {
    /// 200 million steps ([`STEPS`]), the heap of a `refrouter` 9.0.4 ([`HEAP`]) within 2 GiB of this
    /// process ([`MEMORY`]), its frames and 1 MiB of native stack, well inside the 2 MiB of a
    /// spawned thread (016 data model).
    fn default() -> Limits {
        Limits { steps: STEPS, heap: HEAP, memory: MEMORY, depth: MAX_FRAMES, stack: DEFAULT_STACK }
    }
}

/// Why a program does not give a value.
#[derive(Clone, Debug, PartialEq)]
pub enum JsError {
    /// The source does not parse: the message and where.
    Compile {
        /// What is wrong.
        message: String,
        /// The line, from 1.
        line: u32,
        /// The column, from 1.
        col: u32,
    },
    /// The code threw: the exception's message (an error's `message`, or `uncaught exception:`
    /// and the value as text), where SpiderMonkey reports it (where an error was made, or where
    /// the `throw` left the evaluation) in RefDb's columns, and an error's stack.
    Thrown {
        /// The message.
        message: String,
        /// The file an error names (`wasm:function`, or what `Error(message, file)` gave).
        file: String,
        /// The line, from 1.
        line: u32,
        /// The column, from 1, with what RefDb puts before the code on the first line.
        col: u32,
        /// An error's stack, one `name@file:line:column` line per call; `None` for a value that
        /// is not an error.
        stack: Option<String>,
    },
    /// The heap limit was reached.
    OutOfMemory,
    /// The step limit was reached.
    Steps,
    /// The caller's deadline passed.
    Deadline,
    /// The result has a value BSON cannot hold: RefDb's code (16662 for a symbol, 2 for a
    /// malformed `Timestamp`…) and message.
    NotConvertible {
        /// RefDb's error code.
        code: i32,
        /// What is wrong.
        message: String,
    },
    /// The engine of RefDb 9.0 stops with a trap (`wasm trap: uninitialized element`): a
    /// promise job or a `FinalizationRegistry`.
    Trap,
    /// The engine of RefDb 9.0 stops with `wasm trap: wasm \`unreachable\` instruction
    /// executed`: a shell function it declares and cannot run (`version()`).
    Unreachable,
    /// A function program was expected and the source is something else.
    NotAFunction,
    /// The engine of RefDb 9.0 stops with `wasm trap: call stack exhausted`: code whose
    /// parse nests more arrow functions than its stack holds.
    StackExhausted,
}

impl JsError {
    /// The message RefDb shows after `message : '…'`.
    pub fn message(&self) -> String {
        match self {
            JsError::Compile { message, .. }
            | JsError::Thrown { message, .. }
            | JsError::NotConvertible { message, .. } => message.clone(),
            JsError::OutOfMemory => "uncaught exception: out of memory".into(),
            JsError::Steps => "uncaught exception: too many steps".into(),
            JsError::Deadline => "operation exceeded time limit".into(),
            JsError::NotAFunction => "the code is not a function".into(),
            JsError::Trap => "wasm trap: uninitialized element".into(),
            JsError::Unreachable => "wasm trap: wasm `unreachable` instruction executed".into(),
            JsError::StackExhausted => parser::STACK_EXHAUSTED.into(),
        }
    }
}

impl fmt::Display for JsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

/// What RefDb puts before a function's code on its first line: `(`.
const FUNCTION_PREFIX: u32 = 1;
/// What RefDb puts before a `$where` expression on its first line (measured: an error at the
/// start of `this.a.b.c` is at column 15).
const WHERE_PREFIX: u32 = 14;

/// What a call reads from the runtime.
#[derive(Default)]
pub struct JsCtx<'a> {
    /// The call's limits.
    pub limits: Limits,
    /// The cluster time in milliseconds (`Date.now()`, `new Date()`).
    pub now_ms: i64,
    /// The seed of `Math.random` and `ObjectId()`.
    pub seed: u64,
    /// Whether the operation's deadline has passed; read every few thousand steps.
    pub deadline: Option<&'a dyn Fn() -> bool>,
    /// `emit(key, value)` of `mapReduce`'s `map`.
    pub emit: Option<&'a mut dyn FnMut(Value, Value)>,
    /// The global variables of `mapReduce`'s `scope`.
    pub scope: Option<&'a Document>,
}

impl fmt::Debug for JsCtx<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JsCtx")
            .field("limits", &self.limits)
            .field("now_ms", &self.now_ms)
            .finish_non_exhaustive()
    }
}

/// A parsed program: a function (`function(…){…}`, an arrow) or a script whose value is that of
/// its last expression statement (a `$where` string such as `this.a > 1`).
#[derive(Clone)]
pub struct Program {
    body: Rc<Vec<Stmt>>,
    is_function: bool,
    /// The global objects its calls share, built by the first call ([`shared`]).
    globals: Rc<RefCell<Option<Globals>>>,
}

impl fmt::Debug for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Program").field("is_function", &self.is_function).finish_non_exhaustive()
    }
}

impl Program {
    /// Parses `source`.
    pub fn compile(source: &str) -> Result<Program, JsError> {
        let body = parser::parse_script(&text::encode(source)).map_err(|e| {
            if e.message == parser::STACK_EXHAUSTED {
                return JsError::StackExhausted;
            }
            // RefDb compiles a function inside `(` and `)`: its first line moves one column.
            let t = source.trim_start();
            let wrapped = t.starts_with("function") || t.starts_with("async") || t.starts_with('(');
            let shift = if wrapped && e.line == 1 { FUNCTION_PREFIX } else { 0 };
            JsError::Compile {
                message: text::to_utf8(&e.message).into_owned(),
                line: e.line,
                col: e.col + shift,
            }
        })?;
        let is_function = matches!(body.as_slice(), [Stmt::Expr(Expr::Func(_))] | [Stmt::Func(..)]);
        Ok(Program { body: Rc::new(body), is_function, globals: Rc::default() })
    }

    /// Whether the source is one function.
    pub fn is_function(&self) -> bool {
        self.is_function
    }

    /// Calls the program: the function with `this` and `args`, or the script with `this` and
    /// `obj` bound to the document.
    pub fn call(
        &self,
        this: Option<&Document>,
        args: &[Value],
        ctx: &mut JsCtx<'_>,
    ) -> Result<Value, JsError> {
        let globals = match self.globals.borrow_mut().take() {
            Some(g) if g.fits(&ctx.limits) => g,
            _ => Globals::build(ctx.limits)?,
        };
        let emit = ctx.emit.take();
        let mut emitted: Vec<(Value, Value)> = Vec::new();
        let result = {
            let mut it = globals.enter(ctx.limits);
            it.now_ms = ctx.now_ms as f64;
            it.rng = ctx.seed;
            it.deadline = ctx.deadline;
            it.col_offset = if self.is_function { FUNCTION_PREFIX } else { WHERE_PREFIX };
            let r =
                self.run(&mut it, &globals, this, args, ctx.scope, emit.is_some(), &mut emitted);
            let global = it.realm.global.clone();
            drop(it);
            globals.keep_global(global);
            r
        };
        if let Some(e) = emit {
            for (k, v) in emitted {
                e(k, v);
            }
            ctx.emit = Some(e);
        }
        if globals.fits(&ctx.limits) {
            *self.globals.borrow_mut() = Some(globals);
        }
        result
    }

    fn run(
        &self,
        it: &mut Interp,
        globals: &Globals,
        this: Option<&Document>,
        args: &[Value],
        scope: Option<&Document>,
        with_emit: bool,
        emitted: &mut Vec<(Value, Value)>,
    ) -> Result<Value, JsError> {
        let out = (|| -> Result<Value, Ctrl> {
            globals.fill_global(it)?;
            if let Some(scope) = scope {
                for (k, v) in scope.iter() {
                    let x = bson::from_bson(it, v, 0)?;
                    let g = it.realm.global.clone();
                    it.define(&g, &text::encode(k), value::Prop::data(x))?;
                }
            }
            if with_emit {
                let f = it.native("emit", emit_native, 2)?;
                let g = it.realm.global.clone();
                it.define(&g, "emit", value::Prop::hidden(Val::Obj(f)))?;
                let sink = it.array(Vec::new())?;
                it.define(&g, "@@emitted", value::Prop::hidden(sink))?;
            }
            let this_val = match this {
                Some(d) => bson::from_document(it, d, 0)?,
                None => Val::Undef,
            };
            if this.is_some() {
                let g = it.realm.global.clone();
                let pristine = g.0.pristine.get();
                it.define(&g, "obj", value::Prop::data(this_val.clone()))?;
                // `obj` is the call's own and is taken off again when the global object is kept.
                g.0.pristine.set(pristine);
            }
            let env = it.global_env.clone();
            env.borrow_mut().this = Some(this_val.clone());
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(bson::from_bson(it, a, 0)?);
            }
            it.hoist(&self.body, &env, true)?;
            if !crate::parser::has_use_strict(&self.body) {
                it.hoist_block_functions(&self.body, &env, &[]);
            }
            let result = if self.is_function {
                let f = match self.body.first() {
                    Some(Stmt::Expr(e)) => it.eval(e, &env)?,
                    Some(Stmt::Func(n, _)) => it.read_var(&env, n)?,
                    _ => Val::Undef,
                };
                it.call(&f, &this_val, &vals)?
            } else {
                for s in self.body.iter() {
                    it.exec(s, &env)?;
                }
                it.last.take().unwrap_or(Val::Undef)
            };
            if with_emit {
                let g = Val::Obj(it.realm.global.clone());
                let sink = it.get(&g, "@@emitted")?;
                for pair in it.iterate(&sink)? {
                    let k = it.get(&pair, "0")?;
                    let v = it.get(&pair, "1")?;
                    emitted.push((
                        bson::to_bson(it, &k, "_id", 0)?,
                        bson::to_bson(it, &v, "value", 0)?,
                    ));
                }
            }
            bson::to_bson(it, &result, "__returnValue", 0)
        })();
        out.map_err(|c| error_of(it, c))
    }
}

/// `emit(key, value)` keeps the pair until the call ends.
fn emit_native(it: &mut Interp, _: &Val, a: &[Val]) -> Result<Val, Ctrl> {
    let g = Val::Obj(it.realm.global.clone());
    let sink = it.get(&g, "@@emitted")?;
    let pair = it.array(vec![
        a.first().cloned().unwrap_or(Val::Undef),
        a.get(1).cloned().unwrap_or(Val::Undef),
    ])?;
    // The sink is the array `run` made: no program can name it.
    let n = it.get(&sink, "length")?;
    let n = it.str_of(&n)?;
    it.set(&sink, &n, pair)?;
    Ok(Val::Undef)
}

/// The error of a control transfer that left the program.
pub(crate) fn error_of(it: &mut Interp, c: Ctrl) -> JsError {
    match c {
        Ctrl::Fatal(Fatal::OutOfMemory) => JsError::OutOfMemory,
        Ctrl::Fatal(Fatal::Steps) => JsError::Steps,
        Ctrl::Fatal(Fatal::Deadline) => JsError::Deadline,
        Ctrl::Fatal(Fatal::Convert(code, m)) => {
            JsError::NotConvertible { code, message: text::to_utf8(&m).into_owned() }
        }
        Ctrl::Fatal(Fatal::Trap) => JsError::Trap,
        Ctrl::Fatal(Fatal::Unreachable) => JsError::Unreachable,
        Ctrl::Fatal(Fatal::StackExhausted) => JsError::StackExhausted,
        Ctrl::Throw(Val::Str(s)) if it.out_of_memory.get() && &*s == interp::OUT_OF_MEMORY => {
            JsError::OutOfMemory
        }
        Ctrl::Throw(v) => {
            // The heap may be full: the message is read without new allocations counting.
            it.budget.used.set(crate::value::Cost::ZERO);
            let is_error = matches!(&v, Val::Obj(o) if matches!(o.borrow().kind, Kind::Error));
            if !is_error {
                let text =
                    it.str_of(&v).map(|s| text::to_utf8(&s).into_owned()).unwrap_or_default();
                let (line, col) = it.shown(it.thrown_at);
                return JsError::Thrown {
                    message: format!("uncaught exception: {text}"),
                    file: interp::ERROR_FILE.into(),
                    line,
                    col,
                    stack: None,
                };
            }
            let mut read = |key: &str| match it.get(&v, key) {
                Ok(Val::Undef) | Err(_) => None,
                Ok(x) => it.str_of(&x).ok().map(|s| text::to_utf8(&s).into_owned()),
            };
            let message =
                read("message").filter(|m| !m.is_empty()).unwrap_or_else(|| "none".into());
            let file = read("fileName").unwrap_or_else(|| interp::ERROR_FILE.into());
            let line = read("lineNumber").and_then(|l| l.parse().ok()).unwrap_or(0);
            let col = read("columnNumber").and_then(|c| c.parse().ok()).unwrap_or(0);
            JsError::Thrown { message, file, line, col, stack: read("stack") }
        }
        Ctrl::Return(_) | Ctrl::Break(_) | Ctrl::Continue(_) | Ctrl::Suspend => {
            let (line, col) = it.shown(it.pos);
            JsError::Thrown {
                message: "unexpected control flow".into(),
                file: interp::ERROR_FILE.into(),
                line,
                col,
                stack: None,
            }
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn kept(src: &str, this: Option<&Document>, args: &[Value]) -> bool {
        let p = Program::compile(src).unwrap();
        for _ in 0..3 {
            p.call(this, args, &mut JsCtx::default()).unwrap();
        }
        p.globals.borrow().is_some()
    }

    #[test]
    fn calls_that_change_no_shared_object_keep_the_globals() {
        let doc = Document::new().with("a", 41).with("b", "x");
        assert!(kept("function(a, b) { return a + b }", None, &[Value::Int32(1), Value::Int32(2)]));
        assert!(kept("this.a > 40 && this.b == 'x'", Some(&doc), &[]));
        assert!(kept(
            "function(){ var o = {a: [1, 2], m: new Map([[1, 2]])}; \
             return JSON.stringify(o) + [3, 1].sort().join() + 'ab'.toUpperCase() + \
             Object.keys(o).length + new Date(0).getTime() + /a(b)/.exec('ab')[1] + \
             Math.max(1, 2) + [1, 2].map(x => x * 2).reduce((s, x) => s + x) }",
            None,
            &[]
        ));
    }

    #[test]
    fn map_functions_emit_pairs_and_read_their_scope() {
        let p = Program::compile("function() { emit(this.k, this.v * n); emit(1) }").unwrap();
        assert!(p.is_function());
        assert!(format!("{p:?}").contains("is_function: true"));
        let doc = Document::new().with("k", "a").with("v", 2);
        let scope = Document::new().with("n", 10);
        let mut pairs = Vec::new();
        let mut sink = |k: Value, v: Value| pairs.push((k, v));
        let mut ctx = JsCtx { emit: Some(&mut sink), scope: Some(&scope), ..JsCtx::default() };
        assert!(format!("{ctx:?}").contains("limits"));
        p.call(Some(&doc), &[], &mut ctx).unwrap();
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0], (Value::from("a"), Value::Double(20.0)));
        assert!(!Program::compile("this.a").unwrap().is_function());
    }

    #[test]
    fn a_thrown_object_without_a_place_and_strict_programs() {
        let e = Program::compile("function() { throw {message: 'm'} }")
            .unwrap()
            .call(None, &[], &mut JsCtx::default())
            .unwrap_err();
        assert!(format!("{e:?}").contains("Thrown"));
        assert_eq!(e.message(), "uncaught exception: [object Object]");
        let strict = Program::compile("'use strict'; this.a = 1; 2").unwrap();
        let doc = Document::new();
        assert_eq!(strict.call(Some(&doc), &[], &mut JsCtx::default()), Ok(Value::Double(2.0)));
    }

    #[test]
    fn a_call_that_changes_a_shared_object_drops_the_globals() {
        assert!(!kept("function(){ Object.freeze(Math) }", None, &[]));
    }
}
