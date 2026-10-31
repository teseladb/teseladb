//! `configureFailPoint` with `failCommand` (FR-034, DV-025): with `enableTestCommands` a test
//! makes the node fail the commands it names, as the drivers' unified tests do against a
//! `refrouter`. The failpoint answers with an error and its labels, closes the connection, holds it
//! for `blockTimeMS` before running the command, or adds a `writeConcernError` to its reply.
//! Replies and messages are those of a `refrouter` 9.0.4 started with `enableTestCommands=1`
//! (measured). `data` is read when a command arrives, so a fault in it makes the failpoint do
//! nothing instead of failing `configureFailPoint`, as there.

use teseladb_bson::{Document, Value};

use crate::error::{
    BAD_VALUE, Code, CommandError, FAIL_POINT_SET_FAILED, ILLEGAL_OPERATION, TYPE_MISMATCH,
    UNAUTHORIZED, code,
};
use crate::types::number;

/// `configureFailPoint`.
pub const CONFIGURE_FAIL_POINT: &str = "configureFailPoint";
/// The only failpoint of a node.
pub const FAIL_COMMAND: &str = "failCommand";
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
}

/// The node's `failCommand` failpoint.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FailPoint {
    mode: Mode,
    data: Document,
    /// Times it fired since the node started (`count` of the reply).
    entered: i32,
    rng: u64,
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
        match cmd.get(CONFIGURE_FAIL_POINT) {
            Some(Value::String(n)) if n == FAIL_COMMAND => {}
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
        }
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
        self.mode = mode;
        self.data = data;
        Ok(Document::new().with("count", self.entered).with("ok", 1.0))
    }

    /// Whether the failpoint fires now, by its mode.
    fn fires(&mut self) -> bool {
        match &mut self.mode {
            Mode::Off => false,
            Mode::AlwaysOn => true,
            Mode::Times(n) => {
                *n -= 1;
                if *n <= 0 {
                    let fired = *n == 0;
                    self.mode = Mode::Off;
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
                self.rng = (self.rng | 1) ^ (self.rng << 13);
                self.rng ^= self.rng >> 7;
                self.rng ^= self.rng << 17;
                let draw = (self.rng >> 11) as f64 / (1u64 << 53) as f64;
                draw < p
            }
        }
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
        let fault = Fault {
            block_ms,
            blocked: false,
            close: flag("closeConnection"),
            error,
            write_concern_error,
        };
        let acts = fault.block_ms > 0
            || fault.close
            || fault.error.is_some()
            || fault.write_concern_error.is_some();
        if !acts || !self.fires() {
            return None;
        }
        self.entered = self.entered.saturating_add(1);
        Some(fault)
    }
}
