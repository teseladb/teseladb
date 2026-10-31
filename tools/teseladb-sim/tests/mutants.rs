//! Every faulty variant in the code is detected by its checker within the seeds of its row of
//! the variant table (`scripts/sim-mutants.sh`, which builds each variant in its own
//! `$CARGO_TARGET_DIR/mutants/<variant>`).

use std::process::Command;

#[test]
fn every_bug_variant_is_detected() {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let out =
        Command::new("bash").arg(format!("{root}/scripts/sim-mutants.sh")).output().expect("bash");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    for v in [
        "ack_before_fsync",
        "wal_release_hardstate",
        "fsync_retry_success",
        "vote_twice",
        "vote_after_restart",
        "vote_during_lease",
        "read_before_term_commit",
        "lease_without_check_quorum",
        "apply_uncommitted",
        "memory_raft_ack",
        "merge_without_freeze",
        "split_loses_intents",
        "follower_read_above_closed",
    ] {
        assert!(text.contains(&format!("detected  {v}")), "{text}");
    }
}
