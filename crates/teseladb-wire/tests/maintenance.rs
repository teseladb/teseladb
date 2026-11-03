//! The maintenance commands of 006 FR-033 (`dataSize`, `logRotate`, `dropConnections`,
//! `lockInfo`, `connPoolStats`, `isdbgrid`, `killAllSessionsByPattern`), as a `refrouter` 9.0.4
//! answers them (measured).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_hash::sha256;
use teseladb_wire::commands::session::KillScope;
use teseladb_wire::error::{CommandError, INVALID_SSL_CONFIGURATION};

fn s(x: &str) -> Value {
    Value::from(x)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

fn code(r: &Document) -> i32 {
    match r.get("code") {
        Some(Value::Int32(c)) => *c,
        _ => panic!("no error in {r:?}"),
    }
}

fn int(r: &Document, path: &str) -> i64 {
    match field(r, path) {
        Value::Int64(n) => *n,
        other => panic!("{path} is {other:?}"),
    }
}

/// Five documents of 39 bytes each, as in the measurement.
fn five() -> Mem {
    let mut m = Mem::new();
    let docs = (0..5).map(|i| d(&[("_id", i.into()), ("a", i.into()), ("p", s(&"x".repeat(10)))]));
    m.run("ds", doc(&[("insert", s("c")), ("documents", Value::Array(docs.collect()))]));
    m
}

#[test]
fn data_size_counts_bytes_and_documents() {
    let mut m = five();
    let run = |m: &mut Mem, extra: &[(&str, Value)]| {
        let mut cmd = doc(&[("dataSize", s("ds.c"))]);
        for (k, v) in extra {
            cmd.push(*k, v.clone());
        }
        m.run("admin", cmd)
    };
    let r = run(&mut m, &[]);
    assert_eq!((int(&r, "size"), int(&r, "numObjects")), (195, 5));
    assert_eq!(field(&r, "millis"), &Value::Int64(0));
    let range = [("min", d(&[("_id", 1.into())])), ("max", d(&[("_id", 3.into())]))];
    let r = run(&mut m, &range);
    assert_eq!((int(&r, "size"), int(&r, "numObjects")), (78, 2));
    let r =
        run(&mut m, &[("keyPattern", d(&[("_id", 1.into())])), range[0].clone(), range[1].clone()]);
    assert_eq!(int(&r, "numObjects"), 2);
    let r = run(&mut m, &[("maxSize", 1.into())]);
    assert_eq!((int(&r, "size"), int(&r, "numObjects")), (39, 1));
    let r = run(&mut m, &[("maxObjects", 2.into())]);
    assert_eq!(int(&r, "numObjects"), 3);
    let r = m.run("admin", doc(&[("dataSize", s("ds.none"))]));
    assert_eq!((int(&r, "size"), int(&r, "numObjects")), (0, 0));
    let errors = [
        (
            vec![("keyPattern", d(&[("a", 1.into())]))],
            96,
            "Couldn't find valid index containing key pattern",
        ),
        (vec![range[0].clone()], 2, "Max must be set if min is set."),
        (vec![range[1].clone()], 2, "Max must not be set if min is not set."),
        (
            vec![("min", 5.into()), range[1].clone()],
            14,
            "BSON field 'dataSize.min' is the wrong type 'int', expected type 'object'",
        ),
    ];
    for (extra, c, msg) in errors {
        let r = run(&mut m, &extra);
        assert_eq!((code(&r), field(&r, "errmsg")), (c, &s(msg)));
    }
    let r = m.run("admin", doc(&[("dataSize", s("dsc"))]));
    assert_eq!((code(&r), field(&r, "errmsg")), (26, &s("database dsc not found")));
    let r = m.run("admin", doc(&[("dataSize", 5.into())]));
    assert_eq!(code(&r), 14);
}

#[test]
fn admin_commands_without_state_answer_as_a_refrouter() {
    let mut m = five();
    for cmd in [
        doc(&[("logRotate", 1.into())]),
        doc(&[("logRotate", s("server"))]),
        doc(&[("dropConnections", 1.into()), ("hostAndPort", Value::Array(vec![s("x:1")]))]),
    ] {
        assert_eq!(m.run("admin", cmd), doc(&[("ok", Value::Double(1.0))]));
    }
    let r = m.run("admin", doc(&[("logRotate", s("audit"))]));
    assert_eq!(
        (code(&r), field(&r, "errmsg")),
        (4, &s("Log rotation failed due to one or more errors"))
    );
    for name in ["logRotate", "lockInfo", "dropConnections"] {
        let r = m.run("ds", doc(&[(name, 1.into())]));
        assert_eq!(code(&r), 13);
    }
    let r = m.run("admin", doc(&[("dropConnections", 1.into())]));
    assert_eq!(code(&r), 40414);
    let r = m.run("admin", doc(&[("dropConnections", 1.into()), ("hostAndPort", s("x"))]));
    assert_eq!(code(&r), 14);
    let r = m.run(
        "admin",
        doc(&[("dropConnections", 1.into()), ("hostAndPort", Value::Array(vec![5.into()]))]),
    );
    assert_eq!(
        field(&r, "errmsg"),
        &s(
            "BSON field 'dropConnections.hostAndPort.0' is the wrong type 'int', expected type 'string'"
        )
    );
    let r = m.run("admin", doc(&[("lockInfo", 1.into())]));
    assert_eq!(r, doc(&[("lockInfo", Value::Array(vec![])), ("ok", Value::Double(1.0))]));
    let r = m.run("admin", doc(&[("connPoolStats", 1.into())]));
    // 32-bit counters, then 64-bit ones, as a `refrouter` 9.0.4 types them (measured).
    assert_eq!(field(&r, "totalCreated"), &Value::Int32(0));
    assert_eq!(field(&r, "totalConnectionAcquisitionRequests"), &Value::Int64(0));
    assert_eq!(field(&r, "numReplicaSetMonitorsCreated"), &Value::Int32(0));
    assert_eq!(field(&r, "acquisitionWaitTimes.[1000 ms, inf).count"), &Value::Int64(0));
    assert_eq!(field(&r, "replicaSetMatchingStrategy"), &s("matchPrimaryNode"));
    let r = m.run("ds", doc(&[("isdbgrid", 1.into())]));
    assert_eq!((field(&r, "isdbgrid"), field(&r, "ok")), (&Value::Int32(1), &Value::Double(1.0)));
}

#[test]
fn sessions_are_killed_by_pattern() {
    let mut m = Mem::new();
    let id = |n: u8| Value::Binary { subtype: 4, bytes: vec![n; 16] };
    let by = |m: &mut Mem, patterns: Vec<Value>| {
        m.killed.clear();
        let r = m.run("admin", doc(&[("killAllSessionsByPattern", Value::Array(patterns))]));
        (r, m.killed.clone())
    };
    let (r, k) = by(&mut m, vec![]);
    assert_eq!((field(&r, "ok"), k), (&Value::Double(1.0), vec![KillScope::All]));
    let (_, k) = by(&mut m, vec![d(&[])]);
    assert_eq!(k, vec![KillScope::All]);
    let (_, k) = by(&mut m, vec![d(&[("lsid", d(&[("id", id(7))]))])]);
    assert_eq!(k, vec![KillScope::Sessions(vec![u128::from_be_bytes([7; 16])])]);
    let nobody = Value::Binary { subtype: 0, bytes: sha256(b"").to_vec() };
    let (_, k) = by(&mut m, vec![d(&[("uid", nobody)])]);
    assert_eq!(k, vec![KillScope::All]);
    // A user, a role or another `uid` reaches no session until 013.
    let other = Value::Binary { subtype: 0, bytes: vec![1; 32] };
    for p in [
        d(&[("uid", other)]),
        d(&[("users", Value::Array(vec![d(&[("user", s("u")), ("db", s("admin"))])]))]),
        d(&[("roles", Value::Array(vec![s("r")]))]),
    ] {
        let (r, k) = by(&mut m, vec![p]);
        assert_eq!((field(&r, "ok"), k), (&Value::Double(1.0), vec![]));
    }
    let (_, k) =
        by(&mut m, vec![d(&[("users", Value::Array(vec![])), ("lsid", d(&[("id", id(3))]))])]);
    assert_eq!(k, vec![KillScope::Sessions(vec![u128::from_be_bytes([3; 16])])]);
    let list = "KillAllSessionsByPatternCmd.killAllSessionsByPattern";
    let errors = [
        (d(&[("foo", 1.into())]), 40415, format!("BSON field '{list}.foo' is an unknown field.")),
        (
            d(&[("lsid", d(&[("id", 0.into())]))]),
            14,
            format!("BSON field '{list}.lsid.id' is the wrong type 'int', expected type 'binData'"),
        ),
        (
            d(&[("lsid", d(&[]))]),
            40414,
            format!("BSON field '{list}.lsid.id' is missing but a required field"),
        ),
        (
            d(&[("lsid", 5.into())]),
            14,
            format!("BSON field '{list}.lsid' is the wrong type 'int', expected type 'object'"),
        ),
        (
            d(&[("uid", 5.into())]),
            14,
            format!("BSON field '{list}.uid' is the wrong type 'int', expected type 'binData'"),
        ),
        (
            d(&[("users", 5.into())]),
            14,
            format!("BSON field '{list}.users' is the wrong type 'int', expected type 'array'"),
        ),
        (
            5.into(),
            14,
            format!("BSON field '{list}.0' is the wrong type 'int', expected type 'object'"),
        ),
    ];
    for (p, c, msg) in errors {
        let (r, _) = by(&mut m, vec![p]);
        assert_eq!((code(&r), field(&r, "errmsg")), (c, &s(&msg)));
    }
    let r = m.run("admin", doc(&[("killAllSessionsByPattern", 5.into())]));
    assert_eq!(
        field(&r, "errmsg"),
        &s(&format!("BSON field '{list}' is the wrong type 'int', expected type 'array'"))
    );
}

#[test]
fn rotate_certificates_answers_as_refnode_and_reports_the_failure() {
    let mut m = Mem::new();
    let ok = doc(&[("ok", Value::Double(1.0))]);
    assert_eq!(m.run("admin", doc(&[("rotateCertificates", 1.into())])), ok);
    let with_message = doc(&[("rotateCertificates", s("x")), ("message", s("m"))]);
    assert_eq!(m.run("admin", with_message), ok);
    assert_eq!(m.rotations, vec![None, Some("m".to_owned())]);
    // Fields in the order of the document, before the database (measured on 9.0.4).
    let errors = [
        (
            "admin",
            doc(&[("rotateCertificates", 1.into()), ("zz", 1.into()), ("message", 5.into())]),
            40415,
            "BSON field 'rotateCertificates.zz' is an unknown field.",
        ),
        (
            "test",
            doc(&[("rotateCertificates", 1.into()), ("message", 5.into()), ("zz", 1.into())]),
            14,
            "BSON field 'rotateCertificates.message' is the wrong type 'int', expected type \
             'string'",
        ),
        (
            "test",
            doc(&[("rotateCertificates", 1.into())]),
            13,
            "rotateCertificates may only be run against the admin database.",
        ),
    ];
    for (db, cmd, c, msg) in errors {
        let r = m.run(db, cmd);
        assert_eq!((code(&r), field(&r, "errmsg")), (c, &s(msg)));
    }
    assert_eq!(m.rotations.len(), 2);
    m.fail_rotation =
        Some(CommandError::new(INVALID_SSL_CONFIGURATION, "Can not set up PEM key file."));
    let r = m.run("admin", doc(&[("rotateCertificates", 1.into())]));
    assert_eq!((code(&r), field(&r, "codeName")), (140, &s("InvalidSSLConfiguration")));
    assert_eq!(m.rotations.len(), 2);
}

/// The limits of `dataSize` taken as whole numbers (a negative one stops at the first document,
/// one between -1 and 1 is none), `maxReached` when they stopped the count, `estimate` on a
/// collection that exists, and the types of the three (measured on a `refnode` 9.0.4 on
/// 03-11-2026).
#[test]
fn data_size_limits_estimate_and_max_reached() {
    let mut m = five();
    let run = |m: &mut Mem, extra: &[(&str, Value)]| {
        let mut cmd = doc(&[("dataSize", s("ds.c"))]);
        for (k, v) in extra {
            cmd.push(*k, v.clone());
        }
        m.run("admin", cmd)
    };
    let reached = |r: &Document| r.get("maxReached").cloned();
    let cases: [(&str, Value, i64); 6] = [
        ("maxSize", Value::Int32(-5), 1),
        ("maxObjects", Value::Int64(-1), 1),
        ("maxSize", Value::Double(-0.5), 5),
        ("maxSize", Value::Double(2.5), 1),
        ("maxObjects", Value::Double(1.9), 2),
        ("maxSize", Value::Double(f64::NAN), 5),
    ];
    for (k, v, n) in cases {
        let r = run(&mut m, &[(k, v.clone())]);
        assert_eq!(int(&r, "numObjects"), n, "{k} {v:?}: {r:?}");
        let want = (n < 5).then_some(Value::Boolean(true));
        assert_eq!(reached(&r), want, "{k} {v:?}: {r:?}");
        assert_eq!(field(&r, "estimate"), &Value::Boolean(false));
    }
    let keys: Vec<String> =
        run(&mut m, &[("maxObjects", 1.into())]).iter().map(|(k, _)| k.to_owned()).collect();
    assert_eq!(keys, ["size", "numObjects", "millis", "estimate", "maxReached", "ok"]);
    let r = run(&mut m, &[("estimate", 1.into()), ("maxSize", Value::Null)]);
    assert_eq!((field(&r, "estimate"), reached(&r)), (&Value::Boolean(true), None));
    let r = m.run("admin", doc(&[("dataSize", s("ds.none"))]));
    assert_eq!(r.get("estimate"), None, "{r:?}");
    let errors = [
        (
            "maxSize",
            s("x"),
            "BSON field 'dataSize.maxSize' is the wrong type 'string', expected types '[int, decimal, double, long]'",
        ),
        (
            "maxObjects",
            true.into(),
            "BSON field 'dataSize.maxObjects' is the wrong type 'bool', expected types '[int, decimal, double, long]'",
        ),
        (
            "estimate",
            s("x"),
            "BSON field 'dataSize.estimate' is the wrong type 'string', expected types '[double, long, int, bool, decimal]'",
        ),
    ];
    for (k, v, msg) in errors {
        let r = run(&mut m, &[(k, v)]);
        assert_eq!((code(&r), field(&r, "errmsg")), (14, &s(msg)), "{r:?}");
    }
}
