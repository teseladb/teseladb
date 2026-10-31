//! History of the clients' operations and its JSONL format, the contract `history-format.md` of
//! 002 that the Docker environment records too: one line per event (`invoke`, then `ok`, `fail` or
//! `info`), in time order.

use std::collections::BTreeMap;

use teseladb_json::{Object, Value};
use teseladb_types::Nanos;

/// Operation on a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpKind {
    /// Linearizable read.
    Read,
    /// Compare-and-set (from `expected`, `None` = absent, to `new`, `None` = deleted).
    Cas,
    /// A write whose predecessor is not known (`new`, `None` = deleted): an increment, a
    /// replacement or a deletion that got no answer; with an answer it is recorded as a cas from
    /// the value the server saw.
    Write,
}

impl OpKind {
    fn name(&self) -> &'static str {
        match self {
            OpKind::Read => "read",
            OpKind::Cas => "cas",
            OpKind::Write => "write",
        }
    }

    fn parse(s: &str) -> Option<OpKind> {
        Some(match s {
            "read" => OpKind::Read,
            "cas" => OpKind::Cas,
            "write" => OpKind::Write,
            _ => return None,
        })
    }
}

/// Result of an operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpResult {
    /// Done: in a read, the value read; in a write, it was applied.
    Ok(Option<String>),
    /// Not applied: the current value was not the expected one.
    Fail,
    /// Not applied and never evaluated (refused before: `NotLeader`, `ProposalDropped`…): it
    /// imposes nothing on the key's values. In the JSONL it is a `fail` whose `error` is not
    /// [`CAS_MISMATCH`].
    Rejected,
    /// No response (timed out): it may or may not have been applied.
    Unknown,
}

/// An operation of the history.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpRecord {
    /// Client.
    pub client: u32,
    /// Operation identifier (unique per client).
    pub op_id: u64,
    /// Kind.
    pub kind: OpKind,
    /// Raft group of the key.
    pub group: String,
    /// Key.
    pub key: String,
    /// Expected value (writes; `None` = the key does not exist).
    pub expected: Option<String>,
    /// New value (writes; `None` = delete the key).
    pub new: Option<String>,
    /// Invocation.
    pub invoke: Nanos,
    /// Response (`None` if it did not arrive).
    pub complete: Option<Nanos>,
    /// Result.
    pub result: OpResult,
    /// Node that received the request.
    pub node: u32,
    /// Why it was not applied (`fail`) or why its result is unknown (`info`).
    pub error: Option<String>,
    /// When the client gave it up without an answer (`complete` stays `None`: it may still take
    /// effect later).
    pub ended: Option<Nanos>,
    /// A write with `w: 1` (with the journal), acknowledged by its leader alone; otherwise
    /// `w: "majority"`.
    pub w1: bool,
}

/// History.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Operations in the order they were recorded.
    pub ops: Vec<OpRecord>,
}

fn opt(v: &Option<String>) -> Value {
    v.as_deref().map_or(Value::Null, Value::from)
}

fn opt_str(o: &Value, k: &str) -> Result<Option<String>, String> {
    match o.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("`{k}` is not a string")),
    }
}

impl History {
    /// Adds an operation.
    pub fn push(&mut self, op: OpRecord) {
        self.ops.push(op);
    }

    /// One JSON line per event, in time order (on a tie, by operation and with the invocation first).
    pub fn to_jsonl(&self) -> String {
        let mut events: Vec<(u64, usize, u8, String)> = Vec::new();
        for (i, op) in self.ops.iter().enumerate() {
            events.push((op.invoke.0, i, 0, event_line(op, "invoke", op.invoke)));
            let end = match (&op.result, op.complete, op.ended) {
                (OpResult::Ok(_), Some(at), _) => Some(("ok", at)),
                (OpResult::Fail | OpResult::Rejected, Some(at), _) => Some(("fail", at)),
                (_, _, Some(at)) => Some(("info", at)),
                _ => None,
            };
            if let Some((kind, at)) = end {
                events.push((at.0, i, 1, event_line(op, kind, at)));
            }
        }
        events.sort_by_key(|e| (e.0, e.1, e.2));
        events.into_iter().map(|e| e.3 + "\n").collect()
    }

