//! JSON-RPC 2.0 (https://www.jsonrpc.org/specification) as MCP uses it: identifiers are strings
//! or integers and never `null` in a request, parameters are an object, and batches only exist in
//! revision 2025-03-26. Error codes: JSON-RPC's and those MCP reserves (`-32020` to `-32022`,
//! revision 2026-07-28, `basic/index`).

use teseladb_json::{Number, Object, Value, parse};

/// The body is not JSON.
pub const PARSE_ERROR: i64 = -32700;
/// The JSON is not a valid JSON-RPC message.
pub const INVALID_REQUEST: i64 = -32600;
/// Unknown method.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// Parameters that do not fit the method.
pub const INVALID_PARAMS: i64 = -32602;
/// Internal failure.
pub const INTERNAL_ERROR: i64 = -32603;
/// An HTTP header is absent or differs from the body (`HeaderMismatch`).
pub const HEADER_MISMATCH: i64 = -32020;
/// The request needs a client capability it did not declare.
pub const MISSING_REQUIRED_CLIENT_CAPABILITY: i64 = -32021;
/// The request's protocol revision is not supported.
pub const UNSUPPORTED_PROTOCOL_VERSION: i64 = -32022;

/// The version every message carries in `jsonrpc`.
const VERSION: &str = "2.0";

/// A request identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Id {
    /// An integer.
    Number(i64),
    /// A string.
    String(String),
}

impl Id {
    fn to_value(&self) -> Value {
        match self {
            Id::Number(n) => Value::Number(Number::Int(*n)),
            Id::String(s) => Value::String(s.clone()),
        }
    }
}

/// The `error` member of an error reply.
#[derive(Clone, Debug, PartialEq)]
pub struct ErrorObject {
    /// `code`.
    pub code: i64,
    /// `message`.
    pub message: String,
    /// `data`.
    pub data: Option<Value>,
}

impl ErrorObject {
    /// An error without data.
    pub fn new(code: i64, message: impl Into<String>) -> ErrorObject {
        ErrorObject { code, message: message.into(), data: None }
    }

    /// The same error with `data`.
    pub fn with_data(self, data: Value) -> ErrorObject {
        ErrorObject { data: Some(data), ..self }
    }

    fn to_value(&self) -> Value {
        let mut o = Object::new();
        o.insert("code", Value::Number(Number::Int(self.code)));
        o.insert("message", self.message.as_str());
        if let Some(d) = &self.data {
            o.insert("data", d.clone());
        }
        Value::Object(o)
    }
}

/// A message received.
#[derive(Clone, Debug, PartialEq)]
pub enum Message {
    /// A request, answered with a response or an error.
    Request {
        /// `id`.
        id: Id,
        /// `method`.
        method: String,
        /// `params`.
        params: Option<Object>,
    },
    /// A notification: no reply.
    Notification {
        /// `method`.
        method: String,
        /// `params`.
        params: Option<Object>,
    },
    /// A successful reply to a request of the server.
    Response {
        /// `id`.
        id: Id,
        /// `result`.
        result: Value,
    },
    /// An error reply.
    Error {
        /// `id`, absent when the request could not be read.
        id: Option<Id>,
        /// `error`.
        error: ErrorObject,
    },
}

/// What a body carried.
#[derive(Clone, Debug, PartialEq)]
pub enum Incoming {
    /// One message.
    Single(Message),
    /// A batch: each element read on its own, with the error to answer for the invalid ones.
    Batch(Vec<Result<Message, (Option<Id>, ErrorObject)>>),
}

fn invalid_request(id: Option<Id>, why: &str) -> (Option<Id>, ErrorObject) {
    (id, ErrorObject::new(INVALID_REQUEST, format!("Invalid Request: {why}")))
}

/// `id` of a request: a string or an integer.
fn read_id(v: &Value) -> Option<Id> {
    match v {
        Value::String(s) => Some(Id::String(s.clone())),
        Value::Number(n) => n.as_i64().map(Id::Number),
        _ => None,
    }
}

fn read_error(v: &Value) -> Option<ErrorObject> {
    let o = v.as_object()?;
    let code = o.get("code")?.as_i64()?;
    let message = o.get("message")?.as_str()?.to_owned();
    Some(ErrorObject { code, message, data: o.get("data").cloned() })
}

/// One message from its JSON value.
fn message(v: &Value) -> Result<Message, (Option<Id>, ErrorObject)> {
    let Some(o) = v.as_object() else { return Err(invalid_request(None, "not an object")) };
    let id = match o.get("id") {
        None | Some(Value::Null) => None,
        Some(x) => Some(read_id(x).ok_or_else(|| invalid_request(None, "bad id"))?),
    };
    if o.get("jsonrpc").and_then(Value::as_str) != Some(VERSION) {
        return Err(invalid_request(id, "jsonrpc must be \"2.0\""));
    }
    match (o.get("method"), o.get("result"), o.get("error")) {
        (Some(method), None, None) => {
            let method =
                method.as_str().ok_or_else(|| invalid_request(id.clone(), "method"))?.to_owned();
            let params = match o.get("params") {
                None => None,
                Some(Value::Object(p)) => Some(p.clone()),
                Some(_) => {
                    let msg = "Invalid params: params must be an object";
                    return Err((id, ErrorObject::new(INVALID_PARAMS, msg)));
                }
            };
            match (id, o.get("id")) {
                (Some(id), _) => Ok(Message::Request { id, method, params }),
                (None, Some(_)) => Err(invalid_request(None, "a request id cannot be null")),
                (None, None) => Ok(Message::Notification { method, params }),
            }
        }
        (None, Some(result), None) => match id {
            Some(id) => Ok(Message::Response { id, result: result.clone() }),
            None => Err(invalid_request(None, "a response needs an id")),
        },
        (None, None, Some(error)) => match read_error(error) {
            Some(error) => Ok(Message::Error { id, error }),
            None => Err(invalid_request(id, "error")),
        },
        _ => Err(invalid_request(id, "not a request, notification or response")),
    }
}

/// Reads a body. `batches` admits an array of messages (revision 2025-03-26). The error is the
/// reply to send, with its `id` when it could be read.
pub fn read(body: &[u8], batches: bool) -> Result<Incoming, (Option<Id>, ErrorObject)> {
    let v = parse(body).map_err(|e| {
        (
            None,
            ErrorObject::new(
                PARSE_ERROR,
                format!("Parse error at line {}, column {}", e.line, e.column),
            ),
        )
    })?;
    match v {
        Value::Array(items) if batches && !items.is_empty() => {
            Ok(Incoming::Batch(items.iter().map(message).collect()))
        }
        Value::Array(_) if batches => Err(invalid_request(None, "empty batch")),
        Value::Array(_) => Err(invalid_request(None, "batches are not supported")),
        other => message(&other).map(Incoming::Single),
    }
}

fn envelope(id: Option<&Id>) -> Object {
    let mut o = Object::new();
    o.insert("jsonrpc", VERSION);
    o.insert("id", id.map_or(Value::Null, Id::to_value));
    o
}

/// The successful reply to request `id`.
pub fn response(id: &Id, result: Value) -> Object {
    let mut o = envelope(Some(id));
    o.insert("result", result);
    o
}

/// The error reply to request `id` (`null` when it could not be read).
pub fn error_response(id: Option<&Id>, error: &ErrorObject) -> Object {
    let mut o = envelope(id);
    o.insert("error", error.to_value());
    o
}
