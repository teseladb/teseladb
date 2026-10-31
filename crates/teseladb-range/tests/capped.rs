//! Capped collections on a range (015 T007, FR-003): RefDb's limits, the state in its key, an
//! insert that deletes the oldest documents in the same write.

use teseladb_range::capped::{
    BAD_SIZE_CODE, CappedError, CappedLimits, CappedState, ID_INDEX, MAX_CAPPED_SIZE, STATE_INDEX,
    doc_key, of_state_key, plan_delete, plan_insert, plan_update, seq_of, state_key,
};

fn limits(size: u64, max: Option<u64>) -> CappedLimits {
    CappedLimits { size, max }
}

#[test]
fn size_is_used_as_given_and_max_of_zero_or_less_is_no_limit() {
    // Measured on refnode 7.0.45 and 9.0.4: `size` is used as given, without rounding.
    assert_eq!(CappedLimits::new(1, 0), Ok(limits(1, None)));
    assert_eq!(CappedLimits::new(257, -3), Ok(limits(257, None)));
    assert_eq!(CappedLimits::new(100_000, 500), Ok(limits(100_000, Some(500))));
    assert_eq!(
        CappedLimits::bad_size_message(0),
        "BSON field 'size' value must be >= 1, actual value '0'"
    );
    assert_eq!(
        CappedLimits::bad_size_message(1_125_899_906_842_625),
        "BSON field 'size' value must be <= 1125899906842624, actual value '1125899906842625'"
    );
    assert_eq!(BAD_SIZE_CODE, 2);
    let pib = i64::try_from(MAX_CAPPED_SIZE).unwrap();
    assert_eq!(CappedLimits::new(pib, 1), Ok(limits(MAX_CAPPED_SIZE, Some(1))));
    for bad in [0, -1, pib + 1] {
        assert_eq!(CappedLimits::new(bad, 0), Err(CappedError::BadSize));
    }
}

#[test]
fn the_state_round_trips_and_its_key_closes_the_collection_span() {
    let s = CappedState { bytes: 10, docs: 2, next: 7 };
    assert_eq!(CappedState::decode(&s.encode()), Ok(s));
    assert!(CappedState::decode(&s.encode()[..23]).is_err());
    let k = state_key(1, 2);
    assert_eq!(k, [&[1u8][..], &1u32.to_be_bytes(), &2u32.to_be_bytes(), &[0xff; 4]].concat());
    assert_eq!(of_state_key(&k), Some((1, 2)));
    assert_ne!(k, state_key(1, 3));
    // After every document and index entry of the collection, before the next collection.
    assert!(doc_key(1, 2, u64::MAX) < k);
    let id_entry = [&[1u8][..], &1u32.to_be_bytes(), &2u32.to_be_bytes(), &ID_INDEX.to_be_bytes()];
    assert!(id_entry.concat() < k);
    assert!(k < [&[1u8][..], &1u32.to_be_bytes(), &3u32.to_be_bytes()].concat());
    assert_eq!(STATE_INDEX, ID_INDEX + 1);
    for not_state in [&k[..12], &doc_key(1, 2, 5)[..], &[0u8; 13][..], &[][..]] {
        assert_eq!(of_state_key(not_state), None);
    }
}

#[test]
fn a_document_key_carries_its_seq() {
    let k = doc_key(3, 4, 0x0102);
    assert_eq!(seq_of(3, 4, &k), Some(0x0102));
    assert_eq!(seq_of(3, 5, &k), None, "another collection");
    assert_eq!(seq_of(3, 4, &k[..k.len() - 1]), None, "a short key");
    assert!(doc_key(3, 4, 1) < doc_key(3, 4, 256), "in insertion order");
}

#[test]
fn inserting_past_size_keeps_the_newest_in_insertion_order() {
    // A collection of 1 KiB filled with 2 KiB in documents of 100 bytes (the scenario of the
    // story, scaled down).
    let lim = limits(1024, None);
    let mut state = CappedState::default();
    let mut live: Vec<(u64, u64)> = Vec::new();
    for _ in 0..20 {
        let plan = plan_insert(state, lim, 100, &live).unwrap();
        live.retain(|(s, _)| !plan.evict.contains(s));
        live.push((plan.seq, 100));
        state = plan.state;
        assert!(state.bytes <= lim.size);
    }
    let seqs: Vec<u64> = live.iter().map(|(s, _)| *s).collect();
    assert_eq!(seqs, (10..20).collect::<Vec<_>>());
    assert_eq!((state.docs, state.bytes, state.next), (10, 1000, 20));
}

