//! 007 T011 and T012: a split carried out by the range's leader on three replicas (FR-007,
//! FR-022). Writes on the part being split off wait and then get `OutOfRange`, writes on the rest
//! go on, every replica follows the cut, and the new range, created on every node from the state
//! the cut left in the core's LSM, answers a retry of a statement first applied in its parent with
//! the original result.

mod common;

use common::*;
use teseladb_range::host::SplitDone;
use teseladb_range::sm::RangeState;

const AT: &[u8] = b"\x01m";
const NEW: GroupId = GroupId(9);

fn write_on(c: &mut Cluster, n: NodeId, g: GroupId, key: &[u8], v: u8, stmt: u32) -> ReqId {
    let (now, wall) = (c.now, c.wall_of(n));
    let node = c.nodes.get_mut(&n).unwrap();
    let dedup = Some(DedupKey { lsid: 5, txn_number: 1, stmt_id: stmt });
    let m = Box::new(SetKey(key.to_vec(), v));
    let w = WriteReq { g, keys: vec![key.to_vec()], m, dedup, wc: MAJORITY };
    node.host.mutate(&mut node.core, w, now, wall)
}

fn mutated_ts(r: Option<Result<Reply, RangeError>>) -> Hlc {
    match r {
        Some(Ok(Reply::Mutated { ts, .. })) => ts,
        other => panic!("not a write: {other:?}"),
    }
}

fn leader_of(c: &Cluster, g: GroupId) -> Option<NodeId> {
    c.nodes
        .iter()
        .filter(|(_, n)| n.core.status(g).is_some_and(|s| s.role == Role::Leader))
        .max_by_key(|(_, n)| n.core.status(g).unwrap().hard_state.term)
        .map(|(i, _)| *i)
}

#[test]
fn a_leader_splits_its_range_and_the_new_range_keeps_the_idempotence_table() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let right = write_on(&mut c, l, G, b"\x01q", 1, 1);
    let left = write_on(&mut c, l, G, b"\x01b", 2, 2);
    c.follow();
    let right_ts = mutated_ts(c.reply(l, right));
    mutated_ts(c.reply(l, left));

    // Only the leader takes the split, at a key strictly inside, once.
    for o in c.others(l) {
        assert!(!c.nodes.get_mut(&o).unwrap().host.split(G, AT, NEW));
    }
    let host = &mut c.nodes.get_mut(&l).unwrap().host;
    assert!(!host.split(G, START, NEW));
    assert!(!host.split(G, END, NEW));
    assert!(!host.split(GroupId(99), AT, NEW));
    // A write on the part split off, admitted before the split: the leader waits for it, and its
    // record goes to the new range.
    let inflight = write_on(&mut c, l, G, b"\x01p", 6, 6);
    let host = &mut c.nodes.get_mut(&l).unwrap().host;
    assert!(host.split(G, AT, NEW));
    assert!(!host.split(G, b"\x01n", NEW));

    // Writes issued while it runs: the one on the part split off waits and is refused once the
    // cut is applied; the other one goes through.
    let gone = write_on(&mut c, l, G, b"\x01r", 3, 3);
    let kept = write_on(&mut c, l, G, b"\x01c", 4, 4);
    c.follow();
    assert_eq!(c.reply(l, gone), Some(Err(RangeError::OutOfRange)));
    mutated_ts(c.reply(l, kept));
    let inflight_ts = mutated_ts(c.reply(l, inflight));
    let done = c.nodes.get_mut(&l).unwrap().host.take_splits();
    assert_eq!(done, [SplitDone { g: G, at: AT.to_vec(), new_range: NEW }]);
    for node in c.nodes.values() {
        assert_eq!(node.core.state_machine(G).and_then(RangeState::end), Some(AT));
    }
    // A write whose keys are on both sides of the cut is refused as one that needs a
    // transaction across ranges, which the layer above runs; the range cannot take it whole, and
    // an `OutOfRange` would send it back to the same range until its deadline.
    let (now, wall) = (c.now, c.wall_of(l));
    let node = c.nodes.get_mut(&l).unwrap();
    let m = Box::new(SetKey(b"\x01b".to_vec(), 8));
    let keys = vec![b"\x01b".to_vec(), b"\x01q".to_vec()];
    let w = WriteReq { g: G, keys, m, dedup: None, wc: MAJORITY };
    let across = node.host.mutate(&mut node.core, w, now, wall);
    c.follow();
    match c.reply(l, across) {
        Some(Err(RangeError::Mutation(e))) => {
            assert!(e.0.starts_with(teseladb_range::host::CROSSES_RANGES), "{e:?}");
        }
        other => panic!("not refused as crossing ranges: {other:?}"),
    }
    // A follower that leads next refuses the keys it gave away at once.
    let later = write_on(&mut c, l, G, b"\x01s", 5, 5);
    c.follow();
    assert_eq!(c.reply(l, later), Some(Err(RangeError::OutOfRange)));

    // Each half counts its own live bytes (D-068): the right one what the leader read before the
    // split, the left one the rest.
    for node in c.nodes.values() {
        let left = node.core.state_machine(G).unwrap().live_bytes();
        assert!(left > 0, "{left}");
    }
    // The embedder creates the new range on every node, as Group 0 completes the split.
    let at_index = c.nodes[&l].core.status(G).unwrap().applied;
    let voters: Vec<NodeId> = (1..=3).map(id).collect();
    for node in c.nodes.values_mut() {
        let lsm = node.host.lsm().clone();
        let sm = RangeSm::open(NEW, AT.to_vec(), END.to_vec(), lsm, &mut node.disk).unwrap();
        assert!(sm.last_mark() >= right_ts);
        assert!(sm.live_bytes() > 0, "the new range counts the documents it took");
        let desc = GroupDesc { group: NEW, voters: voters.clone(), learners: vec![] };
        node.core.create_group(desc, sm).unwrap();
        node.host.add_born_range(NEW, AT.to_vec(), END.to_vec(), G, at_index);
    }
    let mut nl = None;
    for _ in 0..1_000 {
        c.ticks_following(1);
        nl = leader_of(&c, NEW);
        let ready = nl.and_then(|n| c.nodes[&n].core.status(NEW)).is_some_and(|s| s.applied >= 2);
        if ready {
            break;
        }
    }
    let nl = nl.expect("the new range elects a leader");
    // Its first leader proposed SplitGenesis after its Noop.
    assert!(c.nodes[&nl].core.status(NEW).unwrap().applied >= 2);

    // The retry of the statement applied in the parent gets its original mark, and changes
    // nothing; a new statement is applied in the new range.
    let retry = write_on(&mut c, nl, NEW, b"\x01q", 9, 1);
    let fresh = write_on(&mut c, nl, NEW, b"\x01r", 7, 10);
    let wrong_side = write_on(&mut c, nl, NEW, b"\x01b", 7, 11);
    let retry_inflight = write_on(&mut c, nl, NEW, b"\x01p", 9, 6);
    c.follow();
    assert_eq!(mutated_ts(c.reply(nl, retry_inflight)), inflight_ts);
    assert_eq!(mutated_ts(c.reply(nl, retry)), right_ts);
    assert!(mutated_ts(c.reply(nl, fresh)) > right_ts);
    assert_eq!(c.reply(nl, wrong_side), Some(Err(RangeError::OutOfRange)));
}

