//! Latches (005 T009, FR-007): a write holds its keys exclusively from evaluation to
//! application, so two increments of one document serialize and both count; a read at t waits
//! for the latches of its keys with a mark at or below t, and a `local` read for all of them; the
//! index entries an evaluation finds join its latches.

use teseladb_hlc::Hlc;
use teseladb_range::latch::{Latches, ReadWait};

fn ts(p: u64) -> Hlc {
    Hlc { physical: p, logical: 0 }
}

#[test]
fn two_increments_of_one_key_take_turns_and_both_count() {
    let mut latches = Latches::default();
    let mut doc = 0u64;
    // The first increment evaluates with the latch held.
    let first = latches.acquire(&[b"doc".to_vec()]).unwrap();
    let seen_by_first = doc;
    // The second one cannot evaluate until the first is applied.
    let blocked = latches.acquire(&[b"doc".to_vec(), b"other".to_vec()]);
    assert_eq!(blocked, Err(first));
    assert!(!latches.is_held(b"other"), "a refused acquisition takes nothing");
    doc = seen_by_first + 1;
    latches.release(first);
    let second = latches.acquire(&[b"doc".to_vec()]).unwrap();
    doc += 1;
    latches.release(second);
    assert_eq!(doc, 2);
    assert!(latches.is_empty());
}

#[test]
fn a_read_waits_for_latches_at_or_below_its_mark() {
    let mut latches = Latches::default();
    let low = latches.acquire(&[b"b".to_vec()]).unwrap();
    let high = latches.acquire(&[b"d".to_vec()]).unwrap();
    // Before their marks exist only a `local` read waits for them.
    assert_eq!(latches.read_wait(b"a", b"z", ts(99), false), ReadWait::Free);
    assert_eq!(latches.read_wait(b"a", b"z", ts(99), true), ReadWait::On(low));
    latches.set_mark(low, ts(10));
    latches.set_mark(high, ts(30));
    // A read at 20 over [a, z) waits for the write at 10, not for the one at 30.
    assert_eq!(latches.read_wait(b"a", b"z", ts(20), false), ReadWait::On(low));
    assert_eq!(latches.read_wait(b"c", b"z", ts(20), false), ReadWait::Free);
    // A point read is an interval of one key.
    assert_eq!(latches.read_wait(b"b", b"b\0", ts(10), false), ReadWait::On(low));
    // A `local` read waits for every latch of its keys.
    assert_eq!(latches.read_wait(b"c", b"z", ts(20), true), ReadWait::On(high));
    latches.release(low);
    latches.release(high);
    assert_eq!(latches.read_wait(b"a", b"z", ts(99), true), ReadWait::Free);
}

#[test]
fn releasing_twice_or_an_unknown_latch_changes_nothing() {
    let mut latches = Latches::default();
    let id = latches.acquire(&[b"k".to_vec()]).unwrap();
    latches.release(id);
    latches.release(id);
    assert!(latches.acquire(&[b"k".to_vec()]).is_ok());
}

#[test]
fn index_entries_join_the_latches_of_their_write() {
    let mut latches = Latches::default();
    let w = latches.acquire(&[b"doc".to_vec()]).unwrap();
    latches.set_mark(w, ts(10));
    let other = latches.acquire(&[b"idx/b".to_vec()]).unwrap();
    // An entry another write holds: nothing is added.
    assert_eq!(latches.extend(w, &[b"idx/a".to_vec(), b"idx/b".to_vec()]), Err(other));
    assert!(!latches.is_held(b"idx/a"));
    // Its own keys again and new ones: they take the write's mark.
    assert_eq!(latches.extend(w, &[b"doc".to_vec(), b"idx/a".to_vec()]), Ok(()));
    assert_eq!(latches.read_wait(b"idx/a", b"idx/a\0", ts(10), false), ReadWait::On(w));
    latches.release(w);
    assert!(!latches.is_held(b"idx/a") && !latches.is_held(b"doc"));
    assert!(latches.is_held(b"idx/b"));
}
