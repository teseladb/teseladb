//! `hello` in the replica set presentation and the designation of its primary (018 FR-007,
//! T010, T019). The expected replies are those of a three-member replica set of RefDb 9.0.4
//! (`ref:9.0`), field by field and in their order.

use teseladb_bson::{Document, Value};
use teseladb_topology::replset::{
    ConfigError, HelloContext, LastWrite, Limits, Member, OpTime, PresentationMode, ReplicaSet,
    Timestamp,
};

const H1: &str = "h1:27017";
const H2: &str = "h2:27017";
const H3: &str = "h3:27017";
const PROCESS: [u8; 12] = [7; 12];

/// The context of a plain `hello` on connection 29.
fn ctx() -> HelloContext {
    HelloContext {
        legacy: false,
        hello_ok: false,
        compressors: Vec::new(),
        process_id: PROCESS,
        counter: 6,
        now_ms: 1_793_358_981_915,
        connection_id: 29,
        limits: Limits {
            max_bson_object_size: 16_777_216,
            max_message_size: 48_000_000,
            max_write_batch_size: 100_000,
            logical_session_timeout_minutes: 30,
            min_wire_version: 0,
            max_wire_version: 29,
        },
    }
}

fn last_write() -> LastWrite {
    let at = OpTime { ts: Timestamp { time: 1_793_358_977, increment: 10 }, term: 1 };
    LastWrite { op_time: at, majority_op_time: at }
}

/// h1 and h2 electable, h3 with priority 0 and a tag (a passive member).
fn three() -> ReplicaSet {
    let members = vec![
        Member::new(H1),
        Member::new(H2),
        Member::new(H3).with_priority(0.0).with_tags(vec![("dc".into(), "eu".into())]),
    ];
    ReplicaSet::new("rs0", members, Some(H1), 1, 1).expect("valid configuration")
}

fn hello_with(rs: &ReplicaSet, me: &str, ctx: &HelloContext) -> Document {
    rs.hello(ctx, rs.member(me).expect("member"), &last_write())
}

fn hello(rs: &ReplicaSet, me: &str) -> Document {
    hello_with(rs, me, &ctx())
}

fn keys(d: &Document) -> Vec<&str> {
    d.iter().map(|(k, _)| k).collect()
}

fn last_write_doc() -> Document {
    let ts = Value::Timestamp { time: 1_793_358_977, increment: 10 };
    let op = Document::new().with("ts", ts).with("t", 1i64);
    Document::new()
        .with("opTime", op.clone())
        .with("lastWriteDate", Value::DateTime(1_793_358_977_000))
        .with("majorityOpTime", op)
        .with("majorityWriteDate", Value::DateTime(1_793_358_977_000))
}

#[test]
fn primary_answers_like_a_refnode_primary() {
    let d = hello(&three(), H1);
    let tv = Document::new().with("processId", Value::ObjectId(PROCESS)).with("counter", 6i64);
    let mut eid = [0u8; 12];
    eid[..4].copy_from_slice(&[0x7f, 0xff, 0xff, 0xff]);
    eid[11] = 1;
    let expected = Document::new()
        .with("topologyVersion", tv)
        .with("hosts", vec![Value::from(H1), Value::from(H2)])
        .with("passives", vec![Value::from(H3)])
        .with("setName", "rs0")
        .with("setVersion", 1)
        .with("isWritablePrimary", true)
        .with("secondary", false)
        .with("primary", H1)
        .with("me", H1)
        .with("electionId", Value::ObjectId(eid))
        .with("lastWrite", last_write_doc())
        .with("maxBsonObjectSize", 16_777_216)
        .with("maxMessageSizeBytes", 48_000_000)
        .with("maxWriteBatchSize", 100_000)
        .with("localTime", Value::DateTime(1_793_358_981_915))
        .with("logicalSessionTimeoutMinutes", 30)
        .with("connectionId", 29)
        .with("minWireVersion", 0)
        .with("maxWireVersion", 29)
        .with("readOnly", false)
        .with("ok", 1.0);
    assert_eq!(d, expected);
}

