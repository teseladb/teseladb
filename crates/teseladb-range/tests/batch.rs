//! Large writes on three replicas (005 FR-023): a 16 MiB document goes in one entry and reaches
//! every replica; a batch of 100,000 writes goes in one entry per document, each with its own
//! mark, keeping the ordered semantics (it stops at the first failure and leaves a prefix
//! written, also when the leader goes) or the unordered one (it writes everything else); a full
//! proposal queue holds the batch back instead of failing it.

mod common;

use common::*;
use teseladb_hlc::succ;
use teseladb_lsm::Lookup;
use teseladb_range::batch::{BATCH_WINDOW, Batch, BatchWrite, OVERLOADED_PATIENCE, Order};

const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };
const DOCS: usize = 100_000;

/// Key of document `i`, inside the range.
fn doc(i: usize) -> Vec<u8> {
    [&b"\x01d"[..], &(i as u64).to_be_bytes()].concat()
}

/// Inserts its document, or fails as a duplicate key.
struct Insert {
    key: Vec<u8>,
    value: Vec<u8>,
    fail: bool,
}

impl Mutation for Insert {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        if self.fail {
            return Err(MutationError("duplicate key".into()));
        }
        Ok(Outcome {
            puts: vec![Put { key: self.key.clone(), value_type: 1, value: self.value.clone() }],
            deletes: vec![],
            change: vec![],
            result: vec![],
        })
    }
}

fn write(i: usize, fail: bool) -> BatchWrite {
    let key = doc(i);
    let value = (i as u32).to_le_bytes().to_vec();
    BatchWrite { keys: vec![key.clone()], m: Rc::new(Insert { key, value, fail }), dedup: None }
}

/// Runs the batch on node `n` until it is done, handing the other answers back to the node.
fn run(c: &mut Cluster, n: NodeId, b: &mut Batch) {
    for _ in 0..100_000 {
        let (now, wall) = (c.now, c.wall());
        let node = c.nodes.get_mut(&n).unwrap();
        b.pump(&mut node.host, &mut node.core, now, wall);
        c.ticks(1);
        let node = c.nodes.get_mut(&n).unwrap();
        let replies = std::mem::take(&mut node.replies);
        node.replies = b.take_replies(replies);
        if b.is_done() {
            return;
        }
    }
    panic!("the batch never ends");
}

/// The version of `key` in node `n`'s LSM.
fn stored(c: &Cluster, n: NodeId, key: &[u8]) -> Option<Vec<u8>> {
    let lsm = c.nodes[&n].host.lsm();
    let view = lsm.borrow().view();
    match view.get(&mut lsm.borrow_mut(), key, NEWEST) {
        Lookup::Hit(f) => Some(f.value[1..].to_vec()),
        Lookup::NotFound => None,
        other => panic!("{other:?}"),
    }
}

fn mark(r: &Option<Result<Reply, RangeError>>) -> Hlc {
    match r {
        Some(Ok(Reply::Mutated { ts, .. })) => *ts,
        other => panic!("not written: {other:?}"),
    }
}

/// Every replica applied the same log.
fn same_everywhere(c: &Cluster) {
    let applied: Vec<u64> = c.nodes.values().map(|n| n.core.status(G).unwrap().applied).collect();
    assert!(applied.windows(2).all(|w| w[0] == w[1]), "{applied:?}");
}

#[test]
fn a_16_mib_document_is_one_entry_on_every_replica() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let key = doc(7);
    let value: Vec<u8> = (0..16 << 20).map(|i| (i % 241) as u8).collect();
    let m = Box::new(Insert { key: key.clone(), value: value.clone(), fail: false });
    let before = c.nodes[&l].core.status(G).unwrap().last_index;
    let w = c.mutate_with(l, &[&key], m, None);
    c.ticks(20);
    assert!(matches!(c.reply(l, w), Some(Ok(Reply::Mutated { .. }))));
    assert_eq!(c.nodes[&l].core.status(G).unwrap().last_index, before + 1, "one entry");
    same_everywhere(&c);
    for n in 1..=3 {
        assert!(stored(&c, id(n), &key) == Some(value.clone()), "node {n}");
    }
    let r = c.get(l, &key, ReadConcern::Majority);
    c.settle();
    let Some(Ok(Reply::Value { value: Some((1, got)), .. })) = c.reply(l, r) else { panic!() };
    assert!(got == value);
}

