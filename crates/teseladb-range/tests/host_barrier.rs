//! The statement barrier of the range (005 FR-036, D-163, model `StatementBarrier.tla`): a write
//! answers with the term and the index of its entry; the barrier of a command answers once the
//! leader holds each of its entries with its term and they reach the command's write concern
//! (FR-010), waits while they do not, and fails when the leader lacks one of them with its term,
//! also when another entry took its index.

mod common;

use common::*;
use teseladb_range::ack::EntryId;
use teseladb_range::host::BarrierReq;

const W1: WriteConcern = WriteConcern { w: W::Nodes(1), j: false };
const W1_J: WriteConcern = WriteConcern { w: W::Nodes(1), j: true };

impl Cluster {
    fn barrier(&mut self, n: NodeId, entries: &[EntryId], wc: WriteConcern) -> ReqId {
        let now = self.now;
        let node = self.nodes.get_mut(&n).unwrap();
        let req = BarrierReq { g: G, entries: entries.to_vec(), wc };
        node.host.barrier(&mut node.core, req, now)
    }

    fn term_of(&self, n: NodeId) -> u64 {
        self.nodes[&n].core.status(G).unwrap().hard_state.term
    }
}

/// The entry a write answered with.
fn entry(r: Option<Result<Reply, RangeError>>) -> EntryId {
    match r {
        Some(Ok(Reply::Mutated { entry: Some(e), .. })) => e,
        other => panic!("not a write with its entry: {other:?}"),
    }
}

#[test]
fn a_write_answers_with_the_term_and_the_index_of_its_entry() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let a = c.mutate(l, KEY, 1, W1);
    let b = c.mutate(l, KEY, 2, MAJORITY);
    c.settle();
    let (ea, eb) = (entry(c.reply(l, a)), entry(c.reply(l, b)));
    let term = c.term_of(l);
    assert_eq!((ea.term, eb.term), (term, term));
    assert_eq!(eb.index, ea.index + 1, "entries in the order of the writes");
    let last = c.nodes[&l].core.status(G).unwrap().last_index;
    assert_eq!(eb.index, last);
}

#[test]
fn a_barrier_waits_until_its_entries_reach_the_write_concern() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let followers = c.others(l);
    c.slow.extend(followers);
    // Answered once in the leader's log, before any follower has them.
    let a = c.mutate(l, KEY, 1, W1);
    let b = c.mutate(l, KEY, 2, W1);
    c.settle();
    let entries = [entry(c.reply(l, a)), entry(c.reply(l, b))];
    let majority = c.barrier(l, &entries, MAJORITY);
    let two = c.barrier(l, &entries, WriteConcern { w: W::Nodes(2), j: false });
    let one = c.barrier(l, &entries, W1);
    c.settle();
    assert_eq!(c.reply(l, one), Some(Ok(Reply::Barrier)), "w: 1 holds already");
    assert_eq!(c.reply(l, majority), None, "not committed");
    assert_eq!(c.reply(l, two), None, "on one disk only");
    c.ticks(20);
    assert_eq!(c.reply(l, majority), None, "the followers still do not answer");
    c.slow.clear();
    c.ticks(20);
    assert_eq!(c.reply(l, majority), Some(Ok(Reply::Barrier)));
    assert_eq!(c.reply(l, two), Some(Ok(Reply::Barrier)));
    // What was answered is committed and applied: a majority read sees the last statement.
    let r = c.get(l, KEY, ReadConcern::Majority);
    c.settle();
    assert_eq!(value(c.reply(l, r)), Some(2));
}

#[test]
fn a_barrier_with_j_waits_for_the_leader_s_disk() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.slow.insert(l);
    let a = c.mutate(l, KEY, 1, W1);
    c.settle();
    let e = entry(c.reply(l, a));
    let journaled = c.barrier(l, &[e], W1_J);
    c.settle();
    assert_eq!(c.reply(l, journaled), None, "the entry is not on the leader's disk");
    c.slow.clear();
    c.ticks(5);
    assert_eq!(c.reply(l, journaled), Some(Ok(Reply::Barrier)));
}