#[test]
fn passive_secondary_shows_its_attributes_and_no_election_id() {
    let d = hello(&three(), H3);
    assert_eq!(
        keys(&d),
        [
            "topologyVersion",
            "hosts",
            "passives",
            "setName",
            "setVersion",
            "isWritablePrimary",
            "secondary",
            "primary",
            "passive",
            "tags",
            "me",
            "lastWrite",
            "maxBsonObjectSize",
            "maxMessageSizeBytes",
            "maxWriteBatchSize",
            "localTime",
            "logicalSessionTimeoutMinutes",
            "connectionId",
            "minWireVersion",
            "maxWireVersion",
            "readOnly",
            "ok",
        ]
    );
    assert_eq!(d.get("isWritablePrimary"), Some(&Value::Boolean(false)));
    assert_eq!(d.get("secondary"), Some(&Value::Boolean(true)));
    assert_eq!(d.get("me"), Some(&Value::from(H3)));
    assert_eq!(d.get("tags"), Some(&Value::Document(Document::new().with("dc", "eu"))));
}

#[test]
fn hidden_member_is_left_out_of_hosts() {
    let members = vec![
        Member::new(H1),
        Member::new(H2).with_priority(2.0).with_tags(vec![("dc".into(), "us".into())]),
        Member::new(H3).with_priority(0.0).hidden(),
    ];
    let rs = ReplicaSet::new("rs0", members, Some(H2), 2, 2).expect("valid");
    let d = hello(&rs, H3);
    assert_eq!(d.get("hosts"), Some(&Value::Array(vec![Value::from(H1), Value::from(H2)])));
    assert!(d.get("passives").is_none());
    assert_eq!(d.get("passive"), Some(&Value::Boolean(true)));
    assert_eq!(d.get("hidden"), Some(&Value::Boolean(true)));
    let p = hello(&rs, H2);
    assert_eq!(p.get("tags"), Some(&Value::Document(Document::new().with("dc", "us"))));
    let Some(Value::ObjectId(eid)) = p.get("electionId") else { panic!("electionId") };
    assert_eq!(eid[11], 2);
}

#[test]
fn legacy_hello_with_hello_ok_and_compression() {
    let legacy =
        HelloContext { legacy: true, hello_ok: true, compressors: vec!["zstd", "snappy"], ..ctx() };
    let d = hello_with(&three(), H1, &legacy);
    let k = keys(&d);
    assert_eq!(k[..4], ["compression", "helloOk", "topologyVersion", "hosts"]);
    assert_eq!(d.get("ismaster"), Some(&Value::Boolean(true)));
    assert!(d.get("isWritablePrimary").is_none());
}

#[test]
fn without_primary_every_member_is_secondary() {
    let rs =
        ReplicaSet::new("rs0", vec![Member::new(H1), Member::new(H2)], None, 1, 1).expect("valid");
    let d = hello(&rs, H1);
    assert!(d.get("primary").is_none());
    assert!(d.get("electionId").is_none());
    assert_eq!(d.get("secondary"), Some(&Value::Boolean(true)));
}

#[test]
fn reassign_picks_the_live_member_with_the_highest_priority() {
    let members = vec![
        Member::new(H1),
        Member::new(H2),
        Member::new(H3).with_priority(3.0),
        Member::new("h4:27017").with_priority(0.0),
    ];
    let rs = ReplicaSet::new("rs0", members, Some(H1), 4, 7).expect("valid");
    assert!(rs.reassign(&|_| true).is_none(), "a live primary stays");
    let next = rs.reassign(&|h| h != H1).expect("a new primary");
    assert_eq!(next.primary(), Some(H3));
    assert_eq!((next.set_version(), next.term()), (5, 8));
    let next = rs.reassign(&|h| h != H1 && h != H3).expect("a new primary");
    assert_eq!(next.primary(), Some(H2));
    assert!(rs.reassign(&|h| h == "h4:27017").is_none(), "priority 0 is never designated");
    let tie = ReplicaSet::new(
        "rs0",
        vec![Member::new(H1), Member::new(H2), Member::new(H3)],
        Some(H1),
        1,
        1,
    )
    .expect("valid");
    let next = tie.reassign(&|h| h != H1).expect("a new primary");
    assert_eq!(next.primary(), Some(H2), "on a tie, the first in configuration order");
    let none = ReplicaSet::new("rs0", vec![Member::new(H1)], None, 1, 1).expect("valid");
    assert_eq!(
        none.reassign(&|_| true).and_then(|r| r.primary().map(str::to_owned)),
        Some(H1.into())
    );
}

