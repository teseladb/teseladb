//! A configuration change a replica applied is its configuration after a restart (005 FR-040).
//! The commit index goes to disk with the next batch that is synced, never with a sync of its
//! own (H-008): a group that went quiet after promoting a learner kept the promotion above the
//! commit its replicas had on disk. Restarted, they opened with the configuration of before,
//! where the old leader and one of them were the voters: with the old leader gone the voter
//! left had no majority, and the group had no leader until the old one came back (test
//! `rollback` of the server, four copies at once: 6 and 11 of 24 runs never took a write).

mod common;

use common::*;
use teseladb_multiraft::{Done, GroupDesc};
use teseladb_raft::{ConfChange, ConfChangeKind, Role};
use teseladb_types::NodeId;

#[test]
fn the_replicas_left_elect_a_leader_when_they_restart_right_after_a_promotion() {
    let mut c = Cluster::new(2);
    let l = c.elect();
    c.propose(l, b"a").unwrap();
    c.ticks(5);
    // Node 3 joins as a learner and is promoted: three voters.
    c.add_node(3, cfg);
    let desc = GroupDesc { group: G, voters: vec![id(1), id(2)], learners: vec![id(3)] };
    c.nodes.get_mut(&id(3)).unwrap().core.create_group(desc, Appender::default()).unwrap();
    for kind in [ConfChangeKind::AddLearner, ConfChangeKind::PromoteLearner] {
        let now = c.now;
        let change = ConfChange { kind, node: id(3) };
        let op = c.nodes.get_mut(&l).unwrap().core.propose_conf_change(G, change, now).unwrap();
        c.ticks(20);
        let r = c.results(l, op);
        assert!(matches!(&r[..], [Ok(Done::ConfChanged(_))]), "{kind:?}: {r:?}");
    }
    for n in 1..=3 {
        let conf = c.nodes[&id(n)].core.conf(G).expect("the group's configuration");
        assert_eq!(conf.voters.len(), 3, "node {n} applied the promotion: {conf:?}");
    }
    // Nothing else is written. The leader goes and the other two restart.
    let rest = c.others(l);
    c.isolate(l);
    for &n in &rest {
        c.restart(n);
        let node = c.nodes.get_mut(&n).unwrap();
        assert_eq!(node.core.recovered_groups(), vec![G]);
        let desc = GroupDesc { group: G, voters: vec![], learners: vec![] };
        node.core.create_group(desc, Appender::default()).unwrap();
    }
    c.ticks(300);
    let leads = |n: &NodeId| c.nodes[n].core.status(G).is_some_and(|s| s.role == Role::Leader);
    let roles: Vec<_> = rest.iter().map(|n| c.nodes[n].core.status(G).map(|s| s.role)).collect();
    assert!(rest.iter().any(leads), "no leader among the two that are left: {roles:?}");
}
