//! The range client (005 T015, FR-017): it sends to the leader it knows, follows the hint of a
//! `NotLeader` or tries the next node, retries a write only when it is retryable (an answer after
//! the proposal does not say whether the entry was committed), and gives up after its attempts.

use teseladb_hlc::Hlc;
use teseladb_range::client::{RangeClient, Route};
use teseladb_range::host::{MutationError, RangeError, Reply};
use teseladb_types::{GroupId, NodeId};

const G: GroupId = GroupId(7);

fn n(i: u64) -> NodeId {
    NodeId(i)
}

fn ok() -> Result<Reply, RangeError> {
    Ok(Reply::Value { ts: Hlc { physical: 1, logical: 0 }, value: None })
}

#[test]
fn it_follows_the_hint_and_remembers_the_leader() {
    let mut c = RangeClient::new(vec![n(1), n(2), n(3)], 5);
    assert_eq!(c.target(G), n(1), "nobody known: the first node");
    let hint = Err(RangeError::NotLeader { hint: n(3) });
    assert_eq!(c.on_reply(G, n(1), 1, hint), Route::Send(n(3)));
    assert_eq!(c.target(G), n(3));
    let done = Ok(Reply::Mutated {
        ts: Hlc { physical: 5, logical: 0 },
        change: vec![],
        result: vec![],
        entry: None,
    });
    assert_eq!(c.on_reply(G, n(3), 2, done.clone()), Route::Done(done));
    assert_eq!(c.target(G), n(3), "the node that answered leads");
    assert_eq!(c.target(GroupId(8)), n(1), "each range on its own");
}

#[test]
fn without_a_hint_it_tries_the_next_node() {
    let mut c = RangeClient::new(vec![n(1), n(2), n(3)], 5);
    let none = Err(RangeError::NotLeader { hint: n(0) });
    assert_eq!(c.on_reply(G, n(3), 1, none.clone()), Route::Send(n(1)), "it wraps around");
    assert_eq!(c.on_reply(G, n(1), 2, none.clone()), Route::Send(n(2)));
    // A node outside the list starts over at the first one.
    assert_eq!(c.on_reply(G, n(9), 3, none), Route::Send(n(1)));
    // Overloaded or slow: another node may lead by now.
    assert_eq!(c.on_reply(G, n(1), 4, Err(RangeError::Overloaded)), Route::Send(n(2)));
    assert_eq!(c.on_reply(G, n(2), 1, Err(RangeError::Timeout)), Route::Send(n(3)));
    assert_eq!(c.on_reply(G, n(1), 1, ok()), Route::Done(ok()));
}

#[test]
fn it_gives_up_after_its_attempts_and_on_final_errors() {
    let mut c = RangeClient::new(vec![n(1), n(2)], 3);
    let not = Err(RangeError::NotLeader { hint: n(2) });
    assert_eq!(c.on_reply(G, n(1), 3, not.clone()), Route::Done(not));
    for e in [
        RangeError::OutOfRange,
        RangeError::Unsatisfiable,
        RangeError::TooLarge,
        RangeError::Unavailable,
        RangeError::Mutation(MutationError("duplicate key".into())),
    ] {
        assert_eq!(c.on_reply(G, n(1), 1, Err(e.clone())), Route::Done(Err(e)));
    }
}

#[test]
fn a_write_without_its_idempotence_record_is_not_retried() {
    let mut c = RangeClient::new(vec![n(1), n(2), n(3)], 5);
    let not = Err(RangeError::NotLeader { hint: n(2) });
    // A write: the entry may have been committed by the new leader.
    let write = c.on_reply_write(G, n(1), 1, false, not.clone());
    assert_eq!(write, Route::Done(not.clone()));
    // A retryable write (with its record) and a read go on.
    assert_eq!(c.on_reply_write(G, n(1), 1, true, not.clone()), Route::Send(n(2)));
    assert_eq!(c.on_reply(G, n(2), 1, not), Route::Send(n(2)));
}

#[test]
fn group_0_gives_the_replicas_and_the_leader_hint_of_a_range() {
    // FR-017: the client starts from the range's descriptor in Group 0.
    let mut c = RangeClient::new(vec![n(1), n(2), n(3), n(4)], 5);
    c.learn_range(G, vec![n(2), n(4)], None);
    assert_eq!(c.target(G), n(2), "no hint: the first replica");
    let busy = Err(RangeError::Overloaded);
    assert_eq!(c.on_reply(G, n(2), 1, busy.clone()), Route::Send(n(4)));
    assert_eq!(c.on_reply(G, n(4), 2, busy), Route::Send(n(2)), "only the range's replicas");
    c.learn_range(G, vec![n(2), n(4)], Some(n(4)));
    assert_eq!(c.target(G), n(4), "Group 0's hint replaces what the client guessed");
    assert_eq!(c.target(GroupId(8)), n(1), "a range Group 0 did not describe: the first node");
}
