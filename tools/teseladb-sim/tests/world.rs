//! The world and the local durability checker, without the `run_seed` loop: frozen cores, events
//! from a previous life of the node and every kind of loss the checker has to see.

mod common;

use common::drive;
use teseladb_raftlog::HardState;
use teseladb_sim::checkers::storage::{Confirmed, verify};
use teseladb_sim::world::{Event, World};
use teseladb_types::{Bytes, Durability, GroupId, Nanos};

/// Runs the disk and deadline events until nothing is left.
fn settle(w: &mut World) {
    while let Some((_, _, ev)) = w.pop() {
        match ev {
            Event::Disk { node, core, life } => {
                w.on_disk(node, core, life);
            }
            Event::LogFlush { node, life, sync } => w.on_log_flush(node, life, sync),
            Event::LogDone { node, core, life, done } => {
                w.on_log_done(node, core, life, done);
            }
            Event::Tick { node, core, life } => {
                w.on_tick(node, core, life);
            }
            _ => {}
        }
    }
}

#[test]
fn verify_sees_every_kind_of_loss() {
    let mut w = World::new(1, 1, 1, false);
    let g = GroupId(5);
    let s = w.store(0, 0).unwrap();
    s.create_group(g, Bytes::new(), Durability::Strict).unwrap();
    let hs = HardState { term: 3, vote: 2, commit: 0 };
    let e = teseladb_raftlog::LogEntry {
        index: 1,
        term: 3,
        kind: teseladb_raftlog::format::wal::EntryKind::Normal,
        data: Bytes::from("x"),
    };
    s.append_raft(g, &[e], Some(hs), Durability::Strict).unwrap();
    w.pump(0, 0);
    settle(&mut w);
    w.crash_node(0);
    w.start_node(0).unwrap();
    let mut good = Confirmed::default();
    good.groups.insert(g);
    good.entries.insert((g, 1), Bytes::from("x"));
    good.hard.insert(g, hs);
    assert_eq!(verify(&mut w, 0, 0, &good), Ok(()));
    let check = |w: &mut World, f: &dyn Fn(&mut Confirmed)| {
        let mut c = good.clone();
        f(&mut c);
        verify(w, 0, 0, &c).unwrap_err().detail
    };
    assert!(
        check(&mut w, &|c| {
            c.groups.insert(GroupId(77));
        })
        .contains("group")
    );
    assert!(
        check(&mut w, &|c| {
            c.entries.insert((g, 2), Bytes::new());
        })
        .contains("entry")
    );
    assert!(
        check(&mut w, &|c| {
            c.entries.insert((g, 1), Bytes::from("y"));
        })
        .contains("entry")
    );
    assert!(
        check(&mut w, &|c| {
            c.hard.insert(g, HardState { term: 4, ..hs });
        })
        .contains("goes backwards")
    );
    assert!(
        check(&mut w, &|c| {
            c.hard.insert(g, HardState { vote: 9, ..hs });
        })
        .contains("goes backwards")
    );
}

#[test]
fn frozen_cores_and_stale_events() {
    let mut w = World::new(2, 1, 1, false);
    assert!(w.wall_ms(0) > 1_000_000_000_000);
    // Frozen: it emits no I/O until it thaws (it schedules a deadline for then).
    w.nodes[0].cores[0].frozen_until = Nanos::from_millis(5);
    let s = w.store(0, 0).unwrap();
    s.create_group(GroupId(1), Bytes::new(), Durability::Strict).unwrap();
    w.pump(0, 0);
    let (at, _, ev) = w.pop().unwrap();
    assert_eq!(at, Nanos::from_millis(5));
    assert!(matches!(ev, Event::Tick { .. }));
    // An event from before a crash is dropped.
    w.nodes[0].cores[0].frozen_until = Nanos::ZERO;
    w.pump(0, 0);
    let (_, _, ev) = w.pop().unwrap();
    let Event::Disk { node, core, life } = ev else { panic!("{ev:?}") };
    w.crash_node(0);
    assert!(w.on_disk(node, core, life).is_none());
    assert!(w.on_tick(node, core, life).is_none());
    assert!(w.store(0, 0).is_none());
    w.pump(0, 0); // powered off: does nothing
    w.start_node(0).unwrap();
    assert!(w.on_disk(node, core, life).is_none(), "previous life");
    for ev in [
        Event::Client { client: 1 },
        Event::Deliver { from: 0, to: 1, core: 0, epoch: 0, frame: Bytes::new() },
        Event::Restart { node: 0 },
        Event::Heal,
        Event::CheckPoint,
        Event::FaultEnd(teseladb_sim::faults::FaultAction::SyncError { node: 0 }),
    ] {
        assert!(!ev.kind().is_empty());
    }
}

