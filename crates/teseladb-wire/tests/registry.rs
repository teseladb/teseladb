//! The cluster's record of sessions, `config.system.sessions` (FR-016): each node writes the
//! sessions it used with their last use, as RefDb's documents `{_id: {id, uid}, lastUse}`, and
//! removes the records unused for the session timeout.

mod common;

use std::sync::Mutex;

use common::{Mem, doc, field};
use teseladb_wire::commands::registry::{Refreshed, SESSIONS_COLL, SESSIONS_DB, refresh};
use teseladb_wire::commands::{NodeSessions, Timeouts};
use teseladb_bson::{Document, Value};
use teseladb_hash::sha256;

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