#[test]
fn a_barrier_fails_when_the_leader_lacks_an_entry_with_its_term() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let kept = c.mutate(l, KEY, 1, MAJORITY);
    c.settle();
    let kept = entry(c.reply(l, kept));
    // Cut off, the leader takes a statement into its log alone: it answers at `w: 1`.
    c.isolate(l);
    let lost = c.mutate(l, KEY, 2, W1);
    c.settle();
    let lost = entry(c.reply(l, lost));
    // The others elect a leader of a new term, whose own entries take that index.
    let mut l2 = None;
    for _ in 0..1_000 {
        c.ticks(1);
        l2 = c
            .others(l)
            .into_iter()
            .find(|n| c.nodes[n].core.status(G).unwrap().role == Role::Leader);
        if l2.is_some() {
            break;
        }
    }
    let l2 = l2.expect("a new leader");
    let next = c.mutate(l2, KEY, 3, W1);
    c.cut.clear();
    c.ticks(50);
    let next = entry(c.reply(l2, next));
    assert!(next.term > lost.term);
    let status = c.nodes[&l2].core.status(G).unwrap();
    assert!(status.last_index >= lost.index, "another entry holds that index");
    // The statement of the old term is in doubt: the command fails, as after a leader change.
    let doubt = c.barrier(l2, &[kept, lost, next], MAJORITY);
    c.settle();
    assert_eq!(c.reply(l2, doubt), Some(Err(RangeError::EntryLost)));
    // The entry of the earlier term it holds, with its term, and its own are confirmed.
    let ok = c.barrier(l2, &[kept, next], MAJORITY);
    c.settle();
    assert_eq!(c.reply(l2, ok), Some(Ok(Reply::Barrier)));
    // An entry of a later term than the leader's: another replica leads that term.
    let later = EntryId { term: next.term + 1, index: next.index };
    let ahead = c.barrier(l2, &[later], MAJORITY);
    c.settle();
    assert!(matches!(c.reply(l2, ahead), Some(Err(RangeError::NotLeader { .. }))));
    // The old leader stepped down: it sends the command to the leader.
    let follower = c.barrier(l, &[kept], MAJORITY);
    c.settle();
    assert_eq!(c.reply(l, follower), Some(Err(RangeError::NotLeader { hint: l2 })));
}

#[test]
fn a_barrier_of_a_leader_that_loses_its_term_fails_and_one_without_entries_answers() {
    let mut c = Cluster::standard();
    let l = c.elect();
    let followers = c.others(l);
    c.slow.extend(followers);
    let a = c.mutate(l, KEY, 1, W1);
    c.settle();
    let e = entry(c.reply(l, a));
    let waiting = c.barrier(l, &[e], MAJORITY);
    c.settle();
    assert_eq!(c.reply(l, waiting), None);
    // The others elect a leader while this one waits: its barrier fails.
    c.slow.clear();
    c.isolate(l);
    for _ in 0..1_000 {
        c.ticks(1);
        if c.others(l).iter().any(|n| c.nodes[n].core.status(G).unwrap().role == Role::Leader) {
            break;
        }
    }
    c.cut.clear();
    c.ticks(50);
    assert!(matches!(c.reply(l, waiting), Some(Err(RangeError::NotLeader { .. }))));
    // A barrier with nothing to confirm answers at once on the leader; `w: 0` and a `w` above
    // the replicas follow FR-010.
    let l2 = c.leader().unwrap();
    let empty = c.barrier(l2, &[], MAJORITY);
    let too_many = c.barrier(l2, &[], WriteConcern { w: W::Nodes(4), j: false });
    let none =
        c.barrier(l2, &[EntryId { term: 1, index: 1 }], WriteConcern { w: W::Zero, j: false });
    c.settle();
    assert_eq!(c.reply(l2, empty), Some(Ok(Reply::Barrier)));
    assert_eq!(c.reply(l2, too_many), Some(Err(RangeError::Unsatisfiable)));
    assert_eq!(c.reply(l2, none), Some(Ok(Reply::Barrier)));
}
