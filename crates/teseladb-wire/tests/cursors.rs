//! Cursors and sessions over time (FR-016, FR-017): what `getMore` and `killCursors` answer
//! for a cursor of another namespace or session, and how the node reaps expired cursors and
//! sessions, with the answers of a `refrouter` 9.0.4 (measured).

mod common;

use std::sync::Mutex;

use common::{Mem, doc, field};
use teseladb_wire::commands::{ANONYMOUS_UID, NodeSessions, Reaped, Timeouts, lock, reap};
use teseladb_bson::{Document, Value};
use teseladb_hash::sha256;

fn s(x: &str) -> Value {
    Value::from(x)
}

fn lsid(n: u8) -> Value {
    Value::Document(doc(&[("id", Value::Binary { subtype: 4, bytes: vec![n; 16] })]))
}

fn uuid(n: u8) -> String {
    let h = format!("{n:02x}");
    format!("{0}{0}{0}{0}-{0}{0}-{0}{0}-{0}{0}-{0}{0}{0}{0}{0}{0}", h)
}

fn code(r: &Document) -> i32 {
    match r.get("code") {
        Some(Value::Int32(c)) => *c,
        _ => panic!("no error in {r:?}"),
    }
}

fn errmsg(r: &Document) -> String {
    match r.get("errmsg") {
        Some(Value::String(t)) => t.clone(),
        _ => panic!("no errmsg in {r:?}"),
    }
}

/// A node with documents 0..5 in `db.c` and `db.d`.
fn node() -> Mem {
    let mut m = Mem::new();
    for coll in ["c", "d"] {
        let docs = (0..5).map(|i| Value::Document(doc(&[("_id", Value::Int32(i))]))).collect();
        m.run("db", doc(&[("insert", s(coll)), ("documents", Value::Array(docs))]));
    }
    m
}

/// Opens a cursor of one document per batch on `db.c`, in session `session` if any.
fn open(m: &mut Mem, session: Option<u8>) -> i64 {
    let mut find = doc(&[("find", s("c")), ("batchSize", 1.into())]);
    if let Some(n) = session {
        find.push("lsid", lsid(n));
    }
    match field(&m.run("db", find), "cursor.id") {
        Value::Int64(id) => *id,
        other => panic!("{other:?}"),
    }
}

fn get_more(m: &mut Mem, db: &str, id: i64, coll: &str, session: Option<u8>) -> Document {
    let mut cmd = doc(&[("getMore", Value::Int64(id)), ("collection", s(coll))]);
    if let Some(n) = session {
        cmd.push("lsid", lsid(n));
    }
    m.run(db, cmd)
}

/// Base64 of `bytes`, for the anonymous user's identifier.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[test]
fn the_anonymous_uid_is_the_hash_of_the_empty_user() {
    assert_eq!(ANONYMOUS_UID, base64(&sha256(b"")));
}

#[test]
fn a_get_more_on_another_namespace_is_refused_and_the_cursor_stays_open() {
    let mut m = node();
    let id = open(&mut m, None);
    for (db, coll, ns) in [("db", "d", "db.d"), ("db", "zz", "db.zz"), ("other", "c", "other.c")] {
        let r = get_more(&mut m, db, id, coll, None);
        assert_eq!(code(&r), 13);
        assert_eq!(
            errmsg(&r),
            format!(
                "Requested getMore on namespace '{ns}', but cursor belongs to a different \
                 namespace db.c"
            )
        );
    }
    // The namespace is checked before the session.
    assert_eq!(code(&get_more(&mut m, "db", id, "d", Some(3))), 13);
    let r = get_more(&mut m, "db", id, "c", None);
    assert!(matches!(field(&r, "cursor.nextBatch"), Value::Array(a) if a.len() == 4));
}