#[test]
fn a_document_above_an_entry_is_refused_and_nothing_is_written() {
    // Until the internal transaction of 008 spreads it over several entries.
    let mut c = Cluster::standard();
    let l = c.elect();
    let key = doc(8);
    let m = Box::new(Insert { key: key.clone(), value: vec![5; 64 << 20], fail: false });
    let before = c.nodes[&l].core.status(G).unwrap().last_index;
    let w = c.mutate_with(l, &[&key], m, None);
    c.ticks(5);
    assert_eq!(c.reply(l, w), Some(Err(RangeError::TooLarge)), "never retried");
    assert_eq!(c.nodes[&l].core.status(G).unwrap().last_index, before);
    assert_eq!(stored(&c, l, &key), None);
}

#[test]
fn an_ordered_batch_of_100_000_writes_stops_at_its_first_failure() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let fails = 70_000;
    let writes = (0..DOCS).map(|i| write(i, i == fails || i == 90_000)).collect();
    let mut b = Batch::new(G, writes, Order::Ordered, MAJORITY);
    run(&mut c, l, &mut b);
    let results = b.results();
    assert_eq!(results.len(), DOCS);
    // One mark per document, each one at least `succ` of the one before: the log's order.
    let node = teseladb_hlc::node_of(mark(&results[0]));
    for i in 1..fails {
        assert!(mark(&results[i]) >= succ(mark(&results[i - 1]), node), "document {i}");
    }
    assert_eq!(
        results[fails],
        Some(Err(RangeError::Mutation(MutationError("duplicate key".into()))))
    );
    assert!(results[fails + 1..].iter().all(Option::is_none), "nothing after the failure runs");
    same_everywhere(&c);
    for n in 1..=3 {
        for i in (0..DOCS).step_by(997).chain([fails - 1, fails, fails + 1, DOCS - 1]) {
            let want = (i < fails).then(|| (i as u32).to_le_bytes().to_vec());
            assert_eq!(stored(&c, id(n), &doc(i)), want, "node {n}, document {i}");
        }
    }
}

#[test]
fn an_unordered_batch_of_100_000_writes_writes_all_but_its_failures() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let fails = [10, 70_000, DOCS - 1];
    let writes = (0..DOCS).map(|i| write(i, fails.contains(&i))).collect();
    let mut b = Batch::new(G, writes, Order::Unordered, MAJORITY);
    run(&mut c, l, &mut b);
    let results = b.results();
    let mut marks = std::collections::BTreeSet::new();
    for (i, r) in results.iter().enumerate() {
        if fails.contains(&i) {
            assert!(matches!(r, Some(Err(RangeError::Mutation(_)))), "document {i}");
        } else {
            assert!(marks.insert(mark(r)), "document {i} has its own mark");
        }
    }
    assert_eq!(marks.len(), DOCS - fails.len());
    same_everywhere(&c);
    let r = c.scan(l, &doc(0), &doc(DOCS), DOCS + 1);
    c.settle();
    let Some(Ok(Reply::Batch(rows))) = c.reply(l, r) else { panic!() };
    let keys: Vec<Vec<u8>> = rows.into_iter().map(|(k, _)| k).collect();
    let want: Vec<Vec<u8>> = (0..DOCS).filter(|i| !fails.contains(i)).map(doc).collect();
    assert!(keys == want);
}

#[test]
fn a_full_proposal_queue_holds_the_batch_back() {
    let lsm = LsmConfig { dir: "core-00/lsm".into(), ..LsmConfig::default() };
    let mut c = Cluster::new(lsm, 3);
    let l = c.elect();
    for order in [Order::Ordered, Order::Unordered] {
        let writes = (0..2 * BATCH_WINDOW).map(|i| write(i, false)).collect();
        let mut b = Batch::new(G, writes, order, MAJORITY);
        run(&mut c, l, &mut b);
        assert!(b.results().iter().all(|r| matches!(r, Some(Ok(_)))), "{order:?}");
    }
}

