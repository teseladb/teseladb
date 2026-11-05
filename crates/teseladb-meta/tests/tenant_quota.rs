//! 007 FR-031: a node's part of a tenant's storage quota, by readings of its usage aligned on
//! epochs of the wall clock.

use teseladb_meta::tenant_quota::{
    EPOCH_MS, GUARD_MS, NodeReading, QuotaRefusal, QuotaWindow, Reading, reading_at, window_of,
};

const LIMIT: u64 = 1000;
/// The start of epoch 100 on the wall clock.
const E100: u64 = 100 * EPOCH_MS;

fn at(epoch: u64, used: u64) -> Reading {
    Reading { epoch, used: Some(used) }
}

/// A time well inside window `w`.
fn inside(w: u64) -> u64 {
    w * EPOCH_MS + EPOCH_MS / 2
}

#[test]
fn the_windows_and_the_readings_sit_on_the_epochs() {
    assert_eq!(window_of(E100 - GUARD_MS - 1), 99);
    assert_eq!(window_of(E100 - GUARD_MS), 100);
    assert_eq!(window_of(E100 + EPOCH_MS - GUARD_MS - 1), 100);
    assert_eq!(reading_at(100), E100 + GUARD_MS);
    // The reading of an epoch starts after its window started on every clock within the guard,
    // and well before the window two epochs later starts.
    assert!(reading_at(100) >= E100 - GUARD_MS + 2 * GUARD_MS);
    assert!(reading_at(100) < 102 * EPOCH_MS - GUARD_MS);
}

#[test]
fn a_node_writes_its_part_of_the_margin_and_no_more() {
    let mut w = QuotaWindow::default();
    // In window 102 the reading of 100 goes: 400 used of 1000 on three nodes, 200 for this node.
    let r = [at(100, 400)];
    assert_eq!(w.admit(LIMIT, 3, 0, inside(102), &r, 150), Ok(()));
    assert_eq!(w.admit(LIMIT, 3, 0, inside(102), &r, 50), Ok(()));
    assert_eq!(w.admit(LIMIT, 3, 0, inside(102), &r, 1), Err(QuotaRefusal::ShareSpent(200)));
}

#[test]
fn a_full_quota_refuses_with_the_usage() {
    let mut w = QuotaWindow::default();
    let r = [at(100, LIMIT)];
    assert_eq!(w.admit(LIMIT, 3, 0, inside(102), &r, 1), Err(QuotaRefusal::Full(LIMIT)));
    let r = [at(100, 0), at(101, LIMIT + 5)];
    assert_eq!(w.admit(LIMIT, 1, 0, inside(102), &r, 1), Err(QuotaRefusal::Full(LIMIT + 5)));
}

#[test]
fn without_the_reading_of_two_epochs_before_nothing_grows() {
    let mut w = QuotaWindow::default();
    assert_eq!(w.admit(LIMIT, 3, 0, inside(102), &[], 1), Err(QuotaRefusal::Unread));
    // The epoch before alone, an older one, or one that could not read the tenant: no.
    let r = [at(99, 0), at(101, 0)];
    assert_eq!(w.admit(LIMIT, 3, 0, inside(102), &r, 1), Err(QuotaRefusal::Unread));
    let failed = Reading { epoch: 100, used: None };
    assert_eq!(w.admit(LIMIT, 3, 0, inside(102), &[failed], 1), Err(QuotaRefusal::Unread));
}

/// A node that started in window 99 counts from window 100: the reading of 100 lets it write in
/// window 102, and nothing older does.
#[test]
fn a_node_counts_only_from_the_window_after_its_start() {
    let mut w = QuotaWindow::default();
    let since = window_of(inside(99)) + 1;
    assert_eq!(w.admit(LIMIT, 1, since, inside(101), &[at(99, 0)], 1), Err(QuotaRefusal::Unread));
    assert_eq!(w.admit(LIMIT, 1, since, inside(102), &[at(100, 0)], 10), Ok(()));
}

/// Each write fits the part of the reading of two epochs before over everything let through
/// since that epoch's window, and the part of the epoch before's reading over its own.
#[test]
fn a_write_fits_both_readings_counted_from_their_windows() {
    let mut w = QuotaWindow::default();
    // Window 102: reading 100 gives 1000 on one node.
    assert_eq!(w.admit(LIMIT, 1, 0, inside(102), &[at(100, 0)], 600), Ok(()));
    // Window 103: reading 101 bounds 101 to 103 by 1000; reading 102 (300 used) 102 to 103 by 700.
    let r = [at(101, 0), at(102, 300)];
    assert_eq!(w.admit(LIMIT, 1, 0, inside(103), &r, 100), Ok(()));
    assert_eq!(w.admit(LIMIT, 1, 0, inside(103), &r, 1), Err(QuotaRefusal::ShareSpent(700)));
    // Without the reading of 102, 101's part alone: 1000 over windows 101 to 103.
    assert_eq!(w.admit(LIMIT, 1, 0, inside(103), &r[..1], 300), Ok(()));
    assert_eq!(w.admit(LIMIT, 1, 0, inside(103), &r[..1], 1), Err(QuotaRefusal::ShareSpent(LIMIT)));
    // Window 104: reading 102 (300 used) bounds 102 to 104, where 1000 went: no room.
    let r = [at(102, 300), at(103, 900)];
    assert_eq!(w.admit(LIMIT, 1, 0, inside(104), &r, 1), Err(QuotaRefusal::ShareSpent(100)));
    // Window 106: the old windows left; reading 104 sees 1000 used.
    let r = [at(104, LIMIT)];
    assert_eq!(w.admit(LIMIT, 1, 0, inside(106), &r, 1), Err(QuotaRefusal::Full(LIMIT)));
    // After a deletion the reading of 105 frees room for window 107.
    let r = [at(105, 400)];
    assert_eq!(w.admit(LIMIT, 1, 0, inside(107), &r, 600), Ok(()));
}

#[test]
fn no_nodes_counts_as_one() {
    let mut w = QuotaWindow::default();
    let r = [at(100, 0)];
    assert_eq!(w.admit(LIMIT, 0, 0, inside(102), &r, LIMIT), Ok(()));
    assert_eq!(w.admit(LIMIT, 0, 0, inside(102), &r, 1), Err(QuotaRefusal::ShareSpent(LIMIT)));
}

/// A reading gives the bytes of a tenant it read, and nothing of one it did not: the node's copy
/// of Group 0 may not have known it.
#[test]
fn a_reading_says_nothing_of_a_tenant_it_did_not_read() {
    let r = NodeReading { epoch: 7, used: [(1, 300)].into_iter().collect() };
    assert_eq!(r.of(1), Reading { epoch: 7, used: Some(300) });
    assert_eq!(r.of(2), Reading { epoch: 7, used: None });
}
