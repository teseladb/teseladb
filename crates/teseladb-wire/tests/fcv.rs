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

/// The version Group 0 keeps.
fn version(m: &mut Mem) -> Value {
    Value::from(teseladb_wire::commands::fcv::version_of(&m.params))
}

#[test]
fn get_parameter_has_no_feature_compatibility_version_as_on_a_refrouter() {
    let mut m = Mem::new();
    let r = m.run(
        "admin",
        doc(&[("getParameter", Value::Int32(1)), ("featureCompatibilityVersion", Value::Int32(1))]),
    );
    let msg = Value::from("no option found to get");
    assert_eq!(err(&r), (Some(&Value::Int32(72)), Some(&msg)));
    let r = m.run("admin", doc(&[("getParameter", Value::from("*"))]));
    assert_eq!(r.get("featureCompatibilityVersion"), None, "{r:?}");
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

/// A downgrade finds the collections whose options the target version lacks, as RefDb r9.0.4
/// does in `userCollectionsUassertsForDowngrade` (`legacy_fcv_step.cpp`): `validationAction:
/// "errorAndLog"` exists from 8.1 and the level `constraint` and `prepareConstraintValidationLevel`
/// from 9.0. It refuses with 332 `CannotDowngrade` naming the first such collection, after the
/// confirmation, and leaves the version (measured on the native `refrouter` 9.0.4 on 09-11-2026).
#[test]
fn a_downgrade_refuses_collections_with_options_of_newer_versions() {
    let mut m = Mem::new();
    let created = m.run(
        "db",
        doc(&[("create", Value::from("p")), ("prepareConstraintValidationLevel", true.into())]),
    );
    assert_eq!(field(&created, "ok"), &Value::Double(1.0), "{created:?}");

    let unconfirmed = m.run("admin", set(Value::from("8.3"), None));
    let refused = m.run("admin", set(Value::from("8.3"), YES));

    assert_eq!(unconfirmed.get("code"), Some(&Value::Int32(7_369_100)), "{unconfirmed:?}");
    assert_eq!(refused.get("code"), Some(&Value::Int32(332)), "{refused:?}");
    assert_eq!(refused.get("codeName"), Some(&Value::from("CannotDowngrade")));
    let Some(Value::String(msg)) = refused.get("errmsg") else { panic!("{refused:?}") };
    let start = "Cannot downgrade the cluster when there are collections with \
                 prepareConstraintValidationLevel set. Please unset the option or drop the \
                 collection(s) before downgrading. First detected collection with \
                 prepareConstraintValidationLevel set: db.p (UUID: ";
    assert!(msg.starts_with(start) && msg.ends_with(")."), "{msg}");
    assert_eq!(version(&mut m), Value::from("9.0"));

    // `errorAndLog` exists from 8.1: 8.3 keeps it, 8.0 does not.
    let mut m = Mem::new();
    let created = m.run(
        "db",
        doc(&[
            ("create", Value::from("e")),
            ("validator", doc(&[("a", 1.into())]).into()),
            ("validationAction", Value::from("errorAndLog")),
        ]),
    );
    assert_eq!(field(&created, "ok"), &Value::Double(1.0), "{created:?}");
    let refused = m.run("admin", set(Value::from("8.0"), YES));
    let Some(Value::String(msg)) = refused.get("errmsg") else { panic!("{refused:?}") };
    assert!(msg.contains("'errorAndLog' enabled: db.e (UUID: "), "{msg}");
    let r = m.run("admin", set(Value::from("8.3"), YES));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
}
