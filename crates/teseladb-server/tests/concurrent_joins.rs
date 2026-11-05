//! Several nodes join at once (001 T042, adverse cases): the configuration changes go one at a
//! time, so some joins wait and try again; all of them end with distinct identifiers and Group 0
//! with five voters and the rest learners.

use std::thread;
use std::time::{Duration, Instant};

use teseladb_server::logic::GROUP0_MAX_VOTERS;
use teseladb_server::node::{Node, NodeConfig};
use teseladb_raftlog::MemDisk;
use teseladb_types::NodeId;

const JOINERS: usize = 6;

fn config(join: Vec<String>) -> NodeConfig {
    NodeConfig {
        cores: Some(1),
        join,
        join_deadline: Duration::from_secs(30),
        ..NodeConfig::default()
    }
}

#[test]
fn six_nodes_joining_at_once_all_get_in() {
    let founder = Node::start(config(Vec::new()), || Ok(MemDisk::new()), None).unwrap();
    assert!(founder.wait_ready(Duration::from_secs(10)));
    let seed = founder.peer_addr().to_string();
    let joiners: Vec<_> = (0..JOINERS)
        .map(|_| {
            let seeds = vec![seed.clone()];
            thread::spawn(move || Node::start(config(seeds), || Ok(MemDisk::new()), None))
        })
        .collect();
    let nodes: Vec<Node<MemDisk>> =
        joiners.into_iter().map(|j| j.join().unwrap().unwrap()).collect();
    let mut ids: Vec<NodeId> = nodes.iter().map(|n| n.node_id()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), JOINERS, "distinct identifiers: {ids:?}");
    assert!(nodes.iter().all(|n| n.wait_ready(Duration::from_secs(10))));

    let end = Instant::now() + Duration::from_secs(20);
    let settled = loop {
        let v = founder.view(Duration::from_secs(1)).unwrap();
        let done = v.nodes.len() == JOINERS + 1
            && v.voters.len() == GROUP0_MAX_VOTERS
            && v.learners.len() == JOINERS + 1 - GROUP0_MAX_VOTERS;
        if done || Instant::now() >= end {
            break v;
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(settled.nodes.len(), JOINERS + 1, "{settled:?}");
    assert_eq!(settled.voters.len(), GROUP0_MAX_VOTERS, "{settled:?}");
    assert_eq!(settled.learners.len(), JOINERS + 1 - GROUP0_MAX_VOTERS, "{settled:?}");
    for n in nodes {
        n.stop();
    }
    founder.stop();
}
