//! Reads on the leader (005 T013, FR-009, FR-007, DV-010): each read concern goes through the
//! lease or ReadIndex, waits for the latches it has to, and enters the read cache when it is
//! admitted, so a write proposed afterwards goes above it.

use teseladb_hlc::Hlc;
use teseladb_multiraft::ReadMode;
use teseladb_range::latch::{Latches, ReadWait};
use teseladb_range::read::{ReadConcern, admit_own_read, admit_read};
use teseladb_range::tscache::TsCache;

fn ts(physical: u64) -> Hlc {
    Hlc { physical, logical: 0 }
}

#[test]
fn each_concern_reads_with_the_lease_or_read_index() {
    assert_eq!(ReadConcern::Linearizable.mode(), ReadMode::ReadIndex);
    for c in [ReadConcern::Majority, ReadConcern::Local, ReadConcern::Available] {
        assert_eq!(c.mode(), ReadMode::Lease, "{c:?}");
    }
}

#[test]
fn an_admitted_read_raises_the_floor_of_later_writes() {
    let latches = Latches::default();
    let mut cache = TsCache::new(ts(0), 16);
    let got = admit_read(&latches, &mut cache, b"\x01a", b"\x01m", ts(100), ReadConcern::Majority);
    assert_eq!(got, ReadWait::Free);
    assert_eq!(cache.floor_of(&[b"\x01c".to_vec()]), ts(100));
    assert_eq!(cache.floor_of(&[b"\x01m".to_vec()]), ts(0), "the end is excluded");
}

#[test]
fn a_read_waits_for_marked_writes_below_it_and_is_not_recorded_while_waiting() {
    let mut latches = Latches::default();
    let mut cache = TsCache::new(ts(0), 16);
    let w = latches.acquire(&[b"\x01c".to_vec()]).unwrap();
    // Not proposed yet: a majority read goes on and the write will be proposed above it.
    assert_eq!(
        admit_read(&latches, &mut cache, b"\x01a", b"\x01m", ts(50), ReadConcern::Majority),
        ReadWait::Free
    );
    latches.set_mark(w, ts(80));
    // Proposed below the read: it waits, and nothing enters the cache yet.
    let mut waiting = TsCache::new(ts(0), 16);
    assert_eq!(
        admit_read(&latches, &mut waiting, b"\x01a", b"\x01m", ts(100), ReadConcern::Linearizable),
        ReadWait::On(w)
    );
    assert!(waiting.is_empty());
    // Above the read: no wait.
    assert_eq!(
        admit_read(&latches, &mut cache, b"\x01a", b"\x01m", ts(70), ReadConcern::Majority),
        ReadWait::Free
    );
    latches.release(w);
    assert_eq!(
        admit_read(&latches, &mut waiting, b"\x01a", b"\x01m", ts(100), ReadConcern::Linearizable),
        ReadWait::Free
    );
    assert_eq!(waiting.floor_of(&[b"\x01c".to_vec()]), ts(100));
}

#[test]
fn local_and_available_reads_wait_for_every_latch_of_their_keys() {
    let mut latches = Latches::default();
    let w = latches.acquire(&[b"\x01c".to_vec()]).unwrap();
    for c in [ReadConcern::Local, ReadConcern::Available] {
        let mut cache = TsCache::new(ts(0), 16);
        assert_eq!(
            admit_read(&latches, &mut cache, b"\x01a", b"\x01m", ts(10), c),
            ReadWait::On(w),
            "{c:?}"
        );
        latches.set_mark(w, ts(500));
        assert_eq!(
            admit_read(&latches, &mut cache, b"\x01a", b"\x01m", ts(10), c),
            ReadWait::On(w),
            "{c:?}"
        );
    }
    let mut cache = TsCache::new(ts(0), 16);
    assert_eq!(
        admit_read(&latches, &mut cache, b"\x01d", b"\x01m", ts(10), ReadConcern::Local),
        ReadWait::Free
    );
}

#[test]
fn a_local_read_at_its_own_mark_reads_below_an_update_in_flight_instead_of_waiting() {
    let mut latches = Latches::default();
    let w = latches.acquire(&[b"\x01c".to_vec()]).unwrap();
    latches.set_mark(w, ts(80));
    let below = Hlc { physical: 79, logical: u32::MAX };
    for c in [ReadConcern::Local, ReadConcern::Available] {
        // YCSB's read of a key being updated: before, it waited for the update (H-010).
        let mut cache = TsCache::new(ts(0), 16);
        let (wait, at) = admit_own_read(&latches, &mut cache, b"\x01a", b"\x01m", ts(100), c);
        assert_eq!((wait, at), (ReadWait::Free, below), "{c:?}");
        // The read cache keeps the read's own mark: later writes still go above it.
        assert_eq!(cache.floor_of(&[b"\x01c".to_vec()]), ts(100));
    }
    // `majority` and `linearizable` reads are as before.
    let mut cache = TsCache::new(ts(0), 16);
    assert_eq!(
        admit_own_read(&latches, &mut cache, b"\x01a", b"\x01m", ts(100), ReadConcern::Majority),
        (ReadWait::On(w), ts(100))
    );
    // Answered from the leader's log (`w: 1`): the read waits for it, so it sees it (DV-010).
    latches.mark_answered(w);
    let mut cache = TsCache::new(ts(0), 16);
    assert_eq!(
        admit_own_read(&latches, &mut cache, b"\x01a", b"\x01m", ts(100), ReadConcern::Local),
        (ReadWait::On(w), ts(100))
    );
}
