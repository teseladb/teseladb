//! 018 T020: `setAllowMigrations` and `mergeAllChunksOnShard` (FR-017) over the in-memory node
//! of `common`, with the replies and messages of a `refrouter` 9.0.4 of two shards (measured on
//! 02-11-2026, `t4.js` and `t13.js` in the lane's notes).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_router::table::RouteEntry;
use teseladb_types::{GroupId, NodeId};
use teseladb_wire::generic::parse;

/// `minSnapshotHistoryWindowInSeconds` of the shards of a `refrouter` 9.0.4 by default (measured
/// 03-11-2026), in milliseconds.
const WINDOW_MS: u64 = 300_000;

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

/// Two shards, one range led by node 1, `m.k` sharded by `{a: 1}` with `a` 0 to 19 and `m.u`
/// without shard key.
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
    ok(&admin(&mut m, r#"{"shardCollection":"m.k","key":{"a":1}}"#));
    let docs: Vec<String> = (0..20).map(|i| format!(r#"{{"_id":{i},"a":{i}}}"#)).collect();
    ok(&m.run("m", json(&format!(r#"{{"insert":"k","documents":[{}]}}"#, docs.join(",")))));
    ok(&m.run("m", json(r#"{"insert":"u","documents":[{"_id":1}]}"#)));
    m
}

fn entry(m: &mut Mem) -> Document {
    let r = m.run("config", json(r#"{"find":"collections","filter":{"_id":"m.k"}}"#));
    match field(&r, "cursor.firstBatch.0") {
        Value::Document(d) => d.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn set_allow_migrations_fails_as_on_a_refrouter() {
    let mut m = cluster();
    let cases: [(&str, &str, i32, &str); 7] = [
        (
            "admin",
            r#"{"setAllowMigrations":"m.k"}"#,
            40414,
            "BSON field 'setAllowMigrations.allowMigrations' is missing but a required field",
        ),
        (
            "admin",
            r#"{"setAllowMigrations":"m.k","allowMigrations":1}"#,
            14,
            "BSON field 'setAllowMigrations.allowMigrations' is the wrong type 'int', expected type 'bool'",
        ),
        (
            "admin",
            r#"{"setAllowMigrations":1,"allowMigrations":true}"#,
            14,
            "BSON field 'setAllowMigrations.setAllowMigrations' is the wrong type 'int', expected type 'string'",
        ),
        (
            "m",
            r#"{"setAllowMigrations":"m.k","allowMigrations":true}"#,
            13,
            "setAllowMigrations may only be run against the admin database.",
        ),
        (
            "admin",
            r#"{"setAllowMigrations":"m.nope","allowMigrations":false}"#,
            118,
            "Collection must be sharded so migrations can be blocked",
        ),
        (
            "admin",
            r#"{"setAllowMigrations":"m.u","allowMigrations":false}"#,
            118,
            "Collection must be sharded so migrations can be blocked",
        ),
        (
            "admin",
            r#"{"setAllowMigrations":"m","allowMigrations":false}"#,
            73,
            "Invalid namespace specified: m",
        ),
    ];
    for (db, cmd, code, msg) in cases {
        fails(&m.run(db, json(cmd)), code, msg);
    }
    parse(
        &json(r#"{"setAllowMigrations":"m.k","allowMigrations":false,"x":1}"#),
        "setAllowMigrations",
    )
    .expect("no strict fields");
}

#[test]
fn blocked_migrations_refuse_moves_until_allowed() {
    let mut m = cluster();
    let before = entry(&mut m);
    ok(&admin(&mut m, r#"{"setAllowMigrations":"m.k","allowMigrations":false}"#));
    ok(&admin(&mut m, r#"{"setAllowMigrations":"m.k","allowMigrations":false}"#));
    let blocked = entry(&mut m);
    assert_eq!(blocked.iter().last(), Some(("permitMigrations", &Value::Boolean(false))));
    let refused = "Command request failed on source shard. :: caused by :: Can't execute moveChunk because migrations for this collection are disallowed";
    fails(&admin(&mut m, r#"{"moveChunk":"m.k","find":{"a":1},"to":"node2"}"#), 117, refused);
    fails(&admin(&mut m, r#"{"moveRange":"m.k","min":{"a":5},"toShard":"node2"}"#), 117, refused);
    ok(&admin(&mut m, r#"{"balancerCollectionStatus":"m.k"}"#));
    ok(&admin(&mut m, r#"{"setAllowMigrations":"m.k","allowMigrations":true}"#));
    ok(&admin(&mut m, r#"{"setAllowMigrations":"m.k","allowMigrations":true}"#));
    assert_eq!(entry(&mut m), before);
    ok(&admin(&mut m, r#"{"moveChunk":"m.k","find":{"a":1},"to":"node2"}"#));
}

#[test]
fn merge_all_chunks_on_shard_fails_as_on_a_refrouter() {
    let mut m = cluster();
    let cases: [(&str, &str, i32, &str); 8] = [
        (
            "admin",
            r#"{"mergeAllChunksOnShard":"m.k"}"#,
            40414,
            "BSON field 'mergeAllChunksOnShard.shard' is missing but a required field",
        ),
        ("admin", r#"{"mergeAllChunksOnShard":"m.k","shard":"nope"}"#, 70, "Shard nope not found"),
        (
            "admin",
            r#"{"mergeAllChunksOnShard":"m.k","shard":1}"#,
            14,
            "BSON field 'mergeAllChunksOnShard.shard' is the wrong type 'int', expected type 'string'",
        ),
        (
            "admin",
            r#"{"mergeAllChunksOnShard":"m.nope","shard":"node1"}"#,
            118,
            "Collection m.nope is not sharded",
        ),
        (
            "admin",
            r#"{"mergeAllChunksOnShard":"m.u","shard":"node1"}"#,
            118,
            "Collection m.u is not sharded",
        ),
        (
            "m",
            r#"{"mergeAllChunksOnShard":"m.k","shard":"node1"}"#,
            13,
            "mergeAllChunksOnShard may only be run against the admin database.",
        ),
        (
            "admin",
            r#"{"mergeAllChunksOnShard":"m.k","shard":"node1","maxNumberOfChunksToMerge":"x"}"#,
            14,
            "BSON field 'mergeAllChunksOnShard.maxNumberOfChunksToMerge' is the wrong type 'string', expected type 'int'",
        ),
        (
            "admin",
            r#"{"mergeAllChunksOnShard":"m","shard":"node1"}"#,
            73,
            "invalid namespace specified for request",
        ),
    ];
    for (db, cmd, code, msg) in cases {
        fails(&m.run(db, json(cmd)), code, msg);
    }
    fails(
        &admin(&mut m, r#"{"mergeAllChunksOnShard":1,"shard":"node1"}"#),
        14,
        "BSON field 'mergeAllChunksOnShard.mergeAllChunksOnShard' is the wrong type 'int', expected type 'string'",
    );
    // The shard is looked up before the name is read (measured on 03-11-2026, `ch1.js`, `ch2.js`).
    for ns in [".t", ".", "m."] {
        let cmd = |shard: &str| format!(r#"{{"mergeAllChunksOnShard":"{ns}","shard":"{shard}"}}"#);
        fails(&admin(&mut m, &cmd("x")), 70, "Shard x not found");
        fails(&admin(&mut m, &cmd("node1")), 73, "invalid namespace specified for request");
    }
}

#[test]
fn merge_all_chunks_on_shard_joins_the_neighbours_of_the_shard() {
    let mut m = cluster();
    for at in [4, 8, 12, 16] {
        ok(&admin(&mut m, &format!(r#"{{"split":"m.k","middle":{{"a":{at}}}}}"#)));
    }
    // [12, 16) goes to node 2: node 1 keeps [MinKey, 12) and [16, MaxKey).
    ok(&admin(&mut m, r#"{"moveChunk":"m.k","find":{"a":13},"to":"node2"}"#));
    let ranges = |m: &Mem| m.routes.len();
    assert_eq!(ranges(&m), 5);
    // Past the history window of every split and move.
    m.now += WINDOW_MS;
    // Two chunks at most per merge: [MinKey, 4)+[4, 8), then [8, 12) alone.
    ok(&admin(
        &mut m,
        r#"{"mergeAllChunksOnShard":"m.k","shard":"node1","maxNumberOfChunksToMerge":2,"x":1}"#,
    ));
    assert_eq!(ranges(&m), 4);
    ok(&admin(&mut m, r#"{"mergeAllChunksOnShard":"m.k","shard":"node1"}"#));
    assert_eq!(ranges(&m), 3);
    ok(&admin(&mut m, r#"{"mergeAllChunksOnShard":"m.k","shard":"node2"}"#));
    assert_eq!(ranges(&m), 3);
    let r = m.run("m", doc(&[("count", Value::from("k"))]));
    assert_eq!(r.get("n"), Some(&Value::Int32(20)));
}

#[test]
fn merge_all_chunks_on_shard_leaves_out_what_was_split_or_moved_within_the_window() {
    // A `refrouter` 9.0.4 does not merge a chunk split or moved in the last
    // `minSnapshotHistoryWindowInSeconds` (300 s by default): fresh splits stay apart at 130 s
    // and merge after 25 minutes (measured 02/03-11-2026, `g1.js`-`g4.js`, `w1.js`, `w2.js`).
    let mut m = cluster();
    for at in [4, 8] {
        ok(&admin(&mut m, &format!(r#"{{"split":"m.k","middle":{{"a":{at}}}}}"#)));
    }
    let ranges = |m: &Mem| m.routes.len();
    assert_eq!(ranges(&m), 3);
    m.now += WINDOW_MS - 1_000;
    ok(&admin(&mut m, r#"{"mergeAllChunksOnShard":"m.k","shard":"node1"}"#));
    assert_eq!(ranges(&m), 3, "every chunk was split within the window");
    // [8, MaxKey) is split again: its halves are fresh, [MinKey, 4) and [4, 8) are not.
    ok(&admin(&mut m, r#"{"split":"m.k","middle":{"a":12}}"#));
    m.now += 1_000;
    ok(&admin(&mut m, r#"{"mergeAllChunksOnShard":"m.k","shard":"node1"}"#));
    assert_eq!(ranges(&m), 3, "[MinKey, 8), [8, 12) and [12, MaxKey)");
    // A chunk moved away and back is fresh again, whatever its age.
    m.now += WINDOW_MS;
    ok(&admin(&mut m, r#"{"moveChunk":"m.k","find":{"a":13},"to":"node2"}"#));
    ok(&admin(&mut m, r#"{"moveChunk":"m.k","find":{"a":13},"to":"node1"}"#));
    ok(&admin(&mut m, r#"{"mergeAllChunksOnShard":"m.k","shard":"node1"}"#));
    assert_eq!(ranges(&m), 2, "[MinKey, 12) and the moved [12, MaxKey)");
    m.now += WINDOW_MS;
    ok(&admin(&mut m, r#"{"mergeAllChunksOnShard":"m.k","shard":"node1"}"#));
    assert_eq!(ranges(&m), 1);
    let r = m.run("m", doc(&[("count", Value::from("k"))]));
    assert_eq!(r.get("n"), Some(&Value::Int32(20)));
}
