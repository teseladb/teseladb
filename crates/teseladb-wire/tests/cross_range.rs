//! Writes whose index entries are in another range (009 T007, FR-003): the range refuses them, and
//! the command writes the document and its entries as one internal transaction; a conflict starts
//! the statement again, and a retryable statement answers a retry with the result it left.

mod common;

use common::{Mem, doc, field};
use teseladb_bson::{Document, Value};
use teseladb_keys::KeyBuilder;
use teseladb_session::retry::StatementKey;
use teseladb_wire::commands::cross_range::{
    MAX_CONFLICT_ROUNDS, read_result, result_key, statement_txn, stored_result,
};

const DB: &str = "x";

/// A node with collection `c`, index `a_1`, and a second range from the collection's index
/// entries on.
fn node() -> (Mem, u32) {
    let mut m = Mem::new();
    let spec = Document::new().with("key", Document::new().with("a", 1)).with("name", "a_1");
    let cmd = Document::new()
        .with("createIndexes", "c")
        .with("indexes", Value::Array(vec![Value::Document(spec)]));
    let r = m.run(DB, cmd);
    assert_eq!(r.get("ok"), Some(&Value::Double(1.0)), "{r:?}");
    let c = m.catalog.collections.values().next().expect("c").id;
    m.split = Some(KeyBuilder::index_prefix(0, c, 1).as_bytes().to_vec());
    (m, c)
}

fn entries(m: &Mem, c: u32) -> usize {
    let start = KeyBuilder::index_prefix(0, c, 1).as_bytes().to_vec();
    let end = KeyBuilder::index_prefix(0, c, 2).as_bytes().to_vec();
    m.data.range(start..end).count()
}

fn insert(docs: Vec<Document>) -> Document {
    Document::new()
        .with("insert", "c")
        .with("documents", Value::Array(docs.into_iter().map(Value::Document).collect()))
}

#[test]
fn a_write_whose_entries_are_in_another_range_is_one_transaction() {
    let (mut m, c) = node();
    let r = m.run(DB, insert(vec![Document::new().with("_id", 1).with("a", 5)]));
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    assert_eq!(entries(&m, c), 1);
    assert_eq!(m.txns.len(), 1, "the document and its entry together");
    assert_eq!(m.txns[0].1.len(), 2);
    // An update moves the entry, in another transaction.
    let u = Document::new()
        .with("q", Document::new().with("_id", 1))
        .with("u", Document::new().with("$set", Document::new().with("a", 6)));
    let r = m
        .run(DB, Document::new().with("update", "c").with("updates", Value::Array(vec![u.into()])));
    assert_eq!(field(&r, "nModified"), &Value::Int32(1), "{r:?}");
    assert_eq!(entries(&m, c), 1);
    assert_eq!(m.txns.len(), 2);
    assert_eq!(m.txns[1].1.len(), 4, "the document, its meta (014), the old entry and the new one");
}

#[test]
fn a_conflict_starts_the_statement_again_until_it_gives_up() {
    let (mut m, c) = node();
    m.conflicts = 2;
    let r = m.run(DB, insert(vec![Document::new().with("_id", 1).with("a", 5)]));
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    assert_eq!(entries(&m, c), 1);
    m.conflicts = MAX_CONFLICT_ROUNDS as usize + 1;
    let r = m.run(DB, insert(vec![doc(&[("_id", Value::Int32(2)), ("a", Value::Int32(5))])]));
    let Some(Value::Array(errors)) = r.get("writeErrors") else { panic!("{r:?}") };
    let Value::Document(e) = &errors[0] else { panic!() };
    assert_eq!(e.get("code"), Some(&Value::Int32(112)), "{r:?}");
    assert_eq!(entries(&m, c), 1, "nothing of the statement that gave up");
}

#[test]
fn a_retried_statement_gets_the_result_it_left() {
    let (mut m, c) = node();
    let lsid = Value::Document(
        Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![7; 16] }),
    );
    let cmd = insert(vec![Document::new().with("_id", 1).with("a", 5)])
        .with("lsid", lsid)
        .with("txnNumber", 3_i64);
    let first = m.run(DB, cmd.clone());
    assert_eq!(field(&first, "n"), &Value::Int32(1), "{first:?}");
    let txns = m.txns.len();
    let retry = m.run(DB, cmd);
    assert_eq!(field(&retry, "n"), &Value::Int32(1), "{retry:?}");
    assert_eq!(retry.get("writeErrors"), None, "{retry:?}");
    assert_eq!(m.txns.len(), txns, "nothing written again");
    assert_eq!(entries(&m, c), 1);
}

/// A retryable statement whose transaction a conflict aborted starts again with another one: a
/// range's record, once aborted, refuses every later commit with its identifier, so a statement
/// that kept it conflicted until it gave up, holding its reads of the indexes all along (seen on
/// 14-11-2026: an `update` of 101 s on a node of three).
#[test]
fn a_retryable_statement_aborted_by_a_conflict_commits_with_another_transaction() {
    let (mut m, c) = node();
    m.conflicts = 1;
    let lsid = Value::Document(
        Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![8; 16] }),
    );
    let cmd = insert(vec![Document::new().with("_id", 1).with("a", 5)])
        .with("lsid", lsid)
        .with("txnNumber", 4_i64);
    let r = m.run(DB, cmd.clone());
    assert_eq!(r.get("writeErrors"), None, "{r:?}");
    assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    assert_eq!(entries(&m, c), 1);
    assert_eq!(m.txns.len(), 1);
    assert!(m.txns[0].0.is_none_or(|t| !m.aborted.contains(&t)), "a fresh transaction");
    // The retry is answered from the result the statement left, by its statement's key.
    let retry = m.run(DB, cmd);
    assert_eq!(field(&retry, "n"), &Value::Int32(1), "{retry:?}");
    assert_eq!(m.txns.len(), 1, "nothing written again");
}