/// A `getMore` in a session other than its cursor's: a `refrouter` 9.0.4 refuses it with 13
/// `Unauthorized` naming both sessions, `none` for a missing one (measured on 03-11-2026).
#[test]
fn a_get_more_from_another_session_names_both_sessions() {
    let mut m = node();
    let anon = format!("{ANONYMOUS_UID} -  - ");
    let refused = |cursor: &str, op: &str| {
        format!(
            "Cursor session id ({cursor}) is not the same as the operation context's session id \
             ({op})"
        )
    };
    let named = |n: u8| format!("{} - {anon}", uuid(n));
    let without = open(&mut m, None);
    let r = get_more(&mut m, "db", without, "c", Some(2));
    assert_eq!((code(&r), errmsg(&r)), (13, refused("none", &named(2))));
    assert_eq!(field(&r, "codeName"), &s("Unauthorized"));
    let with = open(&mut m, Some(1));
    let r = get_more(&mut m, "db", with, "c", None);
    assert_eq!((code(&r), errmsg(&r)), (13, refused(&named(1), "none")));
    let r = get_more(&mut m, "db", with, "c", Some(2));
    assert_eq!((code(&r), errmsg(&r)), (13, refused(&named(1), &named(2))));
    // Refused, it is still there for its own session.
    let r = get_more(&mut m, "db", with, "c", Some(1));
    assert!(matches!(field(&r, "cursor.nextBatch"), Value::Array(a) if a.len() == 4));
}

