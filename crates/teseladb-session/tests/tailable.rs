//! Tailable cursors (015 T008, FR-004): when the first batch keeps one open, how long a `getMore`
//! waits, and the messages of a lost position, as a `refrouter` 9.0.4 gives them.

use teseladb_session::tailable::{DEFAULT_AWAIT_MS, Tail, lost_message};

#[test]
fn a_cursor_over_an_empty_collection_closes_and_one_that_read_stays() {
    let mut t = Tail::new(false);
    assert!(!t.stays_open());
    t.read(vec![1], false);
    assert!(t.stays_open(), "a document the filter left out");
    t.reached_newest();
    assert_eq!((t.last.clone(), t.caught_up), (Some(vec![1]), true));
}

#[test]
fn the_position_is_the_last_document_returned_not_the_ones_read_ahead() {
    let mut t = Tail::new(true);
    // Read ahead: 1 (left out), 2 and 3 (buffered), 4 (left out).
    t.read(vec![1], false);
    t.read(vec![2], true);
    t.read(vec![3], true);
    t.read(vec![4], false);
    t.took();
    assert_eq!(t.last, Some(vec![2]));
    t.took();
    assert_eq!(t.last, Some(vec![3]));
    t.reached_newest();
    assert_eq!(t.last, Some(vec![4]));
    // Nothing read since: the position stays.
    t.reached_newest();
    assert_eq!(t.last, Some(vec![4]));
    t.took();
    assert_eq!(t.last, Some(vec![4]));
}

#[test]
fn only_await_data_waits_and_maxtimems_sets_the_wait() {
    assert_eq!(Tail::new(false).wait_ms(Some(500)), 0);
    assert_eq!(Tail::new(true).wait_ms(Some(500)), 500);
    assert_eq!(Tail::new(true).wait_ms(None), DEFAULT_AWAIT_MS);
}

#[test]
fn a_lost_position_names_its_record() {
    assert_eq!(
        lost_message(false, 1),
        "CollectionScan died due to position in capped collection being deleted. Last seen record id: RecordId(1)"
    );
    assert_eq!(
        lost_message(true, 5),
        "Executor error during getMore :: caused by :: CollectionScan died due to failure to restore tailable cursor position. Last seen record id: RecordId(5)"
    );
}
