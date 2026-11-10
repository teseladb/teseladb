//! A mutation that changes nothing, answered on a connection's thread (005 FR-030): the view of
//! the range evaluates it at a mark taken as a read outside the core takes one, and answers it
//! when it changes nothing, has no stored result and none of its keys or its record is held; any
//! other case goes to the core, which answers it as before (FR-026).

mod common;

use common::*;
use teseladb_lsm::ViewCache;
use teseladb_range::view::{FastWrite, NoChange};

const KEY: &[u8] = b"\x01k";
const OTHER: &[u8] = b"\x01m";
const K: DedupKey = DedupKey { lsid: 0x5eed, txn_number: 3, stmt_id: 0 };

/// Changes nothing and answers 9.
struct Nothing;

impl Mutation for Nothing {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        Ok(Outcome { puts: vec![], deletes: vec![], change: vec![], result: vec![9] })
    }
}

/// Writes byte 1 under `KEY` and answers 0.
struct Write;

impl Mutation for Write {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        Ok(Outcome {
            puts: vec![Put { key: KEY.to_vec(), value_type: 1, value: vec![1] }],
            deletes: vec![],
            change: vec![b'+'],
            result: vec![0],
        })
    }
}

/// Changes nothing and answers what it read under its key: 0 for nothing.
struct Echo;

impl Mutation for Echo {
    fn eval(&self, current: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let seen = current[0].map_or(0, |(_, v)| v[0]);
        Ok(Outcome { puts: vec![], deletes: vec![], change: vec![], result: vec![seen] })
    }
}

/// A leader whose view serves the range.
fn cluster() -> (Cluster, NodeId) {
    let mut c = Cluster::standard();
    let l = c.elect();
    // The leader's lease starts once its term commits: its view is published from then on.
    c.ticks(5);
    (c, l)
}

/// `m` on `keys` on the leader's thread outside the core.
fn outside(
    c: &Cluster,
    l: NodeId,
    m: &dyn Mutation,
    keys: &[&[u8]],
    (dedup, wc): (Option<DedupKey>, WriteConcern),
) -> FastWrite {
    let view = c.nodes[&l].host.views().get(G).expect("the leader's view");
    let keys: Vec<Vec<u8>> = keys.iter().map(|k| k.to_vec()).collect();
    let w = NoChange { mutation: m, keys: &keys, dedup, wc, deadline: None, absent_only: false };
    view.no_change(&mut ViewCache::default(), &w, c.wall_of(l), || c.now)
}

fn answered(f: FastWrite) -> (Hlc, Vec<u8>) {
    match f {
        FastWrite::Answered { ts, result } => (ts, result),
        other => panic!("not answered outside the core: {other:?}"),
    }
}

