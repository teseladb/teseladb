//! Checksums of a range (005 T023, FR-014, research R5): the leader proposes `ComputeChecksum`
//! every `CHECKSUM_EVERY_MS` of cluster time and on demand; every replica computes the SHA-256 of
//! the range's versions at the entry's mark and sends it to the leader, which marks for rebuild
//! the replicas whose sum differs from the majority's: a version only one replica holds, a byte
//! altered in one of its SST files, or the leader itself.

mod common;

use std::path::{Path, PathBuf};

use common::*;
use teseladb_raftlog::{FileId, IoOp};
use teseladb_range::checksum::{CHECKSUM_EVERY_MS, Rebuild, Report, Tally};
use teseladb_types::Bytes;

/// A byte inside the first data block of an SST file.
const DATA_BYTE: usize = 8;

fn sum(byte: u8) -> Option<[u8; 32]> {
    Some([byte; 32])
}

fn report(node: u64, index: u64, sum: Option<[u8; 32]>) -> Report {
    Report { range: G, index, at: Hlc { physical: 9, logical: 1 }, node: id(node), sum }
}

fn write(c: &mut Cluster, l: NodeId, key: &[u8], v: u8) -> Hlc {
    let w = c.mutate_with(l, &[key], Box::new(SetKey(key.to_vec(), v)), None);
    c.settle();
    match c.reply(l, w) {
        Some(Ok(Reply::Mutated { ts, .. })) => ts,
        other => panic!("the write failed: {other:?}"),
    }
}

/// A cluster with a few documents written through the leader.
fn with_documents() -> (Cluster, NodeId) {
    let mut c = Cluster::standard();
    let l = c.elect();
    for (i, key) in [b"\x01b", b"\x01c", b"\x01d"].iter().enumerate() {
        write(&mut c, l, *key, i as u8);
    }
    (c, l)
}

/// Asks the leader for a checksum now and delivers every replica's to it.
fn check_now(c: &mut Cluster, l: NodeId) -> Vec<Report> {
    let node = c.nodes.get_mut(&l).unwrap();
    node.host.check(&node.core, G).unwrap();
    c.ticks_following(1);
    c.route_checksums()
}

/// Flushes node `n`'s memtable into an SST file.
fn flush(c: &mut Cluster, n: NodeId) {
    let now = c.now;
    let node = c.nodes.get_mut(&n).unwrap();
    let lsm = node.host.lsm().clone();
    lsm.borrow_mut().flush();
    lsm.borrow_mut().run_io(&mut node.disk, now);
}

/// Flips a byte of the first data block of every SST file of node `n` (their index and filter
/// blocks stay in memory while the file is open).
fn corrupt_ssts(c: &mut Cluster, n: NodeId) {
    let disk = &mut c.nodes.get_mut(&n).unwrap().disk;
    let ssts: Vec<PathBuf> = disk
        .list(Path::new("core-00/lsm"))
        .unwrap()
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "sst"))
        .collect();
    assert!(!ssts.is_empty(), "the flush wrote an SST file");
    for (i, path) in ssts.into_iter().enumerate() {
        let full = Path::new("core-00/lsm").join(path.file_name().unwrap());
        let bytes = disk.read_all(&full).unwrap();
        let at = DATA_BYTE.min(bytes.len() - 1);
        let file = FileId(u32::MAX - i as u32);
        disk.execute(IoOp::Open { file, path: full });
        let data = Bytes::from_vec(vec![bytes[at] ^ 0xff]);
        disk.execute(IoOp::WriteAt { file, offset: at as u64, data, batch: 0 });
        disk.execute(IoOp::Close { file });
    }
}

#[test]
fn a_tally_marks_whoever_differs_from_the_majority() {
    let voters = vec![id(1), id(2), id(3)];
    let mut t = Tally::new(G, 5, voters);
    assert_eq!(t.add(&report(1, 5, sum(1))), vec![]);
    assert_eq!(t.add(&report(2, 4, sum(2))), vec![], "a report of another checksum is ignored");
    assert_eq!(t.add(&report(2, 5, sum(2))), vec![], "no majority yet");
    let marked = t.add(&report(3, 5, sum(1)));
    assert_eq!(marked, vec![Rebuild { range: G, node: id(2), index: 5 }]);
    assert_eq!(t.add(&report(2, 5, sum(2))), vec![], "a replica is marked once");
    // A replica that could not read its data differs as well.
    let mut t = Tally::new(G, 6, vec![id(1), id(2), id(3)]);
    t.add(&report(1, 6, sum(1)));
    t.add(&report(2, 6, sum(1)));
    assert_eq!(t.add(&report(3, 6, None)), vec![Rebuild { range: G, node: id(3), index: 6 }]);
    // Three different sums: no majority, nobody is marked.
    let mut t = Tally::new(G, 7, vec![id(1), id(2), id(3)]);
    for (n, s) in [(1, 1), (2, 2), (3, 3)] {
        assert_eq!(t.add(&report(n, 7, sum(s))), vec![]);
    }
}

