//! 007 T040 (FR-027): `setFeatureCompatibilityVersion` and `getParameter` of
//! `featureCompatibilityVersion`, with the answers and errors of RefDb 9.0.4 (measured on
//! 01-11-2026: the command on a `refrouter`, the parameter on its shard, since a `refrouter` has none).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

fn set(v: Value, confirm: Option<Value>) -> Document {
    let mut d = doc(&[("setFeatureCompatibilityVersion", v)]);
    if let Some(c) = confirm {
        d.push("confirm", c);
    }
    d
}

fn version(m: &mut Mem) -> Value {
    let r = m.run(
        "admin",
        doc(&[("getParameter", Value::Int32(1)), ("featureCompatibilityVersion", Value::Int32(1))]),
    );
    field(&r, "featureCompatibilityVersion.version").clone()
}

fn err(r: &Document) -> (Option<&Value>, Option<&Value>) {
    (r.get("code"), r.get("errmsg"))
}

const YES: Option<Value> = Some(Value::Boolean(true));

#[test]
fn the_version_starts_at_the_latest_and_moves_as_refdb_allows() {
    let mut m = Mem::new();
    assert_eq!(version(&mut m), Value::from("9.0"));
    let r = m.run("admin", set(Value::from("8.0"), YES));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(version(&mut m), Value::from("8.0"));
    let r = m.run("admin", set(Value::from("8.3"), YES));
    let msg = "cannot set featureCompatibilityVersion to '8.3' while featureCompatibilityVersion is '8.0'";
    assert_eq!(err(&r), (Some(&Value::Int32(5147403)), Some(&Value::from(msg))));
    let r = m.run("admin", set(Value::from("9.0"), None));
    let msg = "To proceed with the FCV upgrade, please re-run this command with 'confirm: true'.";
    assert_eq!(err(&r), (Some(&Value::Int32(7369100)), Some(&Value::from(msg))));
    let r = m.run("admin", set(Value::from("9.0"), Some(Value::Int32(1))));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    // The same version without `confirm` is a downgrade too (measured).
    let r = m.run("admin", set(Value::from("9.0"), None));
    let msg = "To proceed with the FCV downgrade, please re-run this command with 'confirm: true'.";
    assert_eq!(err(&r), (Some(&Value::Int32(7369100)), Some(&Value::from(msg))));
    let r = m.run("admin", set(Value::from("8.3"), YES));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    assert_eq!(version(&mut m), Value::from("8.3"));
}

#[test]
fn set_feature_compatibility_version_checks_its_fields() {
    let mut m = Mem::new();
    let invalid = |v: &str| {
        format!(
            "Invalid feature compatibility version value '{v}'. Expected one of the following versions: ''9.0', '8.3', '8.0'. See https://docs.refdb.com/master/release-notes/8.0-compatibility/#feature-compatibility."
        )
    };
    let cases = [
        (set(Value::from("7.0"), YES), 4926900, invalid("7.0")),
        (set(Value::from("latest"), YES), 4926900, invalid("latest")),
        (
            set(Value::Int32(8), YES),
            14,
            "BSON field 'setFeatureCompatibilityVersion.setFeatureCompatibilityVersion' is the wrong type 'int', expected type 'string'".to_owned(),
        ),
        (
            set(Value::from("9.0"), Some(Value::from("yes"))),
            14,
            "BSON field 'setFeatureCompatibilityVersion.confirm' is the wrong type 'string', expected types '[int, decimal, bool, double, long]'".to_owned(),
        ),
        (
            set(Value::from("9.0"), YES).with("foo", 1),
            40415,
            "BSON field 'setFeatureCompatibilityVersion.foo' is an unknown field.".to_owned(),
        ),
    ];
    for (cmd, code, msg) in cases {
        let r = m.run("admin", cmd);
        assert_eq!(err(&r), (Some(&Value::Int32(code)), Some(&Value::from(msg))));
    }
    let r = m.run("t", set(Value::from("9.0"), YES));
    let msg = "setFeatureCompatibilityVersion may only be run against the admin database.";
    assert_eq!(err(&r), (Some(&Value::Int32(13)), Some(&Value::from(msg))));
    let r = m.run("admin", set(Value::from("9.0"), YES).with("fromConfigServer", true));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}

#[test]
fn a_version_above_what_a_node_runs_is_refused() {
    let mut m = Mem::new();
    m.fcv_limit = Some("8.0".to_owned());
    m.run("admin", set(Value::from("8.0"), YES));
    let r = m.run("admin", set(Value::from("9.0"), YES));
    assert_eq!(r.get("code"), Some(&Value::Int32(20)), "{r:?}");
    assert_eq!(version(&mut m), Value::from("8.0"));
}