#[test]
fn with_every_node_down_any_node_is_picked() {
    let mut w = World::new(3, 3, 1, false);
    let f = w.follower();
    assert!(f < 3);
    for n in 0..3 {
        w.crash_node(n);
    }
    for _ in 0..20 {
        assert!(w.follower() < 3);
        assert!(w.busiest_leader() < 3);
    }
}

#[test]
fn every_frame_crosses_the_simulated_network_unchanged() {
    use teseladb_multiraft::chunks::split;
    use teseladb_multiraft::{GroupHeartbeat, GroupHeartbeatResp, PeerFrame};
    use teseladb_raft::{Message, MessageType};
    use teseladb_sim::world::{decode_frame, encode_frame};
    use teseladb_types::NodeId;
    let m = Message::new(MessageType::AppendReq, NodeId(1), NodeId(2), 3);
    let all = vec![
        PeerFrame::Raft {
            wall_ms: 1_800_000_000_789,
            hlc: teseladb_hlc::Hlc { physical: 1_800_000_000_790, logical: 3 },
            msgs: vec![(GroupId(1), m)],
        },
        PeerFrame::Heartbeats {
            wall_ms: 1_800_000_000_123,
            hlc: teseladb_hlc::Hlc { physical: 1_800_000_000_124, logical: 1 },
            beats: vec![
                GroupHeartbeat {
                    group: GroupId(2),
                    term: 4,
                    commit: 5,
                    lease_seq: 6,
                    closed: None,
                },
                GroupHeartbeat {
                    group: GroupId(3),
                    term: 4,
                    commit: 5,
                    lease_seq: 6,
                    closed: Some((teseladb_hlc::Hlc { physical: 7, logical: 8 }, 9)),
                },
            ],
        },
        PeerFrame::HeartbeatResps {
            wall_ms: 1_800_000_000_456,
            hlc: teseladb_hlc::Hlc { physical: 1_800_000_000_457, logical: 2 },
            resps: vec![GroupHeartbeatResp { group: GroupId(2), term: 4, ok: false, lease_seq: 6 }],
        },
        PeerFrame::SnapshotChunk(split(GroupId(3), 9, 2, b"state")[0].clone()),
        PeerFrame::SnapshotAck { group: GroupId(3), index: 9, offset: 5, ok: true },
        PeerFrame::Forward { req: 77, deadline_ms: 500, hlc: None, body: Bytes::from("q") },
        PeerFrame::ForwardResp { req: 77, hlc: None, body: Bytes::from("a") },
    ];
    for f in all {
        assert_eq!(decode_frame(&encode_frame(&f)), f);
    }
}

#[test]
fn a_testkv_cluster_answers_its_clients_under_faults() {
    use teseladb_sim::faults::FaultPlan;
    use teseladb_sim::{SimConfig, Verdict, run_seed};
    for (plan, seeds) in [(FaultPlan::Default, 1..=20), (FaultPlan::Heavy, 1..=10)] {
        for workload in ["testkv", "storage"] {
            let cfg = SimConfig { workload: workload.into(), faults: plan, ..SimConfig::default() };
            for seed in seeds.clone() {
                let r = run_seed(&cfg, seed);
                assert_eq!(r.verdict, Verdict::Pass, "{workload} {plan:?} seed {seed}");
                assert!(r.events > 10_000, "seed {seed}: {} events", r.events);
            }
        }
    }
}

