//! A write refused by its mutation (a compare that does not match) answers with what the replica
//! holds, so a deposed leader must not give that answer from its stale copy: it waits until the
//! replica confirms it still leads, as a read does (005 FR-009). Found with the simulator's
//! variant `write_below_read` and a leader cut off at the edge of the clock margin (seed 214 of
//! `range-docs`, heavy plan): the old leader refused a compare-and-swap against a value the new
//! leader had already replaced and acknowledged.

mod common;

use common::*;

/// Fails unless the document holds `expected`.
struct Expect(u8);

impl Mutation for Expect {
    fn eval(&self, current: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        match current.first().copied().flatten() {
            Some((_, v)) if v == [self.0] => Ok(Outcome {
                puts: vec![Put { key: KEY.to_vec(), value_type: 1, value: vec![self.0 + 1] }],
                deletes: vec![],
                change: vec![],
                result: vec![],
            }),
            _ => Err(MutationError(format!("does not hold {}", self.0))),
        }
    }
}

#[test]
fn a_deposed_leader_does_not_refuse_a_write_from_its_stale_copy() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let w = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    assert!(matches!(c.reply(l, w), Some(Ok(Reply::Mutated { .. }))));
    // Cut off, its lease runs out and the others elect a leader that writes 2.
    c.isolate(l);
    let mut n = None;
    for _ in 0..1_000 {
        c.ticks(1);
        n = c
            .others(l)
            .into_iter()
            .find(|o| c.nodes[o].core.status(G).unwrap().role == Role::Leader);
        if n.is_some() {
            break;
        }
    }
    let n = n.expect("a new leader");
    let w = c.mutate(n, KEY, 2, MAJORITY);
    c.settle();
    assert!(matches!(c.reply(n, w), Some(Ok(Reply::Mutated { .. }))));
    // The old leader, still thinking it leads, gets a compare against the new value: its copy
    // holds 1, but it must not say so.
    let stale = c.mutate_with(l, &[KEY], Box::new(Expect(2)), None);
    c.ticks(20);
    let r = c.reply(l, stale);
    assert!(
        !matches!(r, Some(Err(RangeError::Mutation(_)))),
        "a deposed leader answered from its stale copy: {r:?}"
    );
    // Through the new leader the same compare goes.
    let ok = c.mutate_with(n, &[KEY], Box::new(Expect(2)), None);
    c.settle();
    assert!(matches!(c.reply(n, ok), Some(Ok(Reply::Mutated { .. }))));
}
