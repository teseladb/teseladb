//! The replica set presentation over the in-memory node of `common` (018 FR-007, FR-008, T010,
//! T016): `topologyPresentation`, the `replSet*` commands, `local.system.replset` and what a
//! member refuses, with the replies of a three-member replica set of RefDb 9.0.4 (measured on
//! 02-11-2026).

mod common;

use std::sync::Arc;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_topology::replset::{LastWrite, OpTime, Timestamp};
use teseladb_wire::commands::presentation::replica_set;
use teseladb_wire::connection::{Connection, Request};
use teseladb_wire::dispatch::{Action, ClusterTime, NodeInfo, Topology, dispatch_with};
use teseladb_wire::hello::ReplicaView;
use teseladb_wire::msg::OpMsg;

const N1: &str = "node1.cluster:27018";
const N2: &str = "node2.cluster:27018";

fn node() -> Mem {
    let mut m = Mem::new();
    m.shards = vec!["node1".into(), "node2".into(), "node3".into()];
    m
}

fn present(m: &mut Mem, value: Document) -> Document {
    let cmd = Document::new()
        .with("setClusterParameter", Document::new().with("topologyPresentation", value));
    m.run("admin", cmd)
}

fn replica(m: &mut Mem, primary: &str) {
    let v =
        Document::new().with("mode", "replicaset").with("setName", "rs0").with("primary", primary);
    let r = present(m, v);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

fn code(r: &Document) -> (i32, String) {
    match (r.get("code"), r.get("errmsg")) {
        (Some(Value::Int32(c)), Some(Value::String(s))) => (*c, s.clone()),
        _ => panic!("{r:?}"),
    }
}

fn admin(m: &mut Mem, cmd: Document) -> Document {
    let mut d = doc(&[]);
    for (k, v) in cmd.iter() {
        d.push(k, v.clone());
    }
    m.run("admin", d)
}

/// The node answers as a `refrouter` until the presentation changes; then `replSetGetConfig` and
/// `local.system.replset` give the configuration over the cluster's nodes.
#[test]
fn configuration_over_the_nodes() {
    let mut m = node();
    let r = admin(&mut m, Document::new().with("replSetGetConfig", 1));
    assert_eq!(code(&r), (59, "no such cmd: replSetGetConfig".into()));
    let r = admin(&mut m, Document::new().with("shardingState", 1));
    assert_eq!(code(&r), (59, "no such cmd: shardingState".into()));
    replica(&mut m, N2);
    let r = admin(&mut m, Document::new().with("replSetGetConfig", 1));
    assert_eq!(field(&r, "config._id"), &Value::from("rs0"));
    assert_eq!(field(&r, "config.version"), &Value::Int32(2));
    assert_eq!(field(&r, "config.term"), &Value::Int32(2));
    assert_eq!(field(&r, "config.members.1.host"), &Value::from(N2));
    assert_eq!(field(&r, "config.members.1.priority"), &Value::Int32(1));
    assert_eq!(field(&r, "config.members.1.secondaryDelaySecs"), &Value::Int64(0));
    assert_eq!(field(&r, "config.protocolVersion"), &Value::Int64(1));
    let f = Document::new().with("find", "system.replset");
    let r = m.run("local", f);
    assert_eq!(field(&r, "cursor.firstBatch.0._id"), &Value::from("rs0"));
    // The oplog's term follows the presentation's.
    let r = admin(&mut m, Document::new().with("appendOplogNote", 1).with("data", Document::new()));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = m.run("local", Document::new().with("find", "oplog.rs"));
    assert_eq!(field(&r, "cursor.firstBatch.0.t"), &Value::Int64(2));
}

/// `replSetGetStatus` of a secondary: its state, the primary's and the counts of votes.
#[test]
fn status() {
    let mut m = node();
    replica(&mut m, N2);
    let r = admin(&mut m, Document::new().with("replSetGetStatus", 1));
    assert_eq!(r.get("set"), Some(&Value::from("rs0")));
    assert_eq!(r.get("myState"), Some(&Value::Int32(2)));
    assert_eq!(r.get("term"), Some(&Value::Int64(2)));
    assert_eq!(r.get("majorityVoteCount"), Some(&Value::Int32(2)));
    assert_eq!(field(&r, "members.0.self"), &Value::Boolean(true));
    assert_eq!(field(&r, "members.1.stateStr"), &Value::from("PRIMARY"));
}

/// The commands of a secondary and of the primary, as RefDb 9.0.4 answers them.
#[test]
fn member_commands() {
    let mut m = node();
    replica(&mut m, N2);
    let one = |name: &str| Document::new().with(name, 1);
    assert_eq!(
        code(&admin(&mut m, one("replSetStepDown"))),
        (10107, "not primary so can't step down".into())
    );
    let r = admin(&mut m, Document::new().with("replSetFreeze", 0));
    assert_eq!(r.get("info"), Some(&Value::from("unfreezing")));
    let r = admin(&mut m, Document::new().with("replSetMaintenance", false));
    assert_eq!(code(&r), (96, "already out of maintenance mode".into()));
    assert_eq!(code(&admin(&mut m, one("replSetInitiate"))), (23, "already initialized".into()));
    let r = admin(&mut m, Document::new().with("replSetSyncFrom", N1));
    assert_eq!(code(&r), (72, "I cannot sync from myself".into()));
    let r = admin(&mut m, Document::new().with("replSetSyncFrom", "node3.cluster:27018"));
    assert_eq!(r.get("prevSyncTarget"), Some(&Value::from(N2)));
    assert_eq!(
        code(&admin(&mut m, one("replSetAbortPrimaryCatchUp"))),
        (20, "The node is not in catch-up mode.".into())
    );
    // On the primary.
    replica(&mut m, N1);
    let r = admin(&mut m, Document::new().with("replSetFreeze", 30));
    assert_eq!(code(&r).0, 95);
    assert_eq!(
        code(&admin(&mut m, Document::new().with("replSetMaintenance", true))),
        (95, "primaries can't modify maintenance mode".into())
    );
    assert_eq!(
        code(&admin(&mut m, Document::new().with("replSetSyncFrom", N2))),
        (95, "primaries don't sync".into())
    );
    let before = replica_set(&mut m).expect("read").expect("a set").0;
    let r = admin(&mut m, one("replSetStepDown"));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let after = replica_set(&mut m).expect("read").expect("a set").0;
    assert_eq!(after.primary(), Some(N2));
    assert_eq!((after.term(), after.set_version()), (before.term() + 1, before.set_version()));
    let r =
        admin(&mut m, Document::new().with("replSetResizeOplog", 1).with("minRetentionHours", 48));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let r = admin(&mut m, Document::new().with("getClusterParameter", "teselaHistoryRetention"));
    assert_eq!(field(&r, "clusterParameters.0.seconds"), &Value::Int64(48 * 3600));
}

fn members(prio2: Document) -> Value {
    let m = |id: i32, host: &str| Document::new().with("_id", id).with("host", host);
    Value::Array(vec![
        Value::Document(m(0, N1)),
        Value::Document(prio2),
        Value::Document(
            m(2, "node3.cluster:27018")
                .with("priority", 0)
                .with("tags", Document::new().with("dc", "eu")),
        ),
    ])
}

fn reconfig(m: &mut Mem, id: &str, version: i32, second: Document) -> Document {
    let conf =
        Document::new().with("_id", id).with("version", version).with("members", members(second));
    admin(m, Document::new().with("replSetReconfig", conf))
}

/// `replSetReconfig` with the rules and messages of RefDb 9.0.4.
#[test]
fn reconfig_rules() {
    let mut m = node();
    replica(&mut m, N1);
    let second = Document::new().with("_id", 1).with("host", N2);
    let bad = [
        (
            second.clone().with("priority", 1001),
            (
                93,
                "member: { _id: 1, host: \"node2.cluster:27018\", priority: 1001 } :: caused by :: BSON field 'priority' value must be <= 1000, actual value '1001'",
            ),
        ),
        (
            second.clone().with("votes", 0),
            (
                93,
                "member: { _id: 1, host: \"node2.cluster:27018\", votes: 0 } :: caused by :: priority must be 0 when non-voting (votes:0)",
            ),
        ),
        (
            second.clone().with("hidden", true),
            (
                93,
                "member: { _id: 1, host: \"node2.cluster:27018\", hidden: true } :: caused by :: priority must be 0 when hidden=true",
            ),
        ),
        (
            second.clone().with("votes", 2),
            (
                93,
                "member: { _id: 1, host: \"node2.cluster:27018\", votes: 2 } :: caused by :: BSON field 'votes' value must be <= 1, actual value '2'",
            ),
        ),
        (
            Document::new().with("_id", 1).with("host", N1),
            (
                103,
                "Found two member configurations with same host field, members.0.host == members.1.host == node1.cluster:27018",
            ),
        ),
    ];
    for (member, (c, msg)) in bad {
        assert_eq!(code(&reconfig(&mut m, "rs0", 3, member)), (c, msg.to_owned()));
    }
    assert_eq!(
        code(&reconfig(&mut m, "rs0", 2, second.clone())),
        (103, "New replica set configuration version and term must be greater than old, but {version: 2, term: 2} is not greater than {version: 2, term: 2} for replica set rs0".into())
    );
    assert_eq!(
        code(&reconfig(&mut m, "rsX", 3, second.clone())),
        (93, "Rejecting reconfig where new config set name differs from command line set name, new config set name: rsX, old config set name: rs0".into())
    );
    let r = reconfig(&mut m, "rs0", 3, second.with("priority", 3));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let rs = replica_set(&mut m).expect("read").expect("a set").0;
    assert_eq!(rs.set_version(), 3);
    assert_eq!(rs.members()[2].tags(), [("dc".to_owned(), "eu".to_owned())]);
}

/// `setClusterParameter` checks the presentation: a known mode, a name, an electable primary.
#[test]
fn presentation_parameter() {
    let mut m = node();
    let r = present(&mut m, Document::new().with("mode", "x"));
    assert_eq!(code(&r).0, 2);
    let r = present(&mut m, Document::new().with("mode", "replicaset"));
    assert_eq!(code(&r).0, 2, "{r:?}");
    let r = present(
        &mut m,
        Document::new()
            .with("mode", "replicaset")
            .with("setName", "rs0")
            .with("primary", "nowhere:1"),
    );
    assert_eq!(code(&r).0, 2, "{r:?}");
    let r = present(&mut m, Document::new().with("mode", "replicaset").with("setName", "rs0"));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let rs = replica_set(&mut m).expect("read").expect("a set").0;
    assert_eq!(rs.primary(), Some(N1), "the first electable member");
    let r = present(&mut m, Document::new().with("mode", "refrouter"));
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert!(replica_set(&mut m).expect("read").is_none());
    let r = present(&mut m, Document::new().with("other", 1));
    assert_eq!(code(&r).0, 40415, "{r:?}");
}

fn dispatched(m: &mut Mem, db: &str, cmd: Document, view: Option<Arc<ReplicaView>>) -> Document {
    let info = NodeInfo {
        topology: Topology { process_id: [1; 12], counter: 9 },
        cluster_time: ClusterTime { time: 1, increment: 1 },
        now_ms: 0,
        keys: None,
        replica_set: view,
    };
    let bytes = OpMsg::encode(1, 0, 0, &cmd.with("$db", db).to_bytes().expect("a command"));
    let mut conn = Connection::new(9);
    conn.receive(&bytes);
    let req: Request = conn.next_request().expect("a request").expect("a request");
    let shared = std::mem::take(&mut m.shared);
    let r = dispatch_with(&mut conn, &req, &info, Some((&mut *m, &shared)));
    m.shared = shared;
    match r {
        Action::Reply(d) => d,
        other => panic!("{other:?}"),
    }
}

/// A secondary refuses writes with `NotWritablePrimary` and its `topologyVersion`; a member
/// has no router commands; `hello` answers as the member.
#[test]
fn a_member_answers_as_one() {
    let mut m = node();
    replica(&mut m, N2);
    let (set, _) = replica_set(&mut m).expect("read").expect("a set");
    let me = set.member(N1).expect("member");
    let at = OpTime { ts: Timestamp { time: 5, increment: 1 }, term: set.term() };
    let view =
        Arc::new(ReplicaView { set, me, last: LastWrite { op_time: at, majority_op_time: at } });
    let ins = Document::new()
        .with("insert", "c")
        .with("documents", vec![Value::Document(Document::new())]);
    let r = dispatched(&mut m, "v", ins, Some(view.clone()));
    assert_eq!(code(&r), (10107, "not primary".into()));
    assert_eq!(field(&r, "topologyVersion.counter"), &Value::Int64(9));
    let r = dispatched(&mut m, "admin", Document::new().with("listShards", 1), Some(view.clone()));
    assert_eq!(code(&r), (59, "no such command: 'listShards'".into()));
    let r = dispatched(
        &mut m,
        "admin",
        Document::new().with("shardConnPoolStats", 1),
        Some(view.clone()),
    );
    assert_eq!(code(&r), (59, "no such command: 'shardConnPoolStats'".into()));
    let r =
        dispatched(&mut m, "admin", Document::new().with("shardingState", 1), Some(view.clone()));
    assert_eq!(r.get("enabled"), Some(&Value::Boolean(false)), "{r:?}");
    let r = dispatched(&mut m, "admin", Document::new().with("hello", 1), Some(view));
    assert_eq!(r.get("setName"), Some(&Value::from("rs0")));
    assert_eq!(r.get("secondary"), Some(&Value::Boolean(true)));
    assert_eq!(r.get("primary"), Some(&Value::from(N2)));
    assert_eq!(r.get("me"), Some(&Value::from(N1)));
    assert!(r.get("msg").is_none());
    // On the primary, a config server's command is refused as on a `refnode` that is not one.
    replica(&mut m, N1);
    let (set, _) = replica_set(&mut m).expect("read").expect("a set");
    let me = set.member(N1).expect("member");
    let view =
        Arc::new(ReplicaView { set, me, last: LastWrite { op_time: at, majority_op_time: at } });
    let r =
        dispatched(&mut m, "admin", Document::new().with("_configsvrAddShard", "x"), Some(view));
    assert_eq!(code(&r), (20, "_configsvrAddShard can only be run on config servers".into()));
}

fn config_names(m: &mut Mem) -> Vec<String> {
    let r =
        m.run("config", doc(&[("listCollections", Value::Int32(1)), ("nameOnly", true.into())]));
    match field(&r, "cursor.firstBatch") {
        Value::Array(a) => a
            .iter()
            .map(|d| match d {
                Value::Document(d) => match d.get("name") {
                    Some(Value::String(n)) => n.clone(),
                    other => panic!("{other:?}"),
                },
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

/// A member of a replica set has no `config.changelog`, `config.refrouter` nor `config.settings`
/// (measured on a replica set of `ref:9.0.4`, 02-11-2026): `refdump` of the whole member
/// does not copy them, and `refrestore` into a `refnode` does not meet the validator of
/// `config.settings`, which a `refnode` refuses on a collection of `config` (T013).
#[test]
fn config_lists_what_a_member_has() {
    let mut m = node();
    let refrouter = config_names(&mut m);
    for name in ["changelog", "refrouter", "settings"] {
        assert!(refrouter.iter().any(|n| n == name), "{refrouter:?}");
    }
    replica(&mut m, N2);
    assert_eq!(
        config_names(&mut m),
        ["chunks", "collections", "databases", "shards", "tags", "version"]
    );
}

/// The collections of `config` that show the cluster are empty on a member and a member drops
/// them (measured on a replica set of `ref:9.0.4`, 02-11-2026): `refdump` copies nothing of
/// the cluster's topology into a `refnode` and `refrestore --drop` of a member's dump into
/// TeselaDB goes through (T013), which drops and creates them again.
#[test]
fn config_of_a_member_is_empty_and_droppable() {
    let mut m = node();
    let find = |m: &mut Mem, coll: &str| {
        let r = m.run("config", doc(&[("find", coll.into())]));
        match field(&r, "cursor.firstBatch") {
            Value::Array(a) => a.len(),
            other => panic!("{other:?}"),
        }
    };
    let drop = |m: &mut Mem, coll: &str| m.run("config", doc(&[("drop", coll.into())]));
    let create = |m: &mut Mem, coll: &str| m.run("config", doc(&[("create", coll.into())]));
    assert!(find(&mut m, "shards") > 0);
    assert_eq!(code(&drop(&mut m, "shards")).0, 20);
    assert_eq!(code(&create(&mut m, "chunks")).0, 20);
    replica(&mut m, N2);
    for coll in ["chunks", "collections", "databases", "shards", "tags", "version"] {
        assert_eq!(find(&mut m, coll), 0, "{coll}");
        let r = drop(&mut m, coll);
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
        let r = create(&mut m, coll);
        assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    }
    assert_eq!(code(&drop(&mut m, "settings")).0, 20);
    assert!(replica_set(&mut m).expect("the presentation").is_some());
}