/// A range keeps publishing its view while it splits, and the poll that follows the cut
/// publishes the view of the keys it keeps (005 FR-028, `spec/tla/SplitView.tla`): the scans of
/// the range stay on their connections' threads meanwhile, and none reads the old range's view
/// for a key it gave away.
#[test]
fn a_range_that_splits_keeps_its_view_until_the_cut_and_then_narrows_it() {
    use teseladb_range::read::ReadConcern;
    use teseladb_range::view::Part;
    let mut c = Cluster::single();
    let l = c.elect();
    write_on(&mut c, l, G, b"\x01b", 2, 1);
    write_on(&mut c, l, G, b"\x01q", 1, 2);
    c.follow();
    let scan = |c: &Cluster, span: (&[u8], &[u8])| {
        let view = c.nodes[&l].host.views().get(G)?;
        let mut cache = teseladb_lsm::ViewCache::default();
        let part = Part { docs: 10, backward: false, blocks: 64 };
        let read = view.scan(&mut cache, span, part, || c.now, ReadConcern::Local, |_| false)?;
        Some(read.rows.into_iter().map(|(k, _)| k).collect::<Vec<_>>())
    };
    let both = Some(vec![b"\x01b".to_vec(), b"\x01q".to_vec()]);
    assert_eq!(scan(&c, (START, END)), both);
    // The split starts and its leader polls once: it is reading the idempotence table, the cut
    // is not proposed yet, and the view of the whole range is still there.
    let (now, wall) = (c.now, c.wall_of(l));
    let node = c.nodes.get_mut(&l).unwrap();
    assert!(node.host.split(G, AT, NEW));
    node.host.poll(&mut node.core, now, wall);
    assert!(node.host.take_splits().is_empty(), "the cut is not applied yet");
    assert_eq!(scan(&c, (START, END)), both, "served outside the core while it splits");
    // Once the cut is applied the view is that of the keys the range keeps.
    c.follow();
    let done = c.nodes.get_mut(&l).unwrap().host.take_splits();
    assert_eq!(done, [SplitDone { g: G, at: AT.to_vec(), new_range: NEW }]);
    assert_eq!(scan(&c, (START, END)), None, "the span reaches keys the range gave away");
    assert_eq!(scan(&c, (START, AT)), Some(vec![b"\x01b".to_vec()]));
}

