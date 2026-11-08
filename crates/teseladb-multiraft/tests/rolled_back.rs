//! Entries a replica drops from its log because a later leader's log contradicts them (005 FR-010,
//! D-034, model `WriteConcernW1.tla`, action `Replicate`): the core hands each one out with
//! `take_rolled_back` before it overwrites it, so that the layer above writes its rollback files,
//! also after a restart, and it issues no disk operation until the layer above says those files
//! are durable (`rollback_durable`). Committed entries and those the new leader kept are never
//! handed out.

mod common;

use common::*;
use teseladb_multiraft::envelope;
use teseladb_multiraft::{GroupDesc, PeerFrame};
use teseladb_raft::MessageType;
use teseladb_raftlog::{Disk, LogEntry};
use teseladb_types::NodeId;

fn payloads(entries: &[(teseladb_types::GroupId, LogEntry)]) -> Vec<Vec<u8>> {
    entries
        .iter()
        .map(|(g, e)| {
            assert_eq!(*g, G);
            envelope::decode(&e.data).unwrap().payload.to_vec()
        })
        .collect()
}

/// An isolated leader takes two entries the majority never sees, and the majority elects another
/// leader that commits its own; returns the old leader and its term.
fn diverge(c: &mut Cluster) -> (NodeId, u64) {
    let old = c.elect();
    c.ticks(5);
    c.propose(old, b"kept").unwrap();
    c.settle();
    c.isolate(old);
    let term = c.nodes[&old].core.status(G).unwrap().hard_state.term;
    c.propose(old, b"lost-1").unwrap();
    c.propose(old, b"lost-2").unwrap();
    c.settle();
    assert!(c.nodes[&old].core.status(G).unwrap().persisted >= 4, "on its disk");
    let mut new = old;
    for _ in 0..1_000 {
        c.tick();
        if let Some(l) = c.leader().filter(|&l| l != old) {
            new = l;
            break;
        }
    }
    assert_ne!(new, old, "the majority elects another leader");
    c.ticks(5);
    c.propose(new, b"other").unwrap();
    c.settle();
    (old, term)
}

#[test]
fn a_deposed_leader_hands_out_the_entries_the_new_leader_overwrote() {
    let mut c = Cluster::new(3);
    let (old, term) = diverge(&mut c);
    assert!(std::mem::take(&mut c.nodes.get_mut(&old).unwrap().rolled).is_empty(), "not yet");
    c.heal();
    c.ticks(20);
    let rolled = std::mem::take(&mut c.nodes.get_mut(&old).unwrap().rolled);
    assert_eq!(payloads(&rolled), vec![b"lost-1".to_vec(), b"lost-2".to_vec()]);
    assert!(rolled.iter().all(|(_, e)| e.term == term));
    assert!(rolled.windows(2).all(|w| w[1].1.index == w[0].1.index + 1), "in log order");
    // Handed out once; the others dropped nothing.
    assert!(std::mem::take(&mut c.nodes.get_mut(&old).unwrap().rolled).is_empty());
    for n in c.others(old) {
        assert!(std::mem::take(&mut c.nodes.get_mut(&n).unwrap().rolled).is_empty(), "{n:?}");
    }
    // The old leader now holds the new leader's log.
    let l = c.leader().unwrap();
    assert_eq!(c.sm(old).data, c.sm(l).data);
    assert!(!c.nodes[&old].core.rollback_pending());
}

#[test]
fn a_restarted_leader_hands_out_what_its_disk_kept_and_the_new_leader_overwrote() {
    let mut c = Cluster::new(3);
    let (old, _) = diverge(&mut c);
    c.restart(old);
    let node = c.nodes.get_mut(&old).unwrap();
    let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
    node.core.create_group(desc, Appender::default()).unwrap();
    c.heal();
    c.ticks(20);
    let rolled = std::mem::take(&mut c.nodes.get_mut(&old).unwrap().rolled);
    assert_eq!(payloads(&rolled), vec![b"lost-1".to_vec(), b"lost-2".to_vec()]);
}

