//! A follower that falls behind the compacted part receives a snapshot, installs it and
//! continues with the log; `report_snapshot` and `report_unreachable`.

mod common;

use common::*;
use teseladb_raft::{LogStore, Message, MessageType, Role, SnapshotMeta};
use teseladb_types::Bytes;

#[test]
fn lagging_follower_installs_a_snapshot() {
    let mut net = Net::new(3, 0, 41);
    let l = net.elect(300);
    let f = (1..=3).map(id).find(|&n| n != l).unwrap();
    net.isolate(f);
    for i in 0..30u8 {
        net.propose(l, &[i]).unwrap();
    }
    net.ticks(3);
    // The leader (and the other follower) compact their log up to what is applied.
    for n in (1..=3).map(id).filter(|&n| n != f) {
        let node = net.nodes.get_mut(&n).unwrap();
        let applied = node.applied.last().unwrap().index;
        let term = node.store.term(applied).unwrap();
        let conf = node.raw.conf().clone();
        node.store.compact(SnapshotMeta { index: applied, term, conf });
    }
    // The follower comes back: the leader sends SnapshotReq (without data); the harness delivers it
    // with data, as the core would after sending the chunks, and reports the result.
    net.heal();
    net.ticks(10);
    assert!(net.snapshots_sent >= 1);
    assert_eq!(net.nodes[&f].snapshot_data.as_ref().unwrap(), &Bytes::from("snapshot state"));
    net.propose(l, b"after the snapshot").unwrap();
    net.ticks(5);
    assert_eq!(net.data(f).last().unwrap(), b"after the snapshot");
    assert_eq!(net.role(f), Role::Follower);
    // Reports that change nothing.
    let node = net.nodes.get_mut(&l).unwrap();
    node.raw.report_snapshot(f, false);
    node.raw.report_unreachable(f);
    node.raw.report_unreachable(id(77));
    net.ticks(5);
}

#[test]
fn failed_snapshot_is_retried() {
    let mut net = Net::new(3, 0, 43);
    let l = net.elect(300);
    let f = (1..=3).map(id).find(|&n| n != l).unwrap();
    net.isolate(f);
    for i in 0..30u8 {
        net.propose(l, &[i]).unwrap();
    }
    net.ticks(3);
    for n in (1..=3).map(id).filter(|&n| n != f) {
        let node = net.nodes.get_mut(&n).unwrap();
        let applied = node.applied.last().unwrap().index;
        let term = node.store.term(applied).unwrap();
        let conf = node.raw.conf().clone();
        node.store.compact(SnapshotMeta { index: applied, term, conf });
    }
    // The first send is lost (`heal` also clears the dropped types, so it goes first): the core
    // reports the failure and the leader probes again.
    net.heal();
    net.drop_types.insert(Net::msg_type(teseladb_raft::MessageType::SnapshotReq));
    net.ticks(3);
    assert_eq!(net.snapshots_sent, 0, "the first send is lost");
    net.nodes.get_mut(&l).unwrap().raw.report_snapshot(f, false);
    net.drop_types.clear();
    net.ticks(10);
    assert!(net.snapshots_sent >= 1);
    net.propose(l, b"after retrying").unwrap();
    net.ticks(5);
    assert_eq!(net.data(f).last().unwrap(), b"after retrying");
}

/// The leader of a 3-node net whose follower `f` is behind a compacted log, the first snapshot
/// sent to it lost.
fn behind_a_lost_snapshot(seed: u64) -> (Net, teseladb_types::NodeId, teseladb_types::NodeId) {
    let mut net = Net::new(3, 0, seed);
    let l = net.elect(300);
    let f = (1..=3).map(id).find(|&n| n != l).unwrap();
    net.isolate(f);
    for i in 0..30u8 {
        net.propose(l, &[i]).unwrap();
    }
    net.ticks(3);
    for n in (1..=3).map(id).filter(|&n| n != f) {
        let node = net.nodes.get_mut(&n).unwrap();
        let applied = node.applied.last().unwrap().index;
        let term = node.store.term(applied).unwrap();
        let conf = node.raw.conf().clone();
        node.store.compact(SnapshotMeta { index: applied, term, conf });
    }
    net.heal();
    net.drop_types.insert(Net::msg_type(MessageType::SnapshotReq));
    net.ticks(3);
    assert_eq!(net.snapshots_sent, 0, "the first send is lost");
    (net, l, f)
}