/// A read of `key` through `view` by a thread that took the view at `taken` and was stopped: its
/// mark comes now, and by its own monotonic time the view has not run out.
fn read_stopped(
    c: &Cluster,
    n: NodeId,
    view: &teseladb_range::view::RangeView,
    key: &[u8],
    taken: Nanos,
) -> teseladb_range::view::FastRead {
    let mut cache = teseladb_lsm::ViewCache::default();
    let concern = teseladb_range::read::ReadConcern::Local;
    view.read(&mut cache, key, c.wall_of(n), || taken, concern)
}

/// The view a thread took before a cut serves nothing once the cut is applied (005 FR-028,
/// `spec/tla/ViewWithdrawn.tla`, H-058 of the native tests). The new range, which leads at once
/// when it has one voter, takes the marks of its writes from the same clock as that thread: a
/// read through the old view at a later mark would miss a write of it that is not applied yet.
/// The view was limited only in time, 50 ms from its publication.
#[test]
fn a_view_taken_before_a_cut_serves_nothing_once_the_cut_is_applied() {
    use teseladb_range::view::FastRead;
    let mut c = Cluster::single();
    let l = c.elect();
    write_on(&mut c, l, G, b"\x01b", 2, 1);
    write_on(&mut c, l, G, b"\x01q", 1, 2);
    c.follow();
    let wide = c.nodes[&l].host.views().get(G).expect("the view of the whole range");
    let taken = c.now;
    assert!(matches!(read_stopped(&c, l, &wide, b"\x01q", taken), FastRead::Found { .. }));
    assert!(c.nodes.get_mut(&l).unwrap().host.split(G, AT, NEW));
    c.follow();
    let done = c.nodes.get_mut(&l).unwrap().host.take_splits();
    assert_eq!(done, [SplitDone { g: G, at: AT.to_vec(), new_range: NEW }]);
    // Neither a key it gave away nor one it keeps: the thread asks the core.
    assert!(matches!(read_stopped(&c, l, &wide, b"\x01q", taken), FastRead::Core));
    assert!(matches!(read_stopped(&c, l, &wide, b"\x01b", taken), FastRead::Core));
    // The view published after the cut stands.
    let narrow = c.nodes[&l].host.views().get(G).expect("the view of the keys it keeps");
    assert!(matches!(read_stopped(&c, l, &narrow, b"\x01b", c.now), FastRead::Found { .. }));
}

/// The view of a term serves nothing once its leader lost the term (FR-028): the same replica
/// may win a later one and write, with marks of the same clock, before the view runs out.
#[test]
fn a_view_of_a_term_serves_nothing_once_its_leader_lost_the_term() {
    use teseladb_range::view::FastRead;
    let mut c = Cluster::standard();
    let l = c.elect();
    write_on(&mut c, l, G, b"\x01b", 2, 1);
    c.follow();
    // The view comes with a round of the lease that covers its whole span.
    let mut view = None;
    for _ in 0..50 {
        c.ticks_following(1);
        view = c.nodes[&l].host.views().get(G);
        if view.is_some() {
            break;
        }
    }
    let view = view.expect("the leader's view");
    let taken = c.now;
    assert!(matches!(read_stopped(&c, l, &view, b"\x01b", taken), FastRead::Found { .. }));
    // Cut off, it loses the term to another voter and hears of it once the cut heals.
    c.isolate(l);
    for _ in 0..1_000 {
        c.ticks(1);
        if leader_of(&c, G).is_some_and(|n| n != l) {
            break;
        }
    }
    c.cut.clear();
    c.ticks_following(5);
    assert!(c.nodes[&l].core.status(G).is_some_and(|s| s.role != Role::Leader), "it follows");
    assert!(matches!(read_stopped(&c, l, &view, b"\x01b", taken), FastRead::Core));
}

