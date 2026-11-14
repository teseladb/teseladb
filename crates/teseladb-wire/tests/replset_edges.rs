//! The edges of the replica set presentation over the in-memory node of `common` (018 FR-007,
//! FR-008, FR-010, T015): stored presentations and holds that do not read, the refusals of
//! `replSetReconfig`, `replSetResizeOplog` and `replSetStepDown`, the served collections of
//! `local` and the replica set port's set.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_wire::commands::presentation::{
    Hold, Presentation, member_of, next, parse, port_host,
};

const N1: &str = "node1.cluster:27018";
const N2: &str = "node2.cluster:27018";
const N3: &str = "node3.cluster:27018";

fn node() -> Mem {
    let mut m = Mem::new();
    m.shards = vec!["node1".into(), "node2".into(), "node3".into()];
    m
}

fn admin(m: &mut Mem, cmd: Document) -> Document {
    let mut d = doc(&[]);
    for (k, v) in cmd.iter() {
        d.push(k, v.clone());
    }
    m.run("admin", d)
}

fn present(m: &mut Mem, value: Document) -> Document {
    admin(
        m,
        Document::new()
            .with("setClusterParameter", Document::new().with("topologyPresentation", value)),
    )
}

fn replica(m: &mut Mem, primary: &str) {
    let v =
        Document::new().with("mode", "replicaset").with("setName", "rs0").with("primary", primary);
    let r = present(m, v);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

fn code_or_ok(r: &Document) -> i32 {
    match r.get("code") {
        Some(Value::Int32(c)) => *c,
        _ => 0,
    }
}

fn code(r: &Document) -> i32 {
    match r.get("code") {
        Some(Value::Int32(c)) => *c,
        _ => panic!("{r:?}"),
    }
}

/// A member's attributes and a stored presentation that do not read are refused with their
/// reason; what is missing takes its default.
#[test]
fn stored_documents_that_do_not_read() {
    let host = |h: &str| Document::new().with("host", h);
    assert!(member_of(&Document::new()).is_err());
    assert!(member_of(&host(N1).with("priority", "x")).is_err());
    assert!(member_of(&host(N1).with("votes", "x")).is_err());
    assert!(member_of(&host(N1).with("tags", Document::new().with("dc", 1))).is_err());
    let m = member_of(&host(N1).with("hidden", false).with("votes", Value::Int64(0))).unwrap();
    assert!(!m.is_hidden() && m.votes() == 0);
    let m = member_of(&host(N1).with("priority", Value::Int64(2)).with("votes", 1.0)).unwrap();
    assert_eq!(m.priority(), 2.0);
    assert!(Presentation::from_document(&Document::new().with("mode", "other")).is_err());
    let bad = Document::new().with("members", vec![Value::Int32(1)]);
    assert!(Presentation::from_document(&bad).is_err());
    let odd = Document::new().with("mode", 5).with("setName", 5).with("setVersion", "2");
    assert_eq!(Presentation::from_document(&odd).unwrap(), Presentation::default());
    assert_eq!(Hold::from_bytes(b"no"), Hold::default());
    let wrong =
        Document::new().with("frozenUntil", "x").with("maintenance", "x").with("version", 1);
    assert_eq!(Hold::from_bytes(&wrong.to_bytes().unwrap()), Hold::default());
    let negative = Document::new().with("frozenUntil", -5i64);
    assert_eq!(Hold::from_bytes(&negative.to_bytes().unwrap()).frozen_until_ms, 0);
}

/// `setClusterParameter` checks the request; a set over no nodes or with a primary that is not a
/// member is refused; the replica set port needs a name and keeps the members' attributes.
#[test]
fn requests_and_designations() {
    assert!(parse(&Document::new().with("setName", "rs0")).is_err(), "mode is required");
    let hosts = vec![N1.to_owned(), N2.to_owned()];
    let rs = |r: Document| next(&Presentation::default(), &parse(&r).unwrap(), &hosts);
    let named = Document::new().with("mode", "replicaset").with("setName", "rs0");
    assert!(rs(named.clone().with("primary", "nowhere:1")).is_err());
    assert!(next(&Presentation::default(), &parse(&named).unwrap(), &[]).is_err(), "no members");
    let p = Presentation {
        set_name: "rs0".into(),
        primary: Some(N2.into()),
        members: vec![member_of(&Document::new().with("host", N1).with("priority", 3)).unwrap()],
        replica_set_port: Some(27100),
        ..Presentation::default()
    };
    let on = p.on_port(&hosts).expect("the port's set");
    assert_eq!(on.primary(), Some(port_host(N2, 27100).as_str()));
    assert_eq!(on.members()[0].priority(), 3.0);
    assert!(Presentation { set_name: String::new(), ..p.clone() }.on_port(&hosts).is_none());
    assert!(Presentation { replica_set_port: None, ..p }.on_port(&hosts).is_none());
}

/// `replSetReconfig`, `replSetResizeOplog` and `replSetStepDown` refuse what the reference server
/// 9.0.4 refuses (measured on 02-11-2026, `r4.js`).
#[test]
fn member_command_refusals() {
    let mut m = node();
    replica(&mut m, N1);
    let reconfig =
        |m: &mut Mem, conf: Value| admin(m, Document::new().with("replSetReconfig", conf));
    assert_eq!(code(&reconfig(&mut m, Value::Int32(1))), 2);
    let base = |members: Vec<Value>| {
        Document::new().with("_id", "rs0").with("version", 9).with("members", members)
    };
    let member = |h: &str, extra: Document| {
        let mut d = Document::new().with("_id", 0).with("host", h);
        for (k, v) in extra.iter() {
            d.push(k, v.clone());
        }
        Value::Document(d)
    };
    assert_eq!(code(&reconfig(&mut m, Value::Document(base(vec![Value::Int32(1)])))), 2);
    assert_eq!(code(&reconfig(&mut m, Value::Document(base(vec![]).with("members", 1)))), 2);
    let stranger = base(vec![member("other:1", Document::new())]);
    assert_ne!(reconfig(&mut m, Value::Document(stranger)).get("ok"), Some(&Value::Double(1.0)));
    let votes = base(vec![member(N1, Document::new().with("votes", 0))]);
    assert_eq!(code(&reconfig(&mut m, Value::Document(votes))), 93);
    let hidden = base(vec![member(N1, Document::new().with("hidden", true))]);
    assert_eq!(code(&reconfig(&mut m, Value::Document(hidden))), 93);
    // Wrong types, measured on 9.0.4 (`rc.js`): 93 with the reference server's message.
    let errmsg = |r: &Document| match r.get("errmsg") {
        Some(Value::String(s)) => s.clone(),
        _ => panic!("{r:?}"),
    };
    for (f, v, expected) in [
        ("priority", Value::from("x"), "'string', expected types '[long, double, decimal, int]'"),
        ("votes", Value::from("x"), "'string', expected types '[long, double, decimal, int]'"),
        (
            "hidden",
            Value::from("x"),
            "'string', expected types '[double, bool, int, long, decimal]'",
        ),
    ] {
        let r =
            reconfig(&mut m, Value::Document(base(vec![member(N1, Document::new().with(f, v))])));
        assert_eq!(code(&r), 93, "{r:?}");
        assert!(
            errmsg(&r)
                .ends_with(&format!("BSON field 'MemberConfig.{f}' is the wrong type {expected}")),
            "{r:?}"
        );
    }
    let tags = base(vec![member(N1, Document::new().with("tags", Document::new().with("dc", 1)))]);
    let r = reconfig(&mut m, Value::Document(tags));
    assert_eq!(errmsg(&r), "tags.dc field has non-string value of type int");
    let half = base(vec![member(N1, Document::new().with("votes", 0.5))]);
    let r = reconfig(&mut m, Value::Document(half));
    assert!(errmsg(&r).ends_with("priority must be 0 when non-voting (votes:0)"), "{r:?}");
    let flag = base(vec![member(N1, Document::new().with("hidden", 1).with("priority", 0))]);
    assert_ne!(code_or_ok(&reconfig(&mut m, Value::Document(flag))), 14);
    // `replSetResizeOplog` below the current window changes nothing; a wrong type is refused.
    let resize = |m: &mut Mem, h: Value| {
        admin(m, Document::new().with("replSetResizeOplog", 1).with("minRetentionHours", h))
    };
    assert_eq!(resize(&mut m, Value::Double(0.5)).get("ok"), Some(&Value::Double(1.0)));
    assert_eq!(resize(&mut m, Value::Int64(0)).get("ok"), Some(&Value::Double(1.0)));
    assert_eq!(code(&resize(&mut m, Value::from("x"))), 14);
    // Every other member held back: no one to step down to.
    for h in [N2, N3] {
        let hold = Hold { maintenance: 1, ..Hold::default() };
        teseladb_wire::commands::presentation::set_hold(&mut m, h, hold).unwrap();
    }
    assert_eq!(code(&admin(&mut m, Document::new().with("replSetStepDown", 1.5))), 262);
    let r = admin(&mut m, Document::new().with("replSetGetStatus", 1));
    assert_eq!(field(&r, "members.1.stateStr"), &Value::from("RECOVERING"));
}

/// `local.startup_log` and `local.system.replset` are read as collections and refuse writes;
/// `listCollections` of `local` lists them with their options.
#[test]
fn the_served_collections_of_local() {
    let mut m = node();
    let list = |m: &mut Mem, name_only: bool| {
        let r =
            m.run("local", Document::new().with("listCollections", 1).with("nameOnly", name_only));
        match field(&r, "cursor.firstBatch") {
            Value::Array(a) => a.clone(),
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(list(&mut m, true).len(), 2, "no system.replset as the reference router");
    replica(&mut m, N2);
    let full = list(&mut m, false);
    assert_eq!(full.len(), 3, "{full:?}");
    let Value::Document(oplog) = &full[0] else { panic!() };
    assert_eq!(field(oplog, "options.capped"), &Value::Boolean(true));
    let r = m.run("local", Document::new().with("find", "startup_log"));
    assert!(matches!(field(&r, "cursor.firstBatch.0.startTimeLocal"), Value::String(_)), "{r:?}");
    let r = m.run("local", Document::new().with("find", "system.replset"));
    assert_eq!(field(&r, "cursor.firstBatch.0._id"), &Value::from("rs0"));
    let ins = Document::new()
        .with("insert", "startup_log")
        .with("documents", vec![Value::Document(Document::new())]);
    let r = m.run("local", ins);
    assert_eq!(field(&r, "writeErrors.0.code"), &Value::Int32(20), "{r:?}");
    assert_eq!(
        teseladb_wire::commands::local_db::local_time_text(86_400_000 + 3_723_004),
        "Fri Jan  2 01:02:03.004"
    );
}

/// More of the reference server 9.0.4's answers: a freeze for an infinite time unfreezes, a
/// `replSetReconfig` whose `_id` is not a string goes on to its other checks, `replSetSyncFrom ""`
/// cannot parse, and a fractional priority is shown as a double.
#[test]
fn more_member_answers() {
    let mut m = node();
    replica(&mut m, N2);
    let r = admin(&mut m, Document::new().with("replSetFreeze", f64::INFINITY));
    assert_eq!(r.get("info"), Some(&Value::from("unfreezing")), "{r:?}");
    let r = admin(&mut m, Document::new().with("replSetSyncFrom", ""));
    assert_eq!(code(&r), 9);
    let conf = Document::new().with("_id", 5).with("version", 9);
    let r = admin(&mut m, Document::new().with("replSetReconfig", conf));
    assert_eq!(code(&r), 2, "{r:?}");
    replica(&mut m, N1);
    let half = Document::new().with("_id", "rs0").with("version", 9).with(
        "members",
        vec![Value::Document(
            Document::new().with("_id", 0).with("host", N1).with("priority", 0.5),
        )],
    );
    let r = admin(&mut m, Document::new().with("replSetReconfig", half));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = admin(&mut m, Document::new().with("replSetGetStatus", 1));
    assert_eq!(
        field(&r, "electionCandidateMetrics.priorityAtElection"),
        &Value::Double(0.5),
        "{r:?}"
    );
}

/// `replSetReconfig`'s `version` as the reference server 9.0.4 reads it (measured on 03-11-2026,
/// `rs3.js`): missing, not a number, not whole or not above 0 is 93 with the reference server's
/// message; a whole double, a decimal and a long past 32 bits are taken, and the long is kept
/// whole.
#[test]
fn the_version_of_a_reconfig() {
    let mut m = node();
    replica(&mut m, N1);
    let members =
        vec![Value::Document(Document::new().with("_id", 0).with("host", N1).with("priority", 2))];
    let conf = |v: Option<Value>| {
        let d = Document::new().with("_id", "rs0");
        let d = match v {
            Some(v) => d.with("version", v),
            None => d,
        };
        d.with("members", members.clone())
    };
    let reconfig = |m: &mut Mem, d: Document| admin(m, Document::new().with("replSetReconfig", d));
    let errmsg = |r: &Document| match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(s))) => (*c, s.clone()),
        _ => panic!("{r:?}"),
    };
    let missing = "BSON field 'ReplSetConfig.version' is missing but a required field";
    assert_eq!(errmsg(&reconfig(&mut m, conf(None))), (93, missing.into()));
    for (v, t) in
        [(Value::from("x"), "string"), (Value::Null, "null"), (Value::Boolean(true), "bool")]
    {
        let expected = format!("Expected a number, got value of type {t} for field version");
        assert_eq!(errmsg(&reconfig(&mut m, conf(Some(v)))), (93, expected));
    }
    let half = "Expected field \"version\" to have a value exactly representable as a 64-bit integer, but found version: 4.5";
    assert_eq!(errmsg(&reconfig(&mut m, conf(Some(Value::Double(4.5))))), (93, half.into()));
    let huge = "Expected field \"version\" to have a value exactly representable as a 64-bit integer, but found version: 1e+30";
    assert_eq!(errmsg(&reconfig(&mut m, conf(Some(Value::Double(1e30))))), (93, huge.into()));
    for (v, n) in [(Value::Int32(0), 0), (Value::Double(-2.0), -2), (Value::Int64(-3), -3)] {
        let expected = format!("BSON field 'version' value must be > 0, actual value '{n}'");
        assert_eq!(errmsg(&reconfig(&mut m, conf(Some(v)))), (93, expected));
    }
    let ok = |r: &Document| assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    ok(&reconfig(&mut m, conf(Some(Value::Double(20.0)))));
    let r = admin(&mut m, Document::new().with("replSetGetConfig", 1));
    assert_eq!(field(&r, "config.version"), &Value::Int32(20));
    let d = teseladb_bson::Decimal128::from_parts(false, 21, 0).expect("a decimal");
    ok(&reconfig(&mut m, conf(Some(Value::Decimal128(d)))));
    ok(&reconfig(&mut m, conf(Some(Value::Int64(5_000_000_000)))));
    let r = admin(&mut m, Document::new().with("replSetGetConfig", 1));
    assert_eq!(field(&r, "config.version"), &Value::Int64(5_000_000_000));
    let r = admin(&mut m, Document::new().with("replSetGetStatus", 1));
    assert_eq!(field(&r, "members.0.configVersion"), &Value::Int64(5_000_000_000));
    let r = reconfig(&mut m, conf(Some(Value::Int32(22))));
    assert_eq!(
        errmsg(&r),
        (
            103,
            "New replica set configuration version and term must be greater than old, but {version: 22, term: 2} is not greater than {version: 5000000000, term: 2} for replica set rs0".into()
        )
    );
}