    /// Reads a JSONL history (empty lines are ignored): each invocation is paired with its end by
    /// process, and each process's operations are numbered in order. The error carries the line.
    pub fn from_jsonl(text: &str) -> Result<History, String> {
        let mut h = History::default();
        let mut in_flight: BTreeMap<u32, usize> = BTreeMap::new();
        let mut started: BTreeMap<u32, u64> = BTreeMap::new();
        // When each process's last operation ended: a corrupt history's clock may go back.
        let mut last_end: BTreeMap<u32, Nanos> = BTreeMap::new();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let at = |e: String| format!("line {}: {e}", i + 1);
            let e = Event::parse(line).map_err(at)?;
            if e.kind == "invoke" {
                if in_flight.contains_key(&e.process) {
                    let p = e.process;
                    return Err(at(format!(
                        "process {p} invokes with another operation in flight"
                    )));
                }
                if last_end.get(&e.process).is_some_and(|t| e.time < *t) {
                    let p = e.process;
                    return Err(at(format!(
                        "process {p} invokes before its previous operation ended"
                    )));
                }
                let n = started.entry(e.process).or_insert(0);
                *n += 1;
                let (expected, new) = e.cas_values().map_err(at)?;
                in_flight.insert(e.process, h.ops.len());
                h.push(OpRecord {
                    client: e.process,
                    op_id: *n,
                    kind: e.f,
                    group: e.group,
                    key: e.key,
                    expected,
                    new,
                    invoke: e.time,
                    complete: None,
                    result: OpResult::Unknown,
                    node: e.node,
                    error: None,
                    ended: None,
                    w1: e.raw.get("wc").and_then(|wc| wc.get("w")).and_then(Value::as_u64)
                        == Some(1),
                });
                continue;
            }
            let Some(idx) = in_flight.remove(&e.process) else {
                let p = e.process;
                return Err(at(format!("process {p} ends an operation it did not invoke")));
            };
            let op = &mut h.ops[idx];
            if op.kind != e.f || op.group != e.group || op.key != e.key {
                return Err(at(format!("process {} ends another operation", e.process)));
            }
            if e.time < op.invoke {
                let p = e.process;
                return Err(at(format!("process {p} ends its operation before invoking it")));
            }
            last_end.insert(e.process, e.time);
            op.error = e.error.clone();
            match e.kind.as_str() {
                // Acknowledged with `w: 1`: its leader may lose it before the majority has it
                // (D-034), so its outcome is not known, as the simulator records it.
                "ok" if op.w1 => {
                    op.result = OpResult::Unknown;
                    op.error = Some(W1_ACKNOWLEDGED.into());
                    op.ended = Some(e.time);
                }
                "ok" => {
                    op.complete = Some(e.time);
                    op.result = OpResult::Ok(match op.kind {
                        OpKind::Read => opt_str(&e.raw, "value").map_err(at)?,
                        OpKind::Cas | OpKind::Write => None,
                    });
                }
                "fail" => {
                    op.complete = Some(e.time);
                    op.result = match e.error.as_deref() {
                        Some(CAS_MISMATCH) => OpResult::Fail,
                        _ => OpResult::Rejected,
                    };
                }
                _ => op.ended = Some(e.time),
            }
        }
        Ok(h)
    }
}

const CAS_WC: &str = r#"{"w":"majority","j":true}"#;
const W1_WC: &str = r#"{"w":1,"j":true}"#;

/// `error` of a cas that was evaluated and found another value.
pub const CAS_MISMATCH: &str = "CasMismatch";

/// `error` of a write acknowledged with `w: 1`, whose outcome is not known (D-034).
pub const W1_ACKNOWLEDGED: &str = "W1Acknowledged";

/// The line of an event of `op`.
fn event_line(op: &OpRecord, kind: &str, at: Nanos) -> String {
    let mut o = Object::new();
    o.insert("process", op.client);
    o.insert("type", kind);
    o.insert("f", op.kind.name());
    o.insert("key", format!("{}/{}", op.group, op.key));
    match (&op.kind, kind, &op.result) {
        (OpKind::Cas | OpKind::Write, "invoke" | "ok" | "info", _) => {
            o.insert("value", Value::Array(vec![opt(&op.expected), opt(&op.new)]));
        }
        (OpKind::Read, "ok", OpResult::Ok(v)) => {
            o.insert("value", opt(v));
        }
        _ => {}
    }
    o.insert("time", at.0);
    o.insert("node", op.node);
    if matches!(op.kind, OpKind::Cas | OpKind::Write) {
        let wc = if op.w1 { W1_WC } else { CAS_WC };
        o.insert("wc", teseladb_json::parse(wc.as_bytes()).expect("a constant"));
    }
    // A failed cas was evaluated: its error is `CasMismatch` even when none was noted, so that it
    // is not read back as a refusal.
    let error = match (kind, &op.result) {
        ("fail", OpResult::Fail) => Some(op.error.as_deref().unwrap_or(CAS_MISMATCH)),
        ("fail" | "info", _) => op.error.as_deref(),
        _ => None,
    };
    if let Some(e) = error {
        o.insert("error", e);
    }
    teseladb_json::to_string(&Value::Object(o)).expect("finite values")
}

/// An event line, read.
struct Event {
    process: u32,
    kind: String,
    f: OpKind,
    group: String,
    key: String,
    time: Nanos,
    node: u32,
    error: Option<String>,
    raw: Value,
}

impl Event {
    fn parse(line: &str) -> Result<Event, String> {
        let v = teseladb_json::parse(line.as_bytes()).map_err(|e| e.to_string())?;
        let num =
            |k: &str| v.get(k).and_then(Value::as_u64).ok_or_else(|| format!("missing `{k}`"));
        let text = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("missing `{k}`"))
        };
        let small = |k: &str| u32::try_from(num(k)?).map_err(|_| format!("`{k}` out of range"));
        let process = small("process")?;
        let kind = text("type")?;
        if !["invoke", "ok", "fail", "info"].contains(&kind.as_str()) {
            return Err("unknown `type`".into());
        }
        let f = OpKind::parse(&text("f")?).ok_or("unknown `f`")?;
        let full = text("key")?;
        let (group, key) = full.split_once('/').ok_or("`key` is not <group>/<key>")?;
        let (group, key) = (group.to_owned(), key.to_owned());
        let time = Nanos(num("time")?);
        let node = small("node")?;
        let error = opt_str(&v, "error")?;
        Ok(Event { process, kind, f, group, key, time, node, error, raw: v })
    }

    /// `[expected, new]` of a cas invocation (`(None, None)` for a read).
    fn cas_values(&self) -> Result<(Option<String>, Option<String>), String> {
        if !matches!(self.f, OpKind::Cas | OpKind::Write) {
            return Ok((None, None));
        }
        let bad = || "`value` of a cas is [expected, new]".to_owned();
        let pair = self.raw.get("value").and_then(Value::as_array).ok_or_else(bad)?;
        let one = |v: &Value| match v {
            Value::Null => Ok(None),
            Value::String(s) => Ok(Some(s.clone())),
            _ => Err(bad()),
        };
        match pair {
            [e, n] => Ok((one(e)?, one(n)?)),
            _ => Err(bad()),
        }
    }
}
