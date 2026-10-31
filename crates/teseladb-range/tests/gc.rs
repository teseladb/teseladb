//! GC threshold of a range (005 T022, FR-013, D-020): the leader proposes `SetGcThreshold` with
//! «now − gc_ttl» every `GC_EVERY_MS` of cluster time, with a `gc_ttl` of 24 hours; every replica
//! applies it and hands its threshold to the LSM, whose compaction then drops the versions no read
//! may ask for any more. A new leader goes on with the rounds.

mod common;

use common::*;
use teseladb_lsm::Step;
use teseladb_range::gc::{GC_EVERY_MS, GC_TTL_MS, Round, table, threshold};
use teseladb_range::snapshot::local_interval;

const ZERO: Hlc = Hlc { physical: 0, logical: 0 };
const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };

fn cluster() -> Cluster {
    let lsm = LsmConfig {
        dir: "core-00/lsm".into(),
        compaction_bytes_per_sec: 1 << 40,
        ..LsmConfig::default()
    };
    Cluster::new(lsm, 1024)
}

fn gc_threshold(c: &Cluster, n: NodeId) -> Hlc {
    c.nodes[&n].core.state_machine(G).unwrap().gc_threshold()
}

/// Flushes and compacts every file of node `n`'s LSM into the last level.
fn compact(c: &mut Cluster, n: NodeId) {
    let now = c.now;
    let node = c.nodes.get_mut(&n).unwrap();
    let lsm = node.host.lsm().clone();
    lsm.borrow_mut().flush();
    lsm.borrow_mut().run_io(&mut node.disk, now);
    lsm.borrow_mut().compact_all();
    lsm.borrow_mut().run_io(&mut node.disk, now);
}

/// The versions of `KEY` node `n`'s LSM still holds, from the newest.
fn versions(c: &mut Cluster, n: NodeId) -> Vec<Hlc> {
    versions_of(c, n, KEY)
}

/// The versions of `key` node `n`'s LSM still holds, from the newest.
fn versions_of(c: &mut Cluster, n: NodeId, key: &[u8]) -> Vec<Hlc> {
    let now = c.now;
    let node = c.nodes.get_mut(&n).unwrap();
    let lsm = node.host.lsm().clone();
    let end = [key, &[0][..]].concat();
    let mut scan = lsm.borrow().view().scan_time(key, &end, ZERO, NEWEST);
    let mut found = Vec::new();
    for _ in 0..1000 {
        let step = scan.next(&mut lsm.borrow_mut());
        match step {
            Step::Item(i) => found.push(i.ts),
            Step::Miss(_) => {
                lsm.borrow_mut().run_io(&mut node.disk, now);
            }
            Step::Done => return found,
            Step::Failed(e) => panic!("the LSM failed: {e:?}"),
        }
    }
    panic!("the scan never ended")
}

fn write(c: &mut Cluster, l: NodeId, v: u8) -> Hlc {
    let w = c.mutate(l, KEY, v, MAJORITY);
    c.settle();
    match c.reply(l, w) {
        Some(Ok(Reply::Mutated { ts, .. })) => ts,
        other => panic!("the write failed: {other:?}"),
    }
}

#[test]
fn the_threshold_is_now_minus_24_hours() {
    assert_eq!(GC_TTL_MS, 24 * 60 * 60 * 1000);
    let now = WALL0;
    assert_eq!(threshold(now), Some(Hlc { physical: now - GC_TTL_MS, logical: 0 }));
    assert_eq!(threshold(GC_TTL_MS - 1), None, "no threshold before the first day of the clock");
}

#[test]
fn the_lsm_table_covers_the_local_keys_and_the_data_of_each_range() {
    let t = Hlc { physical: 5, logical: 0 };
    let table = table(&[(G, START.to_vec(), END.to_vec(), t)]);
    let (local, _) = local_interval(G);
    assert_eq!(table.threshold(KEY), Some(t));
    assert_eq!(table.threshold(&local), Some(t));
    assert_eq!(table.threshold(END), None);
}

#[test]
fn a_round_is_due_once_its_time_comes() {
    assert!(Round::Idle(10).due(10));
    assert!(!Round::Idle(10).due(9));
    assert!(!Round::Proposed.due(u64::MAX));
}

#[test]
fn the_leader_raises_the_threshold_on_every_replica_and_the_lsm_drops_old_versions() {
    let mut c = cluster();
    let l = c.elect();
    let old = write(&mut c, l, 1);
    let new = write(&mut c, l, 2);
    for n in c.nodes.keys().copied().collect::<Vec<_>>() {
        compact(&mut c, n);
        assert_eq!(versions(&mut c, n), vec![new, old], "both versions are younger than a day");
    }
    // A day and a bit of cluster time later the threshold passes both versions.
    c.wall_ahead = GC_TTL_MS + 2 * GC_EVERY_MS;
    c.ticks_following(JUMP_TICKS);
    let raised = gc_threshold(&c, l);
    assert!(raised > new, "the threshold {raised:?} passed the newest version {new:?}");
    assert!(raised.physical <= c.wall() - GC_TTL_MS);
    for n in c.nodes.keys().copied().collect::<Vec<_>>() {
        assert_eq!(gc_threshold(&c, n), raised, "every replica applied it");
        compact(&mut c, n);
        assert_eq!(versions(&mut c, n), vec![new], "only the version a read at the threshold sees");
    }
}

