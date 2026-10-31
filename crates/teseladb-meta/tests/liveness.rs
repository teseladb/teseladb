//! The liveness table of Group 0's leader and its frames.

use teseladb_meta::clock::ClockSample;
use teseladb_meta::liveness::{
    DEAD_AFTER, LiveState, Liveness, NodeFrame, NodeLoad, SUSPECT_AFTER, dead_after,
};
use teseladb_types::{Decoder, Encoder, Nanos, NodeId};

fn n(i: u64) -> NodeId {
    NodeId(i)
}

fn sample(i: u64, skew: i64) -> ClockSample {
    ClockSample { node: n(i), sent_ms: 1000 + skew, received_ms: 1000, rtt_ms: 0 }
}

#[test]
fn nodes_go_suspect_then_dead_and_come_back() {
    let t0 = Nanos::from_secs(100);
    let mut l = Liveness::start([n(1), n(2)], t0);
    let nodes = [n(1), n(2)];
    let dead = Nanos::from_secs(20);
    assert!(l.check(&nodes, t0 + Nanos::from_secs(4), dead).is_empty());
    // Node 1 is heard; node 2 is not.
    let load = NodeLoad { groups: 3, leaders: 1, bytes: 10, ..NodeLoad::default() };
    assert!(!l.heartbeat(n(1), t0 + Nanos::from_secs(4), load, sample(1, 0)));
    assert_eq!(
        l.check(&nodes, t0 + SUSPECT_AFTER, dead),
        vec![(n(2), LiveState::Alive, LiveState::Suspect)]
    );
    assert_eq!(
        l.check(&nodes, t0 + dead, dead),
        vec![
            (n(1), LiveState::Alive, LiveState::Suspect),
            (n(2), LiveState::Suspect, LiveState::Dead)
        ]
    );
    assert_eq!(l.get(n(1)).map(|e| e.load), Some(load));
    // A heartbeat brings it back, and says it was not alive.
    assert!(l.heartbeat(n(2), t0 + dead, NodeLoad::default(), sample(2, 0)));
    assert_eq!(l.states(), vec![(n(1), LiveState::Suspect), (n(2), LiveState::Alive)]);
    // A `dead_after` shorter than the suspicion still passes through `Suspect` first.
    let mut l = Liveness::start([n(1)], t0);
    let short = Nanos::from_secs(1);
    assert!(l.check(&[n(1)], t0 + Nanos::from_secs(2), short).is_empty());
    assert_eq!(
        l.check(&[n(1)], t0 + SUSPECT_AFTER, short),
        vec![(n(1), LiveState::Alive, LiveState::Dead)]
    );
}

#[test]
fn the_table_follows_group0s_nodes() {
    let t0 = Nanos::from_secs(1);
    let mut l = Liveness::start([n(1), n(2)], t0);
    // Node 3 joins: alive from when it is first seen. Node 2 is removed: it leaves the table.
    let later = t0 + Nanos::from_secs(4);
    assert!(l.check(&[n(1), n(3)], later, DEAD_AFTER).is_empty());
    assert_eq!(l.states(), vec![(n(1), LiveState::Alive), (n(3), LiveState::Alive)]);
    let changed = l.check(&[n(1), n(3)], t0 + SUSPECT_AFTER, DEAD_AFTER);
    assert_eq!(changed, vec![(n(1), LiveState::Alive, LiveState::Suspect)]);
    assert!(l.get(n(2)).is_none());
    // A heartbeat of a node the table does not have yet adds it.
    assert!(!l.heartbeat(n(4), later, NodeLoad::default(), sample(4, 0)));
    assert_eq!(l.get(n(4)).map(|e| e.state), Some(LiveState::Alive));
}

#[test]
fn clock_reports_use_the_voters_median() {
    let mut l = Liveness::start([n(1), n(2), n(3)], Nanos::ZERO);
    l.heartbeat(n(1), Nanos::ZERO, NodeLoad::default(), sample(1, 0));
    l.heartbeat(n(2), Nanos::ZERO, NodeLoad::default(), sample(2, 30));
    l.heartbeat(n(3), Nanos::ZERO, NodeLoad::default(), sample(3, 900));
    let voters = [n(1), n(2), n(3)];
    assert_eq!(l.clock_reports(n(1), &voters), vec![(n(1), -30), (n(2), 0), (n(3), 870)]);
}

#[test]
fn dead_after_comes_from_the_setting_when_it_is_a_number() {
    let d = Nanos::from_secs(60);
    assert_eq!(dead_after(None, d), d);
    assert_eq!(dead_after(Some(b"5000"), d), Nanos::from_secs(5));
    assert_eq!(dead_after(Some(b"soon"), d), d);
    assert_eq!(dead_after(Some(&[0xff]), d), d);
}

#[test]
fn loads_add_up_and_frames_round_trip() {
    let a = NodeLoad { groups: 1, leaders: 2, bytes: 3, ..NodeLoad::default() };
    assert_eq!(a.plus(a), NodeLoad { groups: 2, leaders: 4, bytes: 6, ..NodeLoad::default() });
    for f in [
        NodeFrame::Heartbeat { now_ms: 1_700_000_000_000, load: a },
        NodeFrame::ClockReport { offset_ms: -750 },
        NodeFrame::Suspect { node: n(7), suspect: true },
        NodeFrame::Suspect { node: n(7), suspect: false },
    ] {
        let mut e = Encoder::new();
        f.encode(&mut e);
        let bytes = e.finish();
        assert_eq!(NodeFrame::decode(&mut Decoder::new(&bytes)), Ok(f));
    }
    assert!(NodeFrame::decode(&mut Decoder::new(&[9])).is_err());
}