#[test]
fn with_every_live_node_leading_any_live_node_is_a_follower() {
    use teseladb_multiraft::GroupDesc;
    use teseladb_types::NodeId;
    let mut w = World::cluster(4, 1, 1, false);
    w.add_group(
        GroupDesc { group: GroupId(1), voters: vec![NodeId(1)], learners: vec![] },
        teseladb_sim::node::SmKind::Kv,
    );
    for _ in 0..1_000 {
        let Some((_, _, ev)) = w.pop() else { break };
        match ev {
            Event::Disk { node, core, life } => {
                w.on_disk(node, core, life);
            }
            Event::LogFlush { node, life, sync } => w.on_log_flush(node, life, sync),
            Event::LogDone { node, core, life, done } => {
                w.on_log_done(node, core, life, done);
            }
            Event::Tick { node, core, life } => {
                w.on_tick(node, core, life);
            }
            _ => {}
        }
        if w.busiest_leader() == 0
            && w.cluster_core(0, 0).unwrap().status(GroupId(1)).unwrap().role
                == teseladb_raft::Role::Leader
        {
            break;
        }
    }
    assert_eq!(w.follower(), 0, "the only node leads its group");
}

#[test]
fn a_crash_during_the_bootstrap_of_group0_lets_another_try() {
    let mut w = World::cluster(9, 3, 2, false);
    assert!(drive(&mut w, |w| w.group0_pending().is_some()), "a bootstrap is proposed");
    let node = w.group0_pending().unwrap();
    w.crash_node(node);
    assert_eq!(w.group0_pending(), None, "gone with its node");
    let at = w.now() + Nanos::from_millis(50);
    w.schedule(at, Event::Restart { node });
    assert!(drive(&mut w, |w| w.group0_bootstrapped() == 3), "Group 0 is created all the same");
}

#[test]
fn a_group_added_while_a_node_is_down_is_created_when_it_starts() {
    use teseladb_multiraft::GroupDesc;
    use teseladb_types::NodeId;
    let mut w = World::cluster(10, 3, 2, false);
    w.crash_node(2);
    let desc =
        GroupDesc { group: GroupId(5), voters: (1..=3).map(NodeId).collect(), learners: vec![] };
    w.add_group(desc, teseladb_sim::node::SmKind::Kv);
    let core = w.core_of(GroupId(5));
    assert!(w.cluster_core(2, core).is_none());
    w.start_node(2).unwrap();
    assert!(w.cluster_core(2, core).unwrap().status(GroupId(5)).is_some());
}

#[test]
fn a_restart_waits_for_the_bound_on_disk_and_never_repeats_a_mark() {
    use teseladb_multiraft::GroupDesc;
    use teseladb_types::NodeId;
    let mut w = World::cluster(11, 1, 1, false);
    w.add_group(
        GroupDesc { group: GroupId(1), voters: vec![NodeId(1)], learners: vec![] },
        teseladb_sim::node::SmKind::Kv,
    );
    // A new node has no bound on disk: its first mark is refused until one is written above it,
    // and the bound then keeps ahead of its marks (005 FR-003).
    assert!(w.nodes[0].hlc_bound < w.wall_ms(0), "no mark asked for yet");
    assert!(drive(&mut w, |w| w.nodes[0].hlc_bound > w.wall_ms(0)), "a mark asks for a bound");
    let deadline = w.now() + Nanos::from_secs(2);
    assert!(drive(&mut w, |w| w.now() >= deadline));
    let bound = w.nodes[0].hlc_bound;
    let mark = w.nodes[0].hlc.0.peek().physical;
    assert!(bound > mark && bound <= mark + teseladb_hlc::AHEAD_MS, "{bound} {mark}");
    // Idle for two seconds, its next mark waits for a new bound (at the next tick).
    let mut last = None;
    assert!(drive(&mut w, |w| {
        if let teseladb_hlc::Next::Mark(m) = w.nodes[0].hlc.0.now(w.wall_ms(0)) {
            last = Some(m);
        }
        last.is_some()
    }));
    let last = last.unwrap();
    let bound = w.nodes[0].hlc_bound;
    // The node goes down and its wall clock goes back 1.5 s.
    w.crash_node(0);
    w.nodes[0].clock.jump(-1_500_000_000);
    let until = w.start_wait(0).expect("the clock is below the bound");
    assert!(until > w.now());
    assert_eq!(w.nodes[0].clock.wall_ms(until), bound + 1, "it waits to pass the bound");
    assert_eq!(w.nodes[0].clock.wall_ms(Nanos(until.0 - 1_000_000)), bound);
    w.schedule(w.now(), Event::Restart { node: 0 });
    assert!(drive(&mut w, |w| w.nodes[0].alive), "it starts once past the bound");
    assert!(w.now() >= until, "not before");
    let mut first = None;
    assert!(drive(&mut w, |w| {
        if let teseladb_hlc::Next::Mark(m) = w.nodes[0].hlc.0.now(w.wall_ms(0)) {
            first = Some(m);
        }
        first.is_some()
    }));
    let first = first.unwrap();
    assert!(first > last, "{first:?} {last:?}");
}

