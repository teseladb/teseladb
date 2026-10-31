//! The clock rule: offsets from the median of Group 0's voters, half the round
//! trip discounted; a single skewed clock, even the leader's, only excludes itself.

use teseladb_meta::clock::{ClockSample, MAX_CLOCK_OFFSET_MS, may_lead, offsets};
use teseladb_types::NodeId;

fn sample(node: u64, sent_ms: i64, received_ms: i64, rtt_ms: i64) -> ClockSample {
    ClockSample { node: NodeId(node), sent_ms, received_ms, rtt_ms }
}

#[test]
fn offsets_are_measured_from_the_voters_median() {
    let voters = [NodeId(1), NodeId(2), NodeId(3)];
    // Node 2 is 20 ms ahead once half its 40 ms round trip is counted; node 3 is 900 ms behind.
    let s = [sample(2, 1_000, 1_000, 40), sample(3, 100, 1_000, 0)];
    assert_eq!(s[0].skew_ms(), 20);
    let o = offsets(NodeId(1), &voters, &s);
    assert_eq!(o, vec![(NodeId(1), 0), (NodeId(2), 20), (NodeId(3), -900)]);
    assert!(may_lead(0) && may_lead(20) && !may_lead(-900));
    // The leader's own clock is the skewed one: the median is the other two's, and only the
    // leader is out.
    let s = [sample(2, 2_000, 1_000, 0), sample(3, 2_010, 1_000, 0)];
    let o = offsets(NodeId(1), &voters, &s);
    assert_eq!(o, vec![(NodeId(1), -1_000), (NodeId(2), 0), (NodeId(3), 10)]);
    // A sampled node that is not a voter gets its offset but does not move the median; a voter
    // without a sample does not count.
    let s = [sample(2, 1_000, 1_000, 0), sample(9, 5_000, 1_000, 0)];
    let o = offsets(NodeId(1), &voters, &s);
    assert_eq!(o, vec![(NodeId(1), 0), (NodeId(2), 0), (NodeId(9), 4_000)]);
    // With an even count the lower median.
    let four = [NodeId(1), NodeId(2), NodeId(3), NodeId(4)];
    let s = [sample(2, 10, 0, 0), sample(3, 20, 0, 0), sample(4, 30, 0, 0)];
    assert_eq!(offsets(NodeId(1), &four, &s)[0], (NodeId(1), -10));
    // Nothing known.
    assert!(offsets(NodeId(1), &[], &[]).is_empty());
    assert!(may_lead(MAX_CLOCK_OFFSET_MS) && !may_lead(MAX_CLOCK_OFFSET_MS + 1));
}

#[test]
fn an_absurd_clock_only_skews_its_own_node() {
    let n = NodeId;
    let voters = [n(1), n(2), n(3)];
    let samples = [sample(2, 1_000, 1_000, 0), sample(3, i64::MAX, 1_000, 40)];
    let out = offsets(n(1), &voters, &samples);
    // The median is still a sane clock: nodes 1 and 2 are on it, node 3 saturates.
    assert_eq!(out, vec![(n(1), 0), (n(2), 0), (n(3), i64::MAX - 1_000)]);
    assert_eq!(sample(3, i64::MIN, i64::MAX, 0).skew_ms(), i64::MIN);
}
