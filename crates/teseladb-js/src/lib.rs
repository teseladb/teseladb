//! TeselaDB's server-side JavaScript (016, D-021): an interpreter of ECMAScript written in safe
//! Rust for `$where`, `$function`, `$accumulator` and `mapReduce`. A program is parsed once and
//! called once per document; each call has its own heap and stops at the limits of steps, heap
//! bytes and call depth, or when the caller's deadline passes. The interpreter reads nothing
//! outside its arguments: no files, network, real clock or server state (016 FR-008); the
//! runtime gives it the cluster time for `Date` and a seed for `Math.random`.
#![cfg_attr(coverage, feature(coverage_attribute))]
#![forbid(unsafe_code)]

mod arrays;
mod ast;
mod bigint;
mod bigints;
mod bson;
mod builtins;
mod date;
mod interp;
mod json;
mod lexer;
mod math;
mod number;
mod parser;
mod regex;
mod strings;
mod value;

use std::cell::RefCell;
use std::fmt;
use std::rc::Rc;

use teseladb_bson::{Document, Value};

use crate::ast::{Expr, Stmt};
use crate::interp::{Ctrl, EnvRec, Fatal, Interp};
use crate::value::{Budget, Kind, Val};

pub use crate::interp::Limits;

impl Default for Limits {
    /// 100 million steps, 64 MiB of heap and 400 nested calls (016 data model).
    fn default() -> Limits {
        Limits { steps: 100_000_000, heap: 64 << 20, depth: 400 }
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
    /// The code threw: the exception's message (an error's `message`, or the value as text) and
    /// where the code that threw starts.
    Thrown {
        /// The message.
        message: String,
        /// The line, from 1.
        line: u32,
        /// The column, from 1.
        col: u32,
    },
    /// The heap limit was reached.
    OutOfMemory,
    /// The step limit was reached.
    Steps,
    /// The caller's deadline passed.
    Deadline,
    /// The result has a value BSON cannot hold (a symbol).
    NotConvertible(String),
    /// A function program was expected and the source is something else.
    NotAFunction,
}

impl JsError {
    /// The message RefDb shows after `message : '…'`.
    pub fn message(&self) -> String {
        match self {
            JsError::Compile { message, .. }
            | JsError::Thrown { message, .. }
            | JsError::NotConvertible(message) => message.clone(),
            JsError::OutOfMemory => "uncaught exception: out of memory".into(),
            JsError::Steps => "uncaught exception: too many steps".into(),
            JsError::Deadline => "operation exceeded time limit".into(),
            JsError::NotAFunction => "the code is not a function".into(),
        }
    }
}

impl fmt::Display for JsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message())
    }
}

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
}

impl fmt::Debug for Program {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Program").field("is_function", &self.is_function).finish_non_exhaustive()
    }
}

impl Program {
    /// Parses `source`.
    pub fn compile(source: &str) -> Result<Program, JsError> {
        let body = parser::parse_script(source).map_err(|e| JsError::Compile {
            message: e.message,
            line: e.line,
            col: e.col,
        })?;
        let is_function = matches!(body.as_slice(), [Stmt::Expr(Expr::Func(_))] | [Stmt::Func(..)]);
        Ok(Program { body: Rc::new(body), is_function })
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
        let budget = Rc::new(Budget { used: std::cell::Cell::new(0), limit: ctx.limits.heap });
        let realm = builtins::realm(&budget);
        let global_env = Rc::new(RefCell::new(EnvRec::default()));
        let emit = ctx.emit.take();
        let mut emitted: Vec<(Value, Value)> = Vec::new();
        let result = {
            let mut it = Interp {
                realm,
                budget: budget.clone(),
                limits: ctx.limits,
                steps: 0,
                depth: 0,
                envs: Vec::new(),
                global_env: global_env.clone(),
                now_ms: ctx.now_ms as f64,
                rng: ctx.seed,
                deadline: ctx.deadline,
                symbol_ids: 0,
                output: Vec::new(),
                last: None,
                pos: ast::Pos::default(),
                strict: false,
            };
            it.envs.push(Rc::downgrade(&global_env));
            let r = self.run(&mut it, this, args, ctx.scope, emit.is_some(), &mut emitted);
            drop(it);
            r
        };
        if let Some(e) = emit {
            for (k, v) in emitted {
                e(k, v);
            }
            ctx.emit = Some(e);
        }
        global_env.borrow_mut().vars.clear();
        result
    }

    fn run(
        &self,
        it: &mut Interp,
        this: Option<&Document>,
        args: &[Value],
        scope: Option<&Document>,
        with_emit: bool,
        emitted: &mut Vec<(Value, Value)>,
    ) -> Result<Value, JsError> {
        let out = (|| -> Result<Value, Ctrl> {
            builtins::install(it)?;
            if let Some(scope) = scope {
                for (k, v) in scope.iter() {
                    let x = bson::from_bson(it, v, 0)?;
                    let g = it.realm.global.clone();
                    it.define(&g, k, value::Prop::data(x))?;
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
                it.define(&g, "obj", value::Prop::data(this_val.clone()))?;
            }
            let env = it.global_env.clone();
            env.borrow_mut().this = Some(this_val.clone());
            let mut vals = Vec::with_capacity(args.len());
            for a in args {
                vals.push(bson::from_bson(it, a, 0)?);
            }
            it.hoist(&self.body, &env, true)?;
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
    if let Val::Obj(o) = &sink {
        it.grow(o, 1)?;
        if let Kind::Array(v) = &mut o.borrow_mut().kind {
            v.push(pair);
        }
    }
    Ok(Val::Undef)
}

/// The error of a control transfer that left the program.
fn error_of(it: &mut Interp, c: Ctrl) -> JsError {
    match c {
        Ctrl::Fatal(Fatal::OutOfMemory) => JsError::OutOfMemory,
        Ctrl::Fatal(Fatal::Steps) => JsError::Steps,
        Ctrl::Fatal(Fatal::Deadline) => JsError::Deadline,
        Ctrl::Fatal(Fatal::Convert(m)) => JsError::NotConvertible(m),
        Ctrl::Throw(v) => {
            // The heap may be full: the message is read without new allocations counting.
            it.budget.used.set(0);
            let message = match &v {
                Val::Obj(o) if matches!(o.borrow().kind, Kind::Error) => {
                    match it.get(&v, "message") {
                        Ok(m) => it.str_of(&m).map(|s| s.to_string()).unwrap_or_default(),
                        Err(_) => String::new(),
                    }
                }
                _ => it
                    .str_of(&v)
                    .map(|s| s.to_string())
                    .unwrap_or_else(|_| "uncaught exception".into()),
            };
            JsError::Thrown { message, line: it.pos.line, col: it.pos.col }
        }
        Ctrl::Return(_) | Ctrl::Break(_) | Ctrl::Continue(_) => JsError::Thrown {
            message: "unexpected control flow".into(),
            line: it.pos.line,
            col: it.pos.col,
        },
    }
}
