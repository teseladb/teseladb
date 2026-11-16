//! Server-side JavaScript of the node (016): `$where` in the matcher and the engine of
//! `$function` and `$accumulator` (the [`JsEngine`] of 010 T012), on `teseladb-js`. Programs are
//! parsed once per text and kept per thread; each call gets the cluster time, a seed of the
//! node's draws, and the command's deadline. The errors are those of the reference router 9.0.4
//! (measured on 02-11).

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
/// A result that BSON cannot hold: a symbol, a malformed value of a BSON type (`BadValue`), a
/// `Timestamp` without its time or its increment (measured on the reference router 9.0.4 on 02-11).
const NOT_CONVERTIBLE: [ErrorCode; 4] = [
    ErrorCode { code: 16_662, name: "Location16662" },
    ErrorCode { code: 2, name: "BadValue" },
    ErrorCode { code: 6_900_900, name: "Location6900900" },
    ErrorCode { code: 6_900_901, name: "Location6900901" },
];
/// The trap of the WebAssembly build of the reference server 9.0's engine (a promise job, a
/// `FinalizationRegistry`), measured on the reference router 9.0.4 on 02-11.
const WASM_TRAP: ErrorCode = ErrorCode { code: 11_542_340, name: "Location11542340" };
const MAX_TIME_MS_EXPIRED: ErrorCode = ErrorCode { code: 50, name: "MaxTimeMSExpired" };
const INTERRUPTED: ErrorCode = ErrorCode { code: 11_601, name: "Interrupted" };

/// The programs a thread keeps, at most this many texts.
const CACHE_SIZE: usize = 256;

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(true) };
    static NOW_MS: Cell<i64> = const { Cell::new(0) };
    static DEADLINE_MS: Cell<Option<u64>> = const { Cell::new(None) };
    /// The time limit of the operation running on this thread ([`set_op_limit`]).
    static OP_LIMIT_MS: Cell<Option<u64>> = const { Cell::new(None) };
    static CACHE: RefCell<HashMap<String, Rc<Program>>> = RefCell::new(HashMap::new());
    static FUNCTION_IDS: Cell<u64> = const { Cell::new(0) };
    /// The native stack this thread gives a call ([`set_stack`]).
    static STACK: Cell<usize> = const { Cell::new(teseladb_js::DEFAULT_STACK) };
    /// The globals the database's `system.js` stores, for the running command.
    static STORED: RefCell<Option<Document>> = const { RefCell::new(None) };
}

/// What the calls of the command running on this thread see ([`super::context`]).
#[derive(Debug)]
pub(crate) struct Context {
    enabled: bool,
    now_ms: i64,
    deadline_ms: Option<u64>,
    op_limit_ms: Option<u64>,
    stack: usize,
    stored: Option<Document>,
    interrupt: crate::ops::Interrupt,
}

/// Takes the state of the running command, leaving a new thread's.
pub(crate) fn take_context() -> Context {
    Context {
        enabled: ENABLED.replace(true),
        now_ms: NOW_MS.replace(0),
        deadline_ms: DEADLINE_MS.replace(None),
        op_limit_ms: OP_LIMIT_MS.replace(None),
        stack: STACK.get(),
        stored: STORED.with(|s| s.borrow_mut().take()),
        interrupt: INTERRUPT.with(|i| i.replace(crate::ops::Interrupt::default())),
    }
}

/// Puts back what [`take_context`] took.
pub(crate) fn put_context(c: Context) {
    ENABLED.set(c.enabled);
    NOW_MS.set(c.now_ms);
    DEADLINE_MS.set(c.deadline_ms);
    OP_LIMIT_MS.set(c.op_limit_ms);
    STACK.set(c.stack);
    STORED.with(|s| *s.borrow_mut() = c.stored);
    INTERRUPT.with(|i| *i.borrow_mut() = c.interrupt);
}

