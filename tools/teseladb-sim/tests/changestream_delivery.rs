//! The change stream delivery checker (014 FR-015, T002) over histories built by hand: a lost
//! change, a duplicate, changes out of order, a transaction split by another change or emitted in
//! part, and a change emitted above the resolved mark.

use teseladb_hlc::Hlc;
use teseladb_sim::checkers::changestream_delivery::{Change, CursorLog, Emitted, check};

fn ts(ms: u64) -> Hlc {
    Hlc { physical: ms, logical: 0 }
}

fn write(ms: u64, key: &str) -> Change {
    Change { ts: ts(ms), txn: None, op: 0, key: key.as_bytes().to_vec() }
}

fn in_txn(ms: u64, txn: u128, op: u32, key: &str) -> Change {
    Change { ts: ts(ms), txn: Some(txn), op, key: key.as_bytes().to_vec() }
}

/// A cursor that emitted `changes` in that order, each with resolved mark `resolved`.
fn cursor(after: u64, changes: &[Change], resolved: u64) -> CursorLog {
    CursorLog {
        name: "c".into(),
        after: ts(after),
        emitted: changes
            .iter()
            .map(|c| Emitted { change: c.clone(), resolved: ts(resolved) })
            .collect(),
        resolved: ts(resolved),
    }
}

fn committed() -> Vec<Change> {
    vec![
        write(10, "a"),
        in_txn(20, 7, 0, "b"),
        in_txn(20, 7, 1, "a"),
        in_txn(20, 7, 2, "c"),
        write(30, "b"),
        write(40, "c"),
    ]
}

#[test]
fn every_change_once_and_in_order_passes() {
    let all = committed();
    assert_eq!(check(&all, &[cursor(0, &all, 40)]), Ok(()));
    // A cursor that started later, or whose resolved mark is behind, owes less.
    assert_eq!(check(&all, &[cursor(20, &all[4..], 40)]), Ok(()));
    assert_eq!(check(&all, &[cursor(0, &all[..4], 25)]), Ok(()));
    assert_eq!(check(&all, &[cursor(0, &[], 5)]), Ok(()));
}

#[test]
fn a_lost_change_is_reported() {
    let all = committed();
    let lost = [all[0].clone(), all[1].clone(), all[2].clone(), all[3].clone(), all[5].clone()];
    let v = check(&all, &[cursor(0, &lost, 40)]).unwrap_err();
    assert!(v.detail.contains("not emitted"), "{v:?}");
}

#[test]
fn a_duplicate_is_reported() {
    let all = committed();
    let mut twice = all.clone();
    twice.insert(5, all[4].clone());
    let v = check(&all, &[cursor(0, &twice, 40)]).unwrap_err();
    assert!(v.detail.contains("twice") || v.detail.contains("order"), "{v:?}");
}

#[test]
fn changes_out_of_order_are_reported() {
    let all = committed();
    let mut swapped = all.clone();
    swapped.swap(4, 5);
    let v = check(&all, &[cursor(0, &swapped, 40)]).unwrap_err();
    assert!(v.detail.contains("order"), "{v:?}");
}

#[test]
fn a_transaction_emitted_in_part_or_split_is_reported() {
    let all = committed();
    // Two of its three changes, and the cursor resolved past it.
    let part = [all[0].clone(), all[1].clone(), all[2].clone(), all[4].clone(), all[5].clone()];
    let v = check(&all, &[cursor(0, &part, 40)]).unwrap_err();
    assert!(v.detail.contains("transaction"), "{v:?}");
    // Its changes with another change of the same mark in between.
    let mut mixed = all.clone();
    let other = in_txn(20, 7, 1, "z");
    let committed_mixed: Vec<Change> = {
        let mut c = all.clone();
        c.push(Change { txn: Some(8), op: 0, ..other.clone() });
        c.sort();
        c
    };
    mixed.insert(2, Change { txn: Some(8), op: 0, ..other });
    let v = check(&committed_mixed, &[cursor(0, &mixed, 40)]).unwrap_err();
    assert!(v.detail.contains("transaction") || v.detail.contains("order"), "{v:?}");
}

#[test]
fn a_batch_that_ends_inside_a_transaction_above_the_last_mark_owes_the_rest() {
    // A batch may end inside a transaction: its rest comes in the next batch, so a cursor whose
    // last resolved mark is below the transaction has not emitted it in part yet.
    let all = committed();
    let mut c = cursor(0, &all[..3], 20);
    c.resolved = ts(15);
    assert_eq!(check(&all, &[c.clone()]), Ok(()));
    // Not when other changes come after the part, nor once the cursor resolved past it.
    c.emitted.push(Emitted { change: all[4].clone(), resolved: ts(30) });
    assert!(check(&all, &[c]).is_err());
    let v = check(&all, &[cursor(0, &all[..3], 20)]).unwrap_err();
    assert!(v.detail.contains("in part"), "{v:?}");
}

#[test]
fn a_change_emitted_above_the_resolved_mark_is_reported() {
    let all = committed();
    let mut c = cursor(0, &all, 40);
    c.emitted[5].resolved = ts(35);
    let v = check(&all, &[c]).unwrap_err();
    assert!(v.detail.contains("resolved"), "{v:?}");
}

#[test]
fn a_change_never_committed_is_reported() {
    let all = committed();
    let mut extra = all.clone();
    extra.push(write(45, "q"));
    let v = check(&all, &[cursor(0, &extra, 50)]).unwrap_err();
    assert!(v.detail.contains("never committed"), "{v:?}");
}

#[test]
fn a_change_at_or_before_the_start_is_reported() {
    let all = committed();
    let v = check(&all, &[cursor(10, &all, 40)]).unwrap_err();
    assert!(v.detail.contains("not after its start"), "{v:?}");
}