/// The members' `priority`, `votes` and `hidden` as decimals, and `hidden` as a number, as the
/// reference server 9.0.4 takes them (measured on 03-11-2026): a decimal is a number, and a numeric
/// `hidden` is shown as the flag it means.
#[test]
fn decimal_and_numeric_member_attributes() {
    let mut m = node();
    replica(&mut m, N1);
    let dec = |c: u128, e: i32| {
        Value::Decimal128(teseladb_bson::Decimal128::from_parts(false, c, e).expect("a decimal"))
    };
    let member = |h: &str, extra: Document| {
        let mut d = Document::new().with("_id", 0).with("host", h);
        for (k, v) in extra.iter() {
            d.push(k, v.clone());
        }
        Value::Document(d)
    };
    let members = vec![
        member(N1, Document::new().with("priority", dec(25, -1)).with("votes", dec(1, 0))),
        member(N2, Document::new().with("priority", 0).with("hidden", 1)),
        member(N3, Document::new().with("hidden", dec(0, 0))),
    ];
    let conf = Document::new().with("_id", "rs0").with("version", 40).with("members", members);
    let r = admin(&mut m, Document::new().with("replSetReconfig", conf));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = admin(&mut m, Document::new().with("replSetGetConfig", 1));
    assert_eq!(field(&r, "config.members.0.priority"), &Value::Double(2.5));
    assert_eq!(field(&r, "config.members.0.votes"), &Value::Int32(1));
    assert_eq!(field(&r, "config.members.1.hidden"), &Value::Boolean(true));
    assert_eq!(field(&r, "config.members.2.hidden"), &Value::Boolean(false));
}

