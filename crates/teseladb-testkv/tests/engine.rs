//! `TestKv` inside `MultiRaftCore`: a single replica applies a `put` and a `cas` and answers with
//! their outputs.

use teseladb_multiraft::{CoreConfig, Done, GroupDesc, MultiRaftCore, OpId};
use teseladb_testkv::{Command, KvOutput, TestKv};
use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};
use teseladb_types::{GroupId, Nanos, NodeId};

fn run(core: &mut MultiRaftCore<TestKv>, disk: &mut MemDisk, out: &mut Vec<(OpId, KvOutput)>) {
    for _ in 0..100 {
        while let Some(op) = core.poll_io(Nanos::ZERO) {
            let d = disk.execute(op);
            core.on_io_done(d, Nanos::ZERO);
        }
        let a = core.drain_actions();
        if a.done.is_empty() {
            return;
        }
        for (op, r) in a.done {
            match r {
                Ok(Done::Applied(o)) => out.push((op, o)),
                other => panic!("{other:?}"),
            }
        }
    }
}

#[test]
fn a_single_replica_applies_kv_commands() {
    let g = GroupId(1);
    let me = NodeId(1);
    let mut disk = MemDisk::new();
    let store = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap().0;
    let mut core = MultiRaftCore::new(CoreConfig::new(me, 0), store, 1);
    core.create_group(
        GroupDesc { group: g, voters: vec![me], learners: vec![] },
        TestKv::default(),
    )
    .unwrap();
    let mut out = Vec::new();
    run(&mut core, &mut disk, &mut out);
    let put = Command::Put { key: b"k".to_vec(), value: b"v".to_vec() };
    let a = core.propose(g, &put.encode(), Nanos::ZERO).unwrap();
    let cas = Command::Cas { key: b"k".to_vec(), expected: Some(b"v".to_vec()), new: None };
    let b = core.propose(g, &cas.encode(), Nanos::ZERO).unwrap();
    run(&mut core, &mut disk, &mut out);
    assert_eq!(
        out,
        vec![(a, KvOutput::Previous(None)), (b, KvOutput::Swapped(true, Some(b"v".to_vec())))]
    );
    assert!(core.state_machine(g).unwrap().is_empty());
}
