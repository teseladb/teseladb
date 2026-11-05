//! `configureFailPoint` with `failCommand` (FR-034, DV-025): with `enableTestCommands` a test
//! makes the node fail the commands it names, as the drivers' unified tests do against a
//! `refrouter`. The failpoint answers with an error and its labels, closes the connection, holds it
//! for `blockTimeMS` before running the command, or adds a `writeConcernError` to its reply.
//! Replies and messages are those of a `refrouter` 9.0.4 started with `enableTestCommands=1`
//! (measured). `data` is read when a command arrives, so a fault in it makes the failpoint do
//! nothing instead of failing `configureFailPoint`, as there.

use teseladb_bson::{Document, Value};

use crate::error::{
    BAD_VALUE, CHANGE_STREAM_FATAL, Code, CommandError, FAIL_POINT_SET_FAILED, HOST_UNREACHABLE,
    ILLEGAL_OPERATION, TRANSIENT_TRANSACTION_ERROR, TYPE_MISMATCH, UNAUTHORIZED, code,
};
use crate::types::number;

/// `configureFailPoint`.
pub const CONFIGURE_FAIL_POINT: &str = "configureFailPoint";
/// The failpoint that fails the commands it names.
pub const FAIL_COMMAND: &str = "failCommand";
/// The failpoint that fails a `getMore` once its cursor was taken.
pub const FAIL_GET_MORE: &str = "failGetMoreAfterCursorCheckout";
/// `errmsg` of a `getMore` that failpoint fails.
const GET_MORE_MESSAGE: &str = "Hit the 'failGetMoreAfterCursorCheckout' failpoint";
/// `errmsg` of an injected error.
pub const FAILING_MESSAGE: &str = "Failing command via 'failCommand' failpoint";
/// `mode` strings.
const OFF: &str = "off";
const ALWAYS_ON: &str = "alwaysOn";

/// When the failpoint fires.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Mode {
    /// Never.
    #[default]
    Off,
    /// On every command it names.
    AlwaysOn,
    /// On the next `n`.
    Times(i64),
    /// After letting `n` through.
    Skip(i64),
    /// With this probability.
    Probability(f64),
}

/// What the failpoint does to one command.
#[derive(Clone, Debug, PartialEq)]
pub struct Fault {
    /// Milliseconds the connection waits before the command (0: none).
    pub block_ms: u64,
    /// Whether the wait already happened.
    pub blocked: bool,
    /// Close the connection without a reply.
    pub close: bool,
    /// The error reply instead of the command's.
    pub error: Option<Document>,
    /// `writeConcernError` added to the command's reply.
    pub write_concern_error: Option<Document>,
    /// The labels the test named, which go with the `writeConcernError` too.
    pub labels: Option<Value>,
}

/// The codes whose error a `refrouter` 9.0.4 labels `TransientTransactionError` in a statement of a
/// transaction (measured on 05-11-2026 with `failCommand` on the router): the retryable ones,
/// `LockTimeout`, `WriteConflict`, `SnapshotUnavailable`, `NoSuchTransaction` and
/// `PreparedTransactionInProgress`.
const TRANSIENT_IN_TRANSACTION: [i32; 17] =
    [6, 7, 24, 89, 91, 112, 189, 246, 251, 262, 267, 9001, 10107, 11600, 11602, 13435, 13436];
/// The codes it labels so in `commitTransaction` and `abortTransaction`.
const TRANSIENT_AT_END: [i32; 5] = [24, 112, 246, 251, 267];

/// Error reply `reply` of command `name` with the `TransientTransactionError` a `refrouter` 9.0.4
/// adds when the command is a statement of a transaction (`autocommit: false`); outside one it
/// adds no label.
pub fn in_transaction(mut reply: Document, name: &str, cmd: &Document) -> Document {
    if cmd.get("autocommit") != Some(&Value::Boolean(false)) {
        return reply;
    }
    let Some(Value::Int32(code)) = reply.get("code") else { return reply };
    let ends = matches!(name, "commitTransaction" | "abortTransaction");
    let transient = match ends {
        true => TRANSIENT_AT_END.contains(code),
        false => TRANSIENT_IN_TRANSACTION.contains(code),
    };
    if !transient {
        return reply;
    }
    let label = Value::from(TRANSIENT_TRANSACTION_ERROR);
    let mut labels = match reply.get("errorLabels") {
        Some(Value::Array(l)) => l.clone(),
        _ => Vec::new(),
    };
    if !labels.contains(&label) {
        labels.push(label);
    }
    reply.insert("errorLabels", Value::Array(labels));
    reply
}

/// The node's failpoints: `failCommand` and `failGetMoreAfterCursorCheckout`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FailPoint {
    mode: Mode,
    data: Document,
    /// Times it fired since the node started (`count` of the reply).
    entered: i32,
    rng: u64,
    /// `failGetMoreAfterCursorCheckout`.
    get_more: GetMoreFail,
}

