//! 007 T032: the cluster's default concerns (FR-018), with the answers and errors of a `refrouter`
//! 9.0.4 (measured): `setDefaultRWConcern` keeps them in Group 0, `getDefaultRWConcern` shows them
//! over the implicit ones, and a command without its own concern takes them.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

fn get() -> Document {
    doc(&[("getDefaultRWConcern", Value::Int32(1))])
}

fn set() -> Document {
    Document::new().with("setDefaultRWConcern", 1)
}

fn err(r: &Document) -> (Option<&Value>, Option<&Value>) {
    (r.get("code"), r.get("errmsg"))
}

fn sub<'a>(r: &'a Document, k: &str) -> &'a Document {
    match r.get(k) {
        Some(Value::Document(d)) => d,
        other => panic!("{k}: {other:?} in {r:?}"),
    }
}

#[test]
fn without_a_default_the_implicit_ones_are_shown() {
    let mut m = Mem::new();
    let r = m.run("admin", get());
    assert_eq!(sub(&r, "defaultReadConcern"), &Document::new().with("level", "local"));
    assert_eq!(
        sub(&r, "defaultWriteConcern"),
        &Document::new().with("w", "majority").with("wtimeout", 0)
    );
    assert_eq!(field(&r, "defaultWriteConcernSource"), &Value::from("implicit"));
    assert_eq!(field(&r, "defaultReadConcern.level"), &Value::from("local"));
    assert_eq!(r.get("defaultReadConcernSource"), Some(&Value::from("implicit")));
    assert_eq!(r.get("updateOpTime"), None);
    assert!(matches!(r.get("localUpdateWallClockTime"), Some(Value::DateTime(_))));
    assert_eq!(r.get("inMemory"), None);
    let r = m.run("admin", get().with("inMemory", true));
    assert_eq!(r.get("inMemory"), Some(&Value::Boolean(true)));
    let r = m.run("admin", get().with("inMemory", false));
    assert_eq!(r.get("inMemory"), None);
    let r = m.run("admin", get().with("inMemory", "yes"));
    let want = "BSON field 'getDefaultRWConcern.inMemory' is the wrong type 'string', expected type 'bool'";
    assert_eq!(err(&r), (Some(&Value::Int32(14)), Some(&Value::from(want))));
    let r = m.run("t", get());
    let want = "getDefaultRWConcern may only be run against the admin database.";
    assert_eq!(err(&r), (Some(&Value::Int32(13)), Some(&Value::from(want))));
}

#[test]
fn defaults_set_are_kept_shown_and_refused_as_a_refrouter_does() {
    let mut m = Mem::new();
    let majority = Document::new().with("level", "majority");
    let r = m.run("admin", set().with("defaultReadConcern", majority.clone()));
    assert_eq!(sub(&r, "defaultReadConcern"), &majority);
    assert_eq!(r.get("defaultReadConcernSource"), Some(&Value::from("global")));
    assert_eq!(r.get("defaultWriteConcernSource"), Some(&Value::from("implicit")));
    assert!(matches!(r.get("updateOpTime"), Some(Value::Timestamp { .. })));
    let r = m.run("admin", set().with("defaultWriteConcern", Document::new().with("w", 1)));
    assert_eq!(sub(&r, "defaultWriteConcern"), &Document::new().with("w", 1).with("wtimeout", 0));
    assert_eq!(sub(&r, "defaultReadConcern"), &majority, "the read concern stays");
    let r = m.run("admin", get());
    assert_eq!(r.get("defaultWriteConcernSource"), Some(&Value::from("global")));
    let j = Document::new().with("w", "majority").with("j", true);
    let r = m.run("admin", set().with("defaultWriteConcern", j));
    let shown = Document::new().with("w", "majority").with("j", true).with("wtimeout", 0);
    assert_eq!(sub(&r, "defaultWriteConcern"), &shown);
    // `{}` unsets the read concern; the write concern cannot be unset once set.
    let r = m.run("admin", set().with("defaultReadConcern", Document::new()));
    assert_eq!(r.get("defaultReadConcernSource"), Some(&Value::from("implicit")));
    let cases: [(Document, i32, &str); 8] = [
        (
            set(),
            2,
            "At least one of the \"defaultReadConcern\" or \"defaultWriteConcern\" fields must be present",
        ),
        (
            set().with("defaultWriteConcern", Document::new()),
            20,
            "The global default write concern cannot be unset once it is set.",
        ),
        (
            set().with("defaultWriteConcern", Document::new().with("w", 0)),
            2,
            "Unacknowledged write concern is not suitable for the default write concern",
        ),
        (
            set().with("defaultReadConcern", Document::new().with("level", "snapshot")),
            2,
            "level: 'snapshot' is not suitable for the default read concern",
        ),
        (
            set().with("defaultReadConcern", Document::new().with("level", "linearizable")),
            2,
            "level: 'linearizable' is not suitable for the default read concern",
        ),
        (
            set().with("defaultReadConcern", 5),
            14,
            "BSON field 'setDefaultRWConcern.defaultReadConcern' is the wrong type 'int', expected type 'object'",
        ),
        (
            set().with("defaultWriteConcern", "x"),
            14,
            "BSON field 'setDefaultRWConcern.defaultWriteConcern' is the wrong type 'string', expected type 'object'",
        ),
        (
            set().with("defaultReadConcern", Document::new().with("level", "local").with("x", 1)),
            40415,
            "BSON field 'readConcern.x' is an unknown field.",
        ),
    ];
    for (cmd, code, msg) in cases {
        let r = m.run("admin", cmd);
        assert_eq!(err(&r), (Some(&Value::Int32(code)), Some(&Value::from(msg))), "{r:?}");
    }
    let r = m.run("t", set().with("defaultReadConcern", majority));
    assert_eq!(r.get("code"), Some(&Value::Int32(13)));
    let after = Document::new()
        .with("level", "local")
        .with("afterClusterTime", Value::Timestamp { time: 1, increment: 1 });
    let r = m.run("admin", set().with("defaultReadConcern", after));
    let want = "'afterClusterTime' is not suitable for the default read concern";
    assert_eq!(err(&r), (Some(&Value::Int32(2)), Some(&Value::from(want))));
}

