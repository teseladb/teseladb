//! A command of several write statements outside a transaction waits for its write concern once
//! (005 FR-036, D-163): each statement goes to its range answered in the leader's log (`w: 1`
//! without `j`), and the command asks for one barrier at its write concern when it ends or stops
//! at a statement that fails. A command of one statement and one with `w: 0` wait as before. A
//! barrier that finds a statement in doubt fails the command, and the driver's retry answers the
//! statements already written from their records.

mod common;

use common::{Mem, doc, field};
use teseladb_range::ack::{W, WriteConcern};
use teseladb_wire::error::{CommandError, DUPLICATE_KEY, INTERRUPTED_DUE_TO_REPL_STATE_CHANGE};
use teseladb_bson::{Document, Value};

const IN_LOG: WriteConcern = WriteConcern { w: W::Nodes(1), j: false };

fn s(x: &str) -> Value {
    Value::from(x)
}

fn d(pairs: &[(&str, Value)]) -> Value {
    Value::Document(doc(pairs))
}

/// A node with documents 0 to 9 in `db.c`.
fn node() -> Mem {
    let mut m = Mem::new();
    let docs = Value::Array((0..10).map(|i| d(&[("_id", Value::Int32(i))])).collect());
    m.run("db", doc(&[("insert", s("c")), ("documents", docs)]));
    m.concerns.clear();
    m
}

fn set(id: i32) -> Value {
    d(&[("q", d(&[("_id", Value::Int32(id))])), ("u", d(&[("$set", d(&[("a", 1.into())]))]))])
}

fn remove(id: i32) -> Value {
    d(&[("q", d(&[("_id", Value::Int32(id))])), ("limit", 1.into())])
}

fn update(stmts: Vec<Value>, ordered: bool) -> Document {
    doc(&[("update", s("c")), ("updates", Value::Array(stmts)), ("ordered", ordered.into())])
}

fn delete(stmts: Vec<Value>, ordered: bool) -> Document {
    doc(&[("delete", s("c")), ("deletes", Value::Array(stmts)), ("ordered", ordered.into())])
}

fn with_wc(cmd: Document, w: Value) -> Document {
    cmd.with("writeConcern", d(&[("w", w)]))
}

fn retryable(cmd: Document) -> Document {
    cmd.with("lsid", d(&[("id", Value::Binary { subtype: 4, bytes: vec![9; 16] })]))
        .with("txnNumber", Value::Int64(3))
}

fn majority() -> WriteConcern {
    // The write concern of a command without one, as a single statement takes it.
    let mut m = node();
    m.run("db", update(vec![set(0)], true));
    m.concerns[0]
}

fn errors(r: &Document) -> usize {
    match r.get("writeErrors") {
        Some(Value::Array(a)) => a.len(),
        None => 0,
        Some(other) => panic!("{other:?}"),
    }
}

#[test]
fn an_update_of_several_statements_waits_for_its_write_concern_once() {
    for ordered in [true, false] {
        let mut m = node();
        let r = m.run("db", update((0..4).map(set).collect(), ordered));
        assert_eq!(field(&r, "nModified"), &Value::Int32(4), "{r:?}");
        assert_eq!(m.concerns, vec![IN_LOG; 4], "each statement in the leader's log");
        assert_eq!(m.statement_barriers, vec![majority()], "one barrier at the end");
    }
}

#[test]
fn a_delete_of_several_statements_waits_for_its_write_concern_once() {
    for ordered in [true, false] {
        let mut m = node();
        let r = m.run("db", with_wc(delete((0..3).map(remove).collect(), ordered), 2.into()));
        assert_eq!(field(&r, "n"), &Value::Int32(3), "{r:?}");
        assert_eq!(m.concerns, vec![IN_LOG; 3]);
        assert_eq!(m.statement_barriers, vec![WriteConcern { w: W::Nodes(2), j: false }]);
    }
}

