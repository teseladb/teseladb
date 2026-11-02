//! Server-side JavaScript of the node (016): `$where` in the matcher and the engine of
//! `$function` and `$accumulator` (the [`JsEngine`] of 010 T012), on `teseladb-js`. Programs are
//! parsed once per text and kept per thread; each call gets the cluster time, a seed of the
//! node's draws, and the command's deadline. The errors are those of a `refrouter` 9.0.4 (measured
//! on 02-11).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use teseladb_bson::{Document, RawDoc, RawValue, Value};
use teseladb_exec::expr::js::JsEngine;
use teseladb_js::{JsCtx, JsError, Program};
use teseladb_mql::{ErrorCode, MqlError};

/// `JSInterpreterFailure`.
pub const JS_INTERPRETER_FAILURE: ErrorCode = ErrorCode { code: 139, name: "JSInterpreterFailure" };
/// A `$where` that does not parse.
const WHERE_COMPILE: ErrorCode = ErrorCode { code: 6_108_306, name: "Location6108306" };
/// `$where` on a node without JavaScript.
const WHERE_DISABLED: ErrorCode = ErrorCode { code: 6_108_304, name: "Location6108304" };
/// `$function`, `$accumulator` or `mapReduce` on a node without JavaScript.
pub const JS_DISABLED: ErrorCode = ErrorCode { code: 31_264, name: "Location31264" };
/// A result that BSON cannot hold.
const NOT_CONVERTIBLE: ErrorCode = ErrorCode { code: 16_662, name: "Location16662" };
/// The trap of the WebAssembly build of RefDb 9.0's engine (a promise job, a
/// `FinalizationRegistry`), measured on a `refrouter` 9.0.4 on 02-11.
const WASM_TRAP: ErrorCode = ErrorCode { code: 11_542_340, name: "Location11542340" };
const MAX_TIME_MS_EXPIRED: ErrorCode = ErrorCode { code: 50, name: "MaxTimeMSExpired" };

/// The programs a thread keeps, at most this many texts.
const CACHE_SIZE: usize = 256;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(true) };
    static NOW_MS: Cell<i64> = const { Cell::new(0) };
    static DEADLINE_MS: Cell<Option<u64>> = const { Cell::new(None) };
    static CACHE: RefCell<HashMap<String, Rc<Program>>> = RefCell::new(HashMap::new());
    static FUNCTION_IDS: Cell<u64> = const { Cell::new(0) };
    /// The globals the database's `system.js` stores, for the running command.
    static STORED: RefCell<Option<Document>> = const { RefCell::new(None) };
}

/// The operators that run JavaScript.
const USES: [&str; 3] = ["$where", "$function", "$accumulator"];

/// Whether `d` (a command, at any depth) names one of [`USES`]; the command is not serialized,
/// since it may be over the size of a document.
fn uses_js(d: &Document) -> bool {
    fn value(v: &Value) -> bool {
        match v {
            Value::Document(d) => uses_js(d),
            Value::Array(a) => a.iter().any(value),
            _ => false,
        }
    }
    d.iter().any(|(k, v)| USES.contains(&k) || value(v))
}

/// Reads the functions and values `system.js` of the command's database stores, which are
/// globals of its JavaScript (measured on a `refrouter` 9.0.4 on 02-11), when the command may run
/// JavaScript.
pub(crate) fn load_stored<B: super::Backend + ?Sized>(
    backend: &mut B,
    db: &str,
    cmd: &Document,
) -> Result<(), crate::error::CommandError> {
    STORED.with(|s| s.borrow_mut().take());
    if !enabled() {
        return Ok(());
    }
    let is_map_reduce = cmd.iter().next().is_some_and(|(k, _)| k == super::mapreduce::MAP_REDUCE);
    if !is_map_reduce && !uses_js(cmd) {
        return Ok(());
    }
    let ns = teseladb_meta::catalog::Namespace {
        db: db.to_owned(),
        coll: super::names::SYSTEM_JS.to_owned(),
    };
    if backend.collection(&ns)?.is_none() {
        return Ok(());
    }
    let docs = super::aggregate::read_through(
        backend,
        &ns,
        &[],
        None,
        teseladb_range::read::ReadConcern::Local,
    )?;
    let mut globals = Document::new();
    for d in docs {
        if let (Some(Value::String(name)), Some(v)) = (d.get("_id"), d.get("value")) {
            globals.push(name, v.clone());
        }
    }
    STORED.with(|s| *s.borrow_mut() = Some(globals));
    Ok(())
}

/// `scope` over the stored globals.
fn globals(scope: Option<&Document>) -> Option<Document> {
    let stored = STORED.with(|s| s.borrow().clone());
    match (stored, scope) {
        (None, None) => None,
        (Some(s), None) => Some(s),
        (None, Some(g)) => Some(g.clone()),
        (Some(mut s), Some(g)) => {
            for (k, v) in g.iter() {
                s.insert(k, v.clone());
            }
            Some(s)
        }
    }
}