#[test]
fn a_write_without_its_own_concern_takes_the_default() {
    let mut m = Mem::new();
    // Five nodes are more than the three that hold data: the write's error names its concern.
    let five = Document::new().with("w", 5).with("wtimeout", 0);
    let r = m.run("admin", set().with("defaultWriteConcern", five));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let docs = Value::Array(vec![Value::Document(Document::new().with("_id", 1))]);
    let r = m.run("t", Document::new().with("insert", "c").with("documents", docs));
    let wce = sub(&r, "writeConcernError");
    let Some(Value::String(msg)) = wce.get("errmsg") else { panic!("{r:?}") };
    let details = "Error details: { writeConcern: { w: 5, wtimeout: 0, provenance: \"customDefault\" } } at node1";
    assert!(msg.ends_with(details), "{msg}");
    // With its own concern, the command's.
    let own = Document::new().with("w", 5);
    let docs = Value::Array(vec![Value::Document(Document::new().with("_id", 2))]);
    let r = m.run(
        "t",
        Document::new().with("insert", "c").with("documents", docs).with("writeConcern", own),
    );
    let Some(Value::String(msg)) = sub(&r, "writeConcernError").get("errmsg") else {
        panic!("{r:?}")
    };
    assert!(msg.contains("provenance: \"clientSupplied\""), "{msg}");
}

/// `fsync`, `compact`, `autoCompact` and `shutdown` (007 T032, FR-018, DV-017), with the checks
/// measured on a `refrouter` 9.0.4 and, for `compact` and `autoCompact`, on its shard.
#[test]
fn fsync_compact_and_shutdown_check_their_fields() {
    let mut m = Mem::new();
    let r = m.run("admin", doc(&[("fsync", Value::Int32(1))]));
    assert_eq!(field(&r, "numFiles"), &Value::Int32(1));
    assert!(matches!(field(&r, "all.raw"), Value::Document(_)), "{r:?}");
    let r = m.run("t", doc(&[("fsync", Value::Int32(1))]));
    assert_eq!(r.get("code"), Some(&Value::Int32(13)));
    let r = m.run("admin", doc(&[("fsync", Value::Int32(1)), ("lock", Value::from("x"))]));
    let want = "BSON field 'fsync.lock' is the wrong type 'string', expected types '[decimal, int, long, bool, double]'";
    assert_eq!(err(&r), (Some(&Value::Int32(14)), Some(&Value::from(want))));
    assert_eq!(field(&r, "numFiles"), &Value::Int32(1), "with each shard's answer");
    let r = m.run("admin", doc(&[("fsync", Value::Int32(1)), ("foo", Value::Int32(1))]));
    assert_eq!(r.get("code"), Some(&Value::Int32(40415)));
    let r = m.run("admin", doc(&[("fsync", Value::Int32(1)), ("lock", Value::Boolean(false))]));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)));
    m.run("t", doc(&[("create", Value::from("c"))]));
    let r = m.run("t", doc(&[("compact", Value::from("c"))]));
    assert_eq!(r.get("code"), Some(&Value::Int32(20)), "a primary needs force");
    let r = m.run("t", doc(&[("compact", Value::from("c")), ("force", Value::Boolean(true))]));
    assert_eq!(field(&r, "bytesFreed"), &Value::Int32(0));
    let r = m.run("t", doc(&[("compact", Value::from("nope")), ("force", Value::Boolean(true))]));
    assert_eq!(r.get("code"), Some(&Value::Int32(26)));
    let r = m.run("t", doc(&[("compact", Value::from("c")), ("foo", Value::Int32(1))]));
    assert_eq!(r.get("code"), Some(&Value::Int32(40415)));
    let r = m.run(
        "admin",
        doc(&[("autoCompact", Value::Boolean(true)), ("freeSpaceTargetMB", Value::Int32(20))]),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)));
    let r = m.run("admin", doc(&[("autoCompact", Value::from("x"))]));
    let want =
        "BSON field 'autoCompact.autoCompact' is the wrong type 'string', expected type 'bool'";
    assert_eq!(err(&r), (Some(&Value::Int32(14)), Some(&Value::from(want))));
    for (cmd, code) in [
        (doc(&[("shutdown", Value::Int32(1)), ("foo", Value::Int32(1))]), 40415),
        (doc(&[("shutdown", Value::Int32(1)), ("force", Value::from("x"))]), 14),
        (doc(&[("shutdown", Value::Int32(1)), ("timeoutSecs", Value::from("x"))]), 14),
        // The test backend cannot stop.
        (doc(&[("shutdown", Value::Int32(1))]), 20),
    ] {
        let r = m.run("admin", cmd);
        assert_eq!(r.get("code"), Some(&Value::Int32(code)), "{r:?}");
    }
    let r = m.run("t", doc(&[("shutdown", Value::Int32(1))]));
    let want = "shutdown may only be run against the admin database.";
    assert_eq!(err(&r), (Some(&Value::Int32(13)), Some(&Value::from(want))));
}