/// More of `replSetReconfig`'s refusals and answers as the reference server 9.0.4 gives them
/// (measured on 03-11-2026): a member without a host or with one that is not a string, a priority
/// that is a boolean, a version far below 64 bits; a member without votes and priority is taken.
#[test]
fn reconfig_members_hosts_and_flags() {
    let mut m = node();
    replica(&mut m, N1);
    let reconfig = |m: &mut Mem, members: Vec<Value>, version: Value| {
        let conf =
            Document::new().with("_id", "rs0").with("version", version).with("members", members);
        admin(m, Document::new().with("replSetReconfig", conf))
    };
    let errmsg = |r: &Document| match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(s))) => (*c, s.clone()),
        _ => panic!("{r:?}"),
    };
    let one = |d: Document| vec![Value::Document(d)];
    let no_host = Document::new().with("_id", 0);
    let r = reconfig(&mut m, one(no_host), Value::Int32(50));
    assert_eq!(
        errmsg(&r),
        (
            93,
            "member: { _id: 0 } :: caused by :: BSON field 'MemberConfig.host' is missing but a required field".into()
        )
    );
    let int_host = Document::new().with("_id", 0).with("host", 5);
    let r = reconfig(&mut m, one(int_host), Value::Int32(50));
    assert_eq!(
        errmsg(&r),
        (
            93,
            "member: { _id: 0, host: 5 } :: caused by :: BSON field 'MemberConfig.host' is the wrong type 'int', expected type 'string'".into()
        )
    );
    let flag = Document::new().with("_id", 0).with("host", N1).with("priority", true);
    let r = reconfig(&mut m, one(flag), Value::Int32(50));
    assert!(
        errmsg(&r).1.ends_with(
            "BSON field 'MemberConfig.priority' is the wrong type 'bool', expected types '[long, double, decimal, int]'"
        ),
        "{r:?}"
    );
    let low = "Expected field \"version\" to have a value exactly representable as a 64-bit integer, but found version: -1e+30";
    let plain = Document::new().with("_id", 0).with("host", N1);
    assert_eq!(
        errmsg(&reconfig(&mut m, one(plain.clone()), Value::Double(-1e30))),
        (93, low.into())
    );
    let silent =
        Document::new().with("_id", 1).with("host", N2).with("votes", 0).with("priority", 0);
    let r =
        reconfig(&mut m, vec![Value::Document(plain), Value::Document(silent)], Value::Int32(50));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

