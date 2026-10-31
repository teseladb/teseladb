//! 007 T010: a new range goes to the least loaded core of its replicas, the same on all of them
//! (FR-004). Group 0 counts as load on core 0 of every node, so the first data range lands on core
//! `1 mod cores` (D-045).

use teseladb_meta::bootstrap::data_range_core;
use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{Group0Result, Group0Sm};
use teseladb_types::{GroupId, NodeId};

fn node(name: &str, cores: u16) -> NodeAddrs {
    NodeAddrs {
        name: name.as_bytes().to_vec(),
        peer_addr: name.as_bytes().to_vec(),
        cores,
        ..NodeAddrs::default()
    }
}

fn cluster(cores: &[u16]) -> Group0Sm {
    let mut sm = Group0Sm::default();
    sm.apply_command(
        1,
        &Group0Command::Bootstrap {
            cluster_id: [1; 16],
            first_node: node("n1", cores[0]),
            cert_fingerprint: [0; 32],
        },
    );
    for (i, c) in cores.iter().enumerate().skip(1) {
        let add = Group0Command::AddNode {
            node: node(&format!("n{}", i + 1), *c),
            cert_fingerprint: [0; 32],
        };
        sm.apply_command(1 + i as u64, &add);
    }
    sm
}

fn create(sm: &mut Group0Sm, start: u8, replicas: &[u64]) -> u16 {
    let cmd = Group0Command::CreateRange {
        start: vec![start],
        end: vec![start + 1],
        replicas: replicas.iter().map(|n| NodeId(*n)).collect(),
    };
    let Group0Result::Range(g) = sm.apply_command(100 + u64::from(start), &cmd) else {
        panic!("range")
    };
    sm.ranges[&g].core
}

#[test]
fn the_first_data_range_lands_where_d045_puts_it() {
    for cores in [1, 2, 3, 8] {
        let mut sm = cluster(&[cores]);
        assert_eq!(create(&mut sm, 1, &[1]), data_range_core(cores), "{cores} cores");
    }
}

#[test]
fn new_ranges_fill_the_least_loaded_core_of_their_replicas() {
    let mut sm = cluster(&[4, 4, 4]);
    // A range on node 2 takes its core 1; node 1 is still empty, so its next range also goes to
    // core 1 there (a choice by range number would give core 2).
    assert_eq!(create(&mut sm, 1, &[2]), 1);
    assert_eq!(create(&mut sm, 2, &[1]), 1);
    // Cores 2 and 3 of node 1, then core 0, which also holds Group 0.
    let on_one: Vec<u16> = (3..=5).map(|s| create(&mut sm, s, &[1])).collect();
    assert_eq!(on_one, [2, 3, 0]);
    // Over nodes 1, 2 and 3 the loads of cores 0 to 3 are 4, 2, 1 and 1: core 2 wins the tie.
    assert_eq!(create(&mut sm, 6, &[1, 2, 3]), 2);
    // On nodes 2 and 3: core 3 is the only one without a range.
    assert_eq!(create(&mut sm, 7, &[2, 3]), 3);
    // A split keeps the range's core: its data stays in the same LSM.
    let g = GroupId(2);
    sm.apply_command(200, &Group0Command::SplitRange { group: g, at: vec![2, 5] });
    let Group0Result::Range(right) = sm.apply_command(
        201,
        &Group0Command::CompleteSplit { group: g, at: vec![2, 5], at_index: 9 },
    ) else {
        panic!("split")
    };
    assert_eq!(sm.ranges[&right].core, sm.ranges[&g].core);
}

#[test]
fn the_smallest_core_count_of_the_replicas_bounds_the_choice() {
    let mut sm = cluster(&[4, 2]);
    let cores: Vec<u16> = (1..=4).map(|s| create(&mut sm, s, &[1, 2])).collect();
    assert_eq!(cores, [1, 0, 1, 0]);
    // A range on core 3 of node 1 alone does not count for a choice bounded to two cores.
    assert_eq!(create(&mut sm, 5, &[1]), 2);
    assert_eq!(create(&mut sm, 6, &[1]), 3);
    assert_eq!(create(&mut sm, 7, &[1, 2]), 1);
}
