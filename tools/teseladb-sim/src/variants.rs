//! Faulty variants (002 FR-011): the names the code declares with `cfg(teseladb_bug = "...")`, and
//! the one this binary was built with, with the checker that must catch each one, its plan and its
//! seeds (002 T013). `scripts/sim-mutants.sh` goes through the list and builds one binary per
//! variant.

use crate::faults::FaultPlan;

/// A faulty variant and how the simulator must catch it (data model §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Variant {
    /// Value of `cfg(teseladb_bug = "...")`.
    pub name: &'static str,
    /// Checker that must report it.
    pub checker: &'static str,
    /// Fault plan it runs with.
    pub plan: FaultPlan,
    /// Seeds within which it must be caught.
    pub seeds: u64,
}

/// Declares the variants once: [`VARIANTS`], [`NAMES`] and [`built`] come from the same list.
macro_rules! variants {
    ($(($name:literal, $checker:literal, $plan:ident, $seeds:literal)),* $(,)?) => {
        /// Every variant of the code, in alphabetical order.
        pub const VARIANTS: &[Variant] = &[$(Variant {
            name: $name,
            checker: $checker,
            plan: FaultPlan::$plan,
            seeds: $seeds,
        }),*];

        /// Every variant's name, in alphabetical order.
        pub const NAMES: &[&str] = &[$($name),*];

        /// The variant this binary was built with (`None` for the correct code).
        pub const fn built() -> Option<&'static str> {
            $(if cfg!(teseladb_bug = $name) {
                return Some($name);
            })*
            None
        }
    };
}

