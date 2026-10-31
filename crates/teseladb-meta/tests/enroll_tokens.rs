//! 013 T016: node enrollment tokens in Group 0 (FR-005): created, used once, refused when used,
//! expired (by Group 0's cluster time, the same on every replica) or with another secret, and
//! carried in the snapshot.

use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::enroll_tokens::EnrollToken;
use teseladb_meta::group0::{Group0Error, Group0Result, Group0Sm};
use teseladb_multiraft::StateMachine;

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

fn consume(id: u8, secret: u8) -> Group0Command {
    Group0Command::ConsumeEnrollToken { id: [id; 16], secret_hash: [secret; 32] }
}

#[test]
fn a_token_is_used_once_before_it_expires() {
    let mut sm = cluster();
    sm.cluster_time_ms = 1_000;
    let token = EnrollToken { id: [1; 16], secret_hash: [2; 32], expires_ms: 5_000, used: false };
    assert_eq!(
        apply(&mut sm, 2, Group0Command::CreateEnrollToken(token.clone())),
        Group0Result::Ok
    );
    let taken = Group0Result::Err(Group0Error::EnrollTokenUnknown);
    assert_eq!(apply(&mut sm, 3, Group0Command::CreateEnrollToken(token)), taken);
    assert_eq!(
        apply(&mut sm, 4, consume(1, 9)),
        Group0Result::Err(Group0Error::EnrollTokenUnknown)
    );
    assert_eq!(apply(&mut sm, 5, consume(1, 2)), Group0Result::Ok);
    assert_eq!(apply(&mut sm, 6, consume(1, 2)), Group0Result::Err(Group0Error::EnrollTokenUsed));
    let late = EnrollToken { id: [3; 16], secret_hash: [3; 32], expires_ms: 2_000, used: false };
    apply(&mut sm, 7, Group0Command::CreateEnrollToken(late));
    sm.cluster_time_ms = 2_000;
    assert_eq!(
        apply(&mut sm, 8, consume(3, 3)),
        Group0Result::Err(Group0Error::EnrollTokenExpired)
    );
    let mut copy = Group0Sm::default();
    copy.restore(&sm.snapshot()).unwrap();
    assert_eq!(copy, sm);
    assert!(Group0Command::decode(&consume(1, 2).encode()[..20]).is_err());
}