/// The same refusals from a member of a replica set, as a `refnode` 9.0.4 gives them (measured on
/// 03-11-2026), and its message for a cursor that does not exist.
#[test]
fn a_member_refuses_a_get_more_from_another_session_as_a_refnode() {
    let mut m = node();
    m.shards = vec!["node1".into(), "node2".into(), "node3".into()];
    let present = Document::new().with("mode", "replicaset").with("setName", "rs0");
    let r = m.run(
        "admin",
        Document::new()
            .with("setClusterParameter", Document::new().with("topologyPresentation", present)),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let anon = format!("{ANONYMOUS_UID} -  - ");
    let without = open(&mut m, None);
    let r = get_more(&mut m, "db", without, "c", Some(2));
    assert_eq!(code(&r), 50736);
    assert_eq!(field(&r, "codeName"), &s("Location50736"));
    assert_eq!(
        errmsg(&r),
        format!(
            "Cannot run getMore on cursor {without}, which was not created in a session, in \
             session {} - {anon}",
            uuid(2)
        )
    );
    let with = open(&mut m, Some(1));
    let r = get_more(&mut m, "db", with, "c", None);
    assert_eq!(code(&r), 50737);
    assert_eq!(
        errmsg(&r),
        format!(
            "Cannot run getMore on cursor {with}, which was created in session {} - {anon}, \
             without an lsid",
            uuid(1)
        )
    );
    let r = get_more(&mut m, "db", with, "c", Some(2));
    assert_eq!(code(&r), 50738);
    assert_eq!(
        errmsg(&r),
        format!(
            "Cannot run getMore on cursor {with}, which was created in session {} - {anon}, in \
             session {} - {anon}",
            uuid(1),
            uuid(2)
        )
    );
    let r = get_more(&mut m, "db", 77, "c", None);
    assert_eq!((code(&r), errmsg(&r)), (43, "cursor id 77 not found".to_owned()));
}

#[test]
fn kill_cursors_kills_a_cursor_whatever_namespace_it_names() {
    let mut m = node();
    let id = open(&mut m, None);
    let r = m.run(
        "other",
        doc(&[("killCursors", s("zz")), ("cursors", Value::Array(vec![Value::Int64(id)]))]),
    );
    assert_eq!(field(&r, "cursorsKilled"), &Value::Array(vec![Value::Int64(id)]));
    assert_eq!(code(&get_more(&mut m, "db", id, "c", None)), 43);
}

#[test]
fn reaping_kills_idle_cursors_without_a_session_and_expired_sessions_with_theirs() {
    let mut m = node();
    m.shared = Mutex::new(NodeSessions::new(Timeouts { cursor_ms: 100, session_ms: 1_000 }));
    let start = m.now;
    let plain = open(&mut m, None);
    let keep = doc(&[("find", s("c")), ("batchSize", 1.into()), ("noCursorTimeout", true.into())]);
    let kept = match field(&m.run("db", keep), "cursor.id") {
        Value::Int64(id) => *id,
        other => panic!("{other:?}"),
    };
    let in_session = open(&mut m, Some(1));
    let other_session = open(&mut m, Some(2));
    // Nothing is idle yet.
    assert_eq!(reap(&m.shared, start + 99), Reaped::default());
    let r = reap(&m.shared, start + 500);
    assert_eq!(r, Reaped { sessions: vec![], cursors: vec![plain] });
    // Session 2 is used again; session 1 expires with its cursor.
    m.now = start + 900;
    m.run("admin", doc(&[("refreshSessions", Value::Array(vec![lsid(2)]))]));
    let r = reap(&m.shared, start + 1_100);
    assert_eq!(r.sessions, vec![u128::from_be_bytes([1; 16])]);
    assert_eq!(r.cursors, vec![in_session]);
    let g = lock(&m.shared);
    assert!(g.cursors.contains(kept) && g.cursors.contains(other_session));
    assert_eq!(g.sessions.len(), 1);
}

#[test]
fn a_node_keeps_refdb_timeouts_by_default() {
    let t = Timeouts::default();
    assert_eq!((t.cursor_ms, t.session_ms), (600_000, 1_800_000));
    let n = NodeSessions::default();
    assert_eq!(n.cursors.timeout_ms(), 600_000);
    assert_eq!(n.sessions.timeout_ms(), 1_800_000);
}

#[test]
fn kills_reach_the_other_nodes_and_ending_stays_local() {
    use teseladb_wire::commands::session::KillScope;
    use teseladb_wire::error::{CommandError, NETWORK_TIMEOUT};
    let mut m = node();
    let one = || Value::Array(vec![lsid(1)]);
    m.run("admin", doc(&[("endSessions", one())]));
    assert!(m.killed.is_empty());
    m.run("admin", doc(&[("killSessions", one())]));
    m.run("admin", doc(&[("killAllSessions", Value::Array(vec![]))]));
    // Without authentication every session is the empty user's.
    let id = u128::from_be_bytes([1; 16]);
    let reached = teseladb_session::session::SessionMatch::one(id, sha256(b""));
    assert_eq!(m.killed, vec![KillScope::Sessions(vec![reached]), KillScope::All]);
    // A node that cannot reach the others reports it, after the local kill.
    let with = open(&mut m, Some(1));
    m.fail_kill = Some(CommandError::new(NETWORK_TIMEOUT, "no answer"));
    assert_eq!(code(&m.run("admin", doc(&[("killSessions", one())]))), 89);
    assert_eq!(code(&get_more(&mut m, "db", with, "c", Some(1))), 43);
}

/// `aggregate` on a collection answers its cursor as a `refrouter` 9.0.4 does (`id`, `ns`,
/// `firstBatch`), and in the replica set presentation (018) as a `refnode` does (`firstBatch`,
/// `id`, `ns`; measured by the battery's `autocomplete-options` on 04-11-2026).
#[test]
fn aggregate_orders_its_cursor_as_the_presented_process_does() {
    let mut m = Mem::new();
    m.shards = vec!["node1".into(), "node2".into(), "node3".into()];
    m.run(
        "db",
        Document::new()
            .with("insert", "c")
            .with("documents", vec![Value::Document(Document::new())]),
    );
    let agg = || {
        Document::new()
            .with("aggregate", "c")
            .with("pipeline", Vec::<Value>::new())
            .with("cursor", Document::new())
    };
    let keys = |r: &Document| match r.get("cursor") {
        Some(Value::Document(c)) => c.iter().map(|(k, _)| k.to_string()).collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(keys(&m.run("db", agg())), ["id", "ns", "firstBatch"]);
    let present = Document::new().with("mode", "replicaset").with("setName", "rs0");
    let r = m.run(
        "admin",
        Document::new()
            .with("setClusterParameter", Document::new().with("topologyPresentation", present)),
    );
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    assert_eq!(keys(&m.run("db", agg())), ["firstBatch", "id", "ns"]);
}