#[test]
fn a_leader_stopped_cleanly_keeps_the_entries_that_waited_behind_a_batch_in_flight() {
    let mut c = Cluster::new(3);
    let old = c.elect();
    c.ticks(5);
    c.propose(old, b"kept").unwrap();
    c.settle();
    // The commit of `kept` waits in the open batch, which goes to disk within 1 ms (H-008 of
    // the native tests): a tick lets it go, as the core loop does at its next deadline.
    c.ticks(1);
    c.isolate(old);
    assert!(c.nodes[&old].core.writes_settled());
    // Its disk is slow: the first entry's batch goes out and stays in flight, and the second
    // waits for it. Both are in the leader's log, which is all a `w: 1` write asks.
    c.slow.insert(old);
    c.propose(old, b"lost-1").unwrap();
    c.settle();
    let node = c.nodes.get_mut(&old).unwrap();
    let in_flight = node.core.poll_io(c.now).expect("the first entry's batch");
    c.propose(old, b"lost-2").unwrap();
    c.settle();
    let node = c.nodes.get_mut(&old).unwrap();
    assert!(!node.core.writes_settled(), "an entry waits behind the batch in flight");
    // The clean stop: the core runs its disk operations until its writes settle, and only then
    // goes. Each operation's completion may hand out the next one.
    let (now, mut next) = (c.now, Some(in_flight));
    while let Some(op) = next {
        let d = node.disk.execute(op);
        node.core.on_io_done(d, now);
        next = node.core.poll_io(now);
    }
    assert!(node.core.writes_settled());
    c.slow.clear();
    c.restart(old);
    let node = c.nodes.get_mut(&old).unwrap();
    let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
    node.core.create_group(desc, Appender::default()).unwrap();
    // The others elect a leader of a later term that writes over both: both are rolled back.
    let mut new = old;
    for _ in 0..1_000 {
        c.tick();
        if let Some(l) = c.leader().filter(|&l| l != old) {
            new = l;
            break;
        }
    }
    assert_ne!(new, old);
    c.ticks(5);
    c.propose(new, b"other").unwrap();
    c.settle();
    c.heal();
    c.ticks(20);
    let rolled = std::mem::take(&mut c.nodes.get_mut(&old).unwrap().rolled);
    assert_eq!(payloads(&rolled), vec![b"lost-1".to_vec(), b"lost-2".to_vec()]);
}

#[test]
fn a_cluster_without_failures_rolls_nothing_back() {
    let mut c = Cluster::new(3);
    let l = c.elect();
    c.ticks(5);
    for p in [b"a", b"b", b"c"] {
        c.propose(l, p).unwrap();
    }
    c.ticks(10);
    for n in [1, 2, 3] {
        assert!(std::mem::take(&mut c.nodes.get_mut(&id(n)).unwrap().rolled).is_empty());
    }
}

/// Like [`diverge`], but the isolated leader takes `lost` entries and the new leader commits
/// `more` entries with snapshots every 3, so that its log no longer reaches the old leader's
/// entries and the old leader catches up with a snapshot.
fn diverge_past_snapshots(c: &mut Cluster, lost: u8, more: u8) -> (NodeId, u64) {
    let old = c.elect();
    c.ticks(5);
    c.propose(old, b"kept").unwrap();
    c.settle();
    c.isolate(old);
    let term = c.nodes[&old].core.status(G).unwrap().hard_state.term;
    for i in 0..lost {
        c.propose(old, &[b'L', i]).unwrap();
    }
    c.settle();
    let mut new = old;
    for _ in 0..1_000 {
        c.tick();
        if let Some(l) = c.leader().filter(|&l| l != old) {
            new = l;
            break;
        }
    }
    assert_ne!(new, old, "the majority elects another leader");
    c.ticks(5);
    for i in 0..more {
        c.propose(new, &[b'N', i]).unwrap();
        c.ticks(1);
    }
    c.settle();
    (old, term)
}

fn small(i: u64) -> teseladb_multiraft::CoreConfig {
    teseladb_multiraft::CoreConfig { snapshot_entries: 3, ..cfg(i) }
}

#[test]
fn a_deposed_leader_that_installs_a_snapshot_hands_out_the_entries_it_covered() {
    let mut c = Cluster::with(3, small);
    let (old, term) = diverge_past_snapshots(&mut c, 2, 12);
    c.heal();
    c.ticks(30);
    let rolled = std::mem::take(&mut c.nodes.get_mut(&old).unwrap().rolled);
    assert_eq!(payloads(&rolled), vec![b"L\x00".to_vec(), b"L\x01".to_vec()]);
    assert!(rolled.iter().all(|(_, e)| e.term == term));
    let l = c.leader().unwrap();
    assert_eq!(c.sm(old).data, c.sm(l).data, "it caught up with the snapshot");
    for n in c.others(old) {
        assert!(std::mem::take(&mut c.nodes.get_mut(&n).unwrap().rolled).is_empty(), "{n:?}");
    }
}

#[test]
fn entries_below_and_above_the_installed_snapshot_are_each_handed_out_once_in_order() {
    let mut c = Cluster::with(3, small);
    // Six lost entries; the new leader's snapshot covers the first ones and its later entries
    // overwrite the rest.
    let (old, _) = diverge_past_snapshots(&mut c, 6, 5);
    c.heal();
    c.ticks(30);
    let l = c.leader().unwrap();
    for i in 0..6u8 {
        c.propose(l, &[b'M', i]).unwrap();
        c.ticks(1);
    }
    c.ticks(30);
    let rolled = std::mem::take(&mut c.nodes.get_mut(&old).unwrap().rolled);
    let want: Vec<Vec<u8>> = (0..6u8).map(|i| vec![b'L', i]).collect();
    assert_eq!(payloads(&rolled), want);
    assert!(rolled.windows(2).all(|w| w[1].1.index == w[0].1.index + 1), "in log order");
    assert_eq!(c.sm(old).data, c.sm(c.leader().unwrap()).data);
}