#[test]
fn a_retried_snapshot_goes_at_once_even_to_a_follower_that_answers_nothing() {
    let (mut net, l, f) = behind_a_lost_snapshot(53);
    // The follower answers nothing (it waits for the data of a snapshot): only a retry sends
    // the snapshot again.
    net.drop_types.clear();
    for t in [MessageType::AppendResp, MessageType::HeartbeatResp] {
        net.drop_types.insert(Net::msg_type(t));
    }
    let node = net.nodes.get_mut(&l).unwrap();
    node.raw.retry_snapshot(f, &node.store);
    node.raw.retry_snapshot(id(77), &node.store);
    net.process(l);
    net.deliver();
    assert!(net.snapshots_sent >= 1);
    // A follower retries nothing.
    let node = net.nodes.get_mut(&f).unwrap();
    node.raw.retry_snapshot(l, &node.store);
}

#[test]
fn an_older_install_counts_only_up_to_its_own_index() {
    let (mut net, l, f) = behind_a_lost_snapshot(59);
    // The leader is sending its snapshot; the follower acknowledges an older one it installed.
    let node = net.nodes.get_mut(&l).unwrap();
    node.raw.retry_snapshot(f, &node.store);
    node.raw.snapshot_installed(f, 2);
    node.raw.snapshot_installed(id(77), 2);
    let matched = node.raw.status(&node.store).progress.iter().find(|(n, _)| *n == f).map(|p| p.1);
    assert_eq!(matched, Some(2));
    // Outside a snapshot transfer it changes nothing.
    node.raw.snapshot_installed(f, 20);
    let matched = node.raw.status(&node.store).progress.iter().find(|(n, _)| *n == f).map(|p| p.1);
    assert_eq!(matched, Some(2));
}

#[test]
fn acknowledgements_while_a_snapshot_is_being_sent() {
    let mut net = Net::new(3, 0, 47);
    let l = net.elect(300);
    let f = (1..=3).map(id).find(|&n| n != l).unwrap();
    net.isolate(f);
    for i in 0..30u8 {
        net.propose(l, &[i]).unwrap();
    }
    net.ticks(3);
    for n in (1..=3).map(id).filter(|&n| n != f) {
        let node = net.nodes.get_mut(&n).unwrap();
        let applied = node.applied.last().unwrap().index;
        let term = node.store.term(applied).unwrap();
        let conf = node.raw.conf().clone();
        node.store.compact(SnapshotMeta { index: applied, term, conf });
    }
    // Ticks with every message delivered until the leader sends the snapshot, which is held back.
    net.heal();
    let mut held = None;
    for _ in 0..100 {
        net.now = net.now + TICK;
        let ids: Vec<_> = net.nodes.keys().copied().collect();
        for n in ids {
            let node = net.nodes.get_mut(&n).unwrap();
            node.raw.tick(&node.store, &mut net.rng, net.now);
        }
        net.process_all();
        while let Some(m) = net.queue.pop_front() {
            if m.ty == MessageType::SnapshotReq && held.is_none() {
                held = Some(m);
            } else {
                net.deliver_one(m);
            }
        }
        if held.is_some() {
            break;
        }
    }
    let held = held.expect("the leader sends a snapshot");
    let snapshot_at = held.snapshot.as_ref().unwrap().meta.index;
    let matched = |net: &Net| {
        let node = &net.nodes[&l];
        node.raw.status(&node.store).progress.iter().find(|(n, _)| *n == f).unwrap().1
    };
    // A late acknowledgement below the snapshot moves `matched` but the snapshot is still due.
    let stale = matched(&net) + 1;
    assert!(stale < snapshot_at);
    let mut ack = Message::new(MessageType::AppendResp, f, l, net.term(l));
    ack.index = stale;
    let node = net.nodes.get_mut(&l).unwrap();
    node.raw.step(ack, &node.store, &mut net.rng).unwrap();
    assert_eq!(matched(&net), stale);
    // The follower installs the snapshot and acknowledges it: the leader replicates again without
    // waiting for `report_snapshot`.
    net.report_snapshots = false;
    net.deliver_one(held);
    net.deliver();
    assert_eq!(matched(&net), snapshot_at);
    net.propose(l, b"after the snapshot").unwrap();
    assert_eq!(net.data(f).last().unwrap(), b"after the snapshot");
}
