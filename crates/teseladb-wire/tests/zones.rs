//! 018 T007: `addShardToZone`, `removeShardFromZone` and `updateZoneKeyRange` (FR-003) over the
//! in-memory node of `common`, with the replies, messages and order of the checks of the reference
//! router 9.0.4 of two shards (measured on 02-11-2026), and the intervals Group 0 keeps for the
//! balancer: the shard key's keys of each bound.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_meta::zones::Zones;

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

/// Two shards and `z.t` sharded by `{a: 1}`.
fn cluster() -> Mem {
    let mut m = Mem::new();
    m.shards = vec!["node1".into(), "node2".into()];
    ok(&admin(&mut m, r#"{"shardCollection":"z.t","key":{"a":1}}"#));
    m
}

fn zones(m: &Mem) -> Zones {
    Zones::from_settings(&m.group0)
}

#[test]
fn shards_join_and_leave_zones() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"addShardToZone":"node1","zone":"EU"}"#));
    ok(&admin(&mut m, r#"{"addShardToZone":"node1","zone":"EU"}"#));
    ok(&admin(&mut m, r#"{"addShardToZone":"node1","zone":""}"#));
    assert_eq!(zones(&m).shards[&teseladb_types::NodeId(1)], ["EU", ""]);
    fails(&admin(&mut m, r#"{"addShardToZone":"nope","zone":"EU"}"#), 70, "Shard nope not found");
    fails(
        &admin(&mut m, r#"{"addShardToZone":"node1"}"#),
        40414,
        "BSON field 'addShardToZone.zone' is missing but a required field",
    );
    fails(
        &admin(&mut m, r#"{"addShardToZone":"node1","zone":5}"#),
        14,
        "BSON field 'addShardToZone.zone' is the wrong type 'int', expected type 'string'",
    );
    fails(
        &admin(&mut m, r#"{"addShardToZone":5,"zone":"x"}"#),
        14,
        "BSON field 'addShardToZone.addShardToZone' is the wrong type 'int', expected type 'string'",
    );
    // The dispatcher refuses unknown fields from the IDL fields (`generic`).
    let bogus = json(r#"{"addShardToZone":"node2","zone":"US","bogus":1}"#);
    let e = teseladb_wire::generic::check_unknown_fields(&bogus, "addShardToZone").unwrap_err();
    assert_eq!(e.errmsg, "BSON field 'addShardToZone.bogus' is an unknown field.");
    let extra = json(r#"{"updateZoneKeyRange":"z.t","min":{},"max":{},"zone":null,"x":1}"#);
    assert!(teseladb_wire::generic::check_unknown_fields(&extra, "updateZoneKeyRange").is_err());
    fails(
        &m.run("z", json(r#"{"addShardToZone":"node1","zone":"EU"}"#)),
        13,
        "addShardToZone may only be run against the admin database.",
    );
    fails(
        &m.run("z", json(r#"{"removeShardFromZone":"node1","zone":"EU"}"#)),
        13,
        "removeShardFromZone may only be run against the admin database.",
    );
    // Leaving: a zone it is not in, unknown fields and a missing shard as measured.
    ok(&admin(&mut m, r#"{"removeShardFromZone":"node2","zone":"NOPE","x":1}"#));
    fails(
        &admin(&mut m, r#"{"removeShardFromZone":"nope","zone":"US"}"#),
        70,
        "Shard nope not found",
    );
    fails(
        &admin(&mut m, r#"{"removeShardFromZone":"node2","zone":5}"#),
        14,
        "\"zone\" had the wrong type. Expected string, found int",
    );
    fails(
        &admin(&mut m, r#"{"removeShardFromZone":"node2"}"#),
        4,
        "Missing expected field \"zone\"",
    );
    // The last shard of a zone with an interval stays; another one may leave.
    ok(&admin(&mut m, r#"{"updateZoneKeyRange":"z.t","min":{"a":0},"max":{"a":10},"zone":"EU"}"#));
    fails(
        &admin(&mut m, r#"{"removeShardFromZone":"node1","zone":"EU"}"#),
        163,
        "cannot remove a shard from zone if a chunk range is associated with it",
    );
    ok(&admin(&mut m, r#"{"addShardToZone":"node2","zone":"EU"}"#));
    ok(&admin(&mut m, r#"{"removeShardFromZone":"node2","zone":"EU"}"#));
    ok(&admin(&mut m, r#"{"removeShardFromZone":"node1","zone":""}"#));
    assert_eq!(zones(&m).shards[&teseladb_types::NodeId(1)], ["EU"]);
}

#[test]
fn intervals_follow_the_reference_router_rules() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"shardCollection":"z.k","key":{"a":1,"b":1}}"#));
    ok(&admin(&mut m, r#"{"shardCollection":"z.h","key":{"a":"hashed"}}"#));
    ok(&admin(&mut m, r#"{"addShardToZone":"node1","zone":"EU"}"#));
    let upd = |m: &mut Mem, body: &str| admin(m, &format!(r#"{{"updateZoneKeyRange":{body}}}"#));
    ok(&upd(&mut m, r#""z.t","min":{"a":0},"max":{"a":10},"zone":"EU""#));
    ok(&upd(&mut m, r#""z.t","min":{"a":0},"max":{"a":10},"zone":"EU""#));
    fails(
        &upd(&mut m, r#""z.t","min":{"a":10},"max":{"a":20},"zone":"US""#),
        177,
        "zone US does not exist",
    );
    fails(
        &upd(&mut m, r#""z.t","min":{"a":30},"max":{"a":20},"zone":"US""#),
        2,
        "min: { a: 30 } should be less than max: { a: 20 }",
    );
    fails(
        &upd(&mut m, r#""z.t","min":{"a":"x"},"max":{"a":40},"zone":"EU""#),
        2,
        "min: { a: \"x\" } should be less than max: { a: 40 }",
    );
    ok(&admin(&mut m, r#"{"addShardToZone":"node2","zone":"US"}"#));
    fails(
        &upd(&mut m, r#""z.t","min":{"a":5},"max":{"a":20},"zone":"US""#),
        178,
        "Zone range: { a: 5 } -->> { a: 20 }  on  US is overlapping with existing: { a: 0 } -->> { a: 10 }  on  EU",
    );
    fails(
        &upd(&mut m, r#""z.t","min":{"b":20},"max":{"b":30},"zone":"US""#),
        61,
        "min: { b: 20 } is not a prefix of the shard key { a: 1 } of ns: z.t",
    );
    ok(&upd(&mut m, r#""z.t","min":{"a":10},"max":{"a":20},"zone":"US""#));
    ok(&upd(&mut m, r#""z.t","min":{"a":{"$minKey":1}},"max":{"a":-50},"zone":"US""#));
    fails(
        &upd(&mut m, r#""nodot","min":{"a":1},"max":{"a":2},"zone":"US""#),
        73,
        "invalid namespace specified for request",
    );
    fails(
        &upd(&mut m, r#""z.t","min":5,"max":{"a":40},"zone":"US""#),
        14,
        "BSON field 'updateZoneKeyRange.min' is the wrong type 'int', expected type 'object'",
    );
    fails(
        &upd(&mut m, r#""z.t","min":{"a":30},"max":{"a":40},"zone":5"#),
        14,
        "Could not deserialize StringOrNull with type 16",
    );
    fails(
        &upd(&mut m, r#""z.t","min":{"a":30},"max":{"a":40}"#),
        40414,
        "BSON field 'updateZoneKeyRange.zone' is missing but a required field",
    );
    fails(
        &m.run(
            "z",
            json(r#"{"updateZoneKeyRange":"z.t","min":{"a":30},"max":{"a":40},"zone":"US"}"#),
        ),
        13,
        "updateZoneKeyRange may only be run against the admin database.",
    );
    // Compound key: missing fields are `MinKey`; a hashed field takes `NumberLong`s.
    ok(&upd(&mut m, r#""z.k","min":{"a":0},"max":{"a":1},"zone":"EU""#));
    fails(
        &upd(&mut m, r#""z.k","min":{"a":5,"b":1,"c":1},"max":{"a":6},"zone":"EU""#),
        61,
        "min: { a: 5, b: 1, c: 1 } is not a prefix of the shard key { a: 1, b: 1 } of ns: z.k",
    );
    ok(&upd(
        &mut m,
        r#""z.h","min":{"a":{"$numberLong":"0"}},"max":{"a":{"$numberLong":"100"}},"zone":"EU""#,
    ));
    fails(
        &upd(&mut m, r#""z.h","min":{"a":5},"max":{"a":6},"zone":"EU""#),
        72,
        "there exists a field in the range { a: 5 } -->> { a: 6 } not of type NumberLong, MinKey, or MaxKey which corresponds to a hashed field in the shard key pattern { a: \"hashed\" }",
    );
    // A collection without a shard key keeps its bounds as given, all with the same fields.
    ok(&upd(&mut m, r#""z.nope","min":{"a":30},"max":{"a":40},"zone":"US""#));
    fails(
        &upd(&mut m, r#""z.nope","min":{"a":35},"max":{"a":36},"zone":"EU""#),
        178,
        "Zone range: { a: 35 } -->> { a: 36 }  on  EU is overlapping with existing: { a: 30 } -->> { a: 40 }  on  US",
    );
    fails(
        &upd(&mut m, r#""z.nope","min":{"b":35},"max":{"b":36},"zone":"EU""#),
        16634,
        "field names of bound { a: 30 } do not match those of keyPattern { b: 1 }",
    );
    let z = zones(&m);
    let k: Vec<_> = z.ranges.iter().filter(|r| r.ns == "z.k").collect();
    let min = Document::from(teseladb_bson::RawDoc::from_bytes(&k[0].min).unwrap());
    assert_eq!(min, doc(&[("a", Value::Int32(0)), ("b", Value::MinKey)]));
    assert!(z.ranges.iter().filter(|r| r.ns != "z.nope").all(|r| r.keys.is_some()));
    assert!(z.ranges.iter().filter(|r| r.ns == "z.nope").all(|r| r.keys.is_none()));
    // Removing takes the interval with exactly those bounds.
    let before = zones(&m).ranges.len();
    ok(&upd(&mut m, r#""z.t","min":{"a":0},"max":{"a":9},"zone":null"#));
    assert_eq!(zones(&m).ranges.len(), before);
    ok(&upd(&mut m, r#""z.t","min":{"a":0},"max":{"a":10},"zone":null"#));
    assert_eq!(zones(&m).ranges.len(), before - 1);
}

#[test]
fn intervals_take_the_keys_of_the_collection_and_follow_its_key() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"addShardToZone":"node1","zone":"EU"}"#));
    // Given before the collection has a shard key: no keys until it has one.
    let r = r#"{"updateZoneKeyRange":"z.later","min":{"a":0},"max":{"a":10},"zone":"EU"}"#;
    ok(&admin(&mut m, r));
    assert_eq!(zones(&m).ranges[0].keys, None);
    ok(&m.run("z", json(r#"{"insert":"later","documents":[{"_id":1,"a":5}]}"#)));
    let index = r#"{"createIndexes":"later","indexes":[{"key":{"a":1,"b":1},"name":"a_1_b_1"}]}"#;
    ok(&m.run("z", json(index)));
    ok(&admin(&mut m, r#"{"shardCollection":"z.later","key":{"a":1,"b":1}}"#));
    let (start, end) = zones(&m).ranges[0].keys.clone().expect("keys");
    let doc_key = m.data.keys().find(|k| k.as_slice() > start.as_slice()).cloned().expect("doc");
    assert!(start < doc_key && doc_key < end, "the document is inside the interval");
    // A write that cannot go through for a while: the change goes again.
    m.group0_conflicts = 1;
    ok(&admin(&mut m, r#"{"addShardToZone":"node2","zone":"EU"}"#));
    m.group0_conflicts = usize::MAX;
    let r = admin(&mut m, r#"{"addShardToZone":"node2","zone":"US"}"#);
    assert_eq!(r.get("code"), Some(&Value::Int32(112)), "{r:?}");
}

/// Refusals of the reference router 9.0.4 (measured on 03-11-2026), an interval with the bounds of
/// another interval's minimum, and intervals whose bounds name no shard key of their collection.
#[test]
fn zone_edges() {
    let mut m = cluster();
    for (v, t) in [("5", "int"), ("true", "bool")] {
        fails(
            &admin(&mut m, &format!(r#"{{"removeShardFromZone":{v},"zone":"x"}}"#)),
            14,
            &format!("\"removeShardFromZone\" had the wrong type. Expected string, found {t}"),
        );
    }
    for (field, cmd) in [
        ("min", r#"{"updateZoneKeyRange":"z.t","max":{"a":1},"zone":null}"#),
        ("max", r#"{"updateZoneKeyRange":"z.t","min":{"a":1},"zone":null}"#),
    ] {
        fails(
            &admin(&mut m, cmd),
            40414,
            &format!("BSON field 'updateZoneKeyRange.{field}' is missing but a required field"),
        );
    }
    fails(
        &admin(&mut m, r#"{"updateZoneKeyRange":"z.","min":{"a":1},"max":{"a":2},"zone":null}"#),
        73,
        "invalid namespace specified for request",
    );
    ok(&admin(&mut m, r#"{"addShardToZone":"node1","zone":"EU"}"#));
    ok(&admin(&mut m, r#"{"updateZoneKeyRange":"z.t","min":{"a":0},"max":{"a":10},"zone":"EU"}"#));
    fails(
        &admin(&mut m, r#"{"updateZoneKeyRange":"z.t","min":{"a":0},"max":{"a":20},"zone":"EU"}"#),
        178,
        "Zone range: { a: 0 } -->> { a: 20 }  on  EU is overlapping with existing: { a: 0 } -->> { a: 10 }  on  EU",
    );
    // A collection with no shard key: its interval has no keys, and keeps none when the shard
    // key given does not start with the bounds' fields.
    ok(&m.run("z", json(r#"{"create":"u"}"#)));
    ok(&admin(&mut m, r#"{"updateZoneKeyRange":"z.u","min":{"b":0},"max":{"b":10},"zone":"EU"}"#));
    ok(&admin(&mut m, r#"{"shardCollection":"z.u","key":{"a":1}}"#));
    let of_u: Vec<_> = zones(&m).ranges.into_iter().filter(|r| r.ns == "z.u").collect();
    assert_eq!(of_u.len(), 1);
    assert_eq!(of_u[0].keys, None);
}

/// A name with no database, and the same range for another zone, refused as the reference router
/// 9.0.4 refuses them (measured on 03-11-2026, `z3.js`); the range then leaves its zone with `zone:
/// null`.
#[test]
fn an_empty_database_and_the_same_range_in_another_zone() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"addShardToZone":"node1","zone":"A"}"#));
    ok(&admin(&mut m, r#"{"addShardToZone":"node1","zone":"B"}"#));
    fails(
        &admin(&mut m, r#"{"updateZoneKeyRange":".t","min":{"a":1},"max":{"a":5},"zone":"A"}"#),
        73,
        "invalid namespace specified for request",
    );
    let range = r#""min":{"a":100},"max":{"a":105}"#;
    ok(&admin(&mut m, &format!(r#"{{"updateZoneKeyRange":"z.t",{range},"zone":"A"}}"#)));
    fails(
        &admin(&mut m, &format!(r#"{{"updateZoneKeyRange":"z.t",{range},"zone":"B"}}"#)),
        178,
        "Zone range: { a: 100 } -->> { a: 105 }  on  B is overlapping with existing: { a: 100 } -->> { a: 105 }  on  A",
    );
    ok(&admin(&mut m, &format!(r#"{{"updateZoneKeyRange":"z.t",{range},"zone":null}}"#)));
}