#[test]
fn a_report_goes_through_its_frame() {
    for r in [report(3, 77, sum(4)), report(1, 2, None)] {
        assert_eq!(Report::decode(&r.encode()), Ok(r));
    }
    let mut bytes = report(3, 77, sum(4)).encode();
    bytes.push(0);
    assert!(Report::decode(&bytes).is_err(), "trailing bytes");
    assert!(Report::decode(&[1, 2]).is_err(), "too short");
}

#[test]
fn equal_replicas_report_the_same_sum_every_ten_minutes() {
    let (mut c, l) = with_documents();
    c.wall_ahead = CHECKSUM_EVERY_MS / 2;
    c.ticks_following(JUMP_TICKS);
    assert_eq!(c.route_checksums(), vec![], "no checksum before its time");
    c.wall_ahead = CHECKSUM_EVERY_MS + CHECKSUM_EVERY_MS / 2;
    c.ticks_following(JUMP_TICKS);
    let reports = c.route_checksums();
    assert_eq!(reports.len(), 3, "one per replica: {reports:?}");
    assert!(reports.iter().all(|r| r.sum.is_some() && r.sum == reports[0].sum));
    assert!(reports.iter().all(|r| r.index == reports[0].index && r.at == reports[0].at));
    assert_eq!(c.nodes.get_mut(&l).unwrap().host.take_rebuilds(), vec![]);
    // The next round comes ten minutes later, with a sum of its own entry.
    c.wall_ahead += CHECKSUM_EVERY_MS;
    c.ticks_following(JUMP_TICKS);
    let next = c.route_checksums();
    assert_eq!(next.len(), 3);
    assert!(next[0].index > reports[0].index);
}

#[test]
fn a_new_document_changes_the_sum() {
    let (mut c, l) = with_documents();
    let before = check_now(&mut c, l);
    write(&mut c, l, b"\x01e", 9);
    let after = check_now(&mut c, l);
    assert_eq!(after.len(), 3);
    assert_ne!(before[0].sum, after[0].sum);
    assert!(after.iter().all(|r| r.sum == after[0].sum));
}

#[test]
fn only_the_leader_takes_a_checksum_on_demand() {
    let (mut c, l) = with_documents();
    let f = c.others(l)[0];
    let node = c.nodes.get_mut(&f).unwrap();
    assert_eq!(node.host.check(&node.core, G), Err(RangeError::NotLeader { hint: l }));
    assert_eq!(node.host.check(&node.core, GroupId(99)), Err(RangeError::OutOfRange));
    assert!(!node.host.on_checksum_report(&report(1, 1, sum(1))), "a follower keeps no tally");
    assert_eq!(check_now(&mut c, l).len(), 3);
}

#[test]
fn a_replica_with_a_version_of_its_own_is_marked_for_rebuild() {
    let (mut c, l) = with_documents();
    let odd = c.others(l)[1];
    // A version only `odd` holds, below the mark of the next checksum.
    let node = c.nodes.get_mut(&odd).unwrap();
    let applied = node.core.status(G).unwrap().applied;
    let mark = node.core.state_machine(G).unwrap().last_mark();
    let lsm = node.host.lsm().clone();
    lsm.borrow_mut().put(G, applied, b"\x01ghost", mark, &[1, 7]).unwrap();
    let reports = check_now(&mut c, l);
    assert_eq!(reports.len(), 3);
    let index = reports[0].index;
    let rebuilds = c.nodes.get_mut(&l).unwrap().host.take_rebuilds();
    assert_eq!(rebuilds, vec![Rebuild { range: G, node: odd, index }]);
}

#[test]
fn a_byte_altered_in_an_sst_marks_its_replica_for_rebuild() {
    let (mut c, l) = with_documents();
    for n in c.nodes.keys().copied().collect::<Vec<_>>() {
        flush(&mut c, n);
    }
    let odd = c.others(l)[0];
    corrupt_ssts(&mut c, odd);
    check_now(&mut c, l);
    let rebuilds = c.nodes.get_mut(&l).unwrap().host.take_rebuilds();
    assert_eq!(rebuilds.len(), 1, "{rebuilds:?}");
    assert_eq!(rebuilds[0].node, odd);
}

