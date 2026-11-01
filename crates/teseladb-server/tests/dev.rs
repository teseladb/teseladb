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

/// A node after the first one that cannot start stops the ones already started and fails the
/// cluster with its error: node 2 of a cluster on base port P takes P + 1, which another process
/// holds, while the first node alone starts on P.
#[test]
fn a_node_that_cannot_start_stops_the_cluster() {
    let (base, busy) = two_ports();
    let o = |nodes| DevOpts { nodes, cores: 1, port: base, test_commands: false };
    let alone = dev::start(&o(1), Some(3)).expect("the first node alone starts on P");
    alone.into_iter().for_each(|n| drop(n.stop()));
    match dev::start(&o(2), Some(3)) {
        Err(teseladb_server::node::NodeError::Io(e)) => {
            assert_eq!(e.kind(), std::io::ErrorKind::AddrInUse, "{e}");
        }
        Err(e) => panic!("{e}"),
        Ok(_) => panic!("node 2 started on a port in use"),
    }
    // The first node let its port go.
    drop(std::net::TcpListener::bind(("127.0.0.1", base)).expect("P is free again"));
    drop(busy);
}

/// A free port P and a listener on P + 1. The system hands out its ports in order, so a port
/// next to one it just gave may be another test's: both are held until P is needed.
fn two_ports() -> (u16, std::net::TcpListener) {
    use std::net::TcpListener;
    for _ in 0..100 {
        let first = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = first.local_addr().unwrap().port();
        let Some(next) = port.checked_add(1) else { continue };
        if let Ok(busy) = TcpListener::bind(("127.0.0.1", next)) {
            return (port, busy);
        }
    }
    panic!("no two free ports in a row on loopback");
}

/// A first node that does not create the cluster in time is stopped and the cluster fails.
#[test]
fn a_first_node_not_ready_in_time_fails_the_cluster() {
    let o = DevOpts { nodes: 2, cores: 1, port: 0, test_commands: false };
    match dev::start_within(&o, Some(3), Duration::ZERO) {
        Err(e) => assert_eq!(e.to_string(), "the first node did not create the cluster"),
        Ok(_) => panic!("ready in no time"),
    }
}

/// `teseladb dev` whose first node cannot start ends with 1 and says why, before any ready line.
#[test]
fn the_development_cluster_that_cannot_start_ends_with_1() {
    let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = busy.local_addr().unwrap().port();
    let o = DevOpts { nodes: 1, cores: 1, port, test_commands: false };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    assert_eq!(dev::run(&o, &mut out, &mut err, &|| true), 1);
    assert!(out.is_empty());
    let err = String::from_utf8(err).unwrap();
    assert!(err.starts_with("teseladb: ") && err.ends_with('\n'), "{err}");
}
