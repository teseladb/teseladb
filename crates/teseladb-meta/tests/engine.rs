//! Group 0 inside `MultiRaftCore`: the founder bootstraps the cluster and creates a range on a
//! single replica; a restart rebuilds the same state from the log.

use teseladb_meta::command::{Group0Command, NodeAddrs};
use teseladb_meta::group0::{GROUP0, Group0Result, Group0Sm};
use teseladb_multiraft::{CoreConfig, Done, GroupDesc, MultiRaftCore};
use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

fn settle(core: &mut MultiRaftCore<Group0Sm>, disk: &mut MemDisk) -> Vec<Group0Result> {
    let mut out = Vec::new();
    for _ in 0..100 {
        while let Some(op) = core.poll_io(Nanos::ZERO) {
            let d = disk.execute(op);
            core.on_io_done(d, Nanos::ZERO);
        }
        let a = core.drain_actions();
        if a.done.is_empty() {
            return out;
        }
        for (_, r) in a.done {
            match r {
                Ok(Done::Applied(x)) => out.push(x),
                other => panic!("{other:?}"),
            }
        }
    }
    out
}

#[test]
fn group0_runs_on_the_engine_and_survives_a_restart() {
    let me = NodeId(1);
    let mut disk = MemDisk::new();
    let store = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap().0;
    let mut core = MultiRaftCore::new(CoreConfig::new(me, 0), store, 1);
    let desc = GroupDesc { group: GROUP0, voters: vec![me], learners: vec![] };
    core.create_group(desc, Group0Sm::default()).unwrap();
    settle(&mut core, &mut disk);
    let boot = Group0Command::Bootstrap {
        cluster_id: [3; 16],
        first_node: NodeAddrs { name: b"n1".to_vec(), cores: 2, ..NodeAddrs::default() },
        cert_fingerprint: [0; 32],
    };
    let range = Group0Command::CreateRange { start: vec![], end: vec![], replicas: vec![me] };
    core.propose(GROUP0, &boot.encode(), Nanos::ZERO).unwrap();
    core.propose(GROUP0, &range.encode(), Nanos::ZERO).unwrap();
    let out = settle(&mut core, &mut disk);
    assert_eq!(out, vec![Group0Result::Node(me), Group0Result::Range(GroupId(1))]);
    let before = core.state_machine(GROUP0).unwrap().clone();
    // Restart: the log replays the same entries into a new state machine.
    let store = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap().0;
    let mut core = MultiRaftCore::new(CoreConfig::new(me, 0), store, 2);
    assert_eq!(core.recovered_groups(), vec![GROUP0]);
    let desc = GroupDesc { group: GROUP0, voters: vec![], learners: vec![] };
    core.create_group(desc, Group0Sm::default()).unwrap();
    settle(&mut core, &mut disk);
    let after = core.state_machine(GROUP0).unwrap();
    assert_eq!((&after.nodes, &after.ranges), (&before.nodes, &before.ranges));
}
