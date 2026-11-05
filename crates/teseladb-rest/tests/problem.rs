//! Errors as RFC 9457 problems (019 FR-013, contract `rest-api.md`, T012): the HTTP status of
//! each RefDb error and the body with its code, name, message and labels.

use teseladb_rest::problem::{PROBLEM_JSON, http_problem, problem, status, write_status};
use teseladb_wire::error::{
    AUTHENTICATION_FAILED, BAD_VALUE, CURSOR_NOT_FOUND, CommandError, DUPLICATE_KEY,
    INTERNAL_ERROR, INTERRUPTED_DUE_TO_REPL_STATE_CHANGE, MAX_TIME_MS_EXPIRED, NAMESPACE_NOT_FOUND,
    NOT_WRITABLE_PRIMARY, RETRYABLE_WRITE_ERROR, SHUTDOWN_IN_PROGRESS, TRANSIENT_TRANSACTION_ERROR,
    UNAUTHORIZED, WRITE_CONFLICT,
};
use teseladb_bson::{Document, Value};

#[test]
fn status_of_each_error() {
    let s = |e: CommandError| status(&e);
    let e = |c| CommandError::new(c, "m");
    assert_eq!(s(e(AUTHENTICATION_FAILED)), 401);
    assert_eq!(s(e(UNAUTHORIZED)), 403);
    assert_eq!(s(e(NAMESPACE_NOT_FOUND)), 404);
    assert_eq!(s(e(CURSOR_NOT_FOUND)), 404);
    assert_eq!(s(e(DUPLICATE_KEY)), 409);
    assert_eq!(s(e(WRITE_CONFLICT)), 409);
    assert_eq!(s(e(BAD_VALUE).with_label(TRANSIENT_TRANSACTION_ERROR)), 409);
    assert_eq!(s(e(MAX_TIME_MS_EXPIRED)), 504);
    assert_eq!(s(e(NOT_WRITABLE_PRIMARY)), 503);
    assert_eq!(s(e(INTERRUPTED_DUE_TO_REPL_STATE_CHANGE)), 503);
    assert_eq!(s(e(SHUTDOWN_IN_PROGRESS).with_label(RETRYABLE_WRITE_ERROR)), 503);
    assert_eq!(s(e(INTERNAL_ERROR)), 500);
    assert_eq!(s(e(BAD_VALUE)), 400);
}

#[test]
fn problem_body() {
    let err = CommandError::new(DUPLICATE_KEY, "E11000 duplicate key error")
        .with_label(TRANSIENT_TRANSACTION_ERROR);
    let err = CommandError {
        extra: Document::new().with("keyValue", Document::new().with("_id", 1)),
        ..err
    };
    let (st, body) = problem(&err);
    let expected = Document::new()
        .with("type", "urn:teseladb:error:DuplicateKey")
        .with("title", "DuplicateKey")
        .with("status", 409)
        .with("code", 11000)
        .with("codeName", "DuplicateKey")
        .with("errmsg", "E11000 duplicate key error")
        .with("errorLabels", vec![Value::from(TRANSIENT_TRANSACTION_ERROR)])
        .with("keyValue", Document::new().with("_id", 1));
    assert_eq!((st, body), (409, expected));
    let (_, plain) = problem(&CommandError::new(BAD_VALUE, "x"));
    assert!(plain.get("errorLabels").is_none());
    assert_eq!(PROBLEM_JSON, "application/problem+json");
}

#[test]
fn http_problems_without_a_refdb_code() {
    let body = http_problem(415);
    let expected = Document::new()
        .with("type", "about:blank")
        .with("title", "Unsupported Media Type")
        .with("status", 415);
    assert_eq!(body, expected);
    for (code, title) in [
        (406, "Not Acceptable"),
        (413, "Content Too Large"),
        (422, "Unprocessable Content"),
        (429, "Too Many Requests"),
        (400, "Bad Request"),
        (401, "Unauthorized"),
        (404, "Not Found"),
        (405, "Method Not Allowed"),
        (500, "Internal Server Error"),
        (503, "Service Unavailable"),
        (599, "Error"),
    ] {
        assert_eq!(http_problem(code).get("title"), Some(&Value::from(title)), "{code}");
    }
}

fn write_error(code: i32) -> Value {
    Value::Document(Document::new().with("index", 0).with("code", code).with("errmsg", "e"))
}

/// A write reply is `200` unless its only operation failed or its write concern did.
#[test]
fn status_of_a_write_reply() {
    let ok = Document::new().with("n", 1).with("ok", 1.0);
    assert_eq!(write_status(&ok, 1), 200);
    let dup =
        Document::new().with("n", 0).with("writeErrors", vec![write_error(11000)]).with("ok", 1.0);
    assert_eq!(write_status(&dup, 1), 409);
    assert_eq!(write_status(&dup, 2), 200, "partial errors of several operations");
    let unknown = Document::new().with("writeErrors", vec![write_error(123_456)]).with("ok", 1.0);
    assert_eq!(write_status(&unknown, 1), 400);
    let odd = Document::new().with("writeErrors", vec![Value::from(1)]).with("ok", 1.0);
    assert_eq!(write_status(&odd, 1), 400);
    let wce = Document::new()
        .with("n", 1)
        .with("writeConcernError", Document::new().with("code", 91).with("errmsg", "x"))
        .with("ok", 1.0);
    assert_eq!(write_status(&wce, 5), 503);
    let wce_label = Document::new()
        .with(
            "writeConcernError",
            Document::new()
                .with("code", 64)
                .with("errorLabels", vec![Value::from(RETRYABLE_WRITE_ERROR)]),
        )
        .with("ok", 1.0);
    assert_eq!(write_status(&wce_label, 1), 503);
    let no_code = Document::new()
        .with("writeErrors", vec![Value::Document(Document::new().with("code", "x"))]);
    assert_eq!(write_status(&no_code, 1), 400);
    let odd_label = Document::new().with(
        "writeConcernError",
        Document::new().with("code", 64).with("errorLabels", vec![Value::from(1)]),
    );
    assert_eq!(write_status(&odd_label, 1), 400);
    let empty = Document::new().with("writeErrors", Vec::<Value>::new()).with("ok", 1.0);
    assert_eq!(write_status(&empty, 1), 200);
    let wce_bad = Document::new().with("writeConcernError", 5).with("ok", 1.0);
    assert_eq!(write_status(&wce_bad, 1), 400);
}