thread_local! {
    /// What `killOp` raises for the operation running on this thread, one for the thread's
    /// whole life (and one for each command that waits in an engine thread, which takes it with
    /// its [`Context`]): an operation takes it without allocating.
    static INTERRUPT: RefCell<crate::ops::Interrupt> =
        RefCell::new(crate::ops::Interrupt::default());
}

/// This thread's interrupt flag, which the calls of the running operation read with their
/// deadline, so that `killOp` stops a long script (019 FR-026).
pub(crate) fn thread_interrupt() -> crate::ops::Interrupt {
    INTERRUPT.with(|i| i.borrow().clone())
}

/// Whether `killOp` raised the running operation's flag.
fn interrupted() -> bool {
    INTERRUPT.with(|i| i.borrow().raised())
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
/// globals of its JavaScript (measured on the reference router 9.0.4 on 02-11), when the command
/// may run JavaScript.
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

/// How the thread of a call waits for `sleep(ms)`, set once for the process (the server's: the
/// thread sleeps); without one `sleep` counts its wait as steps.
static SLEEPER: std::sync::OnceLock<fn(u64)> = std::sync::OnceLock::new();
/// The longest a `sleep` waits before it reads the deadline and `killOp` again.
const SLEEP_SLICE_MS: u64 = 10;

/// Sets the wait of [`SLEEPER`] once for the process.
pub fn set_sleeper(sleeper: fn(u64)) {
    let _ = SLEEPER.set(sleeper);
}

/// The native stack of a thread that runs commands: room for the 51 000 nested calls of
/// the reference router 9.0.4 (016 A016-3-18). The system reserves it and only commits what a call
/// uses.
pub const COMMAND_STACK: usize = 256 << 20;
/// What a thread that runs commands keeps of its stack for the frames below the calls.
const STACK_SPARE: usize = 8 << 20;

/// What runs a call on a native stack of its own (016 A016-3-18): it calls the function it is given
/// once, with `Some(bytes)` on a fresh stack of that size, or with `None` on the caller's stack
/// (the caller already runs on such a stack, or none could be made). A process whose threads have
/// small stacks (the engine's) sets one so that a call keeps the depth of [`COMMAND_STACK`].
pub type DeepStack = fn(&mut dyn FnMut(Option<usize>));

static DEEP_STACK: std::sync::OnceLock<DeepStack> = std::sync::OnceLock::new();

/// Sets the [`DeepStack`] of the process, once.
pub fn set_deep_stack(runner: DeepStack) {
    let _ = DEEP_STACK.set(runner);
}

/// The native stack a call takes on a stack of `size` bytes.
fn stack_for(size: usize) -> usize {
    size.saturating_sub(STACK_SPARE).max(teseladb_js::DEFAULT_STACK)
}

/// The free memory of the thread that runs a call, when its memory is a pool of its own that a
/// failed allocation ends the process on (a shard of the engine); `None` elsewhere.
pub type MemoryRoom = fn() -> Option<usize>;

static MEMORY_ROOM: std::sync::OnceLock<MemoryRoom> = std::sync::OnceLock::new();

/// Sets the [`MemoryRoom`] of the process, once.
pub fn set_memory_room(room: MemoryRoom) {
    let _ = MEMORY_ROOM.set(room);
}

/// What a value takes at most while it is made, in parts of the room: the call's live values,
/// the new value's bytes and the interpreter's copy of them (a joined string is built, then
/// copied into the interpreter's own string).
const ROOM_PARTS: usize = 3;

/// The bytes of this process a call may take ([`teseladb_js::Limits::memory`]) with `room` bytes
/// free on its thread: a part of the room, so that what the call makes fits in it and passing
/// the limit ends the call with an error instead of a failed allocation; without a pool of its
/// own, the interpreter's limit.
fn memory_limit(room: Option<usize>) -> usize {
    room.map_or(teseladb_js::MEMORY, |free| (free / ROOM_PARTS).min(teseladb_js::MEMORY))
}

/// Tells the calls on this thread that it has a stack of `size` bytes ([`COMMAND_STACK`] for the
/// threads that run commands); without it a call takes the 1 MiB that any thread has.
pub fn set_stack(size: usize) {
    STACK.with(|s| s.set(stack_for(size)));
}

/// What the calls of the command about to run see: whether JavaScript is enabled
/// (`--noscripting`, 016 FR-009), the cluster time and the command's deadline on the clock.
pub(crate) fn begin(enabled: bool, now_ms: u64, deadline_ms: Option<u64>) {
    ENABLED.with(|e| e.set(enabled));
    NOW_MS.with(|n| n.set(i64::try_from(now_ms).unwrap_or(i64::MAX)));
    DEADLINE_MS.with(|d| d.set(deadline_ms));
}

/// The deadline of the running command, read on the clock: `None` without one. The limit of
/// the operation the command runs in, when it has one, is the one its calls keep: it holds the
/// tenant's cap and the default of reads as well as the command's `maxTimeMS` (007 FR-036).
pub(crate) fn deadline_from(max_time_ms: Option<u64>) -> Option<u64> {
    let clock = *CLOCK.get()?;
    let limit = OP_LIMIT_MS.with(Cell::get).or(max_time_ms);
    limit.filter(|ms| *ms > 0).map(|ms| clock().saturating_add(ms))
}

/// Sets the time limit of the operation about to run on this thread, `None` once it ends: the
/// limit that was set before, for the operation it runs inside.
pub(crate) fn set_op_limit(limit_ms: Option<u64>) -> Option<u64> {
    OP_LIMIT_MS.with(|l| l.replace(limit_ms))
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
    let passed = move || {
        interrupted()
            || match (deadline, clock) {
                (Some(d), Some(c)) => c() >= d,
                _ => false,
            }
    };
    // `sleep(ms)` waits on this thread in slices, reading the deadline and `killOp` between them.
    let sleeper = SLEEPER.get().copied();
    let wait = |ms: u64| {
        let Some(sleep) = sleeper else { return false };
        let mut left = ms;
        while left > 0 && !passed() {
            let slice = left.min(SLEEP_SLICE_MS);
            sleep(slice);
            left -= slice;
        }
        passed()
    };
    let thread_stack = STACK.with(Cell::get);
    let memory = memory_limit(MEMORY_ROOM.get().and_then(|room| room()));
    let (now_ms, seed) = (NOW_MS.with(Cell::get), super::engines::next_seed());
    let sleep = sleeper.map(|_| &wait as &dyn Fn(u64) -> bool);
    let mut emit = emit;
    let mut result = None;
    let mut call = |deep: Option<usize>| {
        // On a stack of its own the call has all of it; otherwise what this thread gives.
        let stack = deep.map_or(thread_stack, stack_for);
        let mut ctx = JsCtx {
            limits: teseladb_js::Limits { stack, memory, ..Default::default() },
            now_ms,
            seed,
            deadline: Some(&passed),
            sleep,
            emit: match emit.as_mut() {
                Some(e) => Some(&mut **e),
                None => None,
            },
            scope,
        };
        result = Some(p.call(this, args, &mut ctx));
    };
    match DEEP_STACK.get() {
        Some(deep) => deep(&mut call),
        None => call(None),
    }
    // A runner calls it once; one that did not leaves the call unmade, as out of memory.
    result.unwrap_or(Err(JsError::OutOfMemory))
}

/// The error of a call that failed, as the reference server words it after `what` (`Failed to
/// invoke JS predicate`, `Failed to invoke JS function`).
fn runtime_error(what: &str, e: &JsError) -> MqlError {
    match e {
        JsError::Deadline if interrupted() => {
            MqlError::new(INTERRUPTED, "operation was interrupted")
        }
        JsError::Deadline => MqlError::new(MAX_TIME_MS_EXPIRED, "operation exceeded time limit"),
        JsError::Trap | JsError::Unreachable | JsError::StackExhausted => {
            MqlError::new(WASM_TRAP, e.message())
        }
        JsError::NotConvertible { code, message: m } => MqlError::new(
            NOT_CONVERTIBLE.into_iter().find(|c| c.code == *code).unwrap_or(NOT_CONVERTIBLE[0]),
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

/// Parses the `$where`s of `filter` as the reference server does with the query, before any
/// document is read: a code that does not parse is 6108306, and without JavaScript 6108304
/// (measured).
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
        let p = function_program(body)?;
        run(&p, None, args).map_err(|e| runtime_error("Failed to invoke JS function", &e))
    }

    fn call_accumulate(&self, body: &Value, args: &[Value]) -> Result<Value, MqlError> {
        if !enabled() {
            return self.call(body, args);
        }
        let p = function_program(body)?;
        run(&p, None, args)
            .map_err(|e| runtime_error("Failed to invoke JS function", &from_new_function(e)))
    }
}

/// The program of a `$function` or `$accumulator` body, as the reference server builds it
/// (measured on 9.0.4, 14-11-2026, 016 T014): a function as it is; anything else that parses,
/// wrapped in a function that returns it when it is an expression or runs it when it is
/// statements; a body that does not parse fails in the reference's `parseHelper`.
fn function_program(body: &Value) -> Result<Rc<Program>, MqlError> {
    let text = code_text(body).unwrap_or_default();
    if text.trim_start().starts_with(FUNCTION_KEYWORD) {
        return program(text).map_err(|e| compile_error(text, &e));
    }
    program(text).map_err(|e| {
        MqlError::new(
            JS_INTERPRETER_FAILURE,
            format!(
                "Failed to create JS function :: source = {text}: e-compile message : '{}', file : 'common:parseHelper', line : 18, column : 34\nvalue@common:parseHelper:18:34\n",
                e.message()
            ),
        )
    })?;
    let wrapped = match program(&format!("({text})")) {
        // Its leading spaces before the `return`, as the reference's columns show (` zz` fails a
        // column after `zz`).
        Ok(_) => {
            let body = text.trim_start();
            let lead = &text[..text.len() - body.len()];
            format!("function() {{ {lead}return {body} }}")
        }
        Err(_) => format!("function() {{ {text} }}"),
    };
    program(&wrapped).map_err(|e| compile_error(text, &e))
}

/// The keyword a body that is already a function starts with.
const FUNCTION_KEYWORD: &str = "function";

/// A throw in `accumulate` as the reference server reports it: from the `newFunction` it builds
/// `accumulate` with, called by its wrapper at column 185 (measured on 9.0.4).
fn from_new_function(e: JsError) -> JsError {
    match e {
        JsError::Thrown { message, file, line, col, stack } if file == WASM_FUNCTION_FILE => {
            let own = format!("@{WASM_FUNCTION_FILE}:");
            let renamed = format!("@{NEW_FUNCTION_FILE}:");
            let stack = stack.map(|s| {
                let s = s.replacen(&own, &renamed, 1);
                format!("{s}{ACCUMULATE_CALLER}\n")
            });
            JsError::Thrown { message, file: NEW_FUNCTION_FILE.to_owned(), line, col, stack }
        }
        other => other,
    }
}

/// The file of a function's code in the reference server's messages.
const WASM_FUNCTION_FILE: &str = "wasm:function";
/// The file of `accumulate`'s code.
const NEW_FUNCTION_FILE: &str = "wasm:newFunction";
/// The frame of the wrapper that calls `accumulate`.
const ACCUMULATE_CALLER: &str = "@wasm:function:1:185";

fn compile_error(text: &str, e: &JsError) -> MqlError {
    if matches!(e, JsError::StackExhausted) {
        return MqlError::new(WASM_TRAP, e.message());
    }
    let (line, col) = match e {
        JsError::Compile { line, col, .. } => (*line, *col),
        _ => (1, 1),
    };
    MqlError::new(
        JS_INTERPRETER_FAILURE,
        format!(
            "Failed to create JS function :: source = {text}: e-compile message : '{}', file : '{}', line : {line}, column : {col}",
            e.message(),
            // A parse the engine stops for its depth has no place.
            if line == 0 { "none" } else { "wasm:function" }
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

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_call_takes_a_third_of_its_threads_free_memory_and_at_most_the_interpreters_limit() {
        assert_eq!(memory_limit(None), teseladb_js::MEMORY);
        assert_eq!(memory_limit(Some(900 << 20)), 300 << 20);
        assert_eq!(memory_limit(Some(usize::MAX)), teseladb_js::MEMORY);
        assert_eq!(memory_limit(Some(0)), 0);
    }

    static DEEP_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// A runner that counts its calls and runs each on the caller's stack, as the process's
    /// runner does on a stack that is already its own: the other tests see no change.
    fn counting_runner(call: &mut dyn FnMut(Option<usize>)) {
        DEEP_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        call(None);
    }

    /// Every call goes through the process's runner, and a stack of its own gives the call all of
    /// it but the spare.
    #[test]
    fn calls_go_through_the_deep_stack_runner() {
        set_deep_stack(counting_runner);
        let before = DEEP_CALLS.load(std::sync::atomic::Ordering::SeqCst);
        let p = program("function(a) { return a + 1; }").unwrap();
        let v = run(&p, None, &[Value::Int32(1)]).unwrap();
        assert!(matches!(v, Value::Int32(2) | Value::Double(_)), "{v:?}");
        assert!(DEEP_CALLS.load(std::sync::atomic::Ordering::SeqCst) > before);
        assert_eq!(stack_for(COMMAND_STACK), COMMAND_STACK - STACK_SPARE);
        assert_eq!(stack_for(0), teseladb_js::DEFAULT_STACK);
    }

    /// The limit of an operation run inside another comes back when the inner one ends.
    #[test]
    fn an_operation_limit_restores_the_one_it_ran_inside() {
        assert_eq!(set_op_limit(Some(150)), None);
        assert_eq!(set_op_limit(Some(40)), Some(150));
        assert_eq!(set_op_limit(Some(150)), Some(40));
        assert_eq!(set_op_limit(None), Some(150));
    }

    /// `killOp` raises the operation's flag: a long script stops with `Interrupted`, not with
    /// the time limit, and the flag set before comes back.
    #[test]
    fn a_raised_interrupt_stops_a_script_as_interrupted() {
        let flag = thread_interrupt();
        let code = Value::String("function() { for (;;) {} }".into());
        let doc = Document::new().with("a", 1).to_bytes().unwrap();
        let raw_doc = RawDoc::from_bytes(&doc).unwrap();
        let wrapper = Document::new().with("c", code).to_bytes().unwrap();
        let raw = RawDoc::from_bytes(&wrapper).unwrap();
        let c = raw.get("c").unwrap();
        flag.0.store(true, std::sync::atomic::Ordering::SeqCst);
        let e = where_js(c, raw_doc).unwrap_err();
        flag.0.store(false, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(e.code.code, 11_601, "{e:?}");
        assert!(!interrupted());
    }

    /// A function nested deeper than the engine parses fails with no place, and one whose arrow
    /// functions exhaust its stack with its trap (measured on the reference router 9.0.4 on 02-11).
    #[test]
    fn a_function_nested_too_deep_fails_as_on_the_reference_router() {
        let deep = format!("function(){{ return {}1{} }}", "(".repeat(67), ")".repeat(67));
        let e = compile_error(&deep, &program(&deep).unwrap_err());
        assert_eq!(e.code.code, 139);
        assert!(
            e.message.ends_with(
                "e-compile message : 'too much recursion', file : 'none', line : 0, column : 1"
            ),
            "{}",
            e.message
        );
        let arrows = format!("function(){{ return {}1 }}", "x=>".repeat(314));
        let e = compile_error(&arrows, &program(&arrows).unwrap_err());
        assert_eq!(
            (e.code.code, e.message.as_str()),
            (11_542_340, "wasm trap: call stack exhausted")
        );
    }
}
