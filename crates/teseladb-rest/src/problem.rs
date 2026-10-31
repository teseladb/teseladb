//! Errors as RFC 9457 problems (FR-013, contract `rest-api.md`): the HTTP status of each RefDb
//! error and the `application/problem+json` body with its code, name, message, labels and the
//! error's other fields. Errors of HTTP itself, without a RefDb code, are `about:blank`
//! problems titled with the status phrase of RFC 9110.

use teseladb_bson::{Document, Value};
use teseladb_wire::error::{
    AUTHENTICATION_FAILED, CURSOR_NOT_FOUND, Code, CommandError, DUPLICATE_KEY, INTERNAL_ERROR,
    INTERRUPTED_AT_SHUTDOWN, INTERRUPTED_DUE_TO_REPL_STATE_CHANGE, MAX_TIME_MS_EXPIRED,
    NAMESPACE_NOT_FOUND, NETWORK_TIMEOUT, NOT_WRITABLE_PRIMARY, PRIMARY_STEPPED_DOWN,
    RETRYABLE_WRITE_ERROR, SHUTDOWN_IN_PROGRESS, TRANSIENT_TRANSACTION_ERROR, UNAUTHORIZED,
    WRITE_CONFLICT,
};

/// Media type of the error bodies.
pub const PROBLEM_JSON: &str = "application/problem+json";
/// Prefix of `type` for RefDb errors.
const TYPE_PREFIX: &str = "urn:teseladb:error:";
/// `type` of the errors of HTTP itself (RFC 9457, section 4.2.1).
const ABOUT_BLANK: &str = "about:blank";

/// Codes RefDb's retryable writes retry, which answer `503` with or without their label.
const RETRYABLE: [Code; 6] = [
    NOT_WRITABLE_PRIMARY,
    INTERRUPTED_DUE_TO_REPL_STATE_CHANGE,
    PRIMARY_STEPPED_DOWN,
    SHUTDOWN_IN_PROGRESS,
    INTERRUPTED_AT_SHUTDOWN,
    NETWORK_TIMEOUT,
];

/// The status of an error with `code` and `labels`.
fn status_of(code: i32, labels: &[&str]) -> u16 {
    let is = |c: Code| c.code == code;
    if is(AUTHENTICATION_FAILED) {
        401
    } else if is(UNAUTHORIZED) {
        403
    } else if is(NAMESPACE_NOT_FOUND) || is(CURSOR_NOT_FOUND) {
        404
    } else if is(DUPLICATE_KEY)
        || is(WRITE_CONFLICT)
        || labels.contains(&TRANSIENT_TRANSACTION_ERROR)
    {
        409
    } else if is(MAX_TIME_MS_EXPIRED) {
        504
    } else if RETRYABLE.iter().any(|c| c.code == code) || labels.contains(&RETRYABLE_WRITE_ERROR) {
        503
    } else if is(INTERNAL_ERROR) {
        500
    } else {
        400
    }
}

/// The HTTP status of a command error.
pub fn status(err: &CommandError) -> u16 {
    status_of(err.code.code, &err.labels)
}

/// The status and body of a command error.
pub fn problem(err: &CommandError) -> (u16, Document) {
    let st = status(err);
    let mut d = Document::new()
        .with("type", format!("{TYPE_PREFIX}{}", err.code.name))
        .with("title", err.code.name)
        .with("status", i32::from(st))
        .with("code", err.code.code)
        .with("codeName", err.code.name)
        .with("errmsg", err.errmsg.as_str());
    if !err.labels.is_empty() {
        d.push("errorLabels", err.labels.iter().map(|&l| Value::from(l)).collect::<Vec<_>>());
    }
    for (k, v) in err.extra.iter() {
        d.push(k, v.clone());
    }
    (st, d)
}

/// The phrase of an HTTP status (RFC 9110, section 15).
fn phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        406 => "Not Acceptable",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        422 => "Unprocessable Content",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// The body of an error of HTTP itself (`406`, `413`, `415`, `422`, `429`…).
pub fn http_problem(status: u16) -> Document {
    Document::new()
        .with("type", ABOUT_BLANK)
        .with("title", phrase(status))
        .with("status", i32::from(status))
}

/// `code` and `errorLabels` of an error document inside a reply.
fn code_and_labels(v: &Value) -> Option<(i32, Vec<&str>)> {
    let Value::Document(d) = v else { return None };
    let code = match d.get("code") {
        Some(Value::Int32(c)) => *c,
        _ => return None,
    };
    let labels = match d.get("errorLabels") {
        Some(Value::Array(ls)) => ls
            .iter()
            .filter_map(|l| match l {
                Value::String(s) => Some(s.as_str()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    Some((code, labels))
}

/// The status of a write command's reply for a request of `operations` operations: the status
/// of its write error when its only operation failed, of its write concern error when there is
/// one, and `200` otherwise (partial errors of several operations answer `200`, as the command).
pub fn write_status(reply: &Document, operations: usize) -> u16 {
    if operations == 1
        && let Some(Value::Array(errors)) = reply.get("writeErrors")
        && let Some(first) = errors.first()
    {
        return code_and_labels(first).map_or(400, |(c, l)| status_of(c, &l));
    }
    match reply.get("writeConcernError") {
        Some(wce) => code_and_labels(wce).map_or(400, |(c, l)| status_of(c, &l)),
        None => 200,
    }
}
