//! Retryable writes (005 FR-011) without the reads of the LSM: once a session wrote through the
//! leader, a statement of a greater `txnNumber` neither looks for its record nor searches for a
//! newer one, while a retry and an older statement still get their original result or
//! `TransactionTooOld`. Alone in its binary: it counts the searches of the whole process.

mod common;

use common::*;
use teseladb_range::dedup::searches;

struct Inc;

impl Mutation for Inc {
    fn eval(&self, current: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let found = current[0].map_or(0, |(_, v)| v[0]);
        Ok(Outcome {
            puts: vec![Put { key: KEY.to_vec(), value_type: 1, value: vec![found.wrapping_add(1)] }],
            deletes: vec![],
            change: vec![b'+'],
            result: vec![found],
        })
    }
}

fn write(c: &mut Cluster, n: NodeId, txn_number: i64) -> Option<Result<Reply, RangeError>> {
    let key = DedupKey { lsid: 0x5e55, txn_number, stmt_id: 0 };
    let w = c.mutate_with(n, &[KEY], Box::new(Inc), Some(key));
    c.ticks(2);
    c.reply(n, w)
}

fn result(r: Option<Result<Reply, RangeError>>) -> Vec<u8> {
    match r {
        Some(Ok(Reply::Mutated { result, .. })) => result,
        other => panic!("not a write: {other:?}"),
    }
}

#[test]
fn a_session_s_next_txn_numbers_skip_the_searches_and_its_retries_still_find_their_records() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let before = searches();
    for txn in 1..=20 {
        assert_eq!(result(write(&mut c, l, txn)), vec![(txn - 1) as u8]);
    }
    let made = searches() - before;
    assert!(made <= 1, "{made} searches for 20 statements of one session");
    // A retry of the newest statement gets its original result and writes nothing.
    assert_eq!(result(write(&mut c, l, 20)), vec![19]);
    // An older statement without its record is too old.
    match write(&mut c, l, 3) {
        Some(Ok(Reply::Mutated { result, .. })) => assert_eq!(result, vec![2], "its record"),
        Some(Err(RangeError::Mutation(e))) => assert!(e.0.starts_with("225"), "{}", e.0),
        other => panic!("{other:?}"),
    }
    let r = c.get(l, KEY, ReadConcern::Majority);
    c.settle();
    assert_eq!(value(c.reply(l, r)), Some(20), "each statement applied once");
}