/// A node that is not a shard of the cluster is no member; `replSetStepDown` takes a long; while
/// the designated primary's node is out of the cluster (before Group 0 designates another),
/// `replSetSyncFrom` names no previous target and `replSetGetStatus` has no election;
/// `replSetResizeOplog` without `minRetentionHours` changes nothing.
#[test]
fn members_without_a_primary_and_a_node_that_is_none() {
    let mut m = node();
    replica(&mut m, N3);
    m.shards.pop();
    let r = admin(&mut m, Document::new().with("replSetSyncFrom", N2));
    assert_eq!(r.get("syncFromRequested"), Some(&Value::from(N2)), "{r:?}");
    assert!(r.get("prevSyncTarget").is_none(), "{r:?}");
    let r = admin(&mut m, Document::new().with("replSetGetStatus", 1));
    assert!(r.get("electionCandidateMetrics").is_none(), "{r:?}");
    assert!(r.get("electionParticipantMetrics").is_none(), "{r:?}");
    let r = admin(&mut m, Document::new().with("replSetResizeOplog", 1).with("size", 2000.0));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    m.shards.push("node3".into());
    replica(&mut m, N1);
    let r = admin(&mut m, Document::new().with("replSetStepDown", Value::Int64(5)));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let mut other = Mem::new();
    other.shards = vec!["node2".into(), "node3".into()];
    replica(&mut other, N2);
    let r = admin(&mut other, Document::new().with("replSetGetConfig", 1));
    assert_eq!(code(&r), 20, "{r:?}");
}