/// The monotonic clock in milliseconds the deadline of a call is read on.
static CLOCK: std::sync::OnceLock<fn() -> u64> = std::sync::OnceLock::new();

/// Sets the clock of [`CLOCK`] once for the process (the server's; without one a call has no
/// deadline but its step limit).
pub fn set_clock(clock: fn() -> u64) {
    let _ = CLOCK.set(clock);
}

/// What the calls of the command about to run see: whether JavaScript is enabled
/// (`--noscripting`, 016 FR-009), the cluster time and the command's deadline on the clock.
pub(crate) fn begin(enabled: bool, now_ms: u64, deadline_ms: Option<u64>) {
    ENABLED.with(|e| e.set(enabled));
    NOW_MS.with(|n| n.set(i64::try_from(now_ms).unwrap_or(i64::MAX)));
    DEADLINE_MS.with(|d| d.set(deadline_ms));
}

/// The deadline of the running command, read on the clock: `None` without one.
pub(crate) fn deadline_from(max_time_ms: Option<u64>) -> Option<u64> {
    let clock = *CLOCK.get()?;
    max_time_ms.filter(|ms| *ms > 0).map(|ms| clock().saturating_add(ms))
}

fn enabled() -> bool {
    ENABLED.with(Cell::get)
}

fn program(source: &str) -> Result<Rc<Program>, JsError> {
    if let Some(p) = CACHE.with(|c| c.borrow().get(source).cloned()) {
        return Ok(p);
    }
    let p = Rc::new(Program::compile(source)?);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_SIZE {
            c.clear();
        }
        c.insert(source.to_owned(), p.clone());
    });
    Ok(p)
}

fn next_function_id() -> u64 {
    FUNCTION_IDS.with(|n| {
        n.set(n.get() + 1);
        n.get()
    })
}

/// Runs `p` with the node's context.
fn run(p: &Program, this: Option<&Document>, args: &[Value]) -> Result<Value, JsError> {
    run_full(p, this, args, None, None)
}

/// [`run`] with `mapReduce`'s `emit` and `scope`.
fn run_full(
    p: &Program,
    this: Option<&Document>,
    args: &[Value],
    emit: Option<&mut dyn FnMut(Value, Value)>,
    scope: Option<&Document>,
) -> Result<Value, JsError> {
    let scope = globals(scope);
    let scope = scope.as_ref();
    let deadline = DEADLINE_MS.with(Cell::get);
    let clock = CLOCK.get().copied();
    let passed = move || match (deadline, clock) {
        (Some(d), Some(c)) => c() >= d,
        _ => false,
    };
    let mut ctx = JsCtx {
        now_ms: NOW_MS.with(Cell::get),
        seed: super::engines::next_seed(),
        deadline: Some(&passed),
        emit: match emit {
            Some(e) => Some(&mut *e),
            None => None,
        },
        scope,
        ..JsCtx::default()
    };
    p.call(this, args, &mut ctx)
}

/// The error of a call that failed, as RefDb words it after `what` (`Failed to invoke JS
/// predicate`, `Failed to invoke JS function`).
fn runtime_error(what: &str, e: &JsError) -> MqlError {
    match e {
        JsError::Deadline => MqlError::new(MAX_TIME_MS_EXPIRED, "operation exceeded time limit"),
        JsError::Trap => MqlError::new(WASM_TRAP, e.message()),
        JsError::NotConvertible(m) => MqlError::new(
            NOT_CONVERTIBLE,
            format!(
                "{what} :: function id = {}: e-runtime message : '{m}', file : 'none', line : 0, column : 0",
                next_function_id()
            ),
        ),
        JsError::OutOfMemory | JsError::Steps => MqlError::new(
            JS_INTERPRETER_FAILURE,
            format!(
                "{what} :: function id = {}: e-runtime message : '{}', file : 'none', line : 0, column : 1",
                next_function_id(),
                e.message()
            ),
        ),
        JsError::Thrown { message, file, line, col, stack } => MqlError::new(
            JS_INTERPRETER_FAILURE,
            format!(
                "{what} :: function id = {}: e-runtime message : '{message}', file : '{file}', line : {line}, column : {col}{}",
                next_function_id(),
                stack.as_ref().map(|s| format!("\n{s}")).unwrap_or_default()
            ),
        ),
        _ => MqlError::new(
            JS_INTERPRETER_FAILURE,
            format!(
                "{what} :: function id = {}: e-runtime message : '{}', file : 'wasm:function', line : 1, column : 1",
                next_function_id(),
                e.message()
            ),
        ),
    }
}