#[test]
fn step_down_moves_to_another_member_and_changes_the_term_only() {
    let rs = three();
    let next = rs.step_down(&|_| true).expect("another electable member");
    assert_eq!(next.primary(), Some(H2));
    assert_eq!((next.set_version(), next.term()), (1, 2));
    assert!(rs.step_down(&|h| h == H1).is_none());
}

#[test]
fn designate_validates_the_member() {
    let rs = three();
    let next = rs.designate(H2).expect("electable");
    assert_eq!((next.primary(), next.set_version(), next.term()), (Some(H2), 2, 2));
    assert_eq!(rs.designate(H3).err(), Some(ConfigError::NotElectable(H3.into())));
    assert_eq!(rs.designate("x:1").err(), Some(ConfigError::UnknownMember("x:1".into())));
}

#[test]
fn invalid_configurations_are_refused() {
    let new = |members: Vec<Member>, primary: Option<&str>| {
        ReplicaSet::new("rs0", members, primary, 1, 1).err()
    };
    assert_eq!(
        ReplicaSet::new("", vec![Member::new(H1)], None, 1, 1).err(),
        Some(ConfigError::EmptySetName)
    );
    assert_eq!(new(vec![], None), Some(ConfigError::NoMembers));
    assert_eq!(
        new(vec![Member::new(H1), Member::new(H1)], None),
        Some(ConfigError::DuplicateHost(H1.into()))
    );
    assert_eq!(
        new(vec![Member::new(H1).hidden()], None),
        Some(ConfigError::HiddenWithPriority(H1.into()))
    );
    assert_eq!(
        new(vec![Member::new(H1).with_priority(1001.0)], None),
        Some(ConfigError::PriorityOutOfRange(H1.into()))
    );
    assert_eq!(
        new(vec![Member::new(H1).with_priority(-1.0)], None),
        Some(ConfigError::PriorityOutOfRange(H1.into()))
    );
    assert_eq!(
        new(vec![Member::new(H1).with_votes(2)], None),
        Some(ConfigError::InvalidVotes(H1.into()))
    );
    assert_eq!(
        new(vec![Member::new(H1).with_votes(0)], None),
        Some(ConfigError::NonVoterWithPriority(H1.into()))
    );
    assert_eq!(new(vec![Member::new(H1)], Some(H2)), Some(ConfigError::UnknownMember(H2.into())));
    let ok = vec![Member::new(H1), Member::new(H2).with_votes(0).with_priority(0.0)];
    assert!(new(ok, Some(H1)).is_none());
}

#[test]
fn presentation_modes_by_name() {
    for m in [PresentationMode::RefRouter, PresentationMode::ReplicaSet] {
        assert_eq!(PresentationMode::from_name(m.name()), Some(m));
    }
    assert_eq!(PresentationMode::default(), PresentationMode::RefRouter);
    assert_eq!(PresentationMode::from_name("standalone"), None);
}

#[test]
fn accessors() {
    let rs = three();
    assert_eq!(rs.set_name(), "rs0");
    assert_eq!(rs.members().len(), 3);
    assert_eq!(rs.members()[2].host(), H3);
    assert!(rs.member("x:1").is_none());
}

/// A member in maintenance mode answers `hello` with `secondary: false`, as a `RECOVERING`
/// member of RefDb 9.0.4 does (measured on 02-11-2026, `f1.js`); the primary never is, and a
/// member designated primary leaves it.
#[test]
fn a_member_in_maintenance_is_not_secondary() {
    let rs = three().with_recovering(&[H2, H1]);
    let b = rs.member(H2).unwrap();
    assert!(rs.is_recovering(b));
    assert_eq!(hello(&rs, H2).get("secondary"), Some(&Value::Boolean(false)));
    assert_eq!(hello(&rs, H2).get("isWritablePrimary"), Some(&Value::Boolean(false)));
    let a = rs.member(H1).unwrap();
    assert!(!rs.is_recovering(a), "the primary is never recovering");
    let next = rs.designate(H2).unwrap();
    assert!(!next.is_recovering(b));
    assert_eq!(three().with_recovering(&[]), three());
}
