//! MCP revisions (019 FR-019, contract `mcp-server.md`, T024): negotiation in `initialize` as
//! the reference MCP server 3.2.2 answers it, the version of each 2026-07-28 request and
//! `server/discover`.

use teseladb_json::{Object, Value, parse, to_string};
use teseladb_mcp::jsonrpc::{HEADER_MISMATCH, INVALID_PARAMS, UNSUPPORTED_PROTOCOL_VERSION};
use teseladb_mcp::versions::{
    LATEST_SESSION_REVISION, Revision, discover_result, negotiate_initialize, request_revision,
};

const META_KEY: &str = "io.modelcontextprotocol/protocolVersion";

fn meta(version: &str) -> Object {
    let mut m = Object::new();
    m.insert(META_KEY, version);
    let mut params = Object::new();
    params.insert("_meta", Value::Object(m));
    params
}

#[test]
fn revisions_by_name_and_their_form() {
    for r in Revision::ALL {
        assert_eq!(Revision::from_name(r.name()), Some(r));
    }
    assert_eq!(Revision::from_name("2025-01-01"), None);
    assert!(Revision::R2026_07_28.stateless());
    assert!(!Revision::R2025_11_25.stateless());
    assert!(Revision::R2025_03_26.batches());
    assert!(!Revision::R2025_06_18.batches());
    assert!(!Revision::R2024_11_05.batches());
    assert_eq!(LATEST_SESSION_REVISION, Revision::R2025_11_25);
}

/// The reference MCP server 3.2.2 keeps each of its four revisions and answers 2025-11-25 to any
/// other, 2026-07-28 included.
#[test]
fn initialize_negotiates_like_the_reference_mcp_server() {
    for r in
        [Revision::R2025_11_25, Revision::R2025_06_18, Revision::R2025_03_26, Revision::R2024_11_05]
    {
        assert_eq!(negotiate_initialize(r.name()), r);
    }
    assert_eq!(negotiate_initialize("2026-07-28"), Revision::R2025_11_25);
    assert_eq!(negotiate_initialize("1999-01-01"), Revision::R2025_11_25);
}

#[test]
fn stateless_requests_carry_their_version() {
    assert_eq!(
        request_revision(Some(&meta("2026-07-28")), Some("2026-07-28")),
        Ok(Revision::R2026_07_28)
    );
    assert_eq!(
        request_revision(Some(&meta("2026-07-28")), None),
        Ok(Revision::R2026_07_28),
        "stdio"
    );
    let e = request_revision(Some(&meta("2026-07-28")), Some("2025-11-25")).expect_err("mismatch");
    assert_eq!(e.code, HEADER_MISMATCH);
    assert_eq!(
        e.message,
        "Header mismatch: MCP-Protocol-Version header value '2025-11-25' does not match body value '2026-07-28'"
    );
    let e = request_revision(Some(&meta("1900-01-01")), Some("1900-01-01")).expect_err("unknown");
    assert_eq!(
        (e.code, e.message.as_str()),
        (UNSUPPORTED_PROTOCOL_VERSION, "Unsupported protocol version")
    );
    let data = to_string(&e.data.expect("data")).expect("json");
    assert_eq!(
        data,
        r#"{"supported":["2026-07-28","2025-11-25","2025-06-18","2025-03-26","2024-11-05"],"requested":"1900-01-01"}"#
    );
    let e = request_revision(Some(&meta("2025-11-25")), None).expect_err("needs initialize");
    assert_eq!(e.code, UNSUPPORTED_PROTOCOL_VERSION, "a session revision is not stateless");
    assert_eq!(request_revision(None, None).expect_err("missing").code, INVALID_PARAMS);
    assert_eq!(
        request_revision(Some(&Object::new()), None).expect_err("missing").code,
        INVALID_PARAMS
    );
    let mut empty_meta = Object::new();
    empty_meta.insert("_meta", Value::Object(Object::new()));
    assert_eq!(
        request_revision(Some(&empty_meta), None).expect_err("missing").code,
        INVALID_PARAMS
    );
    let mut bad = Object::new();
    bad.insert("_meta", true);
    assert_eq!(request_revision(Some(&bad), None).expect_err("not an object").code, INVALID_PARAMS);
    let mut bad = Object::new();
    bad.insert(META_KEY, true);
    let mut params = Object::new();
    params.insert("_meta", Value::Object(bad));
    assert_eq!(
        request_revision(Some(&params), None).expect_err("not a string").code,
        INVALID_PARAMS
    );
}

#[test]
fn discover_lists_every_revision() {
    let mut caps = Object::new();
    caps.insert("tools", Value::Object(Object::new()));
    let r = discover_result(caps, "TeselaDB", "0.1.0");
    let expected = parse(
        br#"{"resultType":"complete","supportedVersions":["2026-07-28","2025-11-25","2025-06-18","2025-03-26","2024-11-05"],"capabilities":{"tools":{}},"_meta":{"io.modelcontextprotocol/serverInfo":{"name":"TeselaDB","version":"0.1.0"}},"ttlMs":3600000,"cacheScope":"private"}"#,
    )
    .expect("json");
    assert_eq!(Value::Object(r), expected);
}
