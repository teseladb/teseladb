//! 018 T008: the chunks of a collection with a shard key and `split`, `splitVector`,
//! `moveChunk`, `moveRange`, `mergeChunks` and `clearJumboFlag` (FR-004, FR-006) over the
//! in-memory node of `common`, whose router knows ranges that the commands' Group 0 orders change
//! at once; replies and messages of a `refrouter` 9.0.4 of two shards (measured on 02-11-2026).

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value, extjson};
use teseladb_keys::KeyBuilder;
use teseladb_meta::command::Group0Command;
use teseladb_router::table::RouteEntry;
use teseladb_types::{GroupId, NodeId};
use teseladb_wire::commands::chunks::{collection_chunks, config_chunks};
use teseladb_wire::commands::sharding::shard_key_of;

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

/// The answer of a `refrouter` without a code: `ok: 0` and `errmsg` alone.
fn fails_uncoded(r: &Document, msg: &str) {
    assert_eq!(r, &doc(&[("ok", Value::Double(0.0)), ("errmsg", Value::from(msg))]), "{r:?}");
}

/// Two shards, one range over everything led by node 1, and `z.k` sharded by `{a: 1}` with
/// documents `a` 0 to 19.
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
    let docs: Vec<String> = (0..20).map(|i| format!(r#"{{"_id":{i},"a":{i}}}"#)).collect();
    ok(&m.run("z", json(&format!(r#"{{"insert":"k","documents":[{}]}}"#, docs.join(",")))));
    m
}

/// `z.k`'s chunks as `(min a, max a, node)`, `None` for `MinKey` and `MaxKey`.
fn chunks(m: &Mem) -> Vec<(Option<i32>, Option<i32>, u64)> {
    let (_, c) = m.catalog.collections.iter().find(|(ns, _)| ns.coll == "k").unwrap();
    let key = shard_key_of(c).unwrap().unwrap();
    let a = |d: &Document| match d.get("a") {
        Some(Value::Int32(n)) => Some(*n),
        _ => None,
    };
    collection_chunks(&m.routes, c, &key).iter().map(|c| (a(&c.min), a(&c.max), c.node.0)).collect()
}

#[test]
fn a_split_at_middle_cuts_the_range_at_the_shard_key_value() {
    let mut m = cluster();
    assert_eq!(chunks(&m), [(None, None, 1)]);
    ok(&admin(&mut m, r#"{"split":"z.k","middle":{"a":10}}"#));
    assert_eq!(chunks(&m), [(None, Some(10), 1), (Some(10), None, 1)]);
    // Again at a bound, or at MinKey: nothing to cut.
    ok(&admin(&mut m, r#"{"split":"z.k","middle":{"a":10},"bogus":1}"#));
    ok(&admin(&mut m, r#"{"split":"z.k","middle":{"a":{"$minKey":1}}}"#));
    assert_eq!(m.range_orders.len(), 1);
    // `find`: the middle document of its chunk, [MinKey, 10) with a 0..9 cuts at 5.
    ok(&admin(&mut m, r#"{"split":"z.k","find":{"a":3}}"#));
    assert_eq!(chunks(&m), [(None, Some(5), 1), (Some(5), Some(10), 1), (Some(10), None, 1)]);
    ok(&admin(&mut m, r#"{"split":"z.k","bounds":[{"a":10},{"a":{"$maxKey":1}}]}"#));
    assert_eq!(chunks(&m)[2..], [(Some(10), Some(15), 1), (Some(15), None, 1)]);
    // Fields past the shard key in `find` do not matter.
    ok(&admin(&mut m, r#"{"split":"z.k","find":{"a":1,"b":2}}"#));
    assert_eq!(chunks(&m)[..2], [(None, Some(2), 1), (Some(2), Some(5), 1)]);
    // A chunk with no documents, or whose documents all have its start's value, cannot be cut
    // (measured on 03-11-2026).
    let orders = m.range_orders.len();
    ok(&m.run("z", json(r#"{"delete":"k","deletes":[{"q":{"a":{"$gte":15}},"limit":0}]}"#)));
    let empty = "can't open a cursor to scan the range (desired range is possibly empty)";
    fails(&admin(&mut m, r#"{"split":"z.k","find":{"a":16}}"#), 96, empty);
    fails(&admin(&mut m, r#"{"split":"z.k","bounds":[{"a":15},{"a":{"$maxKey":1}}]}"#), 96, empty);
    ok(&admin(&mut m, r#"{"split":"z.k","middle":{"a":11}}"#));
    ok(&m.run("z", json(r#"{"delete":"k","deletes":[{"q":{"a":{"$gt":10}},"limit":0}]}"#)));
    fails(
        &admin(&mut m, r#"{"split":"z.k","find":{"a":10}}"#),
        87,
        "Unable to find median in chunk because chunk is indivisible.",
    );
    assert_eq!(m.range_orders.len(), orders + 1);
}

/// `split`'s checks of `find` and `bounds`, as a `refrouter` 9.0.4 makes them (measured on
/// 03-11-2026, `q.js` in the lane's notes).
#[test]
fn split_checks_find_and_bounds_as_a_refrouter() {
    let mut m = cluster();
    let split = |m: &mut Mem, rest: &str| admin(m, &format!(r#"{{"split":"z.k",{rest}}}"#));
    fails_uncoded(
        &split(&mut m, r#""find":5"#),
        "wrong type for 'find' field, expected object, found find: 5",
    );
    fails_uncoded(
        &split(&mut m, r#""find":{"b":1}"#),
        "no shard key found in chunk query { b: 1 }",
    );
    fails_uncoded(
        &split(&mut m, r#""bounds":"x""#),
        r#"wrong type for 'bounds' field, expected array, found bounds: "x""#,
    );
    fails_uncoded(&split(&mut m, r#""bounds":[]"#), "need to specify find/bounds or middle");
    fails_uncoded(&split(&mut m, r#""bounds":[{"a":1}]"#), "upper bound not specified");
    fails(&split(&mut m, r#""bounds":[1,2]"#), 10065, "invalid parameter: expected an object (0)");
    fails(
        &split(&mut m, r#""bounds":[{"a":1},2]"#),
        10065,
        "invalid parameter: expected an object (1)",
    );
    fails_uncoded(
        &split(&mut m, r#""bounds":[{"b":1},{"b":2}]"#),
        "shard key bounds [{ b: 1 },{ b: 2 }) are not valid for shard key pattern { a: 1 }",
    );
    fails_uncoded(
        &split(&mut m, r#""bounds":[{"a":1},{"a":2},{"a":3}]"#),
        "no chunk found with the shard key bounds [{ a: 1 }, { a: 2 })",
    );
    fails(
        &admin(&mut m, r#"{"split":"z.","middle":{"a":1}}"#),
        73,
        "Invalid namespace specified 'z.'",
    );
    // An empty collection: `middle` cuts, `find` and `bounds` cannot.
    ok(&admin(&mut m, r#"{"shardCollection":"z.e","key":{"a":1}}"#));
    let on_empty = "cannot use split with find or bounds option on an empty collection";
    fails(&admin(&mut m, r#"{"split":"z.e","find":{"a":1}}"#), 72, on_empty);
    fails(
        &admin(&mut m, r#"{"split":"z.e","bounds":[{"a":{"$minKey":1}},{"a":{"$maxKey":1}}]}"#),
        72,
        on_empty,
    );
    ok(&admin(&mut m, r#"{"split":"z.e","middle":{"a":5}}"#));
}

/// A chunk of more documents than one read of the middle's search takes.
#[test]
fn split_finds_the_middle_of_a_large_chunk() {
    let mut m = cluster();
    let docs: Vec<String> = (20..4_120).map(|i| format!(r#"{{"_id":{i},"a":{i}}}"#)).collect();
    ok(&m.run("z", json(&format!(r#"{{"insert":"k","documents":[{}]}}"#, docs.join(",")))));
    ok(&admin(&mut m, r#"{"split":"z.k","find":{"a":7}}"#));
    assert_eq!(chunks(&m), [(None, Some(2_060), 1), (Some(2_060), None, 1)]);
}

/// Orders Group 0 has not applied yet: `moveRange` and `mergeChunks` wait for them a while, and
/// go on when the wait is over or cannot go on.
#[test]
fn move_range_and_merge_wait_for_orders_not_applied() {
    for pauses in [0, 1_000] {
        let mut m = cluster();
        ok(&admin(&mut m, r#"{"split":"z.k","middle":{"a":10}}"#));
        m.hold_orders = true;
        m.pauses_left = pauses;
        ok(&admin(&mut m, r#"{"moveRange":"z.k","min":{"a":5},"toShard":"node2"}"#));
        ok(&admin(
            &mut m,
            r#"{"mergeChunks":"z.k","bounds":[{"a":{"$minKey":1}},{"a":{"$maxKey":1}}]}"#,
        ));
        assert_eq!(chunks(&m), [(None, Some(10), 1), (Some(10), None, 1)]);
        let kinds: Vec<&str> = m
            .range_orders
            .iter()
            .map(|o| match o {
                Group0Command::SplitRange { .. } => "split",
                Group0Command::SetReplicas { .. } => "replicas",
                Group0Command::SetLeaderHint { .. } => "leader",
                Group0Command::MergeRanges { .. } => "merge",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["split", "split", "replicas", "leader", "merge"], "{pauses}");
    }
}

/// The bounds of a hashed shard key are `NumberLong`s, as a `refrouter` 9.0.4 lists them.
#[test]
fn chunks_of_a_hashed_shard_key_have_long_bounds() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"shardCollection":"z.h","key":{"h":"hashed"}}"#));
    ok(&admin(&mut m, r#"{"split":"z.h","middle":{"h":{"$numberLong":"5"}}}"#));
    let docs = config_chunks(&m.routes, m.catalog.collections.iter());
    let five = Value::Document(json(r#"{"h":{"$numberLong":"5"}}"#));
    assert_eq!(field(&docs[0], "max"), &five);
    assert_eq!(field(&docs[1], "min"), &five);
}

/// Ranges cut where no shard key value starts, ranges out of the collection, a range with no
/// leader known, and a collection with no shard key, as `config.chunks` shows them.
#[test]
fn config_chunks_of_odd_ranges() {
    let mut m = cluster();
    ok(&m.run("z", json(r#"{"create":"u"}"#)));
    let (_, c) = m.catalog.collections.iter().find(|(ns, _)| ns.coll == "k").unwrap();
    let lo = KeyBuilder::index_prefix(c.tenant, c.id, teseladb_keys::DOCUMENTS).as_bytes().to_vec();
    ok(&admin(&mut m, r#"{"split":"z.k","middle":{"a":10}}"#));
    let cut = m.routes[0].end.clone();
    // The class byte of `a`'s value alone: no value can be read from it.
    let odd = cut[..=lo.len()].to_vec();
    let class = odd[lo.len()..].to_vec();
    let entry = |group: u64, start: Vec<u8>, end: Vec<u8>, leader: Option<NodeId>| RouteEntry {
        group: GroupId(group),
        start,
        end,
        core: 0,
        voters: vec![NodeId(2)],
        generation: 1,
        leader_hint: leader,
        moved_ms: 0,
        replica_epoch: 1,
    };
    m.routes = vec![
        entry(1, Vec::new(), lo.clone(), Some(NodeId(1))),
        entry(2, lo, odd.clone(), None),
        entry(3, odd.clone(), Vec::new(), Some(NodeId(1))),
    ];
    let docs = config_chunks(&m.routes, m.catalog.collections.iter());
    let at = |i: usize, f: &str| field(&docs[i], f).clone();
    assert_eq!(docs.len(), 2);
    assert_eq!(at(0, "shard"), Value::from("node2"));
    let raw = Value::Binary { subtype: 0, bytes: class };
    assert_eq!(at(0, "max"), Value::Document(Document::new().with("a", raw)));
    assert_eq!(at(1, "shard"), Value::from("node1"));
}

#[test]
fn split_refuses_what_a_refrouter_refuses() {
    let mut m = cluster();
    fails_uncoded(
        &admin(&mut m, r#"{"split":"z.k","bounds":[{"a":12},{"a":{"$maxKey":1}}]}"#),
        "no chunk found with the shard key bounds [{ a: 12 }, { a: MaxKey })",
    );
    fails_uncoded(
        &admin(&mut m, r#"{"split":"z.k","middle":{"b":1}}"#),
        "new split key { b: 1 } is not valid for shard key pattern { a: 1 }",
    );
    fails_uncoded(
        &admin(&mut m, r#"{"split":"z.k","middle":{"a":1,"b":2}}"#),
        "new split key { a: 1, b: 2 } is not valid for shard key pattern { a: 1 }",
    );
    fails_uncoded(&admin(&mut m, r#"{"split":"z.k"}"#), "need to specify find/bounds or middle");
    fails_uncoded(
        &admin(&mut m, r#"{"split":"z.k","middle":{"a":1},"find":{"a":1}}"#),
        "cannot specify find and middle together",
    );
    fails_uncoded(
        &admin(&mut m, r#"{"split":"z.k","middle":5}"#),
        "wrong type for 'middle' field, expected object, found middle: 5",
    );
    ok(&m.run("z", json(r#"{"create":"u"}"#)));
    for ns in ["z.nope", "z.u"] {
        fails(
            &admin(&mut m, &format!(r#"{{"split":"{ns}","middle":{{"a":1}}}}"#)),
            118,
            &format!("Command not supported on unsharded collection {ns}"),
        );
    }
    fails(
        &admin(&mut m, r#"{"split":"nodot","middle":{"a":1}}"#),
        73,
        "Invalid namespace specified 'nodot'",
    );
    fails(
        &admin(&mut m, r#"{"split":5,"middle":{"a":1}}"#),
        14,
        "BSON field 'split.split' is the wrong type 'int', expected type 'string'",
    );
    fails(
        &m.run("z", json(r#"{"split":"z.k","middle":{"a":1}}"#)),
        13,
        "split may only be run against the admin database.",
    );
}

#[test]
fn split_vector_is_refused_as_through_a_refrouter() {
    let mut m = cluster();
    fails(
        &admin(&mut m, r#"{"splitVector":"z.k","keyPattern":{"a":1},"maxChunkSizeBytes":100}"#),
        20,
        "Performing splitVector across dbs isn't supported via refrouter",
    );
    fails(
        &m.run("z", json(r#"{"splitVector":"z.k","keyPattern":{"a":1}}"#)),
        72,
        "The range {min: {} , max{} } for the namespace z.k is required to be owned by one shard",
    );
    fails(
        &m.run("z", json(r#"{"splitVector":5}"#)),
        14,
        "BSON field 'splitVector.splitVector' is the wrong type 'int', expected type 'string'",
    );
}

#[test]
fn move_chunk_sends_the_range_to_the_shard() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"split":"z.k","middle":{"a":10}}"#));
    let r = admin(&mut m, r#"{"moveChunk":"z.k","find":{"a":3},"to":"node2","bogus":1}"#);
    ok(&r);
    assert!(matches!(r.get("millis"), Some(Value::Int64(_))), "{r:?}");
    assert_eq!(chunks(&m), [(None, Some(10), 2), (Some(10), None, 1)]);
    let g = GroupId(1);
    assert_eq!(
        m.range_orders[1..],
        [
            Group0Command::SetReplicas {
                group: g,
                epoch: 1,
                add: vec![NodeId(2)],
                remove: vec![NodeId(1)]
            },
            Group0Command::SetLeaderHint { group: g, node: NodeId(2), term: 0 },
        ]
    );
    // Already there: nothing to do; a replica already on the node only moves the leadership.
    ok(&admin(&mut m, r#"{"moveChunk":"z.k","find":{"a":3},"to":"node2"}"#));
    assert_eq!(m.range_orders.len(), 3);
    m.routes[1].voters.push(NodeId(2));
    ok(&admin(
        &mut m,
        r#"{"moveChunk":"z.k","bounds":[{"a":10},{"a":{"$maxKey":1}}],"to":"node2"}"#,
    ));
    assert_eq!(
        m.range_orders[3..],
        [Group0Command::SetLeaderHint { group: GroupId(2), node: NodeId(2), term: 0 }]
    );
    fails(
        &admin(&mut m, r#"{"moveChunk":"z.k","find":{"a":3},"to":"nope"}"#),
        70,
        "Could not find destination shard :: caused by :: Shard nope not found",
    );
    fails(
        &admin(&mut m, r#"{"moveChunk":"z.k","bounds":[{"a":10},{"a":15}],"to":"node2"}"#),
        656452,
        "no chunk found with the shard key bounds [{ a: 10 },{ a: 15 })",
    );
    fails(
        &admin(&mut m, r#"{"moveChunk":"z.k","to":"node2"}"#),
        72,
        "need to specify query or bounds",
    );
    fails(
        &admin(&mut m, r#"{"moveChunk":"z.k","find":{"b":3},"to":"node2"}"#),
        656450,
        "no shard key found in chunk query { b: 3 }",
    );
    fails(
        &admin(&mut m, r#"{"moveChunk":"z.k","find":{"a":3}}"#),
        40414,
        "BSON field 'moveChunk.to' is missing but a required field",
    );
    fails(
        &admin(&mut m, r#"{"moveChunk":"z.nope","find":{"a":3},"to":"node2"}"#),
        118,
        "Command not supported on unsharded collection z.nope",
    );
    // Group 0's refusal comes back.
    m.refuse_orders = Some(teseladb_wire::error::CommandError::new(
        teseladb_wire::error::ILLEGAL_OPERATION,
        "refused",
    ));
    fails(&admin(&mut m, r#"{"moveChunk":"z.k","find":{"a":3},"to":"node1"}"#), 20, "refused");
}

#[test]
fn move_range_cuts_its_range_out_and_moves_it() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"moveRange":"z.k","min":{"a":10},"toShard":"node2","bogus":1}"#));
    assert_eq!(chunks(&m), [(None, Some(10), 1), (Some(10), None, 2)]);
    ok(&admin(&mut m, r#"{"moveRange":"z.k","min":{"a":12},"max":{"a":14},"toShard":"node1"}"#));
    assert_eq!(
        chunks(&m),
        [
            (None, Some(10), 1),
            (Some(10), Some(12), 2),
            (Some(12), Some(14), 1),
            (Some(14), None, 2)
        ]
    );
    ok(&admin(&mut m, r#"{"moveRange":"z.k","max":{"a":3},"toShard":"node2"}"#));
    assert_eq!(chunks(&m)[..2], [(None, Some(3), 2), (Some(3), Some(10), 1)]);
    fails(
        &admin(&mut m, r#"{"moveRange":"z.k","toShard":"node1"}"#),
        72,
        "Missing required parameter 'min' or 'max'",
    );
    fails(
        &admin(&mut m, r#"{"moveRange":"z.k","min":{"a":10}}"#),
        40414,
        "BSON field 'moveRange.toShard' is missing but a required field",
    );
    fails(
        &admin(&mut m, r#"{"moveRange":"z.nope","min":{"a":10},"toShard":"node2"}"#),
        26,
        "Collection 'z.nope' not found in cluster catalog",
    );
    fails(
        &admin(&mut m, r#"{"moveRange":"z.k","min":{"b":10},"toShard":"node2"}"#),
        72,
        "The 'min' bound { b: 10 } is not valid for shard key pattern { a: 1 }",
    );
    fails(
        &admin(&mut m, r#"{"moveRange":"z.k","min":5,"toShard":"node2"}"#),
        14,
        "BSON field 'moveRange.min' is the wrong type 'int', expected type 'object'",
    );
    fails(
        &admin(&mut m, r#"{"moveRange":"z.k","min":{"a":1},"toShard":"nope"}"#),
        70,
        "Could not find destination shard :: caused by :: Shard nope not found",
    );
}

#[test]
fn merge_chunks_joins_the_chunks_of_one_shard() {
    let mut m = cluster();
    for at in [5, 10, 15] {
        ok(&admin(&mut m, &format!(r#"{{"split":"z.k","middle":{{"a":{at}}}}}"#)));
    }
    ok(&admin(&mut m, r#"{"moveChunk":"z.k","find":{"a":16},"to":"node2"}"#));
    ok(&admin(&mut m, r#"{"mergeChunks":"z.k","bounds":[{"a":{"$minKey":1}},{"a":5}]}"#));
    ok(&admin(&mut m, r#"{"mergeChunks":"z.k","bounds":[{"a":{"$minKey":1}},{"a":15}]}"#));
    assert_eq!(chunks(&m), [(None, Some(15), 1), (Some(15), None, 2)]);
    let refused = |min: &str, max: &str| {
        format!(
            "Exceeded maximum number of 10 retries attempting 'mergeChunks' :: caused by :: Range \
             with bounds [{min}, {max}) is not owned by this shard."
        )
    };
    fails(
        &admin(
            &mut m,
            r#"{"mergeChunks":"z.k","bounds":[{"a":{"$minKey":1}},{"a":{"$maxKey":1}}]}"#,
        ),
        13388,
        &refused("{ a: MinKey }", "{ a: MaxKey }"),
    );
    fails(
        &admin(&mut m, r#"{"mergeChunks":"z.k","bounds":[{"a":5},{"a":14}]}"#),
        13388,
        &refused("{ a: 5 }", "{ a: 14 }"),
    );
    fails(
        &admin(&mut m, r#"{"mergeChunks":"z.k"}"#),
        40414,
        "BSON field 'mergeChunks.bounds' is missing but a required field",
    );
    fails(
        &admin(&mut m, r#"{"mergeChunks":"z.k","bounds":[{"a":1}]}"#),
        2,
        "only a min and max bound may be specified",
    );
    fails(
        &admin(&mut m, r#"{"mergeChunks":"z.k","bounds":5}"#),
        14,
        "BSON field 'mergeChunks.bounds' is the wrong type 'int', expected type 'array'",
    );
    fails(
        &admin(&mut m, r#"{"mergeChunks":"z.nope","bounds":[{"a":1},{"a":2}]}"#),
        118,
        "Command not supported on unsharded collection z.nope",
    );
}

#[test]
fn clear_jumbo_flag_finds_the_chunk() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"split":"z.k","middle":{"a":10}}"#));
    ok(&admin(&mut m, r#"{"clearJumboFlag":"z.k","find":{"a":3},"bogus":1}"#));
    ok(&admin(&mut m, r#"{"clearJumboFlag":"z.k","bounds":[{"a":10},{"a":{"$maxKey":1}}]}"#));
    fails(
        &admin(&mut m, r#"{"clearJumboFlag":"z.k","bounds":[{"a":12},{"a":14}]}"#),
        51261,
        "no chunk found with the shard key bounds [{ a: 12 }, { a: 14 })",
    );
    fails(&admin(&mut m, r#"{"clearJumboFlag":"z.k"}"#), 72, "need to specify find or bounds");
    fails(
        &admin(&mut m, r#"{"clearJumboFlag":"z.nope","find":{"a":3}}"#),
        118,
        "Command not supported on unsharded collection z.nope",
    );
}

#[test]
fn config_chunks_shows_each_chunk_in_shard_key_values() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"split":"z.k","middle":{"a":10}}"#));
    ok(&admin(&mut m, r#"{"moveChunk":"z.k","find":{"a":12},"to":"node2"}"#));
    ok(&admin(&mut m, r#"{"shardCollection":"z.h","key":{"h":"hashed"}}"#));
    let docs = config_chunks(&m.routes, m.catalog.collections.iter());
    let shown: Vec<(Value, Value, Value)> = docs
        .iter()
        .map(|d| (field(d, "min").clone(), field(d, "max").clone(), field(d, "shard").clone()))
        .collect();
    let d = |t: &str| Value::Document(json(t));
    assert_eq!(
        shown,
        [
            (d(r#"{"h":{"$minKey":1}}"#), d(r#"{"h":{"$maxKey":1}}"#), Value::from("node2")),
            (d(r#"{"a":{"$minKey":1}}"#), d(r#"{"a":10}"#), Value::from("node1")),
            (d(r#"{"a":10}"#), d(r#"{"a":{"$maxKey":1}}"#), Value::from("node2")),
        ]
    );
    let first = &docs[1];
    for f in ["_id", "uuid", "lastmod", "onCurrentShardSince", "history"] {
        assert!(first.get(f).is_some(), "{f} in {first:?}");
    }
    assert_eq!(first.get("jumbo"), Some(&Value::Boolean(false)));
}

/// The `bounds` of `moveChunk`, `clearJumboFlag` and `mergeChunks` as their parsers check them,
/// with the messages of a `refrouter` 9.0.4 (measured on 03-11-2026).
#[test]
fn bounds_are_checked_as_a_refrouter_checks_them() {
    let mut m = cluster();
    for (name, extra) in [("moveChunk", r#","to":"node2""#), ("clearJumboFlag", "")] {
        let run =
            |m: &mut Mem, b: &str| admin(m, &format!(r#"{{"{name}":"z.k","bounds":{b}{extra}}}"#));
        fails(
            &run(&mut m, r#""x""#),
            14,
            &format!(
                "BSON field '{name}.bounds' is the wrong type 'string', expected type 'array'"
            ),
        );
        fails(&run(&mut m, r#"[{"a":1}]"#), 72, "bounds can only have exactly 2 elements");
        fails(
            &run(&mut m, "[1,2]"),
            14,
            &format!(
                "BSON field '{name}.bounds.0' is the wrong type 'int', expected type 'object'"
            ),
        );
    }
    let merge =
        |m: &mut Mem, b: &str| admin(m, &format!(r#"{{"mergeChunks":"z.k","bounds":{b}}}"#));
    fails(
        &merge(&mut m, r#"[{"a":1},2]"#),
        14,
        "BSON field 'mergeChunks.bounds.1' is the wrong type 'int', expected type 'object'",
    );
    fails(
        &merge(&mut m, r#"[{"a":1},{"a":2},{"a":3}]"#),
        2,
        "only a min and max bound may be specified",
    );
    fails(
        &merge(&mut m, r#"[{"b":1},{"b":2}]"#),
        72,
        "shard key bounds [{ b: 1 },{ b: 2 }) are not valid for shard key pattern { a: 1 }",
    );
    fails(
        &merge(&mut m, r#"[{"b":2},{"b":1}]"#),
        2,
        "min: { b: 2 } should be less than max: { b: 1 }",
    );
    fails(
        &merge(&mut m, r#"[{"a":5},{"a":5}]"#),
        2,
        "min: { a: 5 } should be less than max: { a: 5 }",
    );
}

/// `moveRange` checks its arguments in the order of a `refrouter` 9.0.4: the bounds given, the
/// shard, the collection and then the bounds against the shard key (measured on 03-11-2026).
#[test]
fn move_range_checks_in_the_order_of_a_refrouter() {
    let mut m = cluster();
    let run = |m: &mut Mem, rest: &str| admin(m, &format!(r#"{{"moveRange":{rest}}}"#));
    fails(
        &run(&mut m, r#"5,"toShard":"node2","min":{"a":1}"#),
        14,
        "BSON field 'moveRange.moveRange' is the wrong type 'int', expected type 'string'",
    );
    fails(&run(&mut m, r#""z.k","toShard":"x""#), 72, "Missing required parameter 'min' or 'max'");
    let no_shard = "Could not find destination shard :: caused by :: Shard x not found";
    fails(&run(&mut m, r#""z.nope","toShard":"x","min":{"a":1}"#), 70, no_shard);
    fails(&run(&mut m, r#""z.k","toShard":"x","min":{"b":1}"#), 70, no_shard);
    for (ns, shown) in [("z", "z"), ("z.", "z"), (".k", ".k")] {
        fails(
            &run(&mut m, &format!(r#""{ns}","toShard":"node2","min":{{"a":1}}"#)),
            26,
            &format!("Collection '{shown}' not found in cluster catalog"),
        );
    }
    // To the shard the range is on already: nothing moves.
    let orders = m.range_orders.len();
    ok(&run(&mut m, r#""z.k","toShard":"node1","min":{"a":{"$minKey":1}}"#));
    assert_eq!(m.range_orders.len(), orders);
}