/// `failGetMoreAfterCursorCheckout`: its mode, its `data` and the times it fired.
#[derive(Clone, Debug, Default, PartialEq)]
struct GetMoreFail {
    mode: Mode,
    data: Document,
    entered: i32,
}

fn bad(msg: impl Into<String>) -> CommandError {
    CommandError::new(BAD_VALUE, msg)
}

/// Reads `mode`.
fn mode(v: &Value) -> Result<Mode, CommandError> {
    let count = |d: &Document, k: &str| -> Result<Option<i64>, CommandError> {
        match d.get(k).map(number) {
            None => Ok(None),
            Some(Some(n)) if n >= 0.0 => Ok(Some(n as i64)),
            Some(_) => Err(bad(format!("'{k}' option to 'mode' must be positive"))),
        }
    };
    match v {
        Value::String(s) if s == OFF => Ok(Mode::Off),
        Value::String(s) if s == ALWAYS_ON => Ok(Mode::AlwaysOn),
        Value::String(s) => Err(bad(format!("unknown mode: {s}"))),
        Value::Document(d) => {
            if let Some(n) = count(d, "times")? {
                return Ok(Mode::Times(n));
            }
            if let Some(n) = count(d, "skip")? {
                return Ok(Mode::Skip(n));
            }
            match d.get("activationProbability").map(number) {
                Some(Some(p)) if (0.0..=1.0).contains(&p) => Ok(Mode::Probability(p)),
                Some(_) => Err(bad("activationProbability must be between 0.0 and 1.0")),
                None => {
                    Err(bad("'mode' must be one of 'off', 'alwaysOn', '{times:n}', '{skip:n}' or \
                     '{activationProbability:p}'"))
                }
            }
        }
        _ => Err(CommandError::new(TYPE_MISMATCH, "'mode' must be a string or JSON object")),
    }
}

/// The error reply of code `n` with the labels the test named: a `refrouter` 9.0.4 adds none of
/// its own, not even `RetryableWriteError` on a retryable write (measured with pyref).
fn error_reply(n: i32, labels: Option<&Value>) -> Document {
    let name = code(n).map_or_else(|| format!("Location{n}"), |c: Code| c.name.to_owned());
    let mut d = Document::new()
        .with("ok", 0.0)
        .with("errmsg", FAILING_MESSAGE)
        .with("code", n)
        .with("codeName", name);
    if let Some(Value::Array(l)) = labels
        && !l.is_empty()
    {
        d.push("errorLabels", l.clone());
    }
    d
}

impl FailPoint {
    /// Runs `configureFailPoint` sent to database `db`: the reply with how many times the
    /// failpoint fired so far.
    pub fn configure(&mut self, db: &str, cmd: &Document) -> Result<Document, CommandError> {
        if db != "admin" {
            return Err(CommandError::new(
                UNAUTHORIZED,
                "configureFailPoint may only be run against the admin database.",
            ));
        }
        let get_more = match cmd.get(CONFIGURE_FAIL_POINT) {
            Some(Value::String(n)) if n == FAIL_COMMAND => false,
            Some(Value::String(n)) if n == FAIL_GET_MORE => true,
            Some(Value::String(n)) => {
                return Err(CommandError::new(FAIL_POINT_SET_FAILED, format!("{n} not found")));
            }
            Some(other) => {
                return Err(crate::types::wrong_type(
                    "configureFailPoint.configureFailPoint",
                    other,
                    "string",
                ));
            }
            None => return Err(bad("missing failpoint name")),
        };
        let Some(m) = cmd.get("mode") else {
            return Err(CommandError::new(
                ILLEGAL_OPERATION,
                "When setting a failpoint, you must supply a 'mode'",
            ));
        };
        let mode = mode(m)?;
        let data = match cmd.get("data") {
            None => Document::new(),
            Some(Value::Document(d)) => d.clone(),
            Some(_) => {
                return Err(CommandError::new(
                    TYPE_MISMATCH,
                    "the 'data' option must be a JSON object",
                ));
            }
        };
        if get_more {
            self.get_more.mode = mode;
            self.get_more.data = data;
            return Ok(Document::new().with("count", self.get_more.entered).with("ok", 1.0));
        }
        self.mode = mode;
        self.data = data;
        Ok(Document::new().with("count", self.entered).with("ok", 1.0))
    }

