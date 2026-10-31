//! 013 T025: the `$clusterTime` signing keys in Group 0 (FR-014): a key is added only on the
//! state its proposal saw, so two leaders proposing on the same state add one key; an expired
//! key is dropped; the keys and the errors travel in the snapshot.

use teseladb_meta::clustertime_keys::SealedKey;
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{Group0Error, Group0Result, Group0Sm};
use teseladb_meta::request::RequestId;
use teseladb_multiraft::StateMachine;
use teseladb_types::NodeId;

fn cluster() -> Group0Sm {
    let mut sm = Group0Sm::default();
    let boot = Group0Command::Bootstrap {
        cluster_id: [7; 16],
        first_node: NodeAddrs { name: b"n1".to_vec(), cores: 2, ..NodeAddrs::default() },
        cert_fingerprint: [0; 32],
    };
    sm.apply_command(1, &boot);
    sm
}

fn apply(sm: &mut Group0Sm, index: u64, cmd: Group0Command) -> Group0Result {
    assert_eq!(Group0Command::decode(&cmd.encode()), Ok(cmd.clone()));
    sm.apply_command(index, &cmd)
}

fn add(id: i64, expires_at: u32, after: u32) -> Group0Command {
    let key = SealedKey { id, expires_at, sealed: vec![1; 48] };
    Group0Command::AddClusterTimeKey { key, after }
}

#[test]
fn keys_are_added_on_the_state_they_saw_and_travel_in_the_snapshot() {
    let mut sm = cluster();
    let raced = Group0Result::Err(Group0Error::ClusterTimeKeyRaced);
    assert_eq!(apply(&mut sm, 2, add(10, 100, 0)), Group0Result::Ok);
    // Another leader's proposal on the same state.
    assert_eq!(apply(&mut sm, 3, add(11, 100, 0)), raced);
    assert_eq!(apply(&mut sm, 4, add(11, 200, 100)), Group0Result::Ok);
    assert_eq!(sm.clustertime_keys.keys.len(), 2);
    assert_eq!(sm.clustertime_keys.last_expiry(), 200);
    assert_eq!(apply(&mut sm, 5, Group0Command::ExpireClusterTimeKey { id: 10 }), Group0Result::Ok);
    assert_eq!(apply(&mut sm, 6, Group0Command::ExpireClusterTimeKey { id: 10 }), raced);
    assert_eq!(sm.clustertime_keys.version, 3);

    let req = RequestId { origin: NodeId(1), id: 4 };
    let again = Group0Command::Idempotent { req, cmd: Box::new(add(12, 100, 0)) };
    assert_eq!(apply(&mut sm, 7, again), raced);
    let mut copy = Group0Sm::default();
    copy.restore(&sm.snapshot()).unwrap();
    assert_eq!(copy, sm);
    assert_eq!(copy.requests.get(req), Some(&raced));
    // A truncated command does not decode.
    let bytes = add(13, 300, 200).encode();
    assert!(Group0Command::decode(&bytes[..bytes.len() - 1]).is_err());
}
