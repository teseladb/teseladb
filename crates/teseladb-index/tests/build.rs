//! Online index builds (009 T012, FR-006): the replicas a `commitQuorum` asks of a range and the
//! progress `currentOp` shows, as a reference replica set member 9.0.4 shows it.

use teseladb_index::build::{CommitQuorum, MAX_COMMIT_QUORUM, Progress, Unsatisfiable};

#[test]
fn voting_members_is_every_voter() {
    for voters in 1..=7 {
        assert_eq!(CommitQuorum::VotingMembers.replicas(voters), Ok(voters));
    }
}

#[test]
fn majority_is_the_range_majority() {
    let cases = [(1, 1), (2, 2), (3, 2), (4, 3), (5, 3), (7, 4)];
    for (voters, want) in cases {
        assert_eq!(CommitQuorum::Majority.replicas(voters), Ok(want), "{voters} voters");
    }
}

#[test]
fn a_number_never_waits_for_less_than_the_majority() {
    assert_eq!(CommitQuorum::Members(0).replicas(3), Ok(2));
    assert_eq!(CommitQuorum::Members(1).replicas(3), Ok(2));
    assert_eq!(CommitQuorum::Members(2).replicas(3), Ok(2));
    assert_eq!(CommitQuorum::Members(3).replicas(3), Ok(3));
    assert_eq!(CommitQuorum::Members(1).replicas(1), Ok(1));
    assert_eq!(CommitQuorum::Members(0).replicas(1), Ok(1));
}

#[test]
fn a_number_above_the_voters_is_unsatisfiable() {
    assert_eq!(CommitQuorum::Members(4).replicas(3), Err(Unsatisfiable));
    assert_eq!(CommitQuorum::Members(2).replicas(1), Err(Unsatisfiable));
    assert_eq!(CommitQuorum::Members(MAX_COMMIT_QUORUM).replicas(5), Err(Unsatisfiable));
}

#[test]
fn progress_reads_as_on_the_reference() {
    // Measured on 9.0.4 halfway through 1000 documents.
    let p = Progress { done: 500, total: 1000 };
    assert_eq!(p.percent(), 50);
    assert_eq!(
        p.message(),
        "Index Build: scanning collection Index Build: scanning collection: 500/1000 50%"
    );
}

#[test]
fn progress_rounds_down_and_survives_an_empty_collection() {
    assert_eq!(Progress { done: 2, total: 3 }.percent(), 66);
    assert_eq!(Progress { done: 0, total: 0 }.percent(), 0);
    assert_eq!(Progress { done: 4, total: 3 }.percent(), 133);
    assert_eq!(
        Progress { done: 0, total: 0 }.message(),
        "Index Build: scanning collection Index Build: scanning collection: 0/0 0%"
    );
}
