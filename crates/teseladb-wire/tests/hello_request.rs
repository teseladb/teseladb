//! The fields of a `hello` as `HelloRequest::parse` reads them (001 FR-004): what the reply
//! needs from the request, and a `topologyVersion` without its process.

use teseladb_bson::{Document, Value};
use teseladb_wire::connection::HelloState;
use teseladb_wire::hello::HelloRequest;

#[test]
fn the_request_keeps_hello_ok_and_the_user_of_sasl_supported_mechs() {
    let state = HelloState::default();
    let cmd = Document::new()
        .with("hello", 1)
        .with("helloOk", true)
        .with("saslSupportedMechs", "admin.ana");
    let req = HelloRequest::parse("hello", &cmd, &state).unwrap();
    assert!(req.hello_ok() && !req.legacy());
    let user = req.sasl_user().expect("the user named");
    assert_eq!((user.db.as_str(), user.user.as_str()), ("admin", "ana"));
    let plain = HelloRequest::parse("isMaster", &Document::new().with("isMaster", 1), &state);
    let plain = plain.unwrap();
    assert!(!plain.hello_ok() && plain.legacy() && plain.sasl_user().is_none());
}

#[test]
fn a_topology_version_without_its_process_is_refused() {
    let state = HelloState::default();
    let tv = Document::new().with("counter", Value::Int64(0));
    let cmd =
        Document::new().with("hello", 1).with("topologyVersion", tv).with("maxAwaitTimeMS", 5);
    let e = HelloRequest::parse("hello", &cmd, &state).unwrap_err();
    assert_eq!(e.code.code, 40414, "{e:?}");
    assert!(e.errmsg.contains("hello.topologyVersion.processId"), "{}", e.errmsg);
}