    /// The error of a `getMore` whose cursor was taken, when `failGetMoreAfterCursorCheckout`
    /// fires; `stream` tells a change stream's cursor. As a `refrouter` 9.0.4 relays it (measured
    /// on 05-11-2026): a shard's not-primary and shutdown codes become `HostUnreachable`, a
    /// change stream labels the resumable ones `ResumableChangeStreamError`,
    /// `ChangeStreamFatalError` is `NonResumableChangeStreamError` on any cursor, and the test's
    /// `errorLabels` go unused.
    pub fn get_more_error(&mut self, stream: bool) -> Option<CommandError> {
        let n = self.get_more.data.get("errorCode").and_then(number)? as i32;
        if !fires(&mut self.get_more.mode, &mut self.rng) {
            return None;
        }
        self.get_more.entered = self.get_more.entered.saturating_add(1);
        let n = if BECOMES_HOST_UNREACHABLE.contains(&n) { HOST_UNREACHABLE.code } else { n };
        let mut e = CommandError::new(code_of(n), GET_MORE_MESSAGE);
        if stream && RESUMABLE_IN_STREAM.contains(&n) {
            e = e.with_label(RESUMABLE_CHANGE_STREAM_ERROR);
        }
        if n == CHANGE_STREAM_FATAL.code {
            e = e.with_label(NON_RESUMABLE_CHANGE_STREAM_ERROR);
        }
        Some(e)
    }

    /// What the failpoint does to command `name` of a connection whose client named
    /// itself `app`; `None` lets it run untouched.
    pub fn check(&mut self, name: &str, app: Option<&str>) -> Option<Fault> {
        if name == CONFIGURE_FAIL_POINT || self.mode == Mode::Off {
            return None;
        }
        let Some(Value::Array(names)) = self.data.get("failCommands") else { return None };
        if !names.iter().any(|v| matches!(v, Value::String(n) if n == name)) {
            return None;
        }
        if let Some(Value::String(want)) = self.data.get("appName")
            && app != Some(want.as_str())
        {
            return None;
        }
        let flag = |k: &str| matches!(self.data.get(k), Some(Value::Boolean(true)));
        let block_ms =
            match (flag("blockConnection"), self.data.get("blockTimeMS").and_then(number)) {
                (true, Some(ms)) if ms > 0.0 => ms as u64,
                _ => 0,
            };
        let error = match self.data.get("errorCode").and_then(number) {
            Some(n) => Some(error_reply(n as i32, self.data.get("errorLabels"))),
            None => None,
        };
        let write_concern_error = match self.data.get("writeConcernError") {
            Some(Value::Document(d)) => Some(d.clone()),
            _ => None,
        };
        let labels = match self.data.get("errorLabels") {
            Some(l @ Value::Array(items)) if !items.is_empty() => Some(l.clone()),
            _ => None,
        };
        let fault = Fault {
            block_ms,
            blocked: false,
            close: flag("closeConnection"),
            error,
            write_concern_error,
            labels,
        };
        let acts = fault.block_ms > 0
            || fault.close
            || fault.error.is_some()
            || fault.write_concern_error.is_some();
        if !acts || !fires(&mut self.mode, &mut self.rng) {
            return None;
        }
        self.entered = self.entered.saturating_add(1);
        Some(fault)
    }
}

/// Whether a failpoint in `mode` fires now; `rng` draws the coin of a probability.
fn fires(mode: &mut Mode, rng: &mut u64) -> bool {
    match mode {
        Mode::Off => false,
        Mode::AlwaysOn => true,
        Mode::Times(n) => {
            *n -= 1;
            if *n <= 0 {
                let fired = *n == 0;
                *mode = Mode::Off;
                return fired;
            }
            true
        }
        Mode::Skip(n) if *n > 0 => {
            *n -= 1;
            false
        }
        Mode::Skip(_) => true,
        Mode::Probability(p) => {
            let p = *p;
            // xorshift64*: enough for a test's coin.
            *rng = (*rng | 1) ^ (*rng << 13);
            *rng ^= *rng >> 7;
            *rng ^= *rng << 17;
            let draw = (*rng >> 11) as f64 / (1u64 << 53) as f64;
            draw < p
        }
    }
}

/// The codes a `refrouter` 9.0.4 relays as `HostUnreachable` from a shard's `getMore`.
const BECOMES_HOST_UNREACHABLE: [i32; 7] = [91, 189, 10107, 11600, 11602, 13435, 13436];
/// The codes it labels `ResumableChangeStreamError` on a change stream's `getMore`.
const RESUMABLE_IN_STREAM: [i32; 8] = [6, 7, 89, 133, 150, 234, 262, 9001];
/// The label of an error a change stream resumes from.
const RESUMABLE_CHANGE_STREAM_ERROR: &str = "ResumableChangeStreamError";
/// The label of an error it cannot resume from.
const NON_RESUMABLE_CHANGE_STREAM_ERROR: &str = "NonResumableChangeStreamError";

/// The code of number `n`: a known one, or `Location<n>` named once for this node.
fn code_of(n: i32) -> Code {
    if let Some(c) = code(n) {
        return c;
    }
    static NAMED: std::sync::Mutex<Vec<Code>> = std::sync::Mutex::new(Vec::new());
    let mut named = NAMED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(c) = named.iter().find(|c| c.code == n) {
        return *c;
    }
    // A test names few codes, each once: the names live as long as the node.
    let name: &'static str = Box::leak(format!("Location{n}").into_boxed_str());
    let c = Code { code: n, name };
    named.push(c);
    c
}