#[test]
fn a_statement_that_fails_stops_an_ordered_command_and_the_barrier_follows() {
    let duplicate = || CommandError::new(DUPLICATE_KEY, "duplicate key");
    // Ordered: it stops at the second statement and confirms the first.
    let mut m = node();
    m.fail_write_at = Some((1, duplicate()));
    let r = m.run("db", update((0..4).map(set).collect(), true));
    assert_eq!((field(&r, "nModified"), errors(&r)), (&Value::Int32(1), 1), "{r:?}");
    assert_eq!(m.concerns, vec![IN_LOG]);
    assert_eq!(m.statement_barriers.len(), 1);
    // Unordered: it goes on past it and confirms the other three.
    let mut m = node();
    m.fail_write_at = Some((1, duplicate()));
    let r = m.run("db", delete((0..4).map(remove).collect(), false));
    assert_eq!((field(&r, "n"), errors(&r)), (&Value::Int32(3), 1), "{r:?}");
    assert_eq!(m.concerns, vec![IN_LOG; 3]);
    assert_eq!(m.statement_barriers.len(), 1);
}

#[test]
fn one_statement_and_w_0_wait_as_before() {
    let mut m = node();
    m.run("db", update(vec![set(0)], true));
    assert_eq!(m.concerns, vec![majority()]);
    assert!(m.statement_barriers.is_empty(), "no barrier for one statement");
    let mut m = node();
    let r = m.run("db", with_wc(delete((0..3).map(remove).collect(), true), 0.into()));
    assert_eq!(r, doc(&[("ok", 1.0.into())]));
    assert!(m.statement_barriers.is_empty(), "w: 0 waits for no one");
}

#[test]
fn a_statement_in_doubt_fails_the_command_and_its_retry_answers_from_the_records() {
    let mut m = node();
    let lost = CommandError::new(INTERRUPTED_DUE_TO_REPL_STATE_CHANGE, "the leader changed");
    m.fail_barrier = Some(lost);
    let cmd = retryable(update((0..3).map(set).collect(), true));
    let r = m.run("db", cmd.clone());
    assert_eq!(field(&r, "code"), &Value::Int32(INTERRUPTED_DUE_TO_REPL_STATE_CHANGE.code));
    // Its code is one the dispatch labels `RetryableWriteError` for a retryable write (D-148).
    assert!(r.get("errorLabels").is_none());
    // The driver sends it again: the statements answer from their records and are confirmed.
    m.concerns.clear();
    let again = m.run("db", cmd);
    assert_eq!(field(&again, "nModified"), &Value::Int32(3), "{again:?}");
    let retried = Value::Array((0..3).map(Value::Int32).collect());
    assert_eq!(field(&again, "retriedStmtIds"), &retried);
    assert_eq!(m.statement_barriers.len(), 2);
}

#[test]
fn a_write_concern_above_the_replicas_is_reported_after_the_statements() {
    let mut m = node();
    let r = m.run("db", with_wc(update((0..2).map(set).collect(), true), 5.into()));
    assert_eq!(field(&r, "nModified"), &Value::Int32(2), "{r:?}");
    assert_eq!(field(&r, "writeConcernError.code"), &Value::Int32(100));
    assert_eq!(m.concerns, vec![IN_LOG; 2]);
}

#[test]
fn a_bulk_write_of_several_operations_takes_one_barrier() {
    let mut m = node();
    let ops = Value::Array(vec![
        d(&[
            ("update", 0.into()),
            ("filter", d(&[("_id", 1.into())])),
            ("updateMods", d(&[("$set", d(&[("b", 2.into())]))])),
        ]),
        d(&[("delete", 0.into()), ("filter", d(&[("_id", 2.into())]))]),
        d(&[("insert", 0.into()), ("document", d(&[("_id", 20.into())]))]),
    ]);
    let cmd = doc(&[
        ("bulkWrite", 1.into()),
        ("ops", ops),
        ("nsInfo", Value::Array(vec![d(&[("ns", s("db.c"))])])),
    ]);
    let r = m.run("admin", cmd);
    assert_eq!(field(&r, "nErrors"), &Value::Int32(0), "{r:?}");
    assert!(m.concerns.iter().all(|wc| *wc == IN_LOG), "{:?}", m.concerns);
    assert_eq!(m.statement_barriers, vec![majority()]);
}
