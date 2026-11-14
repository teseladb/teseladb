//! `internalQueryMaxMemoryUsageBytesPerOperation` (010 T043, FR-010), measured on a reference
//! replica set member 9.0.4 set to 2000 bytes: a `$group` that holds more writes to disk with
//! `allowDiskUse` and fails with 292 without it; a sort is not held to it.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};

/// `QueryExceededMemoryLimitNoDiskUseAllowed`.
const NO_DISK_USE: i32 = 292;

fn cmd(text: &str) -> Document {
    extjson::parse(text).expect("a command")
}

fn seeded() -> Mem {
    let mut m = Mem::new();
    let docs: Vec<String> = (0..50)
        .map(|i| format!(r#"{{"_id":{i},"g":{},"s":"{}"}}"#, i % 3, "x".repeat(100)))
        .collect();
    let r = m.run("memx", cmd(&format!(r#"{{"insert":"c","documents":[{}]}}"#, docs.join(","))));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    m
}

#[test]
fn a_group_past_the_operation_limit_needs_disk_use() {
    let mut m = seeded();
    let r = m.run(
        "admin",
        cmd(r#"{"setParameter":1,"internalQueryMaxMemoryUsageBytesPerOperation":2000}"#),
    );
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let group = r#"{"$group":{"_id":"$_id","x":{"$push":"$s"}}}"#;
    let r =
        m.run("memx", cmd(&format!(r#"{{"aggregate":"c","pipeline":[{group}],"cursor":{{}}}}"#)));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = m.run(
        "memx",
        cmd(&format!(
            r#"{{"aggregate":"c","pipeline":[{group}],"cursor":{{}},"allowDiskUse":false}}"#
        )),
    );
    assert_eq!(r.get("code"), Some(&Value::Int32(NO_DISK_USE)), "{r:?}");
    let msg = "Executor error during aggregate command on namespace: memx.c :: caused by :: Exceeded \
               memory limit for group, but didn't allow external spilling; pass allowDiskUse:true to opt in";
    assert_eq!(r.get("errmsg"), Some(&Value::from(msg)));
    let sort = r#"{"aggregate":"c","pipeline":[{"$sort":{"s":1,"_id":-1}}],"cursor":{},"allowDiskUse":false}"#;
    let r = m.run("memx", cmd(sort));
    assert_eq!(field(&r, "ok"), &Value::Double(1.0), "{r:?}");
    let r = m.run(
        "admin",
        doc(&[
            ("getParameter", 1.into()),
            ("internalQueryMaxMemoryUsageBytesPerOperation", 1.into()),
        ]),
    );
    assert_eq!(field(&r, "internalQueryMaxMemoryUsageBytesPerOperation"), &Value::Int64(2000));
    let r = m.run(
        "admin",
        cmd(r#"{"setParameter":1,"internalQueryMaxMemoryUsageBytesPerOperation":0}"#),
    );
    let msg = "Invalid value for parameter internalQueryMaxMemoryUsageBytesPerOperation: 0 is not \
               greater than 0";
    assert_eq!(r.get("errmsg"), Some(&Value::from(msg)), "{r:?}");
}