/// The text of a function's code.
fn code_text(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) | Value::JavaScript(s) => Some(s),
        Value::JavaScriptWithScope { code, .. } => Some(code),
        _ => None,
    }
}

/// `$where` on document `doc` (016 FR-004): the code is a function called with the document as
/// `this`, or an expression whose value decides; the document is also `obj`.
pub(crate) fn where_js(code: RawValue<'_>, doc: RawDoc<'_>) -> Result<bool, MqlError> {
    if !enabled() {
        return Err(MqlError::new(WHERE_DISABLED, "no globalScriptEngine in $where parsing"));
    }
    let code = Value::from(code);
    let text = code_text(&code)
        .ok_or_else(|| MqlError::new(teseladb_mql::codes::BAD_VALUE, "$where got bad type"))?;
    let p = program(text).map_err(|_| MqlError::new(WHERE_COMPILE, "$where compile error"))?;
    let d = Document::from(doc);
    match run(&p, Some(&d), &[]) {
        Ok(v) => Ok(truthy(&v)),
        Err(e) => Err(runtime_error("Failed to invoke JS predicate", &e)),
    }
}

/// Parses the `$where`s of `filter` as RefDb does with the query, before any document is read:
/// a code that does not parse is 6108306, and without JavaScript 6108304 (measured).
pub(crate) fn check_where(filter: &teseladb_mql::Filter) -> Result<(), MqlError> {
    use teseladb_mql::Expr as F;
    fn walk(e: &F) -> Result<(), MqlError> {
        match e {
            F::And(items) | F::Or(items) | F::Nor(items) => items.iter().try_for_each(walk),
            F::Where(code) => {
                if !enabled() {
                    return Err(MqlError::new(
                        WHERE_DISABLED,
                        "no globalScriptEngine in $where parsing",
                    ));
                }
                let code = Value::from(code.get());
                let text = code_text(&code).unwrap_or_default();
                program(text)
                    .map(|_| ())
                    .map_err(|_| MqlError::new(WHERE_COMPILE, "$where compile error"))
            }
            _ => Ok(()),
        }
    }
    walk(filter.expr())
}

/// JavaScript's truth of a result.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Boolean(b) => *b,
        Value::Null | Value::Undefined => false,
        Value::Double(x) => *x != 0.0 && !x.is_nan(),
        Value::Int32(n) => *n != 0,
        Value::Int64(n) => *n != 0,
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// The engine of `$function` and `$accumulator` on this node.
#[derive(Debug, Default)]
pub struct NodeJs;

impl JsEngine for NodeJs {
    fn call(&self, body: &Value, args: &[Value]) -> Result<Value, MqlError> {
        if !enabled() {
            return Err(MqlError::new(
                JS_DISABLED,
                "Cannot run server-side javascript without the javascript engine enabled",
            ));
        }
        let text = code_text(body).unwrap_or_default();
        let p = program(text).map_err(|e| compile_error(text, &e))?;
        if !p.is_function() {
            return Err(MqlError::new(
                JS_INTERPRETER_FAILURE,
                format!(
                    "Failed to create JS function :: source = {text}: e-compile message : 'unexpected token: identifier', file : 'common:parseHelper', line : 18, column : 34\nvalue@common:parseHelper:18:34\n"
                ),
            ));
        }
        run(&p, None, args).map_err(|e| runtime_error("Failed to invoke JS function", &e))
    }
}

fn compile_error(text: &str, e: &JsError) -> MqlError {
    let (line, col) = match e {
        JsError::Compile { line, col, .. } => (*line, *col),
        _ => (1, 1),
    };
    MqlError::new(
        JS_INTERPRETER_FAILURE,
        format!(
            "Failed to create JS function :: source = {text}: e-compile message : '{}', file : 'wasm:function', line : {line}, column : {col}",
            e.message()
        ),
    )
}

/// Whether server-side JavaScript runs on this node now.
pub(crate) fn is_enabled() -> bool {
    enabled()
}

/// Calls `code` (a function, or a script whose last value is the result) for `mapReduce`, with
/// `this`, `args`, `emit` and the globals of `scope`; an error is worded as for `$function`.
pub(crate) fn call_map_reduce(
    code: &Value,
    this: Option<&Document>,
    args: &[Value],
    emit: Option<&mut dyn FnMut(Value, Value)>,
    scope: Option<&Document>,
) -> Result<Value, MqlError> {
    let text = code_text(code).unwrap_or_default();
    let p = program(text).map_err(|e| compile_error(text, &e))?;
    run_full(&p, this, args, emit, scope)
        .map_err(|e| runtime_error("Failed to invoke JS function", &e))
}

/// The engine every scope of the node calls.
pub(crate) fn engine() -> Rc<dyn JsEngine> {
    thread_local! {
        static ENGINE: Rc<dyn JsEngine> = Rc::new(NodeJs);
    }
    ENGINE.with(Rc::clone)
}
