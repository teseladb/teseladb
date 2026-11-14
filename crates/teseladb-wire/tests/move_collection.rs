//! 018 T020 and T024: `moveCollection`, `unshardCollection`, `rewriteCollection`, their aborts,
//! `commitReshardCollection` and `cleanupReshardCollection` (FR-017, FR-018) over the in-memory
//! node of `common`, with the replies and messages of the reference router 9.0.4 of two shards
//! (measured on 02-11-2026, `t1.js` to `t12.js` in the lane's notes).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_router::table::RouteEntry;
use teseladb_types::{GroupId, NodeId};
use teseladb_wire::commands::is_data_command;
use teseladb_wire::commands::move_collection::node_of;
use teseladb_wire::commands::reshard::{Phase, Plan, Provenance, Resharding};
use teseladb_wire::generic::parse;

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

/// Two shards and one range over everything led by node 1; `m.u` without shard key and `m.s`
/// sharded by `{a: 1}`, two documents each.
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
    let docs = r#"[{"_id":1,"a":1},{"_id":2,"a":2}]"#;
    ok(&m.run("m", json(&format!(r#"{{"insert":"u","documents":{docs}}}"#))));
    ok(&admin(&mut m, r#"{"shardCollection":"m.s","key":{"a":1}}"#));
    ok(&m.run("m", json(&format!(r#"{{"insert":"s","documents":{docs}}}"#))));
    m
}

fn entry(m: &mut Mem, ns: &str) -> Option<Document> {
    let r =
        m.run("config", json(&format!(r#"{{"find":"collections","filter":{{"_id":"{ns}"}}}}"#)));
    match field(&r, "cursor.firstBatch") {
        Value::Array(a) => a.first().map(|v| match v {
            Value::Document(d) => d.clone(),
            other => panic!("{other:?}"),
        }),
        other => panic!("{other:?}"),
    }
}

fn keys(d: &Document) -> Vec<String> {
    d.iter().map(|(k, _)| k.to_owned()).collect()
}

fn docs(m: &mut Mem, coll: &str) -> Vec<Document> {
    let r = m.run("m", json(&format!(r#"{{"find":"{coll}","sort":{{"_id":1}}}}"#)));
    match field(&r, "cursor.firstBatch") {
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Document(d) => d.clone(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    }
}

fn node(m: &mut Mem, coll: &str) -> Option<u64> {
    let (_, c) = m.catalog.collections.iter().find(|(ns, _)| ns.coll == coll).unwrap();
    let c = c.clone();
    node_of(m, &c).unwrap().map(|n| n.0)
}

#[test]
fn move_collection_fails_as_on_the_reference_router() {
    let mut m = cluster();
    ok(&m.run("m", json(r#"{"create":"v","viewOn":"u","pipeline":[]}"#)));
    let cases: [(&str, &str, i32, &str); 12] = [
        (
            "admin",
            r#"{"moveCollection":"m.u"}"#,
            40414,
            "BSON field 'moveCollection.toShard' is missing but a required field",
        ),
        (
            "admin",
            r#"{"moveCollection":"m.u","toShard":1}"#,
            14,
            "BSON field 'moveCollection.toShard' is the wrong type 'int', expected type 'string'",
        ),
        (
            "admin",
            r#"{"moveCollection":1,"toShard":"node2"}"#,
            14,
            "BSON field 'moveCollection.moveCollection' is the wrong type 'int', expected type 'string'",
        ),
        ("admin", r#"{"moveCollection":"m.u","toShard":"nope"}"#, 70, "Shard nope not found"),
        (
            "m",
            r#"{"moveCollection":"m.u","toShard":"node2"}"#,
            13,
            "moveCollection may only be run against the admin database.",
        ),
        (
            "admin",
            r#"{"moveCollection":"m","toShard":"node2"}"#,
            73,
            "Invalid namespace specified: m",
        ),
        (
            "admin",
            r#"{"moveCollection":"m.nope","toShard":"node2"}"#,
            26,
            "Namespace m.nope not found",
        ),
        (
            "admin",
            r#"{"moveCollection":"m.s","toShard":"node2"}"#,
            26,
            "MoveCollection can only be called on an unsharded collection.",
        ),
        (
            "admin",
            r#"{"moveCollection":"m.v","toShard":"node2"}"#,
            26,
            "Collection 'm.v' not found in cluster catalog",
        ),
        (
            "admin",
            r#"{"moveCollection":"admin.foo","toShard":"node2"}"#,
            20,
            "Tracking of collection 'admin.foo' is not supported.",
        ),
        (
            "admin",
            r#"{"moveCollection":"m.system.profile","toShard":"node2"}"#,
            20,
            "Tracking of collection 'm.system.profile' is not supported.",
        ),
        (
            "admin",
            r#"{"moveCollection":"config.system.sessions","toShard":"node2"}"#,
            20,
            "Can't reshard a collection in the config database",
        ),
    ];
    for (db, cmd, code, msg) in cases {
        fails(&m.run(db, json(cmd)), code, msg);
    }
    fails(
        &admin(&mut m, r#"{"moveCollection":"m.u","toShard":"node2","performVerification":true}"#),
        72,
        "Cannot set 'performVerification' to true when reshardingDocumentVerification is false",
    );
    assert_eq!(entry(&mut m, "m.u"), None);
}

#[test]
fn move_collection_tracks_the_collection_on_the_shard() {
    let mut m = cluster();
    let before = docs(&mut m, "u");
    ok(&admin(
        &mut m,
        r#"{"moveCollection":"m.u","toShard":"node2","oplogBatchApplierTaskCount":1}"#,
    ));
    assert_eq!(docs(&mut m, "u"), before);
    assert_eq!(node(&mut m, "u"), Some(2));
    let e = entry(&mut m, "m.u").expect("tracked");
    assert_eq!(
        keys(&e),
        [
            "_id",
            "lastmodEpoch",
            "timestamp",
            "uuid",
            "key",
            "unique",
            "unsplittable",
            "lastmod",
            "noBalance"
        ]
    );
    assert_eq!(e.get("key"), Some(&Value::Document(json(r#"{"_id":1}"#))));
    assert_eq!(e.get("unsplittable"), Some(&Value::Boolean(true)));
    // Again to the same shard: nothing to do, the UUID stays.
    let orders = m.range_orders.len();
    ok(&admin(&mut m, r#"{"moveCollection":"m.u","toShard":"node2"}"#));
    assert_eq!(entry(&mut m, "m.u").and_then(|d| d.get("uuid").cloned()), e.get("uuid").cloned());
    assert_eq!(m.range_orders.len(), orders);
    // Back: a new UUID; writes go on.
    ok(&admin(&mut m, r#"{"moveCollection":"m.u","toShard":"node1"}"#));
    assert_ne!(entry(&mut m, "m.u").and_then(|d| d.get("uuid").cloned()), e.get("uuid").cloned());
    let one = Value::Array(vec![Value::Document(doc(&[("_id", Value::Int32(3))]))]);
    ok(&m.run("m", doc(&[("insert", Value::from("u")), ("documents", one)])));
    assert_eq!(docs(&mut m, "u").len(), 3);
}

#[test]
fn a_capped_collection_moves_its_ranges_alone() {
    let mut m = cluster();
    ok(&m.run("m", json(r#"{"create":"cap","capped":true,"size":4096}"#)));
    ok(&m.run("m", json(r#"{"insert":"cap","documents":[{"_id":1},{"_id":2}]}"#)));
    let uuid =
        |m: &mut Mem| m.catalog.collections.iter().find(|(n, _)| n.coll == "cap").unwrap().1.uuid;
    let before = uuid(&mut m);
    ok(&admin(&mut m, r#"{"moveCollection":"m.cap","toShard":"node2"}"#));
    assert_eq!(uuid(&mut m), before);
    assert_eq!(node(&mut m, "cap"), Some(2));
    assert!(entry(&mut m, "m.cap").is_some());
    assert_eq!(docs(&mut m, "cap").len(), 2);
}

#[test]
fn unshard_collection_fails_as_on_the_reference_router() {
    let mut m = cluster();
    let cases: [(&str, &str, i32, &str); 8] = [
        (
            "admin",
            r#"{"unshardCollection":1}"#,
            14,
            "BSON field 'unshardCollection.unshardCollection' is the wrong type 'int', expected type 'string'",
        ),
        (
            "m",
            r#"{"unshardCollection":"m.s"}"#,
            13,
            "unshardCollection may only be run against the admin database.",
        ),
        (
            "admin",
            r#"{"unshardCollection":"m.nope"}"#,
            26,
            "Collection 'm.nope' not found in cluster catalog",
        ),
        ("admin", r#"{"unshardCollection":"m"}"#, 73, "Namespace mis not a valid collection name"),
        (
            "admin",
            r#"{"unshardCollection":"m.u"}"#,
            26,
            "Collection 'm.u' not found in cluster catalog",
        ),
        ("admin", r#"{"unshardCollection":"m.s","toShard":"nope"}"#, 70, "Shard nope not found"),
        (
            "admin",
            r#"{"unshardCollection":"m.s","toShard":5}"#,
            14,
            "BSON field 'unshardCollection.toShard' is the wrong type 'int', expected type 'string'",
        ),
        (
            "admin",
            r#"{"unshardCollection":"config.system.sessions"}"#,
            20,
            "Can't reshard a collection in the config database",
        ),
    ];
    for (db, cmd, code, msg) in cases {
        fails(&m.run(db, json(cmd)), code, msg);
    }
}

#[test]
fn unshard_collection_drops_the_key_on_the_least_loaded_shard() {
    let mut m = cluster();
    let before = docs(&mut m, "s");
    // Node 1 leads the only range: node 2 leads fewer.
    ok(&admin(&mut m, r#"{"unshardCollection":"m.s"}"#));
    assert_eq!(docs(&mut m, "s"), before);
    assert_eq!(node(&mut m, "s"), Some(2));
    let e = entry(&mut m, "m.s").expect("tracked");
    assert_eq!(
        keys(&e),
        [
            "_id",
            "lastmodEpoch",
            "timestamp",
            "uuid",
            "key",
            "unique",
            "lastmod",
            "noBalance",
            "unsplittable"
        ]
    );
    // Already without key there: nothing to do; elsewhere: moveCollection's job.
    ok(&admin(&mut m, r#"{"unshardCollection":"m.s","toShard":"node2"}"#));
    fails(
        &admin(&mut m, r#"{"unshardCollection":"m.s","toShard":"node1"}"#),
        118,
        "Collection is already unsharded on a different shard. Use moveCollection.",
    );
    ok(&m.run("m", json(r#"{"insert":"s","documents":[{"_id":3}]}"#)));
    // A collection without key accepts moveCollection now.
    ok(&admin(&mut m, r#"{"moveCollection":"m.s","toShard":"node1"}"#));
    assert_eq!(docs(&mut m, "s").len(), 3);
}

/// Ranges past every key, each led by `node`.
fn empty_ranges(m: &mut Mem, node: u64, first_group: u64) {
    for i in 0..3u8 {
        m.routes.push(RouteEntry {
            group: GroupId(first_group + u64::from(i)),
            start: vec![0xFF, i],
            end: if i == 2 { Vec::new() } else { vec![0xFF, i + 1] },
            core: 0,
            voters: vec![NodeId(node)],
            generation: 1,
            leader_hint: Some(NodeId(node)),
            moved_ms: 0,
            replica_epoch: 1,
        });
    }
    m.routes[0].end = vec![0xFF, 0];
}

/// Without `toShard`, the reference router 9.0.4 sends the collection to the shard with the least
/// data, whatever the chunks each one has (measured on 02-11-2026, `u1.js` to `u5.js` in the lane's
/// notes): here the shard that leads more ranges, all of them empty.
#[test]
fn unshard_collection_picks_the_shard_with_the_least_data() {
    let mut m = cluster();
    empty_ranges(&mut m, 2, 10);
    ok(&admin(&mut m, r#"{"unshardCollection":"m.s"}"#));
    assert_eq!(node(&mut m, "s"), Some(2));
    // The data on node 2 and the empty ranges on node 1: back to node 1.
    let mut m = cluster();
    m.routes[0].leader_hint = Some(NodeId(2));
    m.routes[0].voters = vec![NodeId(2)];
    empty_ranges(&mut m, 1, 10);
    ok(&admin(&mut m, r#"{"unshardCollection":"m.s"}"#));
    assert_eq!(node(&mut m, "s"), Some(1));
}

/// A draining shard takes no collection, a range without a leader hint is led by its first
/// voter, and one without voters counts for no shard.
#[test]
fn unshard_collection_leaves_draining_shards_and_reads_the_first_voter() {
    let mut m = cluster();
    m.routes[0].leader_hint = None;
    m.routes[0].end = vec![0xFF, 0];
    m.routes.push(RouteEntry {
        group: GroupId(9),
        start: vec![0xFF, 0],
        end: Vec::new(),
        core: 0,
        voters: Vec::new(),
        generation: 1,
        leader_hint: None,
        moved_ms: 0,
        replica_epoch: 1,
    });
    m.draining = vec!["node2".into()];
    ok(&admin(&mut m, r#"{"unshardCollection":"m.s"}"#));
    assert_eq!(node(&mut m, "s"), Some(1));
    m.draining = vec!["node1".into(), "node2".into()];
    ok(&admin(&mut m, r#"{"shardCollection":"m.u","key":{"_id":1}}"#));
    fails(&admin(&mut m, r#"{"unshardCollection":"m.u"}"#), 70, "No shards found");
}

/// A namespace without a collection name fails as the bare database name does; the ranges
/// before the collection's keys lead nothing of it, and a shard that already holds a replica
/// of a range only takes its leadership.
#[test]
fn the_namespace_the_ranges_and_the_replicas_of_a_move() {
    let mut m = cluster();
    fails(
        &admin(&mut m, r#"{"moveCollection":"m.","toShard":"node2"}"#),
        73,
        "Invalid namespace specified: m.",
    );
    fails(
        &admin(&mut m, r#"{"unshardCollection":"m."}"#),
        73,
        "Namespace m.is not a valid collection name",
    );
    m.routes[0].start = vec![0x01];
    m.routes[0].voters = vec![NodeId(1), NodeId(2)];
    m.routes.insert(
        0,
        RouteEntry {
            group: GroupId(8),
            start: Vec::new(),
            end: vec![0x01],
            core: 0,
            voters: vec![NodeId(2)],
            generation: 1,
            leader_hint: Some(NodeId(2)),
            moved_ms: 0,
            replica_epoch: 1,
        },
    );
    assert_eq!(node(&mut m, "u"), Some(1));
    ok(&admin(&mut m, r#"{"moveCollection":"m.u","toShard":"node2"}"#));
    assert_eq!(node(&mut m, "u"), Some(2));
    assert_eq!(docs(&mut m, "u").len(), 2);
}

/// Without `numInitialChunks`, `rewriteCollection` asks for the default chunks and its event
/// does not name them.
#[test]
fn rewrite_collection_without_chunks_takes_the_default() {
    let mut m = cluster();
    let all: Vec<String> = (10..1000).map(|i| format!(r#"{{"_id":{i},"a":{i}}}"#)).collect();
    ok(&m.run("m", json(&format!(r#"{{"insert":"s","documents":[{}]}}"#, all.join(",")))));
    ok(&admin(&mut m, r#"{"rewriteCollection":"m.s"}"#));
    let n = m.run("m", json(r#"{"count":"s"}"#));
    assert_eq!(n.get("n"), Some(&Value::Int32(992)), "{n:?}");
}

#[test]
fn rewrite_collection_fails_and_rewrites_as_on_the_reference_router() {
    let mut m = cluster();
    let cases: [(&str, &str, i32, &str); 7] = [
        (
            "admin",
            r#"{"rewriteCollection":"m.u"}"#,
            118,
            "Collection has to be a sharded collection.",
        ),
        (
            "admin",
            r#"{"rewriteCollection":"m.nope"}"#,
            26,
            "Collection 'm.nope' not found in cluster catalog",
        ),
        (
            "admin",
            r#"{"rewriteCollection":1}"#,
            14,
            "BSON field 'rewriteCollection.rewriteCollection' is the wrong type 'int', expected type 'string'",
        ),
        (
            "m",
            r#"{"rewriteCollection":"m.s"}"#,
            13,
            "rewriteCollection may only be run against the admin database.",
        ),
        ("admin", r#"{"rewriteCollection":"m"}"#, 73, "Namespace mis not a valid collection name"),
        (
            "admin",
            r#"{"rewriteCollection":"m.s","numInitialChunks":"x"}"#,
            14,
            "BSON field 'rewriteCollection.numInitialChunks' is the wrong type 'string', expected types '[int, long, decimal, double]'",
        ),
        (
            "admin",
            r#"{"rewriteCollection":"m.s","zones":[]}"#,
            4952606,
            "The shard key provided does not have enough cardinality to make the required number of chunks of 90, it can only make 2 chunks",
        ),
    ];
    for (db, cmd, code, msg) in cases {
        fails(&m.run(db, json(cmd)), code, msg);
    }
    let before = entry(&mut m, "m.s").expect("sharded");
    ok(&admin(&mut m, r#"{"rewriteCollection":"m.s","numInitialChunks":1,"demoMode":true}"#));
    let after = entry(&mut m, "m.s").expect("sharded");
    assert_ne!(after.get("uuid"), before.get("uuid"));
    assert_eq!(after.get("key"), before.get("key"));
    assert_eq!(docs(&mut m, "s").len(), 2);
}

/// A resharding of `m.s` for `provenance` started and copied, as the command runs it.
fn started(m: &mut Mem, provenance: Provenance, cmd: &str) -> Resharding {
    let shared = std::mem::take(&mut m.shared);
    let plan = Plan {
        ns: teseladb_meta::catalog::Namespace { db: "m".into(), coll: "s".into() },
        key: teseladb_wire::commands::sharding::ShardKey { key: json(r#"{"a":1}"#), unique: false },
        description: json(&format!(r#"{{"provenance":"{}"}}"#, provenance.name())),
        provenance,
        to: None,
    };
    let mut r = Resharding::start(m, &shared, plan, &json(cmd)).expect("started");
    r.copy(m, &shared).expect("copied");
    m.shared = shared;
    r
}

#[test]
fn each_abort_stops_the_resharding_of_its_command() {
    let mut m = cluster();
    for name in [
        "abortMoveCollection",
        "abortUnshardCollection",
        "abortRewriteCollection",
        "abortReshardCollection",
        "commitReshardCollection",
    ] {
        fails(
            &admin(&mut m, &format!(r#"{{"{name}":"m.s","x":1}}"#)),
            339,
            "Could not find resharding-related metadata that matches the given namespace",
        );
        fails(
            &m.run("m", json(&format!(r#"{{"{name}":"m.s"}}"#))),
            13,
            &format!("{name} may only be run against the admin database."),
        );
        fails(
            &admin(&mut m, &format!(r#"{{"{name}":1}}"#)),
            14,
            &format!("BSON field '{name}.{name}' is the wrong type 'int', expected type 'string'"),
        );
    }
    let cases = [
        (Provenance::Move, "abortMoveCollection"),
        (Provenance::Unshard, "abortUnshardCollection"),
        (Provenance::Rewrite, "abortRewriteCollection"),
    ];
    for (provenance, abort) in cases {
        let mut r = started(&mut m, provenance, r#"{"rewriteCollection":"m.s"}"#);
        ok(&admin(&mut m, r#"{"commitReshardCollection":"m.s"}"#));
        for other in [
            "abortMoveCollection",
            "abortUnshardCollection",
            "abortRewriteCollection",
            "abortReshardCollection",
        ] {
            if other != abort {
                fails(
                    &admin(&mut m, &format!(r#"{{"{other}":"m.s"}}"#)),
                    20,
                    "Could not find in-progress resharding operation with matching provenance",
                );
            }
        }
        ok(&admin(&mut m, &format!(r#"{{"{abort}":"m.s"}}"#)));
        let shared = std::mem::take(&mut m.shared);
        let e = r.drive(&mut m, &shared).expect_err("aborted");
        assert_eq!((e.code.code, e.errmsg.as_str()), (341, "resharding aborted by user"));
        assert_eq!(r.phase(), Phase::Aborted);
        r.finish(&shared);
        m.shared = shared;
    }
    assert_eq!(docs(&mut m, "s").len(), 2);
}

#[test]
fn cleanup_reshard_collection_is_no_command_of_the_reference_router() {
    // The reference router 9.0.4 answers 59 «no such cmd: cleanupReshardCollection»: the dispatcher
    // does the same for a name no module knows.
    assert!(!is_data_command("cleanupReshardCollection"));
    assert!(is_data_command("commitReshardCollection"));
}

#[test]
fn their_unknown_fields_fail_as_on_the_reference_router() {
    let cases: [(&str, &str); 8] = [
        (r#"{"moveCollection":"m.u","toShard":"node2","extra":1}"#, "moveCollection.extra"),
        (r#"{"moveCollection":"m.u","toShard":"node2","zones":[]}"#, "moveCollection.zones"),
        (
            r#"{"moveCollection":"m.u","toShard":"node2","demoMode":true}"#,
            "moveCollection.demoMode",
        ),
        (r#"{"unshardCollection":"m.s","extra":1}"#, "unshardCollection.extra"),
        (
            r#"{"unshardCollection":"m.s","numInitialChunks":1}"#,
            "unshardCollection.numInitialChunks",
        ),
        (r#"{"rewriteCollection":"m.s","extra":1}"#, "rewriteCollection.extra"),
        (r#"{"rewriteCollection":"m.s","key":{"b":1}}"#, "rewriteCollection.key"),
        (
            r#"{"rewriteCollection":"m.s","oplogBatchApplierTaskCount":1}"#,
            "rewriteCollection.oplogBatchApplierTaskCount",
        ),
    ];
    for (cmd, at) in cases {
        let cmd = json(cmd);
        let name = cmd.iter().next().map(|(k, _)| k.to_owned()).unwrap();
        let e = parse(&cmd, &name).expect_err("unknown");
        assert_eq!(
            (e.code.code, e.errmsg),
            (40415, format!("BSON field '{at}' is an unknown field."))
        );
    }
    let known = [
        r#"{"moveCollection":"m.u","toShard":"a","oplogBatchApplierTaskCount":1,"performVerification":false}"#,
        r#"{"unshardCollection":"m.u","toShard":"a","oplogBatchApplierTaskCount":1,"performVerification":false}"#,
        r#"{"rewriteCollection":"m.u","numInitialChunks":1,"zones":[],"performVerification":false,"demoMode":true}"#,
        r#"{"abortMoveCollection":"m.u","x":1}"#,
        r#"{"commitReshardCollection":"m.u","x":1}"#,
    ];
    for cmd in known {
        let cmd = json(cmd);
        let name = cmd.iter().next().map(|(k, _)| k.to_owned()).unwrap();
        parse(&cmd, &name).expect("known");
    }
}

/// The data of each shard are the live bytes its ranges' state machines keep, not what a scan
/// of the collections finds (100 GB scale, 018 FR-017): `m.u` alone on node 1 and `m.s` on
/// node 2, weighed only by the ranges' counts.
#[test]
fn unshard_collection_weighs_the_shards_by_their_ranges_live_bytes() {
    for (one, two, chosen) in [(10, 1_000_000, 1), (1_000_000, 10, 2)] {
        let mut m = cluster();
        let (_, s) = m.catalog.collections.iter().find(|(ns, _)| ns.coll == "s").unwrap();
        let mid = teseladb_keys::KeyBuilder::collection_start(s.tenant, s.id).as_bytes().to_vec();
        m.routes[0].end = mid.clone();
        let mut right = m.routes[0].clone();
        (right.group, right.start, right.end) = (GroupId(2), mid, Vec::new());
        (right.voters, right.leader_hint) = (vec![NodeId(2)], Some(NodeId(2)));
        m.routes.push(right);
        m.range_live = [(GroupId(1), one), (GroupId(2), two)].into_iter().collect();
        let before = m.scanned_rows;
        ok(&admin(&mut m, r#"{"shardCollection":"m.k","key":{"_id":1}}"#));
        ok(&admin(&mut m, r#"{"unshardCollection":"m.k"}"#));
        assert_eq!(node(&mut m, "k"), Some(chosen), "{one} / {two}");
        assert_eq!(
            m.scanned_rows, before,
            "an empty collection is copied without reading the rest"
        );
    }
}

/// A namespace without a database fails as one without a collection does; a collection no
/// range holds has no shard; and a range without voters weighs on no shard.
#[test]
fn namespaces_without_database_and_ranges_without_a_leader() {
    let mut m = cluster();
    fails(
        &admin(&mut m, r#"{"moveCollection":".u","toShard":"node2"}"#),
        73,
        "Invalid namespace specified: .u",
    );
    fails(
        &admin(&mut m, r#"{"unshardCollection":".u"}"#),
        73,
        "Namespace .uis not a valid collection name",
    );
    m.routes[0].start = vec![0xFF];
    assert_eq!(node(&mut m, "u"), None);
    let mut m = cluster();
    (m.routes[0].voters, m.routes[0].leader_hint) = (Vec::new(), None);
    ok(&admin(&mut m, r#"{"shardCollection":"m.k","key":{"_id":1}}"#));
    ok(&admin(&mut m, r#"{"unshardCollection":"m.k"}"#));
}
