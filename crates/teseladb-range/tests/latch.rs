//! Latches (005 T009, FR-007): a write holds its keys exclusively from evaluation to
//! application, so two increments of one document serialize and both count; a read at t waits
//! for the latches of its keys with a mark at or below t, and a `local` read for all of them; the
//! index entries an evaluation finds join its latches.

use teseladb_hlc::Hlc;
use teseladb_range::latch::{Latches, ReadWait, WaitFor};

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
    assert_eq!(latches.read_wait(b"a", b"z", ts(99), WaitFor::AtOrBelow), ReadWait::Free);
    assert_eq!(latches.read_wait(b"a", b"z", ts(99), WaitFor::Every), ReadWait::On(low));
    latches.set_mark(low, ts(10));
    latches.set_mark(high, ts(30));
    // A read at 20 over [a, z) waits for the write at 10, not for the one at 30.
    assert_eq!(latches.read_wait(b"a", b"z", ts(20), WaitFor::AtOrBelow), ReadWait::On(low));
    assert_eq!(latches.read_wait(b"c", b"z", ts(20), WaitFor::AtOrBelow), ReadWait::Free);
    // A point read is an interval of one key.
    assert_eq!(latches.read_wait(b"b", b"b\0", ts(10), WaitFor::AtOrBelow), ReadWait::On(low));
    // A `local` read waits for every latch of its keys.
    assert_eq!(latches.read_wait(b"c", b"z", ts(20), WaitFor::Every), ReadWait::On(high));
    latches.release(low);
    latches.release(high);
    assert_eq!(latches.read_wait(b"a", b"z", ts(99), WaitFor::Every), ReadWait::Free);
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
    assert_eq!(
        latches.read_wait(b"idx/a", b"idx/a\0", ts(10), WaitFor::AtOrBelow),
        ReadWait::On(w)
    );
    latches.release(w);
    assert!(!latches.is_held(b"idx/a") && !latches.is_held(b"doc"));
    assert!(latches.is_held(b"idx/b"));
}

#[test]
fn a_split_or_a_merge_waits_for_every_latch_of_what_it_copies() {
    let mut latches = Latches::default();
    // A split copies the documents and the idempotence records of the right half: a write in
    // flight there has to be applied first, or its retry in the new range finds no record of it.
    let w = latches.acquire(&[b"k".to_vec()]).unwrap();
    assert_eq!(latches.read_wait(b"a", b"z", ts(0), WaitFor::Every), ReadWait::On(w));
    latches.set_mark(w, ts(500));
    assert_eq!(latches.read_wait(b"a", b"z", ts(10), WaitFor::Every), ReadWait::On(w));
    assert_eq!(latches.read_wait(b"l", b"z", ts(10), WaitFor::Every), ReadWait::Free);
    latches.release(w);
    assert_eq!(latches.read_wait(b"a", b"z", ts(10), WaitFor::Every), ReadWait::Free);
}

#[test]
fn a_read_at_its_own_mark_goes_just_below_the_writes_in_flight() {
    let mut latches = Latches::default();
    // Nothing in flight: the read keeps its mark.
    assert_eq!(latches.own_read_mark(b"a", b"z", ts(50)), Some(ts(50)));
    // Two `w: "majority"` writes in flight at 10 and 30, and one not proposed yet: the read goes
    // just below 10, where every write of the range is applied (H-010).
    let low = latches.acquire(&[b"b".to_vec()]).unwrap();
    let high = latches.acquire(&[b"d".to_vec()]).unwrap();
    let unproposed = latches.acquire(&[b"f".to_vec()]).unwrap();
    latches.set_mark(low, ts(10));
    latches.set_mark(high, ts(30));
    let below_10 = Hlc { physical: 9, logical: u32::MAX };
    assert_eq!(latches.own_read_mark(b"a", b"z", ts(50)), Some(below_10));
    // Writes above the read's mark, or outside its keys, leave it where it is.
    assert_eq!(latches.own_read_mark(b"a", b"z", ts(5)), Some(ts(5)));
    assert_eq!(
        latches.own_read_mark(b"c", b"z", ts(50)),
        Some(Hlc { physical: 29, logical: u32::MAX })
    );
    assert_eq!(latches.own_read_mark(b"e", b"z", ts(50)), Some(ts(50)));
    // A `w: 1` write answered before it is applied: the read waits for it instead (DV-010).
    latches.mark_answered(high);
    assert_eq!(latches.own_read_mark(b"a", b"z", ts(50)), None);
    assert_eq!(latches.own_read_mark(b"a", b"c", ts(50)), Some(below_10));
    // Released, it leaves nothing behind for the next write of its key.
    latches.release(high);
    let next = latches.acquire(&[b"d".to_vec()]).unwrap();
    assert_eq!(latches.own_read_mark(b"c", b"e", ts(50)), Some(ts(50)));
    for id in [low, unproposed, next] {
        latches.release(id);
    }
    assert!(latches.is_empty());
}

#[test]
fn a_pipelined_write_lets_the_next_plain_write_go_on_it_and_holds_the_rest() {
    // 005 FR-029: once the first write's entry is in the log, the next write without a
    // transaction takes the key and sees the pending value; a transaction and the reads still
    // wait for the first to be applied.
    let mut latches = Latches::default();
    let doc = b"doc".to_vec();
    // A retryable write takes its idempotence record with its keys, as the host does.
    let first = latches.acquire_pipelined(&[doc.clone(), b"record".to_vec()]).unwrap();
    assert_eq!(
        latches.acquire_pipelined(std::slice::from_ref(&doc)),
        Err(first),
        "not yet proposed"
    );
    latches.set_mark(first, ts(5));
    let mut written = std::collections::BTreeMap::new();
    written.insert(doc.clone(), Some((3u8, vec![1])));
    written.insert(b"index".to_vec(), None);
    latches.pipeline(first, ts(5), &[b"record".to_vec()], written);
    assert_eq!(latches.pending_value(&doc), Some(&Some((3, vec![1]))));
    assert_eq!(latches.pending_value(b"index"), Some(&None), "a delete is pending too");
    let second = latches.acquire_pipelined(std::slice::from_ref(&doc)).expect("goes on the first");
    // A transaction, and a retry of the first statement (its record), wait for the first.
    assert_eq!(latches.acquire(std::slice::from_ref(&doc)), Err(first));
    assert_eq!(latches.acquire_pipelined(&[b"record".to_vec()]), Err(first));
    // A read at or above the first's mark waits for it.
    assert_eq!(latches.read_wait(b"d", b"e", ts(9), WaitFor::AtOrBelow), ReadWait::On(first));
    assert!(latches.is_held(b"index"));
    // The second, proposed after it, is the newest pending value.
    latches.set_mark(second, ts(7));
    let mut written = std::collections::BTreeMap::new();
    written.insert(doc.clone(), Some((3u8, vec![2])));
    latches.pipeline(second, ts(7), &[], written);
    assert_eq!(latches.pending_value(&doc), Some(&Some((3, vec![2]))));
    latches.release(first);
    assert_eq!(latches.read_wait(b"d", b"e", ts(9), WaitFor::AtOrBelow), ReadWait::On(second));
    latches.release(second);
    assert_eq!(latches.pending_value(&doc), None);
    assert!(latches.is_empty());
    assert_eq!(latches.held().load(std::sync::atomic::Ordering::SeqCst), 0);
}
