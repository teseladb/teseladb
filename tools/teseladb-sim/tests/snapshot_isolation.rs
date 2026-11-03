//! The snapshot isolation checker (008 FR-016, T003) on histories built by hand: one per anomaly
//! (incompatible order, G0, G1a, G1b, G1c, G-SI, and the real-time ones) and histories snapshot
//! isolation allows.

use teseladb_sim::checkers::snapshot_isolation::{MicroOp, Outcome, RealTime, Txn, check};

fn append(key: &str, value: i64) -> MicroOp {
    MicroOp::Append { key: key.to_owned(), value }
}

fn read(key: &str, list: &[i64]) -> MicroOp {
    MicroOp::Read { key: key.to_owned(), list: list.to_vec() }
}

fn txn(id: u64, ops: Vec<MicroOp>) -> Txn {
    Txn { id, ops, outcome: Outcome::Committed, realtime: None }
}

/// A committed transaction that began at `began` and whose commit was acknowledged at `acked`.
fn timed(id: u64, ops: Vec<MicroOp>, began: u64, acked: u64) -> Txn {
    Txn { realtime: Some(RealTime { began, acked: Some(acked) }), ..txn(id, ops) }
}

fn anomaly(history: &[Txn]) -> String {
    match check(history) {
        Err(v) => format!("{v:?}"),
        Ok(()) => "none".to_owned(),
    }
}

fn found(history: &[Txn], kind: &str) {
    let a = anomaly(history);
    assert!(a.contains(kind), "expected {kind}, got {a}");
}

#[test]
fn serial_histories_and_write_skew_pass() {
    // Serial: each transaction sees the one before.
    let serial = [
        txn(1, vec![append("x", 1), append("y", 1)]),
        txn(2, vec![read("x", &[1]), append("x", 2), read("y", &[1])]),
        txn(3, vec![read("x", &[1, 2]), read("y", &[1])]),
    ];
    assert_eq!(anomaly(&serial), "none");
    // Write skew: each reads what the other writes, before it; snapshot isolation allows it.
    let skew = [
        txn(1, vec![read("x", &[]), read("y", &[]), append("x", 1)]),
        txn(2, vec![read("x", &[]), read("y", &[]), append("y", 1)]),
        txn(3, vec![read("x", &[1]), read("y", &[1])]),
    ];
    assert_eq!(anomaly(&skew), "none");
    // An aborted transaction's append that nobody read, and an unknown one that someone read.
    let mut aborted = txn(4, vec![append("z", 9)]);
    aborted.outcome = Outcome::Aborted;
    let mut unknown = txn(5, vec![append("z", 1)]);
    unknown.outcome = Outcome::Unknown;
    let reader = txn(6, vec![read("z", &[1])]);
    assert_eq!(anomaly(&[aborted, unknown, reader]), "none");
    // A transaction reading its own intermediate appends.
    let own = [txn(1, vec![append("x", 1), read("x", &[1]), append("x", 2)])];
    assert_eq!(anomaly(&own), "none");
    // A read that goes through all of a writer's appends.
    let through = [txn(1, vec![append("x", 1), append("x", 2)]), txn(2, vec![read("x", &[1, 2])])];
    assert_eq!(anomaly(&through), "none");
}

#[test]
fn incompatible_orders_are_found() {
    let h = [
        txn(1, vec![append("x", 1)]),
        txn(2, vec![append("x", 2)]),
        txn(3, vec![read("x", &[1, 2])]),
        txn(4, vec![read("x", &[2])]),
    ];
    found(&h, "incompatible order");
}

#[test]
fn g1a_aborted_reads_are_found() {
    let mut w = txn(1, vec![append("x", 1)]);
    w.outcome = Outcome::Aborted;
    found(&[w, txn(2, vec![read("x", &[1])])], "G1a");
}

#[test]
fn g1b_intermediate_reads_are_found() {
    let h = [
        txn(1, vec![append("x", 1), append("x", 2)]),
        txn(2, vec![read("x", &[1])]),
        txn(3, vec![read("x", &[1, 2])]),
    ];
    found(&h, "G1b");
}

#[test]
fn g0_write_cycles_are_found() {
    // x: 1 (T1) then 2 (T2); y: 3 (T2) then 4 (T1).
    let h = [
        txn(1, vec![append("x", 1), append("y", 4)]),
        txn(2, vec![append("x", 2), append("y", 3)]),
        txn(3, vec![read("x", &[1, 2]), read("y", &[3, 4])]),
    ];
    found(&h, "G0");
}