/// A node waiting for its clock to pass the bound on disk looks at the clock again every
/// [`START_POLL`], as the server does: once NTP steps the clock back to time, it starts then, not at
/// the instant its old clock would have reached the bound (seed 3772 of `search`: a node whose
/// clock had gone back three seconds slept through the heal and no write was answered after it).
#[test]
fn a_node_waiting_for_the_bound_starts_soon_after_its_clock_is_set_right() {
    use teseladb_multiraft::GroupDesc;
    use teseladb_sim::world::START_POLL;
    use teseladb_types::NodeId;
    let mut w = World::cluster(12, 1, 1, false);
    w.add_group(
        GroupDesc { group: GroupId(1), voters: vec![NodeId(1)], learners: vec![] },
        teseladb_sim::node::SmKind::Kv,
    );
    assert!(drive(&mut w, |w| w.nodes[0].hlc_bound > w.wall_ms(0)), "a bound on disk");
    w.crash_node(0);
    w.nodes[0].clock.jump(-3_000_000_000);
    let until = w.start_wait(0).expect("the clock is below the bound");
    assert_eq!(w.start_recheck(0), Some(w.now() + START_POLL), "it looks again soon");
    w.schedule(w.now(), Event::Restart { node: 0 });
    let set_at = w.now() + Nanos::from_millis(500);
    assert!(drive(&mut w, |w| w.now() >= set_at));
    assert!(!w.nodes[0].alive, "still below the bound");
    w.nodes[0].clock.resync();
    // The bound is up to a second ahead of its last mark: it may still have that to wait.
    let set_right = w.start_wait(0).unwrap_or(set_at);
    assert!(set_right < until);
    assert!(drive(&mut w, |w| w.nodes[0].alive));
    assert!(
        w.now() >= set_right && w.now() <= set_right + START_POLL,
        "{:?} {set_right:?}",
        w.now()
    );
}

/// A frozen core runs nothing until it thaws, also what a workload would ask of it: its process is
/// paused, so it neither serves a read with a lease that ran out meanwhile nor answers a client.
/// Seeds 1232 and 1423 of `txn-bank`: a paused leader's core served a lease read during the pause
/// and its observation reached the Raft checker only at the thaw, after a newer leader.
#[test]
fn a_frozen_core_is_handed_to_no_workload_until_it_thaws() {
    let mut w = World::cluster(12, 3, 2, false);
    assert!(drive(&mut w, |w| w.group0_bootstrapped() == 3), "the cluster starts");
    let thaw = w.now() + Nanos::from_millis(300);
    w.nodes[1].cores[1].frozen_until = thaw;
    assert!(w.cluster_core(1, 1).is_none(), "frozen: no workload drives it");
    assert!(w.cluster_core(1, 0).is_some(), "the node's other cores run");
    assert!(w.paused_core(1, 1).is_some(), "the simulator's checks still see it");
    assert!(drive(&mut w, |w| w.now() >= thaw));
    assert!(w.cluster_core(1, 1).is_some(), "thawed");
}
