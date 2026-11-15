//! The flight of the frames another node sends (012 FR-015): a core that reads a frame of Raft or
//! of heartbeats records, in the stage `peer_frame_flight`, its own wall clock minus the one the
//! frame carried. On nodes of one machine that is the frame's delay; a clock not known yet (0)
//! records nothing. This file is its own test binary, so no other test moves the counters.

mod common;

use common::*;
use teseladb_multiraft::PeerFrame;
use teseladb_stages::Stage;

/// How long the frame of the test was on its way.
const FLIGHT_MS: u64 = 300;

#[test]
fn a_frame_from_another_node_records_its_flight() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.ticks(5);
    let now = c.now;
    let wall = WALL0 + now.as_millis();
    let other = c.nodes.keys().copied().find(|n| *n != l).expect("a follower");
    let core = &mut c.nodes.get_mut(&other).expect("the follower").core;
    let hlc = teseladb_hlc::Hlc { physical: wall - FLIGHT_MS, logical: 0 };
    let (count, nanos) = (
        teseladb_stages::count(Stage::PeerFrameFlight),
        teseladb_stages::total_nanos(Stage::PeerFrameFlight),
    );
    let beat = PeerFrame::Heartbeats { wall_ms: wall - FLIGHT_MS, hlc, beats: Vec::new() };
    core.on_peer_message(l, beat, now);
    assert_eq!(teseladb_stages::count(Stage::PeerFrameFlight), count + 1);
    let took = teseladb_stages::total_nanos(Stage::PeerFrameFlight) - nanos;
    assert!(took >= FLIGHT_MS * 1_000_000, "{took} ns");
    // A sender whose clock is not known yet says nothing of the flight.
    let unknown = PeerFrame::Heartbeats { wall_ms: 0, hlc, beats: Vec::new() };
    core.on_peer_message(l, unknown, now);
    assert_eq!(teseladb_stages::count(Stage::PeerFrameFlight), count + 1);
}