#[test]
fn g1c_circular_information_flow_is_found() {
    // T1 reads T2's write and T2 reads T1's.
    let h = [
        txn(1, vec![append("x", 1), read("y", &[2])]),
        txn(2, vec![append("y", 2), read("x", &[1])]),
    ];
    found(&h, "G1c");
}

#[test]
fn g_si_lost_updates_and_missed_effects_are_found() {
    // Lost update: both read x empty and append; one rw and one ww edge in the cycle.
    let lost = [
        txn(1, vec![read("x", &[]), append("x", 1)]),
        txn(2, vec![read("x", &[]), append("x", 2)]),
        txn(3, vec![read("x", &[1, 2])]),
    ];
    found(&lost, "G-SI");
    // Missed effects: T2 reads T1's x but not its earlier y.
    let missed = [
        txn(1, vec![append("y", 1), append("x", 1)]),
        txn(2, vec![read("x", &[1]), read("y", &[])]),
        txn(3, vec![read("y", &[1])]),
    ];
    found(&missed, "G-SI");
}

#[test]
fn a_transaction_that_misses_one_acknowledged_before_it_began_is_found() {
    // T2 began after T1's commit was acknowledged and read x without T1's append: a stale read.
    let stale = [
        timed(1, vec![append("x", 1)], 10, 20),
        timed(2, vec![read("x", &[])], 30, 40),
        txn(3, vec![read("x", &[1])]),
    ];
    found(&stale, "G-SI-realtime");
    // The same reads with T2 begun before T1's acknowledgement: concurrent, allowed.
    let concurrent = [
        timed(1, vec![append("x", 1)], 10, 20),
        timed(2, vec![read("x", &[])], 15, 40),
        txn(3, vec![read("x", &[1])]),
    ];
    assert_eq!(anomaly(&concurrent), "none");
    // Without times (clocks past the window) the order is not checked.
    let untimed =
        [txn(1, vec![append("x", 1)]), txn(2, vec![read("x", &[])]), txn(3, vec![read("x", &[1])])];
    assert_eq!(anomaly(&untimed), "none");
    // A transaction that sees one that began after its own commit was acknowledged.
    let future = [
        timed(1, vec![append("x", 1), read("y", &[2])], 10, 20),
        timed(2, vec![append("y", 2)], 30, 40),
    ];
    found(&future, "G1c-realtime");
}

#[test]
fn the_real_time_order_goes_through_chains_of_transactions() {
    // T1 -> T2 -> T3 in real time, T3 misses T1's append: only the chain orders T1 before T3,
    // since the edge from T1 to T3 is implied by the other two.
    let chain = [
        timed(1, vec![append("x", 1)], 10, 20),
        timed(2, vec![append("y", 1)], 25, 30),
        timed(3, vec![read("x", &[])], 35, 40),
        txn(4, vec![read("x", &[1]), read("y", &[1])]),
    ];
    found(&chain, "G-SI-realtime");
    // An aborted or unknown transaction gives no real-time order; one that never committed is not
    // ordered after anything.
    let mut aborted = timed(1, vec![append("x", 1)], 10, 20);
    aborted.outcome = Outcome::Aborted;
    let mut unknown = timed(2, vec![append("y", 1)], 10, 20);
    unknown.outcome = Outcome::Unknown;
    unknown.realtime = Some(RealTime { began: 10, acked: None });
    let reader = timed(3, vec![read("x", &[]), read("y", &[])], 30, 40);
    let later = txn(4, vec![read("y", &[1])]);
    assert_eq!(anomaly(&[aborted, unknown, reader, later]), "none");
}

#[test]
fn values_nobody_appended_and_transactions_of_unknown_end_add_no_edges() {
    // `x` holds 1, 2 (an append whose end is unknown) and 9, which no transaction appended.
    let unknown = Txn { outcome: Outcome::Unknown, ..txn(2, vec![append("x", 2)]) };
    let history = [
        txn(1, vec![append("x", 1)]),
        unknown,
        txn(3, vec![read("x", &[1, 2, 9])]),
        txn(4, vec![read("x", &[1, 2, 9]), append("y", 1)]),
        // A transaction acknowledged before it began (a broken external history) has no edge to
        // itself.
        timed(5, vec![append("z", 1)], 10, 5),
        timed(6, vec![read("z", &[1])], 20, 30),
    ];
    assert_eq!(anomaly(&history), "none");
}
