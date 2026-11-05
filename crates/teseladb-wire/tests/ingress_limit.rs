//! The 9.0 limiter of new connections (006 FR-036), measured on a `refnode` 9.0.4 on 05-11-2026:
//! its five parameters, their coercions and errors, and the bucket as `serverStatus` counts it.

mod common;

use std::net::IpAddr;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_wire::commands::lock;
use teseladb_wire::ingress::{
    Admission, BURST_CAPACITY_SECS, BYPASS, ENABLED, MAX_QUEUE_DEPTH, RATE_PER_SEC,
};

fn cmd(text: &str) -> Document {
    extjson::parse(text).expect("a command")
}

fn set(m: &mut Mem, name: &str, value: &str) -> Document {
    m.run("admin", cmd(&format!(r#"{{"setParameter":1,"{name}":{value}}}"#)))
}

fn get(m: &mut Mem, name: &str) -> Value {
    let r = m.run("admin", cmd(&format!(r#"{{"getParameter":1,"{name}":1}}"#)));
    field(&r, name).clone()
}

fn ok(r: &Document) {
    assert_eq!(field(r, "ok"), &Value::Double(1.0), "{r:?}");
}

fn ip(text: &str) -> Option<IpAddr> {
    text.parse().ok()
}

#[test]
fn the_parameters_start_with_a_refnode_s_defaults_and_types() {
    let mut m = Mem::new();
    assert_eq!(get(&mut m, ENABLED), Value::Boolean(false));
    assert_eq!(get(&mut m, RATE_PER_SEC), Value::Int32(i32::MAX));
    assert_eq!(get(&mut m, BURST_CAPACITY_SECS), Value::Double(f64::MAX));
    assert_eq!(get(&mut m, MAX_QUEUE_DEPTH), Value::Int32(0));
    let ranges = cmd(r#"{"ranges":[]}"#);
    assert_eq!(get(&mut m, BYPASS), Value::Document(ranges));
    let details = format!(r#"{{"getParameter":{{"showDetails":true}},"{BYPASS}":1}}"#);
    let r = m.run("admin", cmd(&details));
    assert_eq!(field(&r, &format!("{BYPASS}.settableAtRuntime")), &Value::Boolean(true));
    assert_eq!(field(&r, &format!("{BYPASS}.settableAtStartup")), &Value::Boolean(true));
}

/// What `setParameter` answers to a value of the wrong type or out of bounds.
fn refusals() -> Vec<(&'static str, &'static str, i32, String)> {
    let invalid = |name: &str, what: &str| format!("Invalid value for parameter {name}: {what}");
    let failed = |name: &str, why: &str| format!("Failed validating {name}: {why}");
    let field_error = |what: &str| format!("BSON field 'CIDRRangeListParameters.{what}");
    vec![
        (RATE_PER_SEC, "0", 2, invalid(RATE_PER_SEC, "0 is not greater than or equal to 1")),
        (
            RATE_PER_SEC,
            r#""x""#,
            2,
            failed(RATE_PER_SEC, "Unable to coerce value to integral type"),
        ),
        (
            RATE_PER_SEC,
            r#"{"$numberLong":"1099511627776"}"#,
            2,
            failed(RATE_PER_SEC, "Out of bounds coercing to integral value"),
        ),
        (
            RATE_PER_SEC,
            r#"{"$numberDouble":"Infinity"}"#,
            2,
            failed(RATE_PER_SEC, "Unable to coerce NaN/Inf to integral type"),
        ),
        (
            MAX_QUEUE_DEPTH,
            "-1",
            2,
            invalid(MAX_QUEUE_DEPTH, "-1 is not greater than or equal to 0"),
        ),
        (BURST_CAPACITY_SECS, "0", 2, invalid(BURST_CAPACITY_SECS, "0 is not greater than 0")),
        (BURST_CAPACITY_SECS, "-1", 2, invalid(BURST_CAPACITY_SECS, "-1 is not greater than 0")),
        (
            BURST_CAPACITY_SECS,
            r#"{"$numberDouble":"NaN"}"#,
            2,
            invalid(BURST_CAPACITY_SECS, "nan is not greater than 0"),
        ),
        (
            BURST_CAPACITY_SECS,
            "true",
            2,
            failed(BURST_CAPACITY_SECS, "Unable to coerce value to correct type"),
        ),
        (BYPASS, "5", 10065, format!("invalid parameter: expected an object ({BYPASS})")),
        (BYPASS, "{}", 40414, field_error("ranges' is missing but a required field")),
        (
            BYPASS,
            r#"{"ranges":"10.0.0.0/8"}"#,
            14,
            field_error("ranges' is the wrong type 'string', expected type 'array'"),
        ),
        (
            BYPASS,
            r#"{"ranges":[5]}"#,
            14,
            field_error("ranges.0' is the wrong type 'int', expected type 'string'"),
        ),
        (BYPASS, r#"{"ranges":[],"x":1}"#, 40415, field_error("x' is an unknown field.")),
    ]
}

#[test]
fn a_value_out_of_its_type_or_bounds_is_refused_with_a_refnode_s_error() {
    let mut m = Mem::new();
    for (name, value, code, msg) in refusals() {
        let r = set(&mut m, name, value);
        assert_eq!(field(&r, "code"), &Value::Int32(code), "{name} {value}: {r:?}");
        assert_eq!(field(&r, "errmsg"), &Value::from(msg.as_str()), "{name} {value}");
    }
    // Nothing changed.
    assert_eq!(get(&mut m, RATE_PER_SEC), Value::Int32(i32::MAX));
}

#[test]
fn values_are_coerced_as_a_refnode_coerces_them() {
    let mut m = Mem::new();
    ok(&set(&mut m, RATE_PER_SEC, "2.5"));
    assert_eq!(get(&mut m, RATE_PER_SEC), Value::Int32(2));
    let r = set(&mut m, RATE_PER_SEC, "true");
    assert_eq!(field(&r, "was"), &Value::Int32(2));
    assert_eq!(get(&mut m, RATE_PER_SEC), Value::Int32(1));
    ok(&set(&mut m, MAX_QUEUE_DEPTH, "2.7"));
    assert_eq!(get(&mut m, MAX_QUEUE_DEPTH), Value::Int32(2));
    ok(&set(&mut m, BURST_CAPACITY_SECS, r#"{"$numberLong":"3"}"#));
    assert_eq!(get(&mut m, BURST_CAPACITY_SECS), Value::Double(3.0));
    let r = set(&mut m, BURST_CAPACITY_SECS, r#"{"$numberDecimal":"2.5"}"#);
    assert_eq!(field(&r, "was"), &Value::Double(3.0));
    ok(&set(&mut m, BURST_CAPACITY_SECS, r#"{"$numberDouble":"Infinity"}"#));
    assert_eq!(get(&mut m, BURST_CAPACITY_SECS), Value::Double(f64::INFINITY));
    // Any value is a boolean by its truth.
    for (value, on) in [(r#"{"a":1}"#, true), (r#""no""#, true), ("0", false), ("null", false)] {
        ok(&set(&mut m, ENABLED, value));
        assert_eq!(get(&mut m, ENABLED), Value::Boolean(on), "{value}");
    }
    // The ranges are kept as given, unchecked; `was` is the document.
    ok(&set(&mut m, BYPASS, r#"{"ranges":["nope","10.0.0.0/8"]}"#));
    let r = set(&mut m, BYPASS, r#"{"ranges":[]}"#);
    let before = cmd(r#"{"ranges":["nope","10.0.0.0/8"]}"#);
    assert_eq!(field(&r, "was"), &Value::Document(before));
}

/// Rate 2 and two seconds of burst: four tokens, then three places in the queue, each half a
/// second after the last, then closes (measured with ten connections at once).
#[test]
fn the_bucket_lets_its_capacity_through_queues_the_next_and_closes_the_rest() {
    let mut m = Mem::new();
    for (name, value) in [(RATE_PER_SEC, "2"), (BURST_CAPACITY_SECS, "2"), (MAX_QUEUE_DEPTH, "3")] {
        ok(&set(&mut m, name, value));
    }
    ok(&set(&mut m, ENABLED, "true"));
    let peer = ip("172.17.0.1");
    let got: Vec<Admission> = {
        let mut g = lock(&m.shared);
        (0..10).map(|_| g.ingress.admit(peer, 1_000_000)).collect()
    };
    let wait = |ms: u64| Admission::Wait { micros: ms * 1000 };
    let expected = [
        Admission::Go,
        Admission::Go,
        Admission::Go,
        Admission::Go,
        wait(500),
        wait(1000),
        wait(1500),
        Admission::Reject,
        Admission::Reject,
        Admission::Reject,
    ];
    assert_eq!(got, expected);
    let r = m.run("admin", doc(&[("serverStatus", 1.into())]));
    assert_eq!(field(&r, "connections.rejected"), &Value::Int32(3));
    assert_eq!(field(&r, "connections.queuedForEstablishment"), &Value::Int32(3));
    assert_eq!(field(&r, "connections.establishmentRateLimit.rejected"), &Value::Int32(3));
    {
        let mut g = lock(&m.shared);
        g.ingress.dequeue();
        g.ingress.abandon();
    }
    let r = m.run("admin", doc(&[("serverStatus", 1.into())]));
    assert_eq!(field(&r, "connections.queuedForEstablishment"), &Value::Int32(1));
    let left = "connections.establishmentRateLimit.interruptedDueToClientDisconnect";
    assert_eq!(field(&r, left), &Value::Int32(1));
}

#[test]
fn the_addresses_of_the_bypass_list_take_no_token() {
    let mut m = Mem::new();
    for (name, value) in [(RATE_PER_SEC, "1"), (BURST_CAPACITY_SECS, "1"), (ENABLED, "true")] {
        ok(&set(&mut m, name, value));
    }
    ok(&set(&mut m, BYPASS, r#"{"ranges":["nope","172.17.0.0/16","::1"]}"#));
    let mut g = lock(&m.shared);
    for peer in ["172.17.0.1", "172.17.255.9", "::1", "172.17.0.2"] {
        assert_eq!(g.ingress.admit(ip(peer), 0), Admission::Go, "{peer}");
    }
    assert_eq!(g.ingress.admit(ip("10.0.0.1"), 0), Admission::Go);
    assert_eq!(g.ingress.admit(ip("10.0.0.1"), 0), Admission::Reject);
    assert_eq!(g.ingress.admit(None, 0), Admission::Reject);
    drop(g);
    let r = m.run("admin", doc(&[("serverStatus", 1.into())]));
    assert_eq!(field(&r, "connections.establishmentRateLimit.exempted"), &Value::Int32(4));
}

#[test]
fn with_the_limiter_off_every_connection_goes_and_none_is_counted() {
    let mut m = Mem::new();
    ok(&set(&mut m, RATE_PER_SEC, "1"));
    ok(&set(&mut m, BURST_CAPACITY_SECS, "1"));
    let mut g = lock(&m.shared);
    for _ in 0..5 {
        assert_eq!(g.ingress.admit(ip("10.0.0.1"), 0), Admission::Go);
    }
    assert_eq!(g.ingress.counts().rejected, 0);
}
