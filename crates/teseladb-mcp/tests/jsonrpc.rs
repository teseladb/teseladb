//! JSON-RPC 2.0 as MCP uses it (019 FR-019, T024): requests, notifications, responses, the
//! batches of revision 2025-03-26 and the error replies.

use teseladb_mcp::jsonrpc::{
    ErrorObject, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, Id, Incoming, METHOD_NOT_FOUND,
    Message, PARSE_ERROR, error_response, read, response,
};
use teseladb_json::{Number, Object, Value, parse};

fn single(text: &str) -> Message {
    match read(text.as_bytes(), false) {
        Ok(Incoming::Single(m)) => m,
        other => panic!("{other:?}"),
    }
}

fn refused(text: &str, batches: bool) -> (Option<Id>, ErrorObject) {
    match read(text.as_bytes(), batches) {
        Err(e) => e,
        Ok(other) => panic!("{other:?}"),
    }
}

fn object(text: &str) -> Object {
    match parse(text.as_bytes()) {
        Ok(Value::Object(o)) => o,
        other => panic!("{other:?}"),
    }
}

#[test]
fn requests_notifications_and_responses() {
    let m = single(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{"cursor":"x"}}"#);
    assert_eq!(
        m,
        Message::Request {
            id: Id::Number(1),
            method: "tools/list".into(),
            params: Some(object(r#"{"cursor":"x"}"#)),
        }
    );
    let m = single(r#"{"jsonrpc":"2.0","id":"a","method":"ping"}"#);
    assert_eq!(
        m,
        Message::Request { id: Id::String("a".into()), method: "ping".into(), params: None }
    );
    let m = single(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    assert_eq!(
        m,
        Message::Notification { method: "notifications/initialized".into(), params: None }
    );
    let m = single(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#);
    assert_eq!(m, Message::Response { id: Id::Number(7), result: Value::Object(Object::new()) });
    let m = single(r#"{"jsonrpc":"2.0","id":7,"error":{"code":-1,"message":"no","data":[1]}}"#);
    let data = Value::Array(vec![Value::Number(Number::Int(1))]);
    assert_eq!(
        m,
        Message::Error {
            id: Some(Id::Number(7)),
            error: ErrorObject { code: -1, message: "no".into(), data: Some(data) },
        }
    );
    let m = single(r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"bad"}}"#);
    assert!(matches!(m, Message::Error { id: None, .. }));
}

#[test]
fn malformed_messages_get_the_json_rpc_errors() {
    assert_eq!(refused("{", false).1.code, PARSE_ERROR);
    assert_eq!(refused("{", false).0, None);
    let invalid = |t: &str| refused(t, false);
    assert_eq!(invalid("[1]").1.code, INVALID_REQUEST, "batches only in 2025-03-26");
    assert_eq!(invalid("3").1.code, INVALID_REQUEST);
    assert_eq!(invalid(r#"{"id":1,"method":"m"}"#).1.code, INVALID_REQUEST);
    let code = |(id, e): (Option<Id>, ErrorObject)| (id, e.code);
    assert_eq!(
        code(invalid(r#"{"jsonrpc":"1.0","id":1,"method":"m"}"#)),
        (Some(Id::Number(1)), INVALID_REQUEST)
    );
    assert_eq!(
        code(invalid(r#"{"jsonrpc":"2.0","id":1,"method":5}"#)),
        (Some(Id::Number(1)), INVALID_REQUEST)
    );
    assert_eq!(
        code(invalid(r#"{"jsonrpc":"2.0","id":null,"method":"m"}"#)),
        (None, INVALID_REQUEST)
    );
    assert_eq!(invalid(r#"{"jsonrpc":"2.0","id":1.5,"method":"m"}"#).0, None);
    assert_eq!(invalid(r#"{"jsonrpc":"2.0","id":true,"method":"m"}"#).1.code, INVALID_REQUEST);
    assert_eq!(invalid(r#"{"jsonrpc":"2.0","id":1}"#).1.code, INVALID_REQUEST);
    assert_eq!(
        invalid(r#"{"jsonrpc":"2.0","id":1,"result":1,"error":{}}"#).1.code,
        INVALID_REQUEST
    );
    assert_eq!(invalid(r#"{"jsonrpc":"2.0","result":1}"#).1.code, INVALID_REQUEST);
    assert_eq!(invalid(r#"{"jsonrpc":"2.0","id":1,"error":5}"#).1.code, INVALID_REQUEST);
    assert_eq!(
        invalid(r#"{"jsonrpc":"2.0","id":1,"error":{"code":"x","message":"m"}}"#).1.code,
        INVALID_REQUEST
    );
    assert_eq!(invalid(r#"{"jsonrpc":"2.0","id":1,"error":{"code":1}}"#).1.code, INVALID_REQUEST);
    let (id, e) = invalid(r#"{"jsonrpc":"2.0","id":4,"method":"m","params":[1]}"#);
    assert_eq!((id, e.code), (Some(Id::Number(4)), INVALID_PARAMS));
    let (id, e) = invalid(r#"{"jsonrpc":"2.0","method":"m","params":3}"#);
    assert_eq!((id, e.code), (None, INVALID_PARAMS));
}

#[test]
fn batches_of_revision_2025_03_26() {
    let text = r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0"},{"jsonrpc":"2.0","method":"n"}]"#;
    let Ok(Incoming::Batch(items)) = read(text.as_bytes(), true) else { panic!("batch") };
    assert_eq!(items.len(), 3);
    assert!(matches!(&items[0], Ok(Message::Request { .. })));
    assert!(matches!(&items[1], Err((None, e)) if e.code == INVALID_REQUEST));
    assert!(matches!(&items[2], Ok(Message::Notification { .. })));
    assert_eq!(refused("[]", true).1.code, INVALID_REQUEST, "an empty batch is one error");
}

#[test]
fn replies_are_json_rpc_objects() {
    let mut result = Object::new();
    result.insert("ok", true);
    let r = response(&Id::Number(3), Value::Object(result));
    assert_eq!(r, object(r#"{"jsonrpc":"2.0","id":3,"result":{"ok":true}}"#));
    let e = ErrorObject::new(METHOD_NOT_FOUND, "Method not found");
    assert_eq!(
        error_response(Some(&Id::String("x".into())), &e),
        object(
            r#"{"jsonrpc":"2.0","id":"x","error":{"code":-32601,"message":"Method not found"}}"#
        )
    );
    let e = ErrorObject::new(INTERNAL_ERROR, "boom").with_data(Value::Bool(true));
    assert_eq!(
        error_response(None, &e),
        object(
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"boom","data":true}}"#
        )
    );
}