#[test]
fn an_ordered_batch_that_loses_its_leader_leaves_a_prefix_written() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let writes = (0..2_000).map(|i| write(i, false)).collect();
    let mut b = Batch::new(G, writes, Order::Ordered, MAJORITY);
    let (now, wall) = (c.now, c.wall());
    let node = c.nodes.get_mut(&l).unwrap();
    b.pump(&mut node.host, &mut node.core, now, wall);
    // The leader is cut off with its window proposed and not committed.
    c.isolate(l);
    run(&mut c, l, &mut b);
    let results = b.results();
    assert!(results.iter().any(|r| matches!(r, Some(Err(_)))));
    assert!(results.iter().any(Option::is_none), "it stopped");
    c.cut.clear();
    c.ticks(100);
    let leader = c.elect();
    c.ticks(20);
    let written: Vec<bool> = (0..2_000).map(|i| stored(&c, leader, &doc(i)).is_some()).collect();
    let prefix = written.iter().take_while(|w| **w).count();
    assert!(written[prefix..].iter().all(|w| !w), "the written documents are a prefix");
    for (i, r) in results.iter().enumerate() {
        if matches!(r, Some(Ok(_))) {
            assert!(written[i], "document {i} was answered as written");
        }
    }
}

#[test]
fn an_empty_batch_is_done_at_once_and_other_answers_pass_through() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let mut b = Batch::new(G, vec![], Order::Ordered, MAJORITY);
    assert!(b.is_done());
    let other = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    let replies = std::mem::take(&mut c.nodes.get_mut(&l).unwrap().replies);
    let back = b.take_replies(replies);
    assert!(back.iter().any(|(id, _)| *id == other));
    assert!(b.results().is_empty());
}

#[test]
fn an_unordered_batch_answers_a_document_above_an_entry_and_writes_the_rest() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let mut writes: Vec<BatchWrite> = (0..10).map(|i| write(i, false)).collect();
    let key = doc(10);
    let big = Insert { key: key.clone(), value: vec![1; 64 << 20], fail: false };
    writes.insert(5, BatchWrite { keys: vec![key.clone()], m: Rc::new(big), dedup: None });
    let mut b = Batch::new(G, writes, Order::Unordered, MAJORITY);
    run(&mut c, l, &mut b);
    let results = b.results();
    // Until 008 spreads it over several entries, the document comes back as an error.
    assert_eq!(results[5], Some(Err(RangeError::TooLarge)));
    assert!(results.iter().enumerate().all(|(i, r)| i == 5 || matches!(r, Some(Ok(_)))));
    assert_eq!(stored(&c, l, &key), None);
}

#[test]
fn an_ordered_batch_waits_while_its_core_does_not_hold_the_range() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let mut b = Batch::new(GroupId(99), vec![write(0, false)], Order::Ordered, MAJORITY);
    let (now, wall) = (c.now, c.wall());
    let node = c.nodes.get_mut(&l).unwrap();
    b.pump(&mut node.host, &mut node.core, now, wall);
    assert!(!b.is_done(), "nothing was handed");
}

#[test]
fn a_batch_overloaded_past_its_patience_gives_up_and_an_ordered_one_stops() {
    let mut c = Cluster::standard();
    let l = c.elect();
    // The host numbers requests one after another: the batch's come after this one.
    let before = c.mutate(l, KEY, 1, MAJORITY);
    let mut b = Batch::new(G, (0..3).map(|i| write(i, false)).collect(), Order::Ordered, MAJORITY);
    let overloaded = |from: ReqId, n: u64| -> Vec<(ReqId, Result<Reply, RangeError>)> {
        (from..from + n).map(|id| (id, Err(RangeError::Overloaded))).collect()
    };
    let (now, wall) = (c.now, c.wall());
    let node = c.nodes.get_mut(&l).unwrap();
    b.pump(&mut node.host, &mut node.core, now, wall);
    // Overloaded at once: the documents go again.
    assert!(b.take_replies(overloaded(before + 1, 3)).is_empty());
    assert!(!b.is_done());
    // Still overloaded once the patience is over: the answer is the error and the ordered batch
    // stops (the first attempts still hold their latches, so it hands one document at a time);
    // a later pump hands nothing.
    let later = now + OVERLOADED_PATIENCE + TICK;
    b.pump(&mut node.host, &mut node.core, later, wall);
    let others = b.take_replies(overloaded(before + 4, 3));
    assert!(b.is_done());
    b.pump(&mut node.host, &mut node.core, later, wall);
    assert!(b.is_done(), "a stopped batch hands nothing more");
    let results = b.results();
    assert_eq!(
        results.iter().filter(|r| **r == Some(Err(RangeError::Overloaded))).count(),
        3 - others.len()
    );
    assert!(results.iter().all(|r| r.is_none() || *r == Some(Err(RangeError::Overloaded))));
}