#[test]
fn a_mutation_without_change_is_answered_on_the_connections_thread() {
    let (mut c, l) = cluster();
    let before = c.nodes[&l].core.status(G).map_or(0, |st| st.last_index);
    let (ts, result) = answered(outside(&c, l, &Nothing, &[KEY], (Some(K), MAJORITY)));
    assert_eq!(result, vec![9]);
    let w1 = WriteConcern { w: W::Nodes(1), j: false };
    assert!(matches!(outside(&c, l, &Nothing, &[KEY], (None, w1)), FastWrite::Answered { .. }));
    let after = c.nodes[&l].core.status(G).map_or(0, |st| st.last_index);
    assert_eq!(after, before, "no entry proposed");
    // Its mark bounds the next write of its key, as a read's does.
    let w = c.mutate_with(l, &[KEY], Box::new(Write), None);
    c.ticks(2);
    match c.reply(l, w) {
        Some(Ok(Reply::Mutated { ts: written, .. })) => assert!(written > ts, "{written:?}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_evaluation_sees_what_is_applied_at_its_mark() {
    let (mut c, l) = cluster();
    let w = c.mutate_with(l, &[KEY], Box::new(Write), None);
    c.ticks(2);
    assert!(c.reply(l, w).is_some());
    assert_eq!(answered(outside(&c, l, &Echo, &[KEY], (None, MAJORITY))).1, vec![1]);
}

#[test]
fn with_absent_only_a_key_with_a_version_goes_to_the_core_unevaluated() {
    let (mut c, l) = cluster();
    let w = c.mutate_with(l, &[KEY], Box::new(Write), None);
    c.ticks(2);
    assert!(c.reply(l, w).is_some());
    let view = c.nodes[&l].host.views().get(G).expect("the leader's view");
    let ask = |key: &[u8]| {
        let keys = vec![key.to_vec()];
        let w = NoChange {
            mutation: &Echo,
            keys: &keys,
            dedup: None,
            wc: MAJORITY,
            deadline: None,
            absent_only: true,
        };
        view.no_change(&mut ViewCache::default(), &w, c.wall_of(l), || c.now)
    };
    assert!(matches!(ask(KEY), FastWrite::Core), "the document is there");
    assert_eq!(answered(ask(OTHER)).1, vec![0], "absent: answered");
}

#[test]
fn a_mutation_that_changes_something_or_has_a_stored_result_goes_to_the_core() {
    let (mut c, l) = cluster();
    assert!(matches!(outside(&c, l, &Write, &[KEY], (None, MAJORITY)), FastWrite::Core));
    // The statement wrote the first time: its retry gets the stored result from the core.
    let first = c.mutate_with(l, &[KEY], Box::new(Write), Some(K));
    c.ticks(2);
    assert!(c.reply(l, first).is_some());
    assert!(matches!(outside(&c, l, &Nothing, &[KEY], (Some(K), MAJORITY)), FastWrite::Core));
    let other_statement = DedupKey { stmt_id: 1, ..K };
    let free = outside(&c, l, &Nothing, &[KEY], (Some(other_statement), MAJORITY));
    assert!(matches!(free, FastWrite::Answered { .. }));
}

#[test]
fn a_write_concern_the_applied_state_does_not_meet_goes_to_the_core() {
    let (c, l) = cluster();
    let journaled = WriteConcern { w: W::Nodes(1), j: true };
    let two = WriteConcern { w: W::Nodes(2), j: false };
    for wc in [journaled, two] {
        assert!(matches!(outside(&c, l, &Nothing, &[KEY], (None, wc)), FastWrite::Core), "{wc:?}");
    }
    // A key of another range is not this view's.
    assert!(matches!(outside(&c, l, &Nothing, &[b"\x02z"], (None, MAJORITY)), FastWrite::Core));
}

#[test]
fn a_write_in_flight_on_its_key_or_record_sends_it_to_the_core() {
    let (mut c, l) = cluster();
    let w = c.mutate_with(l, &[KEY], Box::new(Write), Some(K));
    assert!(matches!(outside(&c, l, &Nothing, &[KEY], (None, MAJORITY)), FastWrite::Core));
    // Its record is held too: the retry of the statement on another key waits for it.
    assert!(matches!(outside(&c, l, &Nothing, &[OTHER], (Some(K), MAJORITY)), FastWrite::Core));
    // Another key without the statement does not wait.
    let free = outside(&c, l, &Nothing, &[OTHER], (None, MAJORITY));
    assert!(matches!(free, FastWrite::Answered { .. }));
    c.ticks(2);
    assert!(c.reply(l, w).is_some());
}

#[test]
fn a_mark_past_the_deadline_goes_to_the_core() {
    let (c, l) = cluster();
    let view = c.nodes[&l].host.views().get(G).expect("the leader's view");
    let keys = vec![KEY.to_vec()];
    let deadline = Some(c.wall_of(l) - 1);
    let w = NoChange {
        mutation: &Nothing,
        keys: &keys,
        dedup: None,
        wc: MAJORITY,
        deadline,
        absent_only: false,
    };
    let f = view.no_change(&mut ViewCache::default(), &w, c.wall_of(l), || c.now);
    assert!(matches!(f, FastWrite::Core));
}
