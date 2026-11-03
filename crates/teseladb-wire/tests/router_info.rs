//! 018 T022: `getShardMap`, `getShardVersion`, `flushRouterConfig`, `checkMetadataConsistency`,
//! `appendOplogNote` and `transitionToDedicatedConfigServer` (FR-017) over the in-memory node of
//! `common`, with the replies of a `refrouter` 9.0.4 of two shards (measured on 02-11-2026).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
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

fn cluster() -> Mem {
    let mut m = Mem::new();
    m.shards = vec!["node1".into(), "node2".into()];
    m.routes = vec![RouteEntry {
        group: GroupId(1),
        start: Vec::new(),
        end: Vec::new(),
        core: 0,
        voters: vec![NodeId(1)],
        generation: 1,
        leader_hint: Some(NodeId(1)),
        moved_ms: 0,
        replica_epoch: 1,
    }];
    ok(&admin(&mut m, r#"{"shardCollection":"z.k","key":{"a":1}}"#));
    m
}

#[test]
fn get_shard_map_lists_the_shards_and_group_0_as_config() {
    let mut m = cluster();
    let r = admin(&mut m, r#"{"getShardMap":1,"bogus":1}"#);
    let (h1, h2) = ("node1.cluster:27018", "node2.cluster:27018");
    let both = format!("{h1},{h2}");
    let map = doc(&[
        ("node1", Value::from(h1)),
        ("node2", Value::from(h2)),
        ("config", Value::from(both.as_str())),
    ]);
    assert_eq!(field(&r, "map"), &Value::Document(map));
    let hosts = doc(&[(h1, Value::from("node1")), (h2, Value::from("node2"))]);
    assert_eq!(field(&r, "hosts"), &Value::Document(hosts));
    let Value::Document(conns) = field(&r, "connStrings") else { panic!("{r:?}") };
    assert_eq!(conns.get(&both), Some(&Value::from("config")));
    let Value::String(time) = field(&r, "timeInStore") else { panic!("{r:?}") };
    assert!(time.starts_with("{ forceReloadIncrement: 0, topologyTime: Timestamp("), "{time}");
    fails(
        &m.run("z", json(r#"{"getShardMap":1}"#)),
        13,
        "getShardMap may only be run against the admin database.",
    );
}

#[test]
fn get_shard_version_of_a_collection_and_of_a_database() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"split":"z.k","middle":{"a":10}}"#));
    let r = admin(&mut m, r#"{"getShardVersion":"z.k"}"#);
    assert_eq!(field(&r, "version"), &Value::Timestamp { time: 1, increment: 1 });
    assert!(matches!(field(&r, "versionEpoch"), Value::ObjectId(_)), "{r:?}");
    assert!(matches!(field(&r, "versionTimestamp"), Value::Timestamp { .. }), "{r:?}");
    assert_eq!(r.get("chunks"), None);
    let r = admin(&mut m, r#"{"getShardVersion":"z.k","fullMetadata":true}"#);
    let chunks = json(r#"{"c":[[{"a":{"$minKey":1}},{"a":10}],[{"a":10},{"a":{"$maxKey":1}}]]}"#);
    assert_eq!(r.get("chunks"), chunks.get("c"));
    ok(&m.run("z", json(r#"{"create":"u"}"#)));
    for ns in ["z.u", "z.nocoll"] {
        fails(
            &admin(&mut m, &format!(r#"{{"getShardVersion":"{ns}"}}"#)),
            26,
            &format!("Collection {ns} does not have a routing table."),
        );
    }
    fails(&admin(&mut m, r#"{"getShardVersion":"nodb.nocoll"}"#), 26, "database nodb not found");
    fails(&admin(&mut m, r#"{"getShardVersion":"nodb"}"#), 26, "database nodb not found");
    fails(&admin(&mut m, r#"{"getShardVersion":5}"#), 2, "namespace has invalid type int");
    fails(
        &m.run("z", json(r#"{"getShardVersion":"z.k"}"#)),
        13,
        "getShardVersion may only be run against the admin database.",
    );
    // A database: its primary and version, as `config.databases` has them.
    m.databases_docs = Some(vec![json(r#"{"_id":"z","primary":"node1","version":{"lastMod":1}}"#)]);
    let r = admin(&mut m, r#"{"getShardVersion":"z"}"#);
    assert_eq!(field(&r, "primaryShard"), &Value::from("node1"));
    assert_eq!(field(&r, "version"), &Value::Document(json(r#"{"lastMod":1}"#)));
}

#[test]
fn flush_router_config_and_check_metadata_consistency_have_nothing_to_do() {
    let mut m = cluster();
    for arg in ["1", r#""z.k""#, r#""z""#, "5"] {
        let r = admin(&mut m, &format!(r#"{{"flushRouterConfig":{arg}}}"#));
        assert_eq!(r, doc(&[("flushed", Value::Boolean(true)), ("ok", Value::Double(1.0))]));
    }
    fails(
        &m.run("z", json(r#"{"flushRouterConfig":1}"#)),
        13,
        "flushRouterConfig may only be run against the admin database.",
    );
    let cursor = |r: &Document| field(r, "cursor.ns").clone();
    let r = admin(&mut m, r#"{"checkMetadataConsistency":1,"bogus":1}"#);
    assert_eq!(cursor(&r), Value::from("admin.$cmd.aggregate"));
    assert_eq!(field(&r, "cursor.firstBatch"), &Value::Array(Vec::new()));
    assert_eq!(field(&r, "cursor.id"), &Value::Int64(0));
    let r = m.run("z", json(r#"{"checkMetadataConsistency":1.0}"#));
    assert_eq!(cursor(&r), Value::from("z.$cmd.aggregate"));
    let r = m.run("z", json(r#"{"checkMetadataConsistency":"k"}"#));
    assert_eq!(cursor(&r), Value::from("z.k"));
    let r = m.run("z", json(r#"{"checkMetadataConsistency":{"$numberLong":"1"}}"#));
    assert_eq!(cursor(&r), Value::from("z.$cmd.aggregate"));
    fails(
        &admin(&mut m, r#"{"checkMetadataConsistency":5}"#),
        2,
        "Invalid command format: the 'checkMetadataConsistency' field must specify a collection \
         name or 1",
    );
}

#[test]
fn append_oplog_note_answers_for_every_shard() {
    let mut m = cluster();
    let r = admin(&mut m, r#"{"appendOplogNote":1,"data":{"note":"t030"},"bogus":1}"#);
    let each = || Value::Document(doc(&[("ok", Value::Double(1.0))]));
    let raw = doc(&[("node1.cluster:27018", each()), ("node2.cluster:27018", each())]);
    assert_eq!(r, doc(&[("raw", Value::Document(raw)), ("ok", Value::Double(1.0))]));
    let r = admin(&mut m, r#"{"appendOplogNote":1}"#);
    assert_eq!(field(&r, "code"), &Value::Int32(4));
    assert_eq!(field(&r, "errmsg"), &Value::from(""));
    assert_eq!(field(&r, "ok"), &Value::Double(0.0));
    let Value::Document(raw) = field(&r, "raw") else { panic!("{r:?}") };
    let Some(Value::Document(shard)) = raw.get("node1.cluster:27018") else { panic!("{r:?}") };
    assert_eq!(shard.get("errmsg"), Some(&Value::from("Missing expected field \"data\"")));
    let r = admin(&mut m, r#"{"appendOplogNote":1,"data":5}"#);
    assert_eq!(field(&r, "codeName"), &Value::from("TypeMismatch"));
    fails(
        &m.run("z", json(r#"{"appendOplogNote":1,"data":{}}"#)),
        13,
        "appendOplogNote may only be run against the admin database.",
    );
}

#[test]
fn transition_to_dedicated_config_server_is_already_over() {
    let mut m = cluster();
    let r = admin(&mut m, r#"{"transitionToDedicatedConfigServer":1}"#);
    let done = doc(&[
        ("state", Value::from("completed")),
        ("msg", Value::from("removeshard completed successfully")),
        ("shard", Value::from("config")),
        ("ok", Value::Double(1.0)),
    ]);
    assert_eq!(r, done);
    let bogus = json(r#"{"transitionToDedicatedConfigServer":1,"foo":1}"#);
    let e =
        teseladb_wire::generic::check_unknown_fields(&bogus, "transitionToDedicatedConfigServer")
            .unwrap_err();
    assert_eq!(e.errmsg, "BSON field 'transitionToDedicatedConfigServer.foo' is an unknown field.");
    fails(
        &m.run("z", json(r#"{"transitionToDedicatedConfigServer":1}"#)),
        13,
        "transitionToDedicatedConfigServer may only be run against the admin database.",
    );
}
