//! 007 T042 (FR-029): `fsync` with `lock`, `fsyncUnlock` and `setUserWriteBlockMode` for the whole
//! cluster, with the answers and errors of the reference router 9.0.4 (measured on 01-11-2026).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};

fn lock() -> Document {
    doc(&[("fsync", Value::Int32(1)), ("lock", Value::Boolean(true))])
}

fn unlock() -> Document {
    doc(&[("fsyncUnlock", Value::Int32(1))])
}

fn block(on: bool) -> Document {
    doc(&[("setUserWriteBlockMode", Value::Int32(1)), ("global", Value::Boolean(on))])
}

fn insert(coll: &str) -> Document {
    let one = Value::Document(doc(&[("x", Value::Int32(1))]));
    doc(&[("insert", Value::from(coll)), ("documents", Value::Array(vec![one]))])
}

fn err(r: &Document) -> (Option<&Value>, Option<&Value>) {
    (r.get("code"), r.get("errmsg"))
}

fn first_write_error(r: &Document) -> (&Value, &Value) {
    (field(r, "writeErrors.0.code"), field(r, "writeErrors.0.errmsg"))
}

#[test]
fn locks_count_and_each_unlock_takes_one_away() {
    let mut m = Mem::new();
    let not_locked = Value::from("fsyncUnlock called when not locked");
    let r = m.run("admin", unlock());
    assert_eq!(err(&r), (Some(&Value::Int32(20)), Some(&not_locked)));
    // In the order of the reference router 9.0.4's answer (measured on 03-11-2026).
    let keys: Vec<&str> =
        r.iter().map(|(k, _)| k).filter(|k| !k.starts_with('$') && *k != "operationTime").collect();
    assert_eq!(keys, ["raw", "code", "codeName", "ok", "errmsg"], "{r:?}");
    assert_eq!(r.get("ok"), Some(&Value::Double(0.0)));
    for _ in 0..2 {
        let r = m.run("admin", lock());
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
        assert_eq!(field(&r, "numFiles"), &Value::Int32(1));
    }
    // An integer takes the lock as `true` does.
    let r = m.run("admin", doc(&[("fsync", Value::Int32(1)), ("lock", Value::Int32(1))]));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)));
    for _ in 0..3 {
        let r = m.run("admin", unlock());
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    }
    let r = m.run("admin", unlock());
    assert_eq!(err(&r), (Some(&Value::Int32(20)), Some(&not_locked)));
    let r = m.run("t", unlock());
    let msg = Value::from("fsyncUnlock may only be run against the admin database.");
    assert_eq!(err(&r), (Some(&Value::Int32(13)), Some(&msg)));
}

#[test]
fn a_write_waits_for_the_unlock_and_a_read_does_not() {
    let mut m = Mem::new();
    m.run("t", insert("c"));
    m.run("admin", lock());
    // The write waits until its time runs out.
    m.pauses_left = 3;
    let r = m.run("t", insert("c"));
    let (code, msg) = first_write_error(&r);
    assert_eq!(code, &Value::Int32(50));
    // As the reference router 9.0.4 relays it from the shard (measured on 03-11-2026).
    let Value::String(msg) = msg else { panic!("{r:?}") };
    assert!(msg.starts_with("cluster write results unavailable from "), "{msg}");
    assert!(msg.ends_with(":: caused by :: operation exceeded time limit"), "{msg}");
    let r = m.run("t", doc(&[("find", Value::from("c"))]));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = m.run("t", doc(&[("drop", Value::from("c"))]));
    assert_eq!(r.get("code"), Some(&Value::Int32(50)), "{r:?}");
    m.run("admin", unlock());
    let r = m.run("t", insert("c"));
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
}

