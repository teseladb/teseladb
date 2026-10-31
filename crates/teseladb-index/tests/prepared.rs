//! `prepareUnique` (FR-004, FR-007): a write cannot add an entry whose values another document's
//! entry has, it probes the values first, and a guard whose version moved makes it probe again.

use std::collections::BTreeMap;

use teseladb_index::prepared::{
    FIRST_PROBE, GUARDS, PreparedError, PreparedGuard, Probe, ProbeRequest, guard_key, guards_of,
    version_of,
};
use teseladb_keys::KeyBuilder;

/// The prefix of value `v` in index 3 of collection 7.
fn prefix(v: u8) -> Vec<u8> {
    let mut p = KeyBuilder::index_prefix(0, 7, 3).as_bytes().to_vec();
    p.extend([0x20, v]);
    p
}

/// The entry of value `v` for primary key `pk`.
fn entry(v: u8, pk: u8) -> Vec<u8> {
    let mut k = prefix(v);
    k.extend([0x30, pk]);
    k
}

fn guard(v: u8) -> Vec<u8> {
    guard_key(&prefix(v)).expect("a data key")
}

fn probe(v: u8, version: Option<u64>, found: Vec<Vec<u8>>, limit: u32) -> Probe {
    Probe { guard: guard(v), version, found, limit }
}

fn ask(v: u8, limit: u32) -> PreparedError {
    PreparedError::Probe(ProbeRequest { guard: guard(v), prefix: prefix(v), limit })
}

#[test]
fn a_guard_is_the_prefix_under_the_guards_of_its_index() {
    let g = guard(1);
    let mut expected = KeyBuilder::index_prefix(0, 7, guards_of(3)).as_bytes().to_vec();
    expected.extend([0x20, 1]);
    assert_eq!(g, expected);
    assert_eq!(guards_of(3), 3 | GUARDS);
    assert!(guard_key(&[0x02, 1]).is_err(), "a transaction key is no index prefix");
    assert!(guard_key(&[]).is_err());
    assert_eq!(version_of(&9u64.to_be_bytes()), Ok(9));
    assert!(version_of(&[1, 2]).is_err());
}

#[test]
fn a_value_is_probed_before_and_again_when_its_guard_moved() {
    let store: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let stored = |k: &[u8]| store.get(k).map(Vec::as_slice);
    let mut g = PreparedGuard::default();
    let added = [(prefix(1), entry(1, 1))];
    assert_eq!(g.admit(&[], &added, &[], &stored), Err(ask(1, FIRST_PROBE)));
    // The guard is at version 4 now; the probe saw none.
    let mut store = store;
    store.insert(guard(1), 4u64.to_be_bytes().to_vec());
    let stored = |k: &[u8]| store.get(k).map(Vec::as_slice);
    let stale = [probe(1, None, vec![], FIRST_PROBE)];
    assert_eq!(g.admit(&[], &added, &stale, &stored), Err(ask(1, FIRST_PROBE)));
    let fresh = [probe(1, Some(4), vec![], FIRST_PROBE)];
    let raised = g.admit(&[], &added, &fresh, &stored).expect("a new value");
    assert_eq!(raised, vec![(guard(1), 5u64.to_be_bytes().to_vec())]);
}

#[test]
fn another_documents_entry_refuses_and_its_own_or_a_gone_one_does_not() {
    let mut store: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    store.insert(entry(1, 2), vec![]);
    let stored = |k: &[u8]| store.get(k).map(Vec::as_slice);
    let added = [(prefix(1), entry(1, 1))];
    let held = [probe(1, None, vec![entry(1, 2)], FIRST_PROBE)];
    let mut g = PreparedGuard::default();
    assert_eq!(
        g.admit(&[], &added, &held, &stored),
        Err(PreparedError::Duplicate { prefix: prefix(1) })
    );
    // The statement removes that entry itself (an update of document 2 is not this one, but a
    // write may remove and add in one statement).
    assert!(g.admit(&[entry(1, 2)], &added, &held, &stored).is_ok());
    // Its own entry is not a duplicate; a key the probe saw that is gone neither.
    let own = [probe(1, None, vec![entry(1, 1)], FIRST_PROBE)];
    assert!(PreparedGuard::default().admit(&[], &added, &own, &stored).is_ok());
    let gone = [probe(1, None, vec![entry(1, 9)], FIRST_PROBE)];
    assert!(PreparedGuard::default().admit(&[], &added, &gone, &stored).is_ok());
}

#[test]
fn a_full_probe_of_gone_entries_reads_further() {
    let store: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let stored = |k: &[u8]| store.get(k).map(Vec::as_slice);
    let added = [(prefix(1), entry(1, 1))];
    let full = [probe(1, None, vec![entry(1, 8), entry(1, 9)], 2)];
    assert_eq!(PreparedGuard::default().admit(&[], &added, &full, &stored), Err(ask(1, 4)));
}

#[test]
fn statements_of_one_write_see_each_other() {
    let store: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let stored = |k: &[u8]| store.get(k).map(Vec::as_slice);
    let probes = [probe(1, None, vec![], FIRST_PROBE), probe(2, None, vec![], FIRST_PROBE)];
    let mut g = PreparedGuard::default();
    let first = g.admit(&[], &[(prefix(1), entry(1, 1))], &probes, &stored).expect("first");
    assert_eq!(first.len(), 1);
    // A second document with the same value is refused; the refusal records nothing.
    let second = [(prefix(1), entry(1, 2))];
    assert_eq!(
        g.admit(&[], &second, &probes, &stored),
        Err(PreparedError::Duplicate { prefix: prefix(1) })
    );
    // Document 1 moves to value 2: value 1 is free for document 2, and its guard was raised
    // already by this write.
    let moved = g.admit(&[entry(1, 1)], &[(prefix(2), entry(2, 1))], &probes, &stored);
    assert_eq!(moved.map(|r| r.len()), Ok(1));
    assert_eq!(g.admit(&[], &second, &probes, &stored), Ok(vec![]));
}

#[test]
fn a_guard_that_is_no_version_is_malformed() {
    let mut store: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    store.insert(guard(1), vec![1]);
    let stored = |k: &[u8]| store.get(k).map(Vec::as_slice);
    let probes = [probe(1, None, vec![], FIRST_PROBE)];
    let r = PreparedGuard::default().admit(&[], &[(prefix(1), entry(1, 1))], &probes, &stored);
    assert!(matches!(r, Err(PreparedError::Malformed(_))), "{r:?}");
    let bad = PreparedGuard::default().admit(&[], &[(vec![0x02], vec![0x02, 1])], &probes, &stored);
    assert!(matches!(bad, Err(PreparedError::Malformed(_))), "{bad:?}");
}

#[test]
fn a_multikey_document_raises_the_guard_of_each_of_its_values() {
    let probes = [probe(1, None, vec![], FIRST_PROBE), probe(2, Some(3), vec![], FIRST_PROBE)];
    let store: BTreeMap<Vec<u8>, Vec<u8>> =
        BTreeMap::from([(guard(2), 3u64.to_be_bytes().to_vec())]);
    let stored2 = |k: &[u8]| store.get(k).map(Vec::as_slice);
    let added = [(prefix(1), entry(1, 1)), (prefix(2), entry(2, 1))];
    let raised = PreparedGuard::default().admit(&[], &added, &probes, &stored2).expect("admitted");
    assert_eq!(
        raised,
        vec![(guard(1), 1u64.to_be_bytes().to_vec()), (guard(2), 4u64.to_be_bytes().to_vec())]
    );
}
