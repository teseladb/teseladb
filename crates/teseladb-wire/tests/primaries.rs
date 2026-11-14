//! 018 T008: `movePrimary` and `enableSharding`'s `primaryShard` (FR-005) over the in-memory node
//! of `common`, with the messages and order of the checks of a `refrouter` 9.0.4 of two shards
//! (measured on 02-11-2026): the primary is a Group 0 setting.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_types::NodeId;
use teseladb_wire::commands::primaries::{Primary, key, of_settings};

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
    m
}

fn primary(m: &Mem, db: &str) -> Option<Primary> {
    of_settings(&m.group0).get(db).copied()
}

#[test]
fn move_primary_records_the_new_primary_with_a_new_version() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"enableSharding":"newdb"}"#));
    assert!(m.catalog.dbs.contains_key("newdb"), "enableSharding records the database");
    assert_eq!(primary(&m, "newdb"), None);
    ok(&admin(&mut m, r#"{"movePrimary":"newdb","to":"node2","bogus":1}"#));
    assert_eq!(primary(&m, "newdb"), Some(Primary { node: NodeId(2), last_mod: 2 }));
    ok(&admin(&mut m, r#"{"movePrimary":"newdb","to":"node2"}"#));
    assert_eq!(primary(&m, "newdb"), Some(Primary { node: NodeId(2), last_mod: 2 }));
    // To the first shard, which it had before any move: recorded, same version.
    ok(&m.run("q", json(r#"{"insert":"c","documents":[{"x":1}]}"#)));
    ok(&admin(&mut m, r#"{"movePrimary":"q","to":"node1"}"#));
    assert_eq!(primary(&m, "q"), Some(Primary { node: NodeId(1), last_mod: 1 }));
    // A dropped database forgets its primary.
    ok(&m.run("newdb", json(r#"{"dropDatabase":1}"#)));
    assert_eq!(primary(&m, "newdb"), None);
    assert!(!m.group0.contains_key(&key("newdb")));
}

#[test]
fn move_primary_refuses_what_a_refrouter_refuses() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"enableSharding":"newdb"}"#));
    fails(
        &admin(&mut m, r#"{"movePrimary":"newdb","to":"nope"}"#),
        70,
        "requested primary shard nope does not exist :: caused by :: Shard nope not found",
    );
    fails(&admin(&mut m, r#"{"movePrimary":"nodb","to":"nope"}"#), 26, "database nodb not found");
    fails(
        &admin(&mut m, r#"{"movePrimary":"newdb"}"#),
        40414,
        "BSON field 'movePrimary.to' is missing but a required field",
    );
    fails(
        &admin(&mut m, r#"{"movePrimary":"newdb","to":5}"#),
        14,
        "BSON field 'movePrimary.to' is the wrong type 'int', expected type 'string'",
    );
    fails(
        &admin(&mut m, r#"{"movePrimary":5,"to":"node1"}"#),
        14,
        "BSON field 'movePrimary.movePrimary' is the wrong type 'int', expected type 'string'",
    );
    for db in ["admin", "config"] {
        fails(
            &admin(&mut m, &format!(r#"{{"movePrimary":"{db}","to":"node1"}}"#)),
            72,
            &format!("cannot move primary of internal database {db}"),
        );
    }
    fails(&admin(&mut m, r#"{"movePrimary":"","to":"node1"}"#), 73, " is not a valid db name");
    fails(
        &admin(&mut m, r#"{"movePrimary":"a.b","to":"node1"}"#),
        73,
        "'.' is an invalid character in a db name: a.b",
    );
    fails(
        &m.run("newdb", json(r#"{"movePrimary":"newdb","to":"node1"}"#)),
        13,
        "movePrimary may only be run against the admin database.",
    );
    m.group0_conflicts = 100;
    fails(
        &admin(&mut m, r#"{"movePrimary":"newdb","to":"node2"}"#),
        112,
        "the database's primary kept changing",
    );
}

#[test]
fn enable_sharding_takes_a_primary_shard() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"enableSharding":"pdb","primaryShard":"node2"}"#));
    assert_eq!(primary(&m, "pdb"), Some(Primary { node: NodeId(2), last_mod: 1 }));
    ok(&admin(&mut m, r#"{"enableSharding":"pdb","primaryShard":"node2"}"#));
    ok(&admin(&mut m, r#"{"enableSharding":"pdb"}"#));
    fails(
        &admin(&mut m, r#"{"enableSharding":"pdb","primaryShard":"node1"}"#),
        48,
        "Database pdb could not be created :: caused by :: database already created on a primary \
         which is different from node1",
    );
    fails(
        &admin(&mut m, r#"{"enableSharding":"pdb2","primaryShard":"nope"}"#),
        70,
        "Database pdb2 could not be created :: caused by :: Shard nope not found",
    );
    fails(
        &admin(&mut m, r#"{"enableSharding":"pdb4","primaryShard":5}"#),
        14,
        "BSON field 'enableSharding.primaryShard' is the wrong type 'int', expected type 'string'",
    );
    assert_eq!(Primary::decode(&[1, 2]), None);
    let none = doc(&[]);
    assert!(none.is_empty());
    let p = Primary { node: NodeId(7), last_mod: 3 };
    assert_eq!(Primary::decode(&p.encode()), Some(p));
}

/// A cluster of one shard moves no data: a `refrouter` 9.0.4 refuses `movePrimary` with 20 after the
/// checks of the database and before it looks for the target shard (`assertDataMovementAllowed`
/// of `shardsvr_move_primary_command.cpp`, r9.0.4; measured on the native `refrouter` of one shard on
/// 09-11-2026, scenario `sharding-moves`).
#[test]
fn move_primary_needs_a_second_shard() {
    let mut m = Mem::new();
    m.shards = vec!["node1".into()];
    ok(&admin(&mut m, r#"{"enableSharding":"newdb"}"#));
    let alone =
        "Cannot migrate data in a cluster before a second shard has been successfully added";

    fails(&admin(&mut m, r#"{"movePrimary":"newdb","to":"noSuchShard"}"#), 20, alone);
    fails(&admin(&mut m, r#"{"movePrimary":"newdb","to":"node1"}"#), 20, alone);
    fails(&admin(&mut m, r#"{"movePrimary":"nodb","to":"node1"}"#), 26, "database nodb not found");
    fails(
        &admin(&mut m, r#"{"movePrimary":"admin","to":"node1"}"#),
        72,
        "cannot move primary of internal database admin",
    );
}

/// `untrackUnshardedCollection` and `resetPlacementHistory` as a `refrouter` 9.0.4 answers them
/// (measured on the native `refrouter` on 09-11-2026, scenario `sharding-moves`): the first forgets
/// the tracking of a collection without shard key, accepts one it does not track or that does not
/// exist, refuses a sharded one; the second has no log of placements to reset here. Both take
/// fields they do not know and run only against `admin`.
#[test]
fn untrack_and_reset_placement_history_answer_as_a_refrouter() {
    let mut m = cluster();
    ok(&m.run("u", json(r#"{"insert":"c","documents":[{"a":1}]}"#)));
    ok(&m.run("u", json(r#"{"insert":"k","documents":[{"a":1}]}"#)));
    ok(&m.run("u", json(r#"{"createIndexes":"k","indexes":[{"key":{"a":1},"name":"a_1"}]}"#)));
    ok(&admin(&mut m, r#"{"shardCollection":"u.k","key":{"a":1}}"#));

    ok(&admin(&mut m, r#"{"untrackUnshardedCollection":"u.c"}"#));
    ok(&admin(&mut m, r#"{"untrackUnshardedCollection":"u.none","foo":1}"#));
    fails(
        &admin(&mut m, r#"{"untrackUnshardedCollection":"u.k"}"#),
        73,
        "Collection u.k must not be sharded",
    );
    fails(
        &admin(&mut m, r#"{"untrackUnshardedCollection":"nodb.c"}"#),
        26,
        "database nodb not found",
    );
    fails(
        &admin(&mut m, r#"{"untrackUnshardedCollection":5}"#),
        14,
        "BSON field 'untrackUnshardedCollection.untrackUnshardedCollection' is the wrong type 'int', expected type 'string'",
    );
    fails(
        &m.run("u", json(r#"{"untrackUnshardedCollection":"u.c"}"#)),
        13,
        "untrackUnshardedCollection may only be run against the admin database.",
    );
    ok(&admin(&mut m, r#"{"resetPlacementHistory":1,"foo":1}"#));
    ok(&admin(&mut m, r#"{"resetPlacementHistory":"x"}"#));
    fails(
        &m.run("u", json(r#"{"resetPlacementHistory":1}"#)),
        13,
        "resetPlacementHistory may only be run against the admin database.",
    );
}