#[test]
fn a_table_that_cannot_be_read_drops_the_split_until_it_is_asked_again() {
    let mut c = Cluster::standard();
    let l = c.elect();
    // A key in the idempotence space that is not a record.
    let (bad, _) = teseladb_range::sm::dedup_interval(G);
    let lsm = c.nodes[&l].host.lsm().clone();
    let w = teseladb_lsm::EntryWrite { key: &bad, kind: teseladb_lsm::Kind::Put, value: b"x" };
    lsm.borrow_mut().write_entry(GroupId(1000), 1, Hlc { physical: 1, logical: 0 }, &[w]).unwrap();
    assert!(c.nodes.get_mut(&l).unwrap().host.split(G, AT, NEW));
    c.follow();
    let node = c.nodes.get_mut(&l).unwrap();
    assert!(node.host.take_splits().is_empty());
    assert_eq!(node.core.state_machine(G).and_then(RangeState::end), Some(END));
    // The job is gone, so the embedder can ask again.
    assert!(node.host.split(G, AT, NEW));
}

#[test]
fn a_new_leader_refuses_the_keys_a_cut_it_applied_gave_away_before_its_host_followed_it() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.follow();
    assert!(c.nodes.get_mut(&l).unwrap().host.split(G, AT, NEW));
    // Every replica applies the cut, but no host follows its state machine yet.
    c.ticks(5);
    for node in c.nodes.values() {
        assert_eq!(node.core.state_machine(G).and_then(RangeState::end), Some(AT));
    }
    // Another replica leads; a write past the cut reaches it before its host followed the cut.
    c.isolate(l);
    let f = (0..1_000)
        .find_map(|_| {
            c.ticks(1);
            leader_of(&c, G).filter(|n| *n != l).filter(|n| {
                let s = c.nodes[n].core.status(G).unwrap();
                s.applied >= s.hard_state.commit
            })
        })
        .expect("a new leader");
    // Asked for the same split again (Group 0 has not seen the cut yet), it refuses it: a second
    // `Split` at the range's end would stop every replica.
    assert!(!c.nodes.get_mut(&f).unwrap().host.split(G, AT, NEW));
    let w = write_on(&mut c, f, G, b"\x01s", 5, 5);
    c.ticks(5);
    assert_eq!(c.reply(f, w), Some(Err(RangeError::OutOfRange)));
    assert!(!c.nodes[&f].core.has_stopped_group(), "the range never applies a key it gave away");
}

/// A change stream's read of the whole range that waits (here for the wall clock to reach its
/// mark) while a split cuts the range: once it can go on, the part split off is no longer this
/// range's, and on a replica of the new range waiting for a snapshot its data may be gone (cleared
/// for the ingest). The read is refused with `OutOfRange` and the reader asks the ranges again;
/// it does not resolve the whole interval up to its mark without the versions on the part split
/// off (seed 548 of the simulator's `search`, plan `default`: a search missed a committed write).
#[test]
fn a_stream_read_that_waits_across_a_split_is_refused() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let w = write_on(&mut c, l, G, b"\x01q", 1, 1);
    c.follow();
    mutated_ts(c.reply(l, w));
    let mark = Hlc { physical: c.wall_of(l) + 300, logical: 0 };
    let now = c.now;
    let node = c.nodes.get_mut(&l).unwrap();
    let zero = Hlc { physical: 0, logical: 0 };
    let req = teseladb_range::host::ChangesReq {
        g: G,
        start: START.to_vec(),
        end: END.to_vec(),
        from: zero,
        at: Some(mark),
    };
    let read = node.host.changes(&mut node.core, req, now);
    c.settle();
    assert_eq!(c.reply(l, read), None, "it waits for the clock");
    assert!(c.nodes.get_mut(&l).unwrap().host.split(G, AT, NEW));
    c.follow();
    for node in c.nodes.values() {
        assert_eq!(node.core.state_machine(G).and_then(RangeState::end), Some(AT));
    }
    for _ in 0..100 {
        c.ticks_following(1);
        if c.reply(l, read).is_some() {
            break;
        }
    }
    assert_eq!(c.reply(l, read), Some(Err(RangeError::OutOfRange)));
}

/// Writes its document and, as an index entry does, another key its evaluation finds.
struct WithEntry(Vec<u8>, Vec<u8>);

impl Mutation for WithEntry {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let put = |key: &[u8]| Put { key: key.to_vec(), value_type: 1, value: vec![1] };
        Ok(Outcome {
            puts: vec![put(&self.0), put(&self.1)],
            deletes: vec![],
            change: vec![],
            result: vec![],
        })
    }
}

