//! The cluster's record of sessions, `config.system.sessions` (FR-016): each node writes the
//! sessions it used with their last use, as the reference server's documents `{_id: {id, uid},
//! lastUse}`, and removes the records unused for the session timeout.

mod common;

use std::sync::Mutex;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_hash::sha256;
use teseladb_wire::commands::registry::{Refreshed, SESSIONS_COLL, SESSIONS_DB, refresh};
use teseladb_wire::commands::{NodeSessions, Timeouts};

fn lsid(n: u8) -> Value {
    Value::Document(doc(&[("id", Value::Binary { subtype: 4, bytes: vec![n; 16] })]))
}

fn records(m: &mut Mem) -> Vec<Document> {
    let sort = Value::Document(doc(&[("_id", 1.into())]));
    let r = m.run(SESSIONS_DB, doc(&[("find", Value::from(SESSIONS_COLL)), ("sort", sort)]));
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

fn record(n: u8, last_use: i64) -> Document {
    let id = doc(&[
        ("id", Value::Binary { subtype: 4, bytes: vec![n; 16] }),
        ("uid", Value::Binary { subtype: 0, bytes: sha256(b"").to_vec() }),
    ]);
    doc(&[("_id", Value::Document(id)), ("lastUse", Value::DateTime(last_use))])
}

#[test]
fn a_refresh_records_the_sessions_used_since_the_last_one() {
    let mut m = Mem::new();
    m.shared = Mutex::new(NodeSessions::new(Timeouts { cursor_ms: 100, session_ms: 1_000 }));
    let t0 = m.now;
    m.run("admin", doc(&[("refreshSessions", Value::Array(vec![lsid(1), lsid(2)]))]));
    let shared = std::mem::take(&mut m.shared);
    let r = refresh(&mut m, &shared, 0, t0).unwrap();
    assert_eq!(r, Refreshed { written: 2, removed: 0 });
    m.shared = shared;
    assert_eq!(records(&mut m), vec![record(1, t0 as i64), record(2, t0 as i64)]);
    // Only session 2 is used again; a later refresh moves its last use.
    m.now = t0 + 500;
    m.run("admin", doc(&[("refreshSessions", Value::Array(vec![lsid(2)]))]));
    let shared = std::mem::take(&mut m.shared);
    assert_eq!(refresh(&mut m, &shared, t0 + 1, t0 + 500).unwrap().written, 1);
    // At t0 + 1000 session 1's record is unused for the timeout and goes.
    let r = refresh(&mut m, &shared, t0 + 501, t0 + 1_000).unwrap();
    assert_eq!(r, Refreshed { written: 0, removed: 1 });
    m.shared = shared;
    assert_eq!(records(&mut m), vec![record(2, (t0 + 500) as i64)]);
    // `endSessions` removes the record at the next refresh, and not before.
    m.run("admin", doc(&[("endSessions", Value::Array(vec![lsid(2)]))]));
    assert_eq!(records(&mut m).len(), 1);
    let shared = std::mem::take(&mut m.shared);
    let r = refresh(&mut m, &shared, t0 + 1_001, t0 + 1_100).unwrap();
    assert_eq!(r, Refreshed { written: 0, removed: 1 });
    m.shared = shared;
    assert!(records(&mut m).is_empty());
}

#[test]
fn a_refresh_with_nothing_to_record_creates_nothing() {
    let mut m = Mem::new();
    let shared = std::mem::take(&mut m.shared);
    let now = m.now;
    assert_eq!(refresh(&mut m, &shared, 0, now).unwrap(), Refreshed::default());
    assert!(m.catalog.collections.is_empty());
}

/// The record has the reference server's TTL index `lsidTTLIndex` on `lastUse`, with the session
/// timeout, from the refresh that creates it (006 FR-038; measured on the native the reference
/// router 9.0.4 on 09-11-2026: `{v: 2, key: {lastUse: 1}, name: "lsidTTLIndex", expireAfterSeconds:
/// 1800}`).
#[test]
fn the_record_has_the_ttl_index_of_the_reference() {
    let mut m = Mem::new();
    let t0 = m.now;
    m.run("admin", doc(&[("refreshSessions", Value::Array(vec![lsid(1)]))]));
    let shared = std::mem::take(&mut m.shared);
    refresh(&mut m, &shared, 0, t0).unwrap();
    refresh(&mut m, &shared, 0, t0 + 1).unwrap();
    m.shared = shared;

    let r = m.run(SESSIONS_DB, doc(&[("listIndexes", Value::from(SESSIONS_COLL))]));

    let Value::Array(listed) = field(&r, "cursor.firstBatch") else { panic!("{r:?}") };
    assert_eq!(listed.len(), 2, "{r:?}");
    let Value::Document(ttl) = &listed[1] else { panic!("{r:?}") };
    assert_eq!(ttl.get("name"), Some(&Value::from("lsidTTLIndex")));
    assert_eq!(ttl.get("key"), Some(&Value::Document(doc(&[("lastUse", 1.into())]))));
    assert_eq!(ttl.get("expireAfterSeconds"), Some(&Value::Int32(1800)));
}

/// Once created, the record shows as the reference router 9.0.4 shows it, a collection sharded by
/// `{_id: 1}` that lives on the shards (006 FR-038, measured on the native the reference router on
/// 09-11-2026).
#[test]
fn the_record_shows_as_a_sharded_collection() {
    let mut m = Mem::new();
    let t0 = m.now;
    m.run("admin", doc(&[("refreshSessions", Value::Array(vec![lsid(1)]))]));
    let shared = std::mem::take(&mut m.shared);
    refresh(&mut m, &shared, 0, t0).unwrap();
    m.shared = shared;
    let find = |m: &mut Mem, coll: &str| {
        let r = m.run("config", doc(&[("find", Value::from(coll))]));
        match field(&r, "cursor.firstBatch") {
            Value::Array(a) => a.clone(),
            other => panic!("{other:?}"),
        }
    };
    let is_sessions = |v: &Value| match v {
        Value::Document(d) => d.get("_id") == Some(&Value::from("config.system.sessions")),
        _ => false,
    };

    let collections = find(&mut m, "collections");
    // The node generates `config.chunks` from Group 0's ranges with `config_chunks`.
    let chunks: Vec<Value> =
        teseladb_wire::commands::chunks::config_chunks(&m.routes, m.catalog.collections.iter())
            .into_iter()
            .map(Value::Document)
            .collect();
    let stats = m.run(SESSIONS_DB, doc(&[("collStats", Value::from(SESSIONS_COLL))]));
    let listed =
        m.run(SESSIONS_DB, doc(&[("listCollections", 1.into()), ("nameOnly", true.into())]));
    let distribution = m.run(
        "admin",
        doc(&[
            ("aggregate", 1.into()),
            (
                "pipeline",
                Value::Array(vec![Value::Document(doc(&[(
                    "$shardedDataDistribution",
                    Value::Document(Document::new()),
                )]))]),
            ),
            ("cursor", Value::Document(Document::new())),
        ]),
    );

    let Some(Value::Document(entry)) = collections.iter().find(|v| is_sessions(v)) else {
        panic!("{collections:?}")
    };
    assert_eq!(entry.get("key"), Some(&Value::Document(doc(&[("_id", 1.into())]))));
    assert_eq!(entry.get("unique"), Some(&Value::Boolean(false)));
    assert_eq!(entry.get("maxChunkSizeBytes"), Some(&Value::Int64(200_000)));
    let uuid = entry.get("uuid").cloned();
    let own: Vec<&Value> = chunks
        .iter()
        .filter(|c| matches!(c, Value::Document(d) if d.get("uuid").cloned() == uuid))
        .collect();
    assert_eq!(own.len(), 1, "{chunks:?}");
    let Value::Document(chunk) = own[0] else { panic!() };
    assert_eq!(chunk.get("min"), Some(&Value::Document(doc(&[("_id", Value::MinKey)]))));
    assert_eq!(chunk.get("max"), Some(&Value::Document(doc(&[("_id", Value::MaxKey)]))));
    assert_eq!(
        (field(&stats, "sharded"), field(&stats, "nchunks")),
        (&Value::Boolean(true), &Value::Int32(1))
    );
    assert!(!format!("{listed:?}").contains("system.sessions"), "{listed:?}");
    assert!(format!("{distribution:?}").contains("config.system.sessions"), "{distribution:?}");
}
