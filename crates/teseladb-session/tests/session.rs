//! Sessions, cursors and retryable writes of 006 (FR-016 to FR-018).

use teseladb_session::cursor::{
    CURSOR_TIMEOUT_MS, Cursor, CursorError, CursorTable, cursor_id, node_of,
};
use teseladb_session::retry::{StatementKey, TxnCheck, check_txn_number, statement_keys};
use teseladb_session::session::{SESSION_TIMEOUT_MS, SessionTable};

fn cursor(ns: &str, lsid: Option<u128>, no_timeout: bool) -> Cursor<u32> {
    Cursor { ns: ns.into(), lsid, owner: None, no_timeout, last_use_ms: 0, state: 7 }
}

#[test]
fn cursor_ids_carry_their_node_and_are_never_zero() {
    for node in [0, 1, 3, 1023] {
        for random in [0, 1, u64::MAX, 0x1234_5678_9abc] {
            let id = cursor_id(node, random);
            assert_ne!(id, 0);
            assert_eq!(node_of(id), node);
        }
    }
    // Only the low bits of the node fit.
    assert_eq!(node_of(cursor_id(1024 + 5, 9)), 5);
}

#[test]
fn a_cursor_is_taken_by_its_namespace_and_session_only() {
    let mut t = CursorTable::new();
    assert!(t.is_empty());
    t.put(5, cursor("db.c", Some(1), false));
    t.put(8, cursor("db.c", None, false));
    assert_eq!(t.take(6, "db.c", Some(1), 1).err(), Some(CursorError::NotFound));
    // The namespace is checked before the session, as a `refrouter` 9.0 does.
    assert_eq!(t.take(5, "db.d", Some(2), 1).err(), Some(CursorError::OtherNamespace));
    assert_eq!(t.take(5, "db.c", None, 1).err(), Some(CursorError::SessionRequired));
    assert_eq!(t.take(5, "db.c", Some(2), 1).err(), Some(CursorError::OtherSession));
    assert_eq!(t.take(8, "db.c", Some(2), 1).err(), Some(CursorError::NotInSession));
    // A refused `getMore` leaves the cursor where it was.
    assert_eq!(t.get(5).map(|c| c.ns.as_str()), Some("db.c"));
    let c = t.take(5, "db.c", Some(1), 42).expect("its own");
    assert_eq!((c.state, c.last_use_ms), (7, 42));
    // Taken, it is not there for anyone else.
    assert!(!t.contains(5));
    assert!(t.get(5).is_none());
    t.put(5, c);
    assert_eq!(t.len(), 2);
    assert!(t.remove(5));
    assert!(!t.remove(5));
}

#[test]
fn only_cursors_without_a_session_expire_after_ten_minutes_unless_no_timeout() {
    let mut t = CursorTable::new();
    assert_eq!(t.timeout_ms(), CURSOR_TIMEOUT_MS);
    t.put(1, cursor("db.c", None, false));
    t.put(2, cursor("db.c", None, true));
    t.put(3, Cursor { last_use_ms: 5, ..cursor("db.c", None, false) });
    // A session's cursor lives as long as its session (measured on 9.0.4).
    t.put(4, cursor("db.c", Some(9), false));
    assert!(t.expire(CURSOR_TIMEOUT_MS - 1).is_empty());
    assert_eq!(t.expire(CURSOR_TIMEOUT_MS), vec![1]);
    assert_eq!(t.expire(CURSOR_TIMEOUT_MS + 5), vec![3]);
    assert_eq!(t.expire(u64::MAX), Vec::<i64>::new());
    assert!(t.contains(2) && t.contains(4));
}

#[test]
fn the_cursor_timeout_can_be_changed() {
    let mut t = CursorTable::with_timeout(100);
    assert_eq!(t.timeout_ms(), 100);
    t.put(1, cursor("db.c", None, false));
    assert!(t.expire(99).is_empty());
    assert_eq!(t.expire(100), vec![1]);
}

#[test]
fn ending_sessions_kills_their_cursors() {
    let mut t = CursorTable::new();
    t.put(1, cursor("db.c", Some(10), false));
    t.put(2, cursor("db.c", Some(11), false));
    t.put(3, cursor("db.c", None, false));
    assert_eq!(t.kill_sessions(&[10]), vec![1]);
    t.put(4, cursor("db.c", Some(12), false));
    assert_eq!(t.kill_all_sessions(), vec![2, 4]);
    assert_eq!(t.len(), 1);
}

#[test]
fn sessions_live_thirty_minutes_without_use() {
    let mut s = SessionTable::new();
    assert!(s.is_empty());
    s.touch(1, 100);
    s.touch(2, 200);
    // A late clock does not move the last use back.
    s.touch(2, 50);
    assert_eq!(s.get(2).map(|x| x.last_use_ms), Some(200));
    assert_eq!(s.ids(), vec![1, 2]);
    assert_eq!(s.expire(100 + SESSION_TIMEOUT_MS), vec![1]);
    assert_eq!(s.len(), 1);
    assert_eq!(s.end(&[2, 3]), vec![2]);
    s.touch(4, 0);
    s.touch(5, 0);
    assert_eq!(s.end_all(), vec![4, 5]);
    assert!(s.get(4).is_none());
}

#[test]
fn the_session_timeout_can_be_changed() {
    let mut s = SessionTable::with_timeout(100);
    assert_eq!(s.timeout_ms(), 100);
    assert_eq!(SessionTable::new().timeout_ms(), SESSION_TIMEOUT_MS);
    s.touch(1, 0);
    assert!(s.expire(99).is_empty());
    assert_eq!(s.expire(100), vec![1]);
}

#[test]
fn txn_numbers_only_move_forward() {
    let mut s = SessionTable::new();
    let session = s.touch(1, 0);
    assert_eq!(check_txn_number(session, 3), TxnCheck::New);
    assert_eq!(check_txn_number(session, 3), TxnCheck::Same);
    assert_eq!(check_txn_number(session, 2), TxnCheck::TooOld { highest: 3 });
    assert_eq!(check_txn_number(session, 4), TxnCheck::New);
    assert_eq!(session.txn_number, Some(4));
    let keys = statement_keys(9, 4, 2, 2);
    assert_eq!(
        keys,
        vec![
            StatementKey { lsid: 9, txn_number: 4, stmt_id: 2 },
            StatementKey { lsid: 9, txn_number: 4, stmt_id: 3 }
        ]
    );
}

#[test]
fn the_sessions_used_since_a_time_are_listed_with_their_last_use() {
    let mut s = SessionTable::new();
    s.touch(1, 100);
    s.touch(2, 300);
    s.touch(3, 200);
    assert_eq!(s.used_since(200), vec![(2, 300), (3, 200)]);
    assert!(s.used_since(301).is_empty());
}

/// `endSessions` only marks the sessions: the next refresh of the record ends them and kills
/// their cursors (measured in `refrouter` 9.0.4: a `getMore` right after `endSessions` still works).
#[test]
fn ended_sessions_wait_for_the_refresh() {
    let mut t = SessionTable::new();
    t.touch(1, 10);
    t.touch(2, 10);
    t.mark_ending(&[1, 3]);
    assert!(t.get(1).is_some(), "still there until the refresh");
    assert_eq!(t.used_since(0), vec![(2, 10)], "its record is not written again");
    assert_eq!(t.take_ending(), vec![1, 3]);
    assert!(t.take_ending().is_empty());
}
