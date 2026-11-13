//! Evaluating a mutation (005 T012, FR-007, FR-008, FR-004): the upper layer's function gets the
//! current versions and the write's mark and returns the new versions, the index changes and the
//! change description; the write holds the documents and every key it touches, and a key read at
//! or above the mark sends it back for a higher one.

use teseladb_hlc::Hlc;
use teseladb_range::command::{Command, DedupKey, DedupRecord, Put};
use teseladb_range::latch::{Latches, ReadWait, WaitFor};
use teseladb_range::mutate::{Outcome, Refused, evaluate};
use teseladb_range::tscache::TsCache;

const DOC: &[u8] = b"\x01doc/1";
const INDEX_OLD: &[u8] = b"\x01idx/a/0";
const INDEX_NEW: &[u8] = b"\x01idx/a/1";

fn ts(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

fn counter(current: &[Option<(u8, &[u8])>], at: Hlc) -> Result<Outcome, String> {
    let n = current[0].map_or(0, |(_, v)| v[0]);
    Ok(Outcome {
        puts: vec![Put { key: DOC.to_vec(), value_type: 1, value: vec![n + 1] }],
        deletes: vec![],
        change: format!("+{} at {}", n + 1, at.physical).into_bytes(),
        result: vec![],
    })
}

fn reindex(_: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, String> {
    Ok(Outcome {
        puts: vec![
            Put { key: DOC.to_vec(), value_type: 1, value: b"x".to_vec() },
            Put { key: INDEX_NEW.to_vec(), value_type: 2, value: vec![] },
        ],
        deletes: vec![INDEX_OLD.to_vec()],
        change: vec![],
        result: vec![],
    })
}

#[test]
fn two_increments_of_one_document_serialize_and_both_count() {
    let mut latches = Latches::default();
    let cache = TsCache::new(ts(0), 16);
    let docs = [DOC.to_vec()];
    let mut stored: Option<Vec<u8>> = None;
    let first = latches.acquire(&[DOC.to_vec()]).unwrap();
    // The second one waits for the first before it reads anything.
    assert_eq!(latches.acquire(&[DOC.to_vec()]), Err(first));
    let cur = [stored.as_deref().map(|v| (1, v))];
    let e = evaluate(&mut latches, &cache, first, (&docs, &cur), ts(10), counter, None).unwrap();
    assert_eq!(e.change, b"+1 at 10", "evaluated at the write's mark");
    let Command::Write { puts, .. } = &e.command else { panic!("{:?}", e.command) };
    stored = Some(puts[0].value.clone());
    latches.set_mark(first, ts(10));
    latches.release(first);
    let second = latches.acquire(&[DOC.to_vec()]).unwrap();
    let cur = [stored.as_deref().map(|v| (1, v))];
    let e = evaluate(&mut latches, &cache, second, (&docs, &cur), ts(11), counter, None).unwrap();
    assert_eq!(e.change, b"+2 at 11");
}

#[test]
fn an_index_entry_read_at_or_above_the_mark_asks_for_a_higher_one() {
    let mut latches = Latches::default();
    let mut cache = TsCache::new(ts(0), 16);
    let docs = [DOC.to_vec()];
    cache.record(INDEX_NEW, b"\x01idx/a/2", ts(300));
    let id = latches.acquire(&[DOC.to_vec()]).unwrap();
    let dedup = DedupKey { lsid: 1, txn_number: 2, stmt_id: 0 };
    let cur = [Some((1, &b"w"[..]))];
    // The document's reads gave mark 250, but the new index entry was read at 300.
    let got = evaluate(&mut latches, &cache, id, (&docs, &cur), ts(250), reindex, Some(dedup));
    assert_eq!(got, Err(Refused::Below(ts(300))));
    assert!(latches.is_held(DOC) && latches.is_held(INDEX_NEW), "it keeps its latches");
    let e =
        evaluate(&mut latches, &cache, id, (&docs, &cur), ts(301), reindex, Some(dedup)).unwrap();
    assert_eq!(e.latch, id);
    let Command::Write { deletes, dedup: d, live_delta, .. } = &e.command else {
        panic!("{:?}", e.command)
    };
    // The document keeps its size; the new index entry adds its own (D-068).
    assert_eq!(*live_delta, teseladb_range::mutate::live_size(INDEX_NEW, &[]));
    // The record keeps the mutation's result.
    let record = DedupRecord {
        lsid: 1,
        txn_number: 2,
        stmt_id: 0,
        result: e.result.clone(),
        reaching: false,
    };
    assert_eq!((deletes.as_slice(), d.as_ref()), (&[INDEX_OLD.to_vec()][..], Some(&record)));
    for key in [DOC, INDEX_OLD, INDEX_NEW] {
        assert!(latches.is_held(key), "{key:?}");
    }
    latches.set_mark(id, ts(301));
    assert_eq!(
        latches.read_wait(INDEX_OLD, b"\x01idx/a/0\0", ts(400), WaitFor::AtOrBelow),
        ReadWait::On(id)
    );
}

#[test]
fn an_index_entry_held_by_another_write_gives_the_document_back() {
    let mut latches = Latches::default();
    let cache = TsCache::new(ts(0), 16);
    let docs = [DOC.to_vec()];
    let other = latches.acquire(&[INDEX_NEW.to_vec()]).unwrap();
    let id = latches.acquire(&[DOC.to_vec()]).unwrap();
    let got =
        evaluate(&mut latches, &cache, id, (&docs, &[None]), ts(5), reindex, None).map(|e| e.latch);
    assert_eq!(got, Err(Refused::Busy(other)));
    assert!(!latches.is_held(DOC), "the document is free again: no write waits while holding keys");
}

#[test]
fn a_mutation_the_upper_layer_refuses_writes_nothing() {
    let mut latches = Latches::default();
    let cache = TsCache::new(ts(0), 16);
    let docs = [DOC.to_vec()];
    let id = latches.acquire(&[DOC.to_vec()]).unwrap();
    let f = |current: &[Option<(u8, &[u8])>], _: Hlc| -> Result<Outcome, String> {
        assert_eq!(current, [Some((1, &b"v"[..]))]);
        Err("duplicate key".into())
    };
    let got = evaluate(&mut latches, &cache, id, (&docs, &[Some((1, b"v"))]), ts(5), f, None)
        .map(|e| e.latch);
    assert_eq!(got, Err(Refused::Failed("duplicate key".to_string())));
    assert!(latches.is_empty());
}