#[test]
fn blocked_user_writes_fail_and_internal_ones_and_reads_go_on() {
    let mut m = Mem::new();
    m.run("t", insert("c"));
    let r = m.run("admin", block(true));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let blocked = Value::from("User writes blocked, reason: Unspecified");
    let r = m.run("t", insert("c"));
    assert_eq!(first_write_error(&r), (&Value::Int32(371), &blocked));
    assert_eq!(field(&r, "n"), &Value::Int32(0));
    // An update that matches fails; a delete that matches nothing does not.
    let set = doc(&[("$set", Value::Document(doc(&[("y", Value::Int32(1))])))]);
    let one = doc(&[("q", Value::Document(Document::new())), ("u", Value::Document(set))]);
    let r = m.run(
        "t",
        doc(&[("update", Value::from("c")), ("updates", Value::Array(vec![Value::Document(one)]))]),
    );
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(371), "{r:?}");
    let none =
        doc(&[("q", Value::Document(doc(&[("x", Value::Int32(99))]))), ("limit", Value::Int32(1))]);
    let r = m.run(
        "t",
        doc(&[
            ("delete", Value::from("c")),
            ("deletes", Value::Array(vec![Value::Document(none)])),
        ]),
    );
    assert_eq!((r.get("ok"), r.get("writeErrors")), (Some(&Value::Double(1.0)), None), "{r:?}");
    let r = m.run("t", doc(&[("drop", Value::from("c"))]));
    assert_eq!(err(&r), (Some(&Value::Int32(371)), Some(&Value::from("User writes blocked"))));
    let r = m.run("t", doc(&[("find", Value::from("c"))]));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = m.run("admin", insert("foo"));
    assert_eq!(field(&r, "n"), &Value::Int32(1), "admin is not a user database: {r:?}");
    // Turned off from any database.
    let r = m.run("t", block(false));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)));
    let r = m.run("t", insert("c"));
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
}

#[test]
fn set_user_write_block_mode_checks_its_fields() {
    let mut m = Mem::new();
    m.run("t", insert("c"));
    let cases = [
        (
            doc(&[("setUserWriteBlockMode", Value::Int32(1))]),
            40414,
            "BSON field 'setUserWriteBlockMode.global' is missing but a required field",
        ),
        (
            doc(&[("setUserWriteBlockMode", Value::Int32(1)), ("global", Value::Int32(1))]),
            14,
            "BSON field 'setUserWriteBlockMode.global' is the wrong type 'int', expected type 'bool'",
        ),
        (
            block(false).with("foo", 1),
            40415,
            "BSON field 'setUserWriteBlockMode.foo' is an unknown field.",
        ),
        (
            block(true).with("reason", "Nope"),
            2,
            "Enumeration value 'Nope' for field 'setUserWriteBlockMode.reason' is not a valid value.",
        ),
    ];
    for (cmd, code, msg) in cases {
        let r = m.run("admin", cmd);
        assert_eq!(err(&r), (Some(&Value::Int32(code)), Some(&Value::from(msg))));
    }
    for reason in ["Unspecified", "DiskUseThresholdExceeded", "ClusterToClusterMigrationInProgress"]
    {
        let r = m.run("admin", block(true).with("reason", reason));
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{reason}: {r:?}");
    }
    // The reason the reference router 9.0.4 writes in the error stays `Unspecified` (measured).
    let r = m.run("t", insert("c"));
    let blocked = Value::from("User writes blocked, reason: Unspecified");
    assert_eq!(first_write_error(&r), (&Value::Int32(371), &blocked));
}

#[test]
fn the_gates_answer_once_every_node_applies_them() {
    let mut m = Mem::new();
    // Two answers of a node still behind: the command waits for it, as the reference router waits
    // for every shard.
    for cmd in [lock(), unlock(), block(true), block(false)] {
        m.gate_lagging = 2;
        m.pauses_left = 5;
        let r = m.run("admin", cmd);
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
        assert_eq!(m.gate_lagging, 0, "it answered before every node applied the gate");
    }
    // A node that never catches up: the command runs out of time.
    m.gate_lagging = usize::MAX;
    m.pauses_left = 3;
    let r = m.run("admin", lock());
    let msg = Value::from("operation exceeded time limit");
    assert_eq!(err(&r), (Some(&Value::Int32(50)), Some(&msg)));
}