#[test]
fn a_divergent_leader_marks_itself() {
    let (mut c, l) = with_documents();
    let node = c.nodes.get_mut(&l).unwrap();
    let applied = node.core.status(G).unwrap().applied;
    let mark = node.core.state_machine(G).unwrap().last_mark();
    let lsm = node.host.lsm().clone();
    lsm.borrow_mut().put(G, applied, b"\x01ghost", mark, &[1, 7]).unwrap();
    check_now(&mut c, l);
    let rebuilds = c.nodes.get_mut(&l).unwrap().host.take_rebuilds();
    assert_eq!(rebuilds.len(), 1);
    assert_eq!(rebuilds[0].node, l);
}

#[test]
fn a_tally_ignores_a_report_of_another_range() {
    let mut t = Tally::new(G, 5, vec![id(1), id(2), id(3)]);
    t.add(&report(1, 5, sum(1)));
    let other = Report { range: GroupId(99), ..report(2, 5, sum(1)) };
    assert_eq!(t.add(&other), vec![]);
    assert_eq!(t.add(&report(3, 5, sum(2))), vec![], "the other range's sum did not count");
}

#[test]
fn a_sum_of_the_wrong_length_is_refused() {
    let full = report(3, 77, sum(4)).encode();
    // The sum is last: its length (one byte) and its 32 bytes, replaced by 5 bytes.
    let bytes = [&full[..full.len() - 33], &[5, 1, 2, 3, 4, 5]].concat();
    assert!(Report::decode(&bytes).is_err());
}

#[test]
fn asking_again_while_a_checksum_is_proposed_changes_nothing() {
    let (mut c, l) = with_documents();
    let (now, wall) = (c.now, c.wall());
    let node = c.nodes.get_mut(&l).unwrap();
    node.host.check(&node.core, G).unwrap();
    // The leader proposes it; nothing is delivered yet.
    node.host.poll(&mut node.core, now, wall);
    assert_eq!(node.host.check(&node.core, G), Ok(()));
    let reports = c.route_checksums();
    assert_eq!(reports.len(), 3, "one checksum: {reports:?}");
    assert!(reports.iter().all(|r| r.index == reports[0].index));
}

/// Writes `n` documents in one entry.
struct Many(usize);

impl Mutation for Many {
    fn eval(&self, _: &[Option<(u8, &[u8])>], _: Hlc) -> Result<Outcome, MutationError> {
        let puts = (0..self.0)
            .map(|i| Put {
                key: [&b"\x01m"[..], &(i as u32).to_be_bytes()].concat(),
                value_type: 1,
                value: vec![1],
            })
            .collect();
        Ok(Outcome { puts, deletes: vec![], change: vec![], result: vec![] })
    }
}

#[test]
fn a_sum_longer_than_a_step_goes_on_over_several_polls() {
    let (mut c, l) = with_documents();
    let n = teseladb_range::checksum::SUM_STEP_VERSIONS + 100;
    let w = c.mutate_with(l, &[b"\x01m"], Box::new(Many(n)), None);
    c.settle();
    assert!(matches!(c.reply(l, w), Some(Ok(_))));
    let reports = check_now(&mut c, l);
    assert_eq!(reports.len(), 3);
    assert!(reports.iter().all(|r| r.sum.is_some() && r.sum == reports[0].sum));
}

#[test]
fn a_range_removed_from_its_host_answers_its_clients_and_drops_its_own_work() {
    let (mut c, l) = with_documents();
    // A write that waits for a majority it cannot reach, and a checksum of the leader's own.
    c.isolate(l);
    let w = c.mutate(l, KEY, 7, MAJORITY);
    // A linearizable read that waits for the leader to confirm it still leads.
    let r = c.get(l, KEY, ReadConcern::Linearizable);
    let node = c.nodes.get_mut(&l).unwrap();
    node.host.check(&node.core, G).unwrap();
    c.ticks(2);
    let node = c.nodes.get_mut(&l).unwrap();
    let _ = node.host.take_replies();
    assert!(node.host.remove_range(G));
    assert!(!node.host.remove_range(G), "already gone");
    // Only the clients hear of it. The read asks another replica; the write was proposed and
    // may still be applied (a merge removes the right range while a write waits for its
    // copies), so its client does not learn that it failed.
    let mut replies = node.host.take_replies();
    replies.sort_by_key(|(id, _)| *id);
    let mut want = vec![
        (w, Err(RangeError::Unavailable)),
        (r, Err(RangeError::NotLeader { hint: NodeId(0) })),
    ];
    want.sort_by_key(|(id, _)| *id);
    assert_eq!(replies, want);
    assert!(node.host.take_checksum_reports().is_empty() && node.host.take_rebuilds().is_empty());
    assert!(node.host.metrics(&node.core, G, c.now).is_none());
}