variants!(
    ("ack_before_fsync", "durability", Default, 1000),
    ("apply_uncommitted", "raft_safety", Default, 1000),
    // A capped insert that evicts only past `max`: the `special-collections` workload's next read
    // finds the collection over `size` with several documents (015 T010).
    ("capped_exceeds_size", "capped", Default, 1000),
    // A commit below a push (008 T024): seed 6 of `txn-bank` reads every account at a mark the
    // transfer committed below, and the total is off; `intents` in `splits` finds it later.
    ("commit_below_push", "bank-total", Default, 1000),
    // A recovery that looks for another transaction's record (008): it finds none, answers that
    // the transaction never began and leaves that record aborted; the transaction itself goes on
    // and commits, and `txn_recovery` finds the recovery that answered «not committed».
    ("decision_not_durable", "txn_recovery", Default, 1000),
    // A cursor whose resolved mark ignores the pending intents (014 T020): a transaction commits
    // at its intents' mark below what the cursor read past, and `changestream` never emits it.
    ("emit_above_resolved", "changestream_delivery", Default, 1000),
    ("follower_local_read", "linearizable", Default, 1000),
    ("follower_read_above_closed", "mvcc", Default, 1000),
    ("fsync_retry_success", "recovery", Default, 1000),
    // A restarted node whose clock is behind writes below marks the range already gave out: the
    // first thing that breaks is the order of a document's versions against the reads answered
    // before (`mvcc`); a lost version (`durability`) needs two writes to fall on one mark, which
    // the node number in the logical counter makes rarer (D-003).
    ("hlc_regress_restart", "mvcc", Heavy, 1000),
    // A search index that keeps a document's old version when a newer one comes (011 T018): the
    // `search` workload finds a search whose documents are not those of the collection.
    ("indexer_skips_version", "search_results", Default, 1000),
    ("lease_without_check_quorum", "raft_safety", Default, 1000),
    // Search segments that score with the statistics of their own range only (011 T018): once the
    // ranges split, the `search` workload's scores differ from one segment's over every document.
    ("local_only_stats", "search_results", Default, 1000),
    // A range a split shrank answers the part of a stream's interval it still holds (014 T020):
    // the cursor takes the rest as read and `changestream` misses the changes there.
    ("lose_events_on_split", "changestream_delivery", Default, 1000),
    ("memory_raft_ack", "raft_safety", Default, 1000),
    // A range frozen for a merge goes on writing (007 T017): a write the right range acknowledged
    // after its state went to the left one is missing on the replicas that had not applied it.
    ("merge_without_freeze", "durability", Default, 1000),
    // Group 0 goes on taking orders on the descriptors it handed over to the Meta2 ranges (007
    // T028): a change it acknowledged afterwards is missing from the Meta2 range that holds the
    // descriptor, and the next lookup there sees an older term.
    ("meta2_stale_route", "meta2", Default, 1000),
    // Writes that do not wait for each other's latches: two read-modify-writes of one document read
    // the same version and both succeed. Each answer names the value its write found (a fresh
    // evaluation gives it as its result), so the history holds two writes over the same value:
    // `durability`, which `range-docs` runs before `linearizable`, finds the first one acknowledged,
    // overwritten by no write and missing from the chain of the last read (seed 1: write 2/157 of
    // `c2-157`); `linearizable` also rejects that history.
    // A write across ranges that does not remove the entries of the version it replaces: the
    // `indexes` workload's next read of everything finds an entry no document needs.
    ("multirange_write_without_index_delta", "index_consistency", Default, 1000),
    ("no_latch_inc", "durability", Default, 1000),
    ("no_timer_after_lost_vote", "liveness", Heavy, 1000),
    // A coordinator that combines one range's partial group twice (010 T026): the `queries`
    // workload's next group differs from the reference evaluator's.
    ("partial_double_count", "queries-oracle", Default, 1000),
    ("plain_write_fails_on_intent", "plain_writes", Default, 1000),
    // A batch of ReadIndex reads answered as soon as its round leaves (005 FR-009): a leader cut
    // off from its quorum serves a read at an index below a commit the new leader already made.
    ("read_batch_without_round", "raft_safety", Default, 1000),
    ("read_before_term_commit", "raft_safety", Default, 1000),
    ("read_skips_inflight_write", "mvcc", Default, 1000),
    // A reader that ignores an intent (008 T024): in seed 1 the index maintenance of `indexes`
    // reads past an intent and leaves an entry missing, before `intents` in `splits` sees it.
    ("reader_ignores_intent", "index_consistency", Default, 1000),
    // A reader that ignores what its push found (008 T029): it reads below the intent of a
    // transaction that committed at or below its mark. Seed 1 fails in two workloads: `indexes`,
    // whose index maintenance reads the old document and leaves entries no document needs, and
    // `txn-bank`, whose read of every account misses one side of a transfer. `scripts/sim-mutants.sh`
    // takes the first workload by name within the lowest seed, so `indexes` decides.
    ("reader_ignores_push_result", "index_consistency", Default, 1000),
    // A recovery that answers «aborted» for a record that does not exist and leaves none (008):
    // the transaction's coordinator, whose first write comes after it, begins and commits it.
    // A reaper that removes a client transaction's record as soon as it is final (008 record GC):
    // `txn_recovery` finds a removal with no later transaction of its session and its session's
    // time not over.
    ("record_forgotten_early", "txn_recovery", Default, 1000),
    ("record_not_fenced", "txn_recovery", Default, 1000),
    ("release_log_before_flush", "lsm", Default, 1000),
    // Intents resolved before the commit is durable (008 T024): in seed 1 of `txn-append` a
    // transaction reads a list in an order no other one saw, before `intents` in `splits`.
    // A resharding that renames without the writes after its last read of the history (018
    // T002): the `reshard` workload reads a value of the old order the copy never took.
    ("reshard_cutover_loses_writes", "reshard", Default, 1000),
    // The ranges take the writes of a resharding's critical section (018 T005): a plain write that
    // read `old` lands in the old order after the copy's last read of it.
    ("reshard_without_fence", "reshard", Default, 1000),
    ("resolve_before_durable_commit", "snapshot-isolation", Default, 1000),
    ("resolved_intent_reappears", "lsm", Default, 1000),
    ("rollback_unrecorded", "rollback", Default, 1000),
    // A search that answers before its segments reach its mark (011 T018): the `search` workload
    // finds an answer below the mark the search asked for.
    ("search_without_wait", "search_results", Default, 1000),
    // A split completed on an end that matches its cut (007 A007-3-1): with a merge that undid an
    // earlier cut at the same key, a replica behind that merge ends there too, Group 0 records a
    // range that no split made, and the replicas past the merge still hold its keys.
    ("split_done_by_end", "range_bounds", Heavy, 1000),
    // The split retracts the intents of the half that moves (007 T017): a transaction that commits
    // loses its write there, and `splits` reads an older value at a mark above its commit.
    ("split_loses_intents", "intents", Default, 1000),
    // A stream resumed inside a transaction takes the token's mark for the whole transaction
    // (014 T020): `changestream` sees the rest of the transaction dropped after a short batch.
    ("split_txn_events", "changestream_delivery", Default, 1000),
    // A deposed leader's read: `range-docs` reads again at that mark and sees another version, which
    // the `mvcc` check reports before the history is checked; `follower_local_read` covers
    // `linearizable`.
    ("stale_lease_read", "mvcc", Default, 1000),
    // The TTL monitor's cutoff adds the index's seconds instead of taking them away: the `indexes`
    // workload's delete takes a document that has not expired.
    ("ttl_deletes_live", "ttl", Default, 1000),
    // A transaction's intent on an index entry taken as free: two documents with one value of a
    // unique index both write its entry, and the `indexes` workload's read finds one without it.
    ("unique_ignores_intents", "index_consistency", Default, 1000),
    ("vote_after_restart", "raft_safety", Default, 1000),
    ("vote_during_lease", "raft_safety", Default, 1000),
    ("vote_twice", "raft_safety", Default, 1000),
    ("wal_release_hardstate", "recovery", Default, 1000),
    // A leader at the edge of the clock margin, cut off while its lease reads go on (002 research
    // R3e): the next leader writes below them unless the read cache's floor holds it. The voters
    // carry the old leader's clock to the new one (D-069), so only a write within the edge's
    // 90-110 ms of the last lease read shows it, and the election guard (150 ms) comes 40 ms after
    // the lease (110 ms). `range-docs` makes that write: while such a leader is cut off its
    // clients read through it until its lease ends and then write the same documents through the
    // next leader as soon as it leads (75 seeds in 1 000 caught, the first at seed 11). The
    // variant also leaves out the raise of the new leader's clock to the cache's minimum (005
    // research R12): every read takes its mark from the leader's clock or puts it in the clock, so
    // its own later writes go above it anyway, and the cache starts afresh each term, so its
    // minimum is all the floor carries from one leader to the next (D-083, the same rule as
    // `HlcLease.tla`).
    ("write_below_read", "mvcc", Heavy, 1000),
    // Reads without the uncertainty window (008 FR-008): a transaction that begins on a node whose
    // clock is behind, after another's commit on a node whose clock is ahead was acknowledged,
    // reads below that commit and misses it; the real-time order of `txn-append` finds the cycle
    // of its `rw` and the other's `rt` (G-SI-realtime). The ±50 ms of the default clocks suffice.
    ("zero_uncertainty", "snapshot-isolation", Default, 1000),
    // The balancer places replicas without the zones (018 T007): a range inside an interval keeps
    // replicas outside its zone's nodes, or one crosses an interval's edge.
    ("zones_ignored", "zones", Default, 1000),
);