#[test]
fn a_write_whose_entry_falls_past_a_cut_in_progress_needs_a_transaction_across_ranges() {
    // Found by the sustained load of 002 (11-11-2026): a document left of the cut, admitted
    // while the split ran, wrote an index entry right of it; the entry went into the log after
    // the cut and the range stopped applying, its keys unreachable until the node restarted.
    let mut c = Cluster::standard();
    let l = c.elect();
    c.follow();
    assert!(c.nodes.get_mut(&l).unwrap().host.split(G, AT, NEW));
    let (now, wall) = (c.now, c.wall_of(l));
    let node = c.nodes.get_mut(&l).unwrap();
    let m = Box::new(WithEntry(b"\x01b".to_vec(), b"\x01q".to_vec()));
    let w = WriteReq { g: G, keys: vec![b"\x01b".to_vec()], m, dedup: None, wc: MAJORITY };
    let across = node.host.mutate(&mut node.core, w, now, wall);
    c.follow();
    match c.reply(l, across) {
        Some(Err(RangeError::Mutation(e))) => {
            assert!(e.0.starts_with(teseladb_range::host::CROSSES_RANGES), "{e:?}");
        }
        other => panic!("not refused as crossing ranges: {other:?}"),
    }
    assert_eq!(c.nodes[&l].core.state_machine(G).and_then(RangeState::end), Some(AT));
    // The range goes on applying: a write of its own keys after the cut.
    let after = write_on(&mut c, l, G, b"\x01c", 4, 4);
    c.follow();
    mutated_ts(c.reply(l, after));
}

/// H-065 of the native tests (FR-022): a split gives its new range only the records of the
/// statements that wrote a key in it, since a retry goes to the range of its key. A table copied
/// whole made the cut's entry pass the log's largest entry once the range held minutes of writes,
/// and the leader asked for the cut again without end.
/// What the new range's table holds of the statements 1 (right of the cut) and 2 (left) after a
/// split, on every node, with the nodes reading records that keep their key or not.
fn copied_after_split(reaching: bool) -> Vec<(Option<Vec<u8>>, Option<Vec<u8>>)> {
    use teseladb_lsm::Lookup;
    use teseladb_range::sm::dedup_key;
    let mut c = Cluster::standard();
    for node in c.nodes.values_mut() {
        node.host.set_counted_writes(reaching);
        node.host.set_reaching_records(reaching);
    }
    let l = c.elect();
    let right = write_on(&mut c, l, G, b"\x01q", 1, 1);
    let left = write_on(&mut c, l, G, b"\x01b", 2, 2);
    c.follow();
    mutated_ts(c.reply(l, right));
    mutated_ts(c.reply(l, left));
    assert!(c.nodes.get_mut(&l).unwrap().host.split(G, AT, NEW));
    c.follow();
    let done = c.nodes.get_mut(&l).unwrap().host.take_splits();
    assert_eq!(done, [SplitDone { g: G, at: AT.to_vec(), new_range: NEW }]);
    assert!(c.nodes.get_mut(&l).unwrap().host.take_split_drops().is_empty());
    let newest = Hlc { physical: u64::MAX, logical: u32::MAX };
    c.nodes
        .values()
        .map(|node| {
            let lsm = node.host.lsm();
            let view = lsm.borrow().view();
            let stored = |g: GroupId, stmt: u32| match view.get(
                &mut lsm.borrow_mut(),
                &dedup_key(g, 5, 1, stmt),
                newest,
            ) {
                Lookup::Hit(f) => Some(f.value.to_vec()),
                Lookup::NotFound => None,
                other => panic!("{other:?}"),
            };
            // The parent keeps both records.
            assert!(stored(G, 1).is_some() && stored(G, 2).is_some());
            (stored(NEW, 1), stored(NEW, 2))
        })
        .collect()
}

/// H-065 of the native tests (FR-022): a split gives its new range only the records of the
/// statements that wrote a key in it, since a retry goes to the range of its key. A table copied
/// whole made the cut's entry pass the log's largest entry once the range held minutes of writes,
/// and the leader asked for the cut again without end.
#[test]
fn a_split_gives_the_new_range_only_the_records_a_retry_can_bring_there() {
    use teseladb_range::sm::{parse_record_value, record_reach};
    for (right, left) in copied_after_split(true) {
        let right = right.expect("the right statement's record is copied");
        assert!(parse_record_value(&right).is_some());
        assert_eq!(record_reach(&right), Some(&b"\x01q"[..]));
        assert_eq!(left, None, "a left statement cannot be retried in the new range");
    }
    // Nodes that do not read the records that keep their key: the table goes whole, as before.
    for (right, left) in copied_after_split(false) {
        assert!(right.is_some() && left.is_some());
        assert_eq!(record_reach(&right.unwrap()), None);
    }
}