#[test]
fn the_threshold_goes_up_every_round_also_under_a_new_leader() {
    let mut c = cluster();
    let l = c.elect();
    c.wall_ahead = 10 * GC_EVERY_MS;
    c.ticks_following(JUMP_TICKS);
    let first = gc_threshold(&c, l);
    assert!(first.physical > 0, "a new leader proposes the threshold at once");
    // Before the next round nothing changes.
    c.wall_ahead += GC_EVERY_MS / 2;
    c.ticks_following(JUMP_TICKS);
    assert_eq!(gc_threshold(&c, l), first);
    c.wall_ahead += GC_EVERY_MS;
    c.ticks_following(JUMP_TICKS);
    let second = gc_threshold(&c, l);
    assert!(second > first, "the next round raises it");
    // Another leader takes over and goes on.
    c.isolate(l);
    let mut next = l;
    for _ in 0..1_000 {
        c.ticks_following(1);
        if let Some(n) = c.leader().filter(|&n| n != l) {
            next = n;
            break;
        }
    }
    assert_ne!(next, l, "the majority elects another leader");
    c.wall_ahead += GC_EVERY_MS;
    c.ticks_following(JUMP_TICKS);
    assert!(gc_threshold(&c, next) > second, "the new leader raises it too");
}

#[test]
fn a_handover_leaves_the_thresholds_of_other_keys_alone() {
    // Another range of the same core, outside this one, has its threshold in the LSM (it could be
    // a clearing's): handing this range's threshold replaces only this range's intervals.
    const OTHER: &[u8] = b"\x02other";
    let other = GroupId(99);
    let (old, new) = (Hlc { physical: 1, logical: 0 }, Hlc { physical: 2, logical: 0 });
    let mut c = cluster();
    let l = c.elect();
    write(&mut c, l, 1);
    for node in c.nodes.values() {
        let mut lsm = node.host.lsm().borrow_mut();
        lsm.put(other, 1, OTHER, old, b"old").unwrap();
        lsm.put(other, 2, OTHER, new, b"new").unwrap();
        let end = [OTHER, &[0][..]].concat();
        lsm.set_range_gc_threshold(&[(OTHER.to_vec(), end)], new);
    }
    // A day and a bit later this range's threshold goes up on every replica, and is handed.
    c.wall_ahead = GC_TTL_MS + 2 * GC_EVERY_MS;
    c.ticks_following(JUMP_TICKS);
    assert!(gc_threshold(&c, l) > ZERO);
    for n in c.nodes.keys().copied().collect::<Vec<_>>() {
        compact(&mut c, n);
        assert_eq!(versions_of(&mut c, n, OTHER), vec![new], "the other threshold still holds");
    }
}

#[test]
fn a_new_leader_proposes_its_rounds_above_the_entries_of_the_term_before() {
    // 005 FR-004, audit A005-1: the old leader writes at a mark far ahead of the clocks (its HLC
    // received that mark), and the followers keep that entry in their logs without knowing it
    // committed. The new leader's first `SetGcThreshold` goes after it in the log, so its mark
    // has to be above it. (A read at a client's mark far ahead is no longer served at once, 008:
    // it waits for the clock or fails, so the mark reaches the HLC here directly.)
    let mut c = cluster();
    let l = c.elect();
    let far = Hlc { physical: c.wall() + 10_000, logical: 0 };
    c.nodes[&l].clock.0.update(far);
    for o in c.others(l) {
        c.cut.insert((o, l));
    }
    c.mutate(l, KEY, 7, MAJORITY);
    c.settle();
    c.isolate(l);
    let mut next = l;
    for _ in 0..1_000 {
        c.ticks_following(1);
        if let Some(n) = c.leader().filter(|&n| n != l) {
            next = n;
            break;
        }
    }
    assert_ne!(next, l, "the majority elects another leader");
    // A write that arrives first waits for the same entries and then goes through.
    let late = c.mutate_key(next, b"\x01late", 3, MAJORITY);
    c.ticks_following(20);
    assert!(matches!(c.reply(next, late), Some(Ok(Reply::Mutated { .. }))));
    let written = versions(&mut c, next);
    assert!(written.first().is_some_and(|ts| *ts > far), "the old leader's write committed");
    let gc = versions_of(&mut c, next, &teseladb_range::sm::gc_threshold_key(G));
    assert!(gc[0] > written[0], "the new leader's round is above the entry before it");
}

#[test]
fn a_read_below_the_gc_threshold_fails_with_snapshot_too_old() {
    // Audit A005-4: below the threshold the compaction may have dropped the versions the read
    // needs; RefDb answers `SnapshotTooOld` (239) to an `atClusterTime` older than its history.
    let mut c = cluster();
    let l = c.elect();
    write(&mut c, l, 1);
    c.wall_ahead = 10 * GC_EVERY_MS;
    c.ticks_following(JUMP_TICKS);
    let thr = gc_threshold(&c, l);
    assert!(thr.physical > 0);
    let below = Hlc { physical: thr.physical - 1, logical: 0 };
    let (now, wall) = (c.now, c.wall());
    let node = c.nodes.get_mut(&l).unwrap();
    let old = node.host.get_at(&mut node.core, G, KEY, below, now, wall);
    let at = node.host.get_at(&mut node.core, G, KEY, thr, now, wall);
    c.settle();
    assert_eq!(c.reply(l, old), Some(Err(RangeError::SnapshotTooOld)));
    assert!(matches!(c.reply(l, at), Some(Ok(Reply::Value { .. }))));
}