/// The line of `--list-variants`: `<cfg> <checker> <plan> <seeds>`.
pub fn line(v: &Variant) -> String {
    format!("{} {} {} {}", v.name, v.checker, v.plan.name(), v.seeds)
}

/// Checks that this binary is the one of `wanted`: a usage error otherwise, with how to build it.
pub fn check(wanted: &str) -> Result<(), String> {
    if !NAMES.contains(&wanted) {
        return Err(format!("unknown variant: {wanted} ({})", NAMES.join(", ")));
    }
    check_built(built(), wanted)
}

/// [`check`] for a binary built with `built`.
fn check_built(built: Option<&str>, wanted: &str) -> Result<(), String> {
    match built {
        Some(b) if b == wanted => Ok(()),
        Some(b) => Err(format!("this binary was built with variant {b}, not {wanted}")),
        None => Err(format!(
            "this binary was built without a faulty variant; build it with \
             RUSTFLAGS='--cfg teseladb_bug=\"{wanted}\"' (scripts/sim-mutants.sh does it)"
        )),
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_sorted_and_the_check_says_what_is_wrong() {
        assert!(NAMES.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(VARIANTS.iter().map(|v| v.name).collect::<Vec<_>>(), NAMES);
        assert_eq!(line(&VARIANTS[0]), "ack_before_fsync durability default 1000");
        assert_eq!(built(), None, "the tests run on the correct code");
        assert!(check("vote_twice").unwrap_err().contains("built without a faulty variant"));
        assert!(check("nope").unwrap_err().starts_with("unknown variant: nope"));
        // A binary built with a faulty variant runs only that one.
        assert_eq!(check_built(Some("vote_twice"), "vote_twice"), Ok(()));
        let other = check_built(Some("ack_before_fsync"), "vote_twice").unwrap_err();
        assert!(other.contains("built with variant ack_before_fsync, not vote_twice"), "{other}");
    }
}