/// A retryable statement whose earlier send committed after this send looked for its result is
/// answered with that result and changes nothing more: its evaluation reads the result with the
/// documents, at one mark (005 FR-011). Before, an `$inc` applied twice.
#[test]
fn a_result_committed_between_the_look_and_the_evaluation_answers_the_statement() {
    let lsid = Value::Document(
        Document::new().with("id", Value::Binary { subtype: 4, bytes: vec![9; 16] }),
    );
    let u = Document::new()
        .with("q", Document::new().with("_id", 1))
        .with("u", Document::new().with("$inc", Document::new().with("a", 1)));
    let cmd = Document::new()
        .with("update", "c")
        .with("updates", Value::Array(vec![u.into()]))
        .with("lsid", lsid)
        .with("txnNumber", 5_i64);
    // The earlier send, on a node of its own: what it leaves is what this send meets.
    let (mut earlier, _) = node();
    let (mut m, c) = node();
    for n in [&mut earlier, &mut m] {
        let r = n.run(DB, insert(vec![Document::new().with("_id", 1).with("a", 5)]));
        assert_eq!(field(&r, "n"), &Value::Int32(1), "{r:?}");
    }
    let before = earlier.data.clone();
    let r = earlier.run(DB, cmd.clone());
    assert_eq!(field(&r, "nModified"), &Value::Int32(1), "{r:?}");
    let version = |k: &Vec<u8>| {
        earlier.data.get(k).map(|v| (earlier.types.get(k).copied().unwrap_or(1), v.clone()))
    };
    let keys: std::collections::BTreeSet<&Vec<u8>> =
        before.keys().chain(earlier.data.keys()).collect();
    let left: std::collections::BTreeMap<Vec<u8>, Option<(u8, Vec<u8>)>> = keys
        .into_iter()
        .filter(|k| before.get(*k) != earlier.data.get(*k))
        .map(|k| (k.clone(), version(k)))
        .collect();
    let s = StatementKey { lsid: u128::from_be_bytes([9; 16]), txn_number: 5, stmt_id: 0 };
    let key = result_key(statement_txn(&s));
    assert!(left.contains_key(&key), "the earlier send left its result");
    let txns = m.txns.len();
    // The earlier send commits after the second read of the result: the look before the
    // statement's first send and the one of its evaluation across ranges.
    m.after_look = Some((key, 2, left.clone()));
    let r = m.run(DB, cmd);
    assert_eq!(r.get("writeErrors"), None, "{r:?}");
    assert_eq!(field(&r, "nModified"), &Value::Int32(1), "{r:?}");
    assert_eq!(m.txns.len(), txns, "nothing written again: {:?}", &m.txns[txns..]);
    for (k, v) in &left {
        let got = m.data.get(k);
        assert_eq!(got, v.as_ref().map(|(_, v)| v), "the earlier send's version stays");
    }
    assert_eq!(entries(&m, c), 1);
}

#[test]
fn a_statements_transaction_and_result_are_stable() {
    let s = StatementKey { lsid: 9, txn_number: 2, stmt_id: 1 };
    assert_eq!(statement_txn(&s), statement_txn(&s));
    let other = StatementKey { stmt_id: 2, ..s };
    assert_ne!(statement_txn(&s), statement_txn(&other));
    assert_eq!(result_key(statement_txn(&s)).len(), 18);
    let mark = teseladb_hlc::Hlc { physical: 5, logical: 6 };
    let stored = stored_result(mark, b"r");
    assert_eq!(read_result(&stored), Some((mark, &b"r"[..])));
    assert_eq!(read_result(&stored[..5]), None);
}

#[test]
fn the_ttl_pass_purges_results_past_the_retry_window() {
    use teseladb_wire::commands::cross_range::purge_results;
    let (mut m, _) = node();
    let s = StatementKey { lsid: 1, txn_number: 1, stmt_id: 0 };
    let fresh = StatementKey { stmt_id: 1, ..s };
    let at = |ms: u64| teseladb_hlc::Hlc { physical: ms, logical: 0 };
    m.data.insert(result_key(statement_txn(&s)), stored_result(at(1_000), b"old"));
    m.data.insert(result_key(statement_txn(&fresh)), stored_result(at(1_900_000), b"new"));
    // A transaction record next to them is not a result.
    m.data.insert(teseladb_range::intents::record_key(statement_txn(&s)), vec![1]);
    let thirty_minutes = 30 * 60_000;
    assert_eq!(purge_results(&mut m, 500), Ok(0), "before any window ended");
    assert_eq!(purge_results(&mut m, 1_000 + thirty_minutes), Ok(1));
    assert!(!m.data.contains_key(&result_key(statement_txn(&s))));
    assert!(m.data.contains_key(&result_key(statement_txn(&fresh))));
    assert!(m.data.contains_key(&teseladb_range::intents::record_key(statement_txn(&s))));
}