#[test]
fn max_evicts_by_count_and_size_takes_precedence() {
    let lim = limits(1024, Some(2));
    let s = CappedState { bytes: 20, docs: 2, next: 2 };
    let p = plan_insert(s, lim, 10, &[(0, 10), (1, 10)]).unwrap();
    assert_eq!((p.evict, p.state.docs), (vec![0], 2));
    // Under `max` but over `size`: size wins.
    let p = plan_insert(s, limits(256, Some(100)), 250, &[(0, 10), (1, 10)]).unwrap();
    assert_eq!((p.evict, p.state.docs, p.state.bytes), (vec![0, 1], 1, 250));
}

#[test]
fn a_document_above_size_stays_alone() {
    let s = CappedState { bytes: 200, docs: 2, next: 5 };
    let p = plan_insert(s, limits(256, None), 1000, &[(3, 100), (4, 100)]).unwrap();
    assert_eq!(p.evict, vec![3, 4]);
    assert_eq!(p.state, CappedState { bytes: 1000, docs: 1, next: 6 });
}

#[test]
fn too_few_oldest_documents_are_refused() {
    let s = CappedState { bytes: 200, docs: 2, next: 5 };
    let lim = limits(256, None);
    assert_eq!(plan_insert(s, lim, 100, &[]), Err(CappedError::NeedOlder));
    // Nothing to evict: no scan needed.
    assert_eq!(plan_insert(s, limits(1024, None), 100, &[]).map(|p| p.evict), Ok(vec![]));
}

#[test]
fn an_update_may_shrink_or_grow_and_the_next_insert_evicts_the_excess() {
    // Measured on refnode 7.0.45: an update that makes a document larger is accepted even past
    // `size`, and the next insert evicts the oldest documents until the collection fits.
    let s = CappedState { bytes: 300, docs: 3, next: 3 };
    assert_eq!(plan_update(s, 100, 60).bytes, 260);
    let grown = plan_update(s, 100, 400);
    assert_eq!(grown.bytes, 600);
    let lim = limits(256, None);
    let oldest = [(0, 100), (1, 100), (2, 400)];
    let p = plan_insert(grown, lim, 10, &oldest).unwrap();
    assert_eq!(p.evict, vec![0, 1, 2]);
    assert_eq!(p.state, CappedState { bytes: 10, docs: 1, next: 4 });
}

#[test]
fn running_out_of_oldest_documents_while_still_over_asks_for_more() {
    let s = CappedState { bytes: 250, docs: 5, next: 5 };
    let lim = limits(256, None);
    assert_eq!(plan_insert(s, lim, 150, &[(0, 50), (1, 50)]), Err(CappedError::NeedOlder));
    let p = plan_insert(s, lim, 100, &[(0, 50), (1, 50)]).unwrap();
    assert_eq!(p.evict, vec![0, 1]);
    assert_eq!(p.state, CappedState { bytes: 250, docs: 4, next: 6 });
}

#[test]
fn a_deletion_takes_its_bytes_and_the_last_one_empties_the_collection() {
    let s = CappedState { bytes: 300, docs: 3, next: 3 };
    assert_eq!(plan_delete(s, 100), CappedState { bytes: 200, docs: 2, next: 3 });
    let one = CappedState { bytes: 100, docs: 1, next: 3 };
    assert_eq!(plan_delete(one, 100), CappedState { bytes: 0, docs: 0, next: 3 });
    // A state that lost track never wraps.
    assert_eq!(plan_delete(CappedState::default(), 5), CappedState::default());
}

#[test]
fn refusals_travel_as_their_messages() {
    for e in [CappedError::BadSize, CappedError::NeedOlder, CappedError::Stale] {
        assert_eq!(CappedError::from_message(e.message()), Some(e));
    }
    assert_eq!(CappedError::from_message("duplicate key"), None);
}
