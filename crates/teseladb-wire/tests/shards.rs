//! 018 T008 and T025: `removeShard` and the draining commands (FR-005, FR-018) over the in-memory
//! node of `common`: a shard is a node and its removal is its drain, ordered to Group 0; replies
//! of a `refrouter` 9.0.4 of two shards (measured on 02-11-2026).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_meta::command::{Group0Command, NodeState};
use teseladb_router::table::RouteEntry;
use teseladb_types::{GroupId, NodeId};

fn json(text: &str) -> Document {
    extjson::parse(text).unwrap_or_else(|e| panic!("{text}: {e:?}"))
}

fn admin(m: &mut Mem, cmd: &str) -> Document {
    m.run("admin", json(cmd))
}

fn ok(r: &Document) {
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
}

fn fails(r: &Document, code: i32, msg: &str) {
    assert_eq!(field(r, "code"), &Value::Int32(code), "{r:?}");
    assert_eq!(field(r, "errmsg"), &Value::from(msg), "{r:?}");
}

/// Two shards, node 2 the primary of `newdb`, one range on both nodes.
fn cluster() -> Mem {
    let mut m = Mem::new();
    m.shards = vec!["node1".into(), "node2".into()];
    m.routes = vec![RouteEntry {
        group: GroupId(1),
        start: Vec::new(),
        end: Vec::new(),
        core: 0,
        voters: vec![NodeId(1), NodeId(2)],
        generation: 1,
        leader_hint: Some(NodeId(1)),
        moved_ms: 0,
        replica_epoch: 1,
    }];
    ok(&admin(&mut m, r#"{"enableSharding":"newdb","primaryShard":"node2"}"#));
    m
}

const NOTE: &str = "you need to call moveCollection for collectionsToMove and afterwards movePrimary for the dbsToMove";

#[test]
fn remove_shard_starts_the_drain_and_then_reports_it() {
    let mut m = cluster();
    let r = admin(&mut m, r#"{"removeShard":"node2","bogus":1}"#);
    let started = Document::new()
        .with("note", NOTE)
        .with("dbsToMove", vec![Value::from("newdb")])
        .with("collectionsToMove", Value::Array(Vec::new()))
        .with("shard", "node2")
        .with("msg", "draining started successfully")
        .with("state", "started")
        .with("ok", 1.0);
    assert_eq!(r, started);
    let drain = Group0Command::SetNodeState { node: NodeId(2), state: NodeState::Draining };
    assert_eq!(m.range_orders, [drain]);
    // The shard list says draining from then on (Group 0 applied the order).
    m.draining = vec!["node2".into()];
    let r = admin(&mut m, r#"{"removeShard":"node2"}"#);
    assert_eq!(field(&r, "state"), &Value::from("ongoing"));
    assert_eq!(field(&r, "msg"), &Value::from("draining ongoing"));
    let Value::Document(rest) = field(&r, "remaining") else { panic!("{r:?}") };
    assert_eq!(rest.get("chunks"), Some(&Value::Int64(1)));
    assert_eq!(rest.get("dbs"), Some(&Value::Int64(1)));
    assert_eq!(rest.get("jumboChunks"), Some(&Value::Int64(0)));
    let status = admin(&mut m, r#"{"shardDrainingStatus":"node2"}"#);
    assert_eq!(status.get("remaining"), r.get("remaining"));
    assert_eq!(field(&status, "note"), &Value::from(NOTE));
    fails(
        &admin(&mut m, r#"{"commitShardRemoval":"node2"}"#),
        20,
        "The shard node2 isn't completely drained. Please use the shardDrainingStatus command to \
         check the draining status.",
    );
    // The other shard cannot go too.
    fails(
        &admin(&mut m, r#"{"removeShard":"node1"}"#),
        20,
        "Operation not allowed because it would drain the last shard",
    );
    fails(
        &admin(&mut m, r#"{"startShardDraining":"node1"}"#),
        20,
        "Operation not allowed because it would drain the last shard",
    );
    // Drained: no replica and no database left; the balancer removes it.
    m.routes[0].voters = vec![NodeId(1)];
    ok(&admin(&mut m, r#"{"movePrimary":"newdb","to":"node1"}"#));
    let r = admin(&mut m, r#"{"shardDrainingStatus":"node2"}"#);
    assert_eq!(r.get("note"), None);
    ok(&admin(&mut m, r#"{"commitShardRemoval":"node2"}"#));
    ok(&admin(&mut m, r#"{"stopShardDraining":"node2"}"#));
    let active = Group0Command::SetNodeState { node: NodeId(2), state: NodeState::Active };
    assert_eq!(m.range_orders.last(), Some(&active));
}

#[test]
fn a_shard_that_does_not_exist() {
    let mut m = cluster();
    let r = admin(&mut m, r#"{"removeShard":"noSuchShard"}"#);
    let done = Document::new()
        .with("shard", "noSuchShard")
        .with("msg", "removeshard completed successfully")
        .with("state", "completed")
        .with("ok", 1.0);
    assert_eq!(r, done);
    for cmd in ["startShardDraining", "stopShardDraining", "shardDrainingStatus"] {
        fails(
            &admin(&mut m, &format!(r#"{{"{cmd}":"noSuchShard"}}"#)),
            70,
            "Shard noSuchShard not found",
        );
        fails(
            &admin(&mut m, &format!(r#"{{"{cmd}":5}}"#)),
            14,
            &format!("BSON field '{cmd}.{cmd}' is the wrong type 'int', expected type 'string'"),
        );
        fails(
            &m.run("z", json(&format!(r#"{{"{cmd}":"node2"}}"#))),
            13,
            &format!("{cmd} may only be run against the admin database."),
        );
    }
    ok(&admin(&mut m, r#"{"commitShardRemoval":"noSuchShard"}"#));
    fails(
        &admin(&mut m, r#"{"removeShard":5}"#),
        14,
        "BSON field 'removeShard.removeShard' is the wrong type 'int', expected type 'string'",
    );
    fails(
        &m.run("z", json(r#"{"removeShard":"node2"}"#)),
        13,
        "removeShard may only be run against the admin database.",
    );
    // An active shard.
    fails(
        &admin(&mut m, r#"{"shardDrainingStatus":"node2"}"#),
        20,
        "The shard node2 isn't draining",
    );
    fails(
        &admin(&mut m, r#"{"commitShardRemoval":"node2"}"#),
        117,
        "Shard node2 is not currently draining",
    );
    ok(&admin(&mut m, r#"{"stopShardDraining":"node2"}"#));
    ok(&admin(&mut m, r#"{"startShardDraining":"node2","bogus":1}"#));
    m.draining = vec!["node2".into()];
    ok(&admin(&mut m, r#"{"startShardDraining":"node2"}"#));
    assert_eq!(m.range_orders.len(), 1);
}

#[test]
fn add_shard_names_a_node_of_the_cluster() {
    let mut m = cluster();
    let added = |r: &Document, name: &str| {
        assert_eq!(r, &doc(&[("shardAdded", Value::from(name)), ("ok", Value::Double(1.0))]));
    };
    added(&admin(&mut m, r#"{"addShard":"node1.cluster:27018"}"#), "node1");
    added(&admin(&mut m, r#"{"addShard":"node2/x:1,node2.cluster:27018","name":"n"}"#), "node2");
    let joins = |host: &str| {
        format!(
            "{host} is not a node of this cluster: a node joins the cluster with an enrollment \
             token (`teseladb admin token create`, then start it with `--join <addr> \
             --enroll-token <text>`)"
        )
    };
    let other = "other/node2.cluster:27018";
    fails(&admin(&mut m, &format!(r#"{{"addShard":"{other}"}}"#)), 96, &joins(other));
    fails(&admin(&mut m, r#"{"addShard":"not a host"}"#), 96, &joins("not a host"));
    fails(
        &admin(&mut m, r#"{"addShard":5}"#),
        14,
        "BSON field 'addShard.addShard' is the wrong type 'int', expected type 'string'",
    );
    fails(
        &m.run("z", json(r#"{"addShard":"node1.cluster:27018"}"#)),
        13,
        "addShard may only be run against the admin database.",
    );
}
