//! 018 T008: `listShards`, `balancerStart`, `balancerStop`, `balancerStatus`,
//! `configureCollectionBalancing` and `balancerCollectionStatus` (FR-004, FR-005, FR-018) over
//! the in-memory node of `common`, with the replies and messages of a `refrouter` 9.0.4 of two
//! shards (measured on 02-11-2026).

mod common;

use common::{Mem, doc, field};
use teseladb_wire::commands::balancing::max_chunk_bytes;
use teseladb_wire::generic::check_unknown_fields;
use teseladb_bson::{Document, Value, extjson};

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

/// Two shards, `config.settings` and `z.t` sharded by `{a: 1}`.
fn cluster() -> Mem {
    let mut m = Mem::new();
    m.shards = vec!["node1".into(), "node2".into()];
    m.settings = Some(Vec::new());
    ok(&admin(&mut m, r#"{"shardCollection":"z.t","key":{"a":1}}"#));
    m
}

/// `z.t`'s document of `config.collections` without its identity fields.
fn entry(m: &mut Mem) -> Document {
    let r = m.run("config", json(r#"{"find":"collections"}"#));
    let Value::Document(c) = field(&r, "cursor") else { panic!("{r:?}") };
    let Value::Array(batch) = field(c, "firstBatch") else { panic!("{r:?}") };
    let Value::Document(d) = &batch[0] else { panic!("{r:?}") };
    let mut out = Document::new();
    for (k, v) in d.iter() {
        if !["_id", "lastmodEpoch", "timestamp", "uuid", "key", "unique", "lastmod"].contains(&k) {
            out.push(k, v.clone());
        }
    }
    out
}

#[test]
fn list_shards_lists_config_shards_with_the_draining_filter() {
    let mut m = cluster();
    let r = admin(&mut m, r#"{"listShards":1}"#);
    let Value::Array(shards) = field(&r, "shards") else { panic!("{r:?}") };
    assert_eq!(shards.len(), 2);
    let r = admin(&mut m, r#"{"listShards":1,"filter":{}}"#);
    let Value::Array(shards) = field(&r, "shards") else { panic!("{r:?}") };
    assert_eq!(shards.len(), 2);
    let r = admin(&mut m, r#"{"listShards":1,"filter":{"draining":true}}"#);
    assert_eq!(field(&r, "shards"), &Value::Array(Vec::new()));
    let r = admin(&mut m, r#"{"listShards":1,"filter":{"draining":false}}"#);
    let Value::Array(shards) = field(&r, "shards") else { panic!("{r:?}") };
    assert_eq!(shards.len(), 2);
    fails(
        &admin(&mut m, r#"{"listShards":1,"filter":{"_id":"sh2"}}"#),
        40415,
        "BSON field 'listShardsRequest.filter._id' is an unknown field.",
    );
    fails(
        &admin(&mut m, r#"{"listShards":1,"filter":5}"#),
        14,
        "BSON field 'listShardsRequest.filter' is the wrong type 'int', expected type 'object'",
    );
    fails(
        &m.run("z", json(r#"{"listShards":1}"#)),
        13,
        "listShards may only be run against the admin database.",
    );
}

#[test]
fn the_balancer_stops_and_starts_through_config_settings() {
    let mut m = cluster();
    m.facts.start_ms = 1_000;
    m.facts.now_ms = 7_000;
    let r = admin(&mut m, r#"{"balancerStatus":1}"#);
    assert_eq!(field(&r, "mode"), &Value::from("full"));
    assert_eq!(field(&r, "inBalancerRound"), &Value::Boolean(false));
    assert_eq!(field(&r, "numBalancerRounds"), &Value::Int64(3));
    assert_eq!(field(&r, "term"), &Value::Int64(1));
    ok(&admin(&mut m, r#"{"balancerStop":"x"}"#));
    let balancer = m.settings.clone().unwrap_or_default();
    assert_eq!(balancer, vec![json(r#"{"_id":"balancer","mode":"off","stopped":true}"#)]);
    let r = admin(&mut m, r#"{"balancerStatus":1,"bogus":1}"#);
    assert_eq!(field(&r, "mode"), &Value::from("off"));
    ok(&admin(&mut m, r#"{"balancerStart":1,"bogus":1}"#));
    let balancer = m.settings.clone().unwrap_or_default();
    assert_eq!(balancer, vec![json(r#"{"_id":"balancer","mode":"full","stopped":false}"#)]);
    for name in ["balancerStart", "balancerStop", "balancerStatus"] {
        fails(
            &m.run("z", json(&format!(r#"{{"{name}":1}}"#))),
            13,
            &format!("{name} may only be run against the admin database."),
        );
    }
    // A write that keeps meeting another change gives up.
    m.settings_conflicts = 100;
    fails(&admin(&mut m, r#"{"balancerStop":1}"#), 112, "the balancer's settings kept changing");
    let r = m.run("config", json(r#"{"count":"settings"}"#));
    assert_eq!(field(&r, "n"), &Value::Int32(1));
}

#[test]
fn configure_collection_balancing_keeps_the_settings_with_the_shard_key() {
    let mut m = cluster();
    let base = json(r#"{"noBalance":false}"#);
    assert_eq!(entry(&mut m), base);
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","enableAutoMerger":false}"#));
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","chunkSize":64}"#));
    assert_eq!(
        entry(&mut m),
        json(
            r#"{"noBalance":false,"enableAutoMerge":false,
                "maxChunkSizeBytes":{"$numberLong":"67108864"}}"#
        )
    );
    let c = m.catalog.collections.values().next().cloned().unwrap_or_else(|| panic!("z.t"));
    assert_eq!(max_chunk_bytes(&c.options), Some(64 << 20));
    // Truncated MiB: 1.5 is 1 MiB, 1024.9 is 1 GiB, 0.5 and -0.5 drop it.
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","chunkSize":1.5}"#));
    let bytes = |m: &mut Mem| entry(m).get("maxChunkSizeBytes").cloned();
    assert_eq!(bytes(&mut m), Some(Value::Int64(1 << 20)));
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","chunkSize":1024.9}"#));
    assert_eq!(bytes(&mut m), Some(Value::Int64(1 << 30)));
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","chunkSize":-0.5}"#));
    assert_eq!(bytes(&mut m), None);
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","chunkSize":{"$numberLong":"8"}}"#));
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","chunkSize":0}"#));
    assert_eq!(bytes(&mut m), None);
    ok(&admin(
        &mut m,
        r#"{"configureCollectionBalancing":"z.t","chunkSize":{"$numberDecimal":"2"}}"#,
    ));
    assert_eq!(bytes(&mut m), Some(Value::Int64(2 << 20)));
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t"}"#));
    fails(
        &admin(&mut m, r#"{"configureCollectionBalancing":"z.t","chunkSize":2000}"#),
        72,
        "Chunk size '2000' out of range [1MB, 1GB]",
    );
    fails(
        &admin(&mut m, r#"{"configureCollectionBalancing":"z.t","chunkSize":-1}"#),
        72,
        "Chunk size '-1' out of range [1MB, 1GB]",
    );
    fails(
        &admin(&mut m, r#"{"configureCollectionBalancing":"z.t","chunkSize":"x"}"#),
        14,
        "BSON field 'configureCollectionBalancing.chunkSize' is the wrong type 'string', \
         expected types '[decimal, double, long, int]'",
    );
    fails(
        &admin(&mut m, r#"{"configureCollectionBalancing":"z.t","enableAutoMerger":1}"#),
        14,
        "BSON field 'configureCollectionBalancing.enableAutoMerger' is the wrong type 'int', \
         expected type 'bool'",
    );
    fails(
        &admin(&mut m, r#"{"configureCollectionBalancing":5}"#),
        14,
        "BSON field 'configureCollectionBalancing.configureCollectionBalancing' is the wrong \
         type 'int', expected type 'string'",
    );
    fails(
        &admin(&mut m, r#"{"configureCollectionBalancing":"nodot"}"#),
        73,
        "Invalid namespace specified 'nodot'",
    );
    fails(
        &admin(&mut m, r#"{"configureCollectionBalancing":"z.nope","chunkSize":64}"#),
        118,
        "Expected to match one doc for query { _id: \"z.nope\" }",
    );
    ok(&m.run("z", json(r#"{"create":"u"}"#)));
    fails(
        &admin(&mut m, r#"{"configureCollectionBalancing":"z.u","chunkSize":64}"#),
        118,
        "Expected to match one doc for query { _id: \"z.u\" }",
    );
    fails(
        &m.run("z", json(r#"{"configureCollectionBalancing":"z.t"}"#)),
        13,
        "configureCollectionBalancing may only be run against the admin database.",
    );
    let bogus = json(r#"{"configureCollectionBalancing":"z.t","bogus":1}"#);
    let e = check_unknown_fields(&bogus, "configureCollectionBalancing").unwrap_err();
    assert_eq!(e.errmsg, "BSON field 'configureCollectionBalancing.bogus' is an unknown field.");
}

#[test]
fn a_change_of_the_balancing_settings_gives_no_change_event() {
    let mut m = cluster();
    let open = r#"{"aggregate":"t","pipeline":[{"$changeStream":{"showExpandedEvents":true}}],
                   "cursor":{}}"#;
    let r = m.run("z", json(open));
    let Value::Int64(id) = field(&r, "cursor.id").clone() else { panic!("{r:?}") };
    m.now += 1;
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","chunkSize":64}"#));
    m.now += 1;
    ok(&m.run("z", json(r#"{"insert":"t","documents":[{"_id":1,"a":1}]}"#)));
    m.now += 1;
    let more = doc(&[
        ("getMore", Value::Int64(id)),
        ("collection", Value::from("t")),
        ("maxTimeMS", Value::Int32(1)),
    ]);
    let r = m.run("z", more);
    let Value::Array(events) = field(&r, "cursor.nextBatch") else { panic!("{r:?}") };
    assert_eq!(events.len(), 1, "{events:?}");
    let Value::Document(e) = &events[0] else { panic!("{events:?}") };
    assert_eq!(e.get("operationType"), Some(&Value::from("insert")));
}

#[test]
fn a_defragmentation_waits_for_the_balancer() {
    let mut m = cluster();
    // Running: the ranges merge their small neighbours already, so it is done at once.
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","defragmentCollection":true}"#));
    let status = admin(&mut m, r#"{"balancerCollectionStatus":"z.t"}"#);
    assert_eq!(status, json(r#"{"chunkSize":128,"balancerCompliant":true,"ok":1.0}"#));
    ok(&admin(&mut m, r#"{"balancerStop":1}"#));
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","defragmentCollection":true}"#));
    assert_eq!(entry(&mut m), json(r#"{"noBalance":false,"defragmentCollection":true}"#));
    let status = admin(&mut m, r#"{"balancerCollectionStatus":"z.t"}"#);
    assert_eq!(
        status,
        json(
            r#"{"chunkSize":128,"balancerCompliant":false,
                "firstComplianceViolation":"defragmentingChunks",
                "details":{"currentPhase":"none"},"ok":1.0}"#
        )
    );
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","defragmentCollection":false}"#));
    assert_eq!(
        entry(&mut m),
        json(
            r#"{"noBalance":false,"defragmentCollection":true,
                "defragmentationPhase":"finished"}"#
        )
    );
    // `balancerStart` passes over a collection with no shard key.
    ok(&m.run("z", json(r#"{"create":"plain"}"#)));
    ok(&admin(&mut m, r#"{"balancerStart":1}"#));
    assert_eq!(entry(&mut m), json(r#"{"noBalance":false}"#));
    // Turning off a defragmentation that is not there changes nothing (measured on 03-11-2026).
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","defragmentCollection":false}"#));
    assert_eq!(entry(&mut m), json(r#"{"noBalance":false}"#));
}

/// The balancer's document with the old `stopped: true` stops it whatever its `mode` says, and a
/// name `db.` is shown without its dot, as a `refrouter` 9.0.4 does (measured on 03-11-2026).
#[test]
fn an_old_stopped_flag_and_a_database_name() {
    let mut m = cluster();
    m.settings = Some(vec![json(r#"{"_id":"balancer","mode":"full","stopped":true}"#)]);
    let r = admin(&mut m, r#"{"balancerStatus":1}"#);
    assert_eq!(field(&r, "mode"), &Value::from("off"));
    for name in ["configureCollectionBalancing", "balancerCollectionStatus"] {
        fails(
            &admin(&mut m, &format!(r#"{{"{name}":"z."}}"#)),
            73,
            "Invalid namespace specified 'z'",
        );
    }
    fails(
        &admin(&mut m, r#"{"setAllowMigrations":"z.","allowMigrations":true}"#),
        73,
        "Invalid namespace specified: z",
    );
}

#[test]
fn balancer_collection_status_reads_the_chunk_size() {
    let mut m = cluster();
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","chunkSize":64}"#));
    let r = admin(&mut m, r#"{"balancerCollectionStatus":"z.t"}"#);
    assert_eq!(field(&r, "chunkSize"), &Value::Int32(64));
    ok(&admin(&mut m, r#"{"configureCollectionBalancing":"z.t","chunkSize":0}"#));
    for (value, mb) in [("32", 32), (r#"{"$numberLong":"16"}"#, 16), ("8.0", 8), ("\"x\"", 128)] {
        m.settings = Some(vec![json(&format!(r#"{{"_id":"chunksize","value":{value}}}"#))]);
        let r = admin(&mut m, r#"{"balancerCollectionStatus":"z.t"}"#);
        assert_eq!(field(&r, "chunkSize"), &Value::Int32(mb), "{value}");
    }
    ok(&m.run("z", json(r#"{"create":"u"}"#)));
    for ns in ["z.u", "z.nope"] {
        fails(
            &admin(&mut m, &format!(r#"{{"balancerCollectionStatus":"{ns}"}}"#)),
            118,
            "Collection unsharded or undefined",
        );
    }
    fails(
        &admin(&mut m, r#"{"balancerCollectionStatus":"nodot"}"#),
        73,
        "Invalid namespace specified 'nodot'",
    );
    fails(
        &admin(&mut m, r#"{"balancerCollectionStatus":5}"#),
        14,
        "BSON field 'balancerCollectionStatus.balancerCollectionStatus' is the wrong type 'int', \
         expected type 'string'",
    );
    fails(
        &m.run("z", json(r#"{"balancerCollectionStatus":"z.t"}"#)),
        13,
        "balancerCollectionStatus may only be run against the admin database.",
    );
    let bogus = json(r#"{"balancerCollectionStatus":"z.t","bogus":1}"#);
    let e = check_unknown_fields(&bogus, "balancerCollectionStatus").unwrap_err();
    assert_eq!(e.errmsg, "BSON field 'balancerCollectionStatus.bogus' is an unknown field.");
}

#[test]
fn max_chunk_bytes_reads_only_a_shard_key_s_settings() {
    assert_eq!(max_chunk_bytes(&[]), None);
    let none = json(r#"{"capped":true}"#).to_bytes().unwrap_or_default();
    assert_eq!(max_chunk_bytes(&none), None);
    let bare = json(r#"{"$shardKey":{"key":{"a":1}}}"#).to_bytes().unwrap_or_default();
    assert_eq!(max_chunk_bytes(&bare), None);
}

/// The names `setAllowMigrations` and the other balancing commands refuse, as a `refrouter` 9.0.4
/// refuses them (measured on 03-11-2026, `b3.js` to `b5.js`): `setAllowMigrations` checks the
/// database's name, then that it exists, then the collection's; the others check the whole name.
#[test]
fn names_the_balancing_commands_refuse() {
    let mut m = cluster();
    let allow = |m: &mut Mem, ns: &str| {
        admin(m, &format!(r#"{{"setAllowMigrations":"{ns}","allowMigrations":true}}"#))
    };
    for (ns, db) in [(".t", ""), (".", ""), ("", ""), ("a b.t", "a b"), ("a b", "a b")] {
        fails(&allow(&mut m, ns), 73, &format!("{db} is not a valid db name"));
    }
    for (ns, db) in [("nodb.t", "nodb"), ("nodb.", "nodb"), ("a$b.t", "a$b")] {
        fails(&allow(&mut m, ns), 26, &format!("database {db} not found"));
    }
    fails(&allow(&mut m, "z"), 73, "Invalid namespace specified: z");
    fails(&allow(&mut m, "z.nope"), 118, "Collection must be sharded so migrations can be blocked");
    for name in ["configureCollectionBalancing", "balancerCollectionStatus"] {
        let refused = |m: &mut Mem, ns: &str| admin(m, &format!(r#"{{"{name}":"{ns}"}}"#));
        fails(&refused(&mut m, ".t"), 73, "Invalid namespace specified '.t'");
        fails(&refused(&mut m, "."), 73, "Invalid namespace specified ''");
        fails(&refused(&mut m, ""), 73, "Invalid namespace specified ''");
    }
}