#[test]
fn a_lagging_follower_that_installs_a_snapshot_rolls_nothing_back() {
    let mut c = Cluster::with(3, small);
    let l = c.elect();
    c.ticks(5);
    let f = c.others(l)[0];
    c.propose(l, b"a").unwrap();
    c.settle();
    c.isolate(f);
    for i in 0..12u8 {
        c.propose(l, &[b'b', i]).unwrap();
        c.ticks(1);
    }
    c.heal();
    c.ticks(30);
    assert_eq!(c.sm(f).data, c.sm(l).data, "it caught up with the snapshot");
    for n in [1, 2, 3] {
        assert!(std::mem::take(&mut c.nodes.get_mut(&id(n)).unwrap().rolled).is_empty());
    }
}

#[test]
fn the_terms_below_a_snapshot_survive_a_restart_of_the_node_that_sends_it() {
    let mut c = Cluster::with(3, small);
    let (old, _) = diverge_past_snapshots(&mut c, 2, 12);
    // Every node of the majority restarts from its snapshot file before the old leader returns.
    for n in c.others(old) {
        c.restart(n);
        let node = c.nodes.get_mut(&n).unwrap();
        let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
        node.core.create_group(desc, Appender::default()).unwrap();
    }
    let mut l = old;
    for _ in 0..1_000 {
        c.tick();
        if let Some(n) = c.leader().filter(|&n| n != old) {
            l = n;
            break;
        }
    }
    assert_ne!(l, old, "the majority elects a leader again");
    c.propose(l, b"after").unwrap();
    c.ticks(10);
    c.heal();
    c.ticks(50);
    let rolled = std::mem::take(&mut c.nodes.get_mut(&old).unwrap().rolled);
    assert_eq!(payloads(&rolled), vec![b"L\x00".to_vec(), b"L\x01".to_vec()]);
    assert_eq!(c.sm(old).data, c.sm(c.leader().unwrap()).data);
}

#[test]
fn the_log_waits_for_the_rollback_files_before_it_overwrites_the_entries() {
    // Seed 4838 of range-docs heavy at 200 %: the new leader's entries replaced a `w: 1` write on
    // disk while its rollback file waited in RAM on a full disk; a crash then lost both.
    let mut c = Cluster::new(3);
    let (old, _) = diverge(&mut c);
    c.rollback_unwritten.insert(old);
    c.heal();
    c.ticks(20);
    let l = c.leader().unwrap();
    assert_ne!(c.sm(old).data, c.sm(l).data, "its log still holds what it would drop");
    let node = c.nodes.get_mut(&old).unwrap();
    assert_eq!(node.core.poll_io(c.now), None, "no disk operation before the files");
    assert!(node.core.rollback_pending() && node.core.next_deadline().is_none());
    let rolled = node.core.take_rolled_back();
    assert_eq!(payloads(&rolled), vec![b"lost-1".to_vec(), b"lost-2".to_vec()]);
    assert_eq!(node.core.poll_io(c.now), None, "handed out is not durable yet");
    // A restart before the files are durable finds the entries still in its log.
    c.restart(old);
    let node = c.nodes.get_mut(&old).unwrap();
    let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
    node.core.create_group(desc, Appender::default()).unwrap();
    c.rollback_unwritten.clear();
    c.ticks(20);
    let rolled = std::mem::take(&mut c.nodes.get_mut(&old).unwrap().rolled);
    assert_eq!(payloads(&rolled), vec![b"lost-1".to_vec(), b"lost-2".to_vec()]);
    let l = c.leader().unwrap();
    assert_eq!(c.sm(old).data, c.sm(l).data);
    assert!(!c.nodes[&old].core.rollback_pending());
}

fn has_append_req(_: NodeId, _: NodeId, f: &PeerFrame) -> bool {
    matches!(f, PeerFrame::Raft { msgs, .. } if msgs.iter().any(|(_, m)| m.ty == MessageType::AppendReq))
}

/// A leader whose first entry of the term (empty) never reached anyone: the next leader replaces
/// it, by overwriting it or with a snapshot past it, and nothing is handed out, since it carried
/// no write.
#[test]
fn a_deposed_leaders_empty_entry_of_its_term_is_not_handed_out() {
    for (mk, writes) in [(cfg as fn(u64) -> teseladb_multiraft::CoreConfig, 1), (small, 12)] {
        let mut c = Cluster::with(3, mk);
        c.drop = Some(has_append_req);
        let old = loop {
            c.tick();
            if let Some(l) = c.leader() {
                break l;
            }
        };
        c.settle();
        assert!(c.nodes[&old].core.status(G).unwrap().persisted >= 1, "on its disk");
        c.isolate(old);
        c.drop = None;
        let mut new = old;
        for _ in 0..1_000 {
            c.tick();
            if let Some(l) = c.leader().filter(|&l| l != old) {
                new = l;
                break;
            }
        }
        assert_ne!(new, old, "the majority elects another leader");
        for i in 0..writes {
            c.propose(new, &[b'N', i]).unwrap();
            c.ticks(1);
        }
        c.settle();
        c.heal();
        c.ticks(30);
        assert!(std::mem::take(&mut c.nodes.get_mut(&old).unwrap().rolled).is_empty());
        assert_eq!(c.sm(old).data, c.sm(new).data);
    }
}
