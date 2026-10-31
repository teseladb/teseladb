//! `teseladb dev --nodes 3` (001 FR-018, SC-007): Group 0 has a leader in less than 1 s, the
//! three nodes join the cluster and the learners become voters.

use std::thread;
use std::time::{Duration, Instant};

use teseladb_raft::Role;
use teseladb_server::cli::DevOpts;
use teseladb_server::dev;
use teseladb_server::logic::Group0View;
use teseladb_types::NodeId;

const ASK: Duration = Duration::from_secs(1);

fn views(nodes: &[teseladb_server::node::Node<teseladb_raftlog::MemDisk>]) -> Vec<Group0View> {
    nodes.iter().map(|n| n.view(ASK).expect("core 0 answers")).collect()
}

/// Waits until `ok` holds for the views of every node.
fn wait_until(
    nodes: &[teseladb_server::node::Node<teseladb_raftlog::MemDisk>],
    timeout: Duration,
    ok: impl Fn(&[Group0View]) -> bool,
) -> Vec<Group0View> {
    let end = Instant::now() + timeout;
    loop {
        let v = views(nodes);
        if ok(&v) || Instant::now() >= end {
            return v;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn three_nodes_elect_a_group0_leader_in_less_than_a_second() {
    let t = Instant::now();
    let nodes = dev::start(&DevOpts { nodes: 3, cores: 1, port: 0, test_commands: false }, Some(7))
        .expect("dev cluster");
    let v = wait_until(&nodes, Duration::from_secs(1), |v| {
        v.iter().all(|x| x.bootstrapped && x.leader != NodeId(0))
            && v.iter().filter(|x| x.role == Some(Role::Leader)).count() == 1
    });
    let elected = t.elapsed();
    assert!(elected < Duration::from_secs(1), "Group 0 leader after {elected:?}: {v:?}");
    let ids: Vec<NodeId> = nodes.iter().map(|n| n.node_id()).collect();
    assert_eq!(ids, [NodeId(1), NodeId(2), NodeId(3)]);
    // Each node opens its administration port on loopback, on a port the system chose.
    for n in &nodes {
        let admin = n.admin_addr().expect("an administration port");
        assert!(admin.ip().is_loopback() && admin.port() != 0, "{admin}");
    }
    assert!(nodes.iter().all(|n| n.cluster_id() == nodes[0].cluster_id()));
    assert!(v.iter().all(|x| x.cluster_id == nodes[0].cluster_id()));

    // Every node ends as a voter of Group 0 once it has caught up.
    let v = wait_until(&nodes, Duration::from_secs(10), |v| {
        v.iter().all(|x| x.voters == ids && x.learners.is_empty() && x.nodes == ids)
    });
    assert!(v.iter().all(|x| x.voters == ids && x.learners.is_empty()), "{v:?}");
    nodes.into_iter().for_each(|n| drop(n.stop()));
}

#[test]
fn group0_survives_the_loss_of_its_leader() {
    let nodes =
        dev::start(&DevOpts { nodes: 3, cores: 2, port: 0, test_commands: false }, Some(11))
            .expect("dev cluster");
    let ids: Vec<NodeId> = nodes.iter().map(|n| n.node_id()).collect();
    let v = wait_until(&nodes, Duration::from_secs(10), |v| v.iter().all(|x| x.voters == ids));
    assert!(v.iter().all(|x| x.voters == ids), "{v:?}");
    assert!(nodes.iter().all(|n| n.cores() == 2));
    let leader = v[0].leader;
    let (gone, rest): (Vec<_>, Vec<_>) = nodes.into_iter().partition(|n| n.node_id() == leader);
    gone.into_iter().for_each(|n| drop(n.stop()));
    let v = wait_until(&rest, Duration::from_secs(5), |v| {
        v.iter().all(|x| x.leader != leader && x.leader != NodeId(0))
            && v.iter().any(|x| x.role == Some(Role::Leader))
    });
    assert!(v.iter().all(|x| x.leader != leader && x.leader != NodeId(0)), "{v:?}");
    rest.into_iter().for_each(|n| drop(n.stop()));
}

#[test]
fn one_node_is_a_cluster_of_its_own() {
    let nodes = dev::start(&DevOpts { nodes: 1, cores: 1, port: 0, test_commands: false }, None)
        .expect("dev cluster");
    let v = views(&nodes);
    assert!(v[0].bootstrapped);
    assert_eq!((v[0].role, v[0].voters.as_slice()), (Some(Role::Leader), &[NodeId(1)][..]));
    nodes.into_iter().for_each(|n| drop(n.stop()));
}
