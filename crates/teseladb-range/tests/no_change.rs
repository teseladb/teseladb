//! A mutation that changes nothing (005 FR-026): the leader answers it without proposing an entry,
//! as RefDb 9.0.4 answers a write without effect without writing to its oplog, once it confirms
//! it still leads; a retry whose statement has a stored result gets that result, and a write
//! concern the shortcut does not cover goes through Raft as before.

mod common;

use common::*;

/// Changes nothing and answers 9.
struct Nothing;

impl Mutation for Nothing {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        Ok(Outcome { puts: vec![], deletes: vec![], change: vec![], result: vec![9] })
    }
}

/// Writes byte 1 and answers 0.
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

const K: DedupKey = DedupKey { lsid: 0x5eed, txn_number: 3, stmt_id: 0 };

fn last_index(c: &Cluster, n: NodeId) -> u64 {
    c.nodes[&n].core.status(G).map_or(0, |st| st.last_index)
}

fn result(r: Option<Result<Reply, RangeError>>) -> Vec<u8> {
    match r {
        Some(Ok(Reply::Mutated { result, .. })) => result,
        other => panic!("not a write: {other:?}"),
    }
}

#[test]
fn a_mutation_without_change_is_answered_without_an_entry() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    let before = last_index(&c, l);

    let id = c.mutate_with(l, &[KEY], Box::new(Nothing), None);
    c.ticks(2);

    assert_eq!(result(c.reply(l, id)), vec![9]);
    assert_eq!(last_index(&c, l), before, "no entry proposed");
}

#[test]
fn a_retry_with_a_stored_result_gets_it_through_the_usual_path() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let first = c.mutate_with(l, &[KEY], Box::new(Write), Some(K));
    c.ticks(2);
    assert_eq!(result(c.reply(l, first)), vec![0]);

    // The same statement again, now finding nothing to change: the stored result answers.
    let again = c.mutate_with(l, &[KEY], Box::new(Nothing), Some(K));
    c.ticks(2);

    assert_eq!(result(c.reply(l, again)), vec![0], "the original result");
}

#[test]
fn a_retryable_statement_without_a_stored_result_takes_the_shortcut() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    let before = last_index(&c, l);

    let id = c.mutate_with(l, &[KEY], Box::new(Nothing), Some(K));
    c.ticks(2);

    assert_eq!(result(c.reply(l, id)), vec![9]);
    assert_eq!(last_index(&c, l), before, "no entry and no record, as RefDb");
}

#[test]
fn a_write_concern_beyond_the_leader_still_goes_through_raft() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.settle();
    let before = last_index(&c, l);
    let all = WriteConcern { w: W::Nodes(3), j: true };

    let id = c.mutate_with_wc(l, &[KEY], Box::new(Nothing), None, all);
    c.ticks(4);

    assert_eq!(result(c.reply(l, id)), vec![9]);
    assert_eq!(last_index(&c, l), before + 1, "an entry for its write concern");
}
