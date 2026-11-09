//! `w: 1` and its loss (005 T033, FR-010, D-034, model `WriteConcernW1.tla`): `w: 1` answers once
//! the entry is in the leader's log (after its `fdatasync` with `j: true`) without waiting for the
//! majority; if the leader loses its term before the majority has the entry, the write is lost
//! and the replica that drops it hands it to its rollback files, as RefDb writes
//! `<dbpath>/rollback/…/removed.<timestamp>.bson`.

mod common;

use common::*;
use teseladb_hlc::to_timestamp;
use teseladb_multiraft::envelope;
use teseladb_range::command::Command;
use teseladb_range::rollback::{
    BadRollbackFile, ROLLBACK_DIR, RollbackDoc, RollbackFile, read_rollback_file, rollback_files,
};
use teseladb_raftlog::LogEntry;
use teseladb_raftlog::format::wal::EntryKind;
use teseladb_types::Bytes;

const W1: WriteConcern = WriteConcern { w: W::Nodes(1), j: false };
const W1_J: WriteConcern = WriteConcern { w: W::Nodes(1), j: true };

fn mark(r: Option<Result<Reply, RangeError>>) -> Hlc {
    match r {
        Some(Ok(Reply::Mutated { ts, .. })) => ts,
        other => panic!("not an acknowledged write: {other:?}"),
    }
}

/// Ticks until one of the nodes other than `old` leads a newer term.
fn new_leader(c: &mut Cluster, old: NodeId) -> NodeId {
    for _ in 0..1_000 {
        c.ticks(1);
        let found = c
            .others(old)
            .into_iter()
            .find(|n| c.nodes[n].core.status(G).unwrap().role == Role::Leader);
        if let Some(n) = found {
            return n;
        }
    }
    panic!("no new leader");
}

#[test]
fn w_1_answers_from_the_leaders_log_before_the_majority_and_j_waits_for_its_disk() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.ticks(5);
    // No follower hears the leader, and its disk does not answer.
    c.isolate(l);
    c.slow.insert(l);
    let fast = c.mutate_key(l, b"\x01f", 1, W1);
    let journaled = c.mutate_key(l, b"\x01j", 2, W1_J);
    let majority = c.mutate_key(l, b"\x01m", 3, MAJORITY);
    c.settle();
    let st = c.nodes[&l].core.status(G).unwrap();
    assert!(st.hard_state.commit < st.last_index, "nothing new is committed: {st:?}");
    assert!(mark(c.reply(l, fast)) > Hlc { physical: 0, logical: 0 }, "in its log: answered");
    assert_eq!(c.reply(l, journaled), None, "`j: true` waits for the leader's fdatasync");
    // Its disk answers: `j: true` too; the majority still does not have them.
    c.slow.clear();
    c.settle();
    mark(c.reply(l, journaled));
    assert_eq!(c.reply(l, majority), None, "`w: \"majority\"` waits whatever `j` says");
}

#[test]
fn a_leader_that_falls_before_the_commit_loses_its_w_1_writes_to_its_rollback_files() {
    let mut c = Cluster::standard();
    let l = c.elect();
    c.ticks(5);
    let kept = c.mutate(l, KEY, 1, MAJORITY);
    c.ticks(2);
    mark(c.reply(l, kept));
    c.isolate(l);
    let term = c.nodes[&l].core.status(G).unwrap().hard_state.term;
    let lost = c.mutate(l, KEY, 5, W1_J);
    c.settle();
    let lost_ts = mark(c.reply(l, lost));
    // The majority goes on without it and writes the document again.
    let l2 = new_leader(&mut c, l);
    let w = c.mutate(l2, KEY, 9, MAJORITY);
    c.ticks(5);
    mark(c.reply(l2, w));
    assert!(std::mem::take(&mut c.nodes.get_mut(&l).unwrap().rolled).is_empty(), "not yet");
    // It comes back: the new leader's log replaces its entry, which goes to its rollback file.
    c.cut.clear();
    c.ticks(50);
    let rolled = std::mem::take(&mut c.nodes.get_mut(&l).unwrap().rolled);
    let files = rollback_files(&rolled, c.wall());
    assert_eq!(files.len(), 1, "{files:?}");
    let f = &files[0];
    assert!(f.path.starts_with(&format!("{ROLLBACK_DIR}/range-7/removed.")), "{}", f.path);
    let docs = read_rollback_file(&f.bytes).unwrap();
    assert_eq!(docs.len(), 1, "{docs:?}");
    match &docs[0] {
        RollbackDoc::Version { range, term: t, ts, key, value_type, value, .. } => {
            assert_eq!((*range, *t, *ts), (G, term, lost_ts));
            assert_eq!((key.as_slice(), *value_type, value.as_slice()), (KEY, 1, &[5u8][..]));
        }
        other => panic!("{other:?}"),
    }
    // Nobody reads the lost version; the replicas hold the new leader's.
    let r = c.get(l2, KEY, ReadConcern::Linearizable);
    c.ticks(2);
    assert_eq!(value(c.reply(l2, r)), Some(9));
    for n in c.others(l) {
        assert!(std::mem::take(&mut c.nodes.get_mut(&n).unwrap().rolled).is_empty());
    }
}

fn entry(index: u64, term: u64, ts: Hlc, command: &Command) -> LogEntry {
    let data = envelope::encode(ts, &command.encode());
    LogEntry { index, term, kind: EntryKind::Normal, data: Bytes::from_vec(data) }
}

fn put(key: &[u8], value: &[u8]) -> Put {
    Put { key: key.to_vec(), value_type: 1, value: value.to_vec() }
}

#[test]
fn a_rollback_file_keeps_each_version_lost_per_range_and_drops_deletions() {
    let ts = Hlc { physical: 1_793_363_696_789, logical: 3 };
    let write = Command::Write {
        puts: vec![put(b"\x01a", b"A"), put(b"\x01b", b"B")],
        deletes: vec![b"\x01c".to_vec()],
        dedup: None,
        live_delta: 0,
        docs: None,
    };
    let only_delete = Command::Write {
        puts: vec![],
        deletes: vec![b"\x01d".to_vec()],
        dedup: None,
        live_delta: 0,
        docs: None,
    };
    let other = Command::SetGcThreshold { ts };
    let g8 = GroupId(8);
    let rolled = vec![
        (G, entry(11, 4, ts, &write)),
        (G, entry(12, 4, ts, &other)),
        (G, entry(13, 4, ts, &only_delete)),
        (
            g8,
            entry(
                3,
                2,
                ts,
                &Command::Write {
                    puts: vec![put(b"x", b"X")],
                    deletes: vec![],
                    dedup: None,
                    live_delta: 0,
                    docs: None,
                },
            ),
        ),
    ];
    // 2026-10-30T12:34:56.789Z.
    let files = rollback_files(&rolled, 1_793_363_696_789);
    let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "rollback/range-7/removed.2026-10-30T12-34-56.4.11.bson",
            "rollback/range-8/removed.2026-10-30T12-34-56.2.3.bson",
        ]
    );
    let docs = read_rollback_file(&files[0].bytes).unwrap();
    let keys: Vec<(u64, Vec<u8>)> = docs
        .iter()
        .map(|d| match d {
            RollbackDoc::Version { index, key, .. } => (*index, key.clone()),
            RollbackDoc::Raw { .. } => panic!("{d:?}"),
        })
        .collect();
    assert_eq!(keys, [(11, b"\x01a".to_vec()), (11, b"\x01b".to_vec())]);
    // The mark goes as a BSON timestamp, which is exact for an HLC mark (FR-024).
    let RollbackDoc::Version { ts: got, .. } = &docs[0] else { unreachable!() };
    assert_eq!(to_timestamp(*got), to_timestamp(ts));
    assert!(rollback_files(&[], 0).is_empty());
}

#[test]
fn an_entry_that_does_not_decode_is_kept_whole() {
    let raw =
        LogEntry { index: 5, term: 1, kind: EntryKind::Normal, data: Bytes::from_vec(vec![9; 3]) };
    let files = rollback_files(&[(G, raw.clone())], 0);
    assert_eq!(files[0].path, "rollback/range-7/removed.1970-01-01T00-00-00.1.5.bson");
    let docs = read_rollback_file(&files[0].bytes).unwrap();
    assert_eq!(docs, [RollbackDoc::Raw { range: G, index: 5, term: 1, data: raw.data.to_vec() }]);
}

#[test]
fn a_damaged_rollback_file_is_refused() {
    let ts = Hlc { physical: 5_000, logical: 0 };
    let write = Command::Write {
        puts: vec![put(b"k", b"v")],
        deletes: vec![],
        dedup: None,
        live_delta: 0,
        docs: None,
    };
    let RollbackFile { bytes, .. } = rollback_files(&[(G, entry(1, 1, ts, &write))], 0).remove(0);
    assert!(read_rollback_file(&bytes[..bytes.len() - 1]).is_err(), "cut short");
    let mut longer = bytes.clone();
    longer[0] += 1;
    assert!(read_rollback_file(&longer).is_err(), "length that does not match");
    assert!(read_rollback_file(&[]).unwrap().is_empty());
}

#[test]
fn a_damaged_rollback_file_is_refused_at_the_document_that_breaks() {
    // Shorter than an empty document.
    assert_eq!(read_rollback_file(&[4, 0, 0, 0]), Err(BadRollbackFile { offset: 0 }));
    // Without its terminating zero.
    assert_eq!(read_rollback_file(&[5, 0, 0, 0, 1]), Err(BadRollbackFile { offset: 0 }));
    // An int32 whose value runs past the end of its document (the next bytes are there).
    let bytes = [9, 0, 0, 0, 0x10, b'a', 0, 0, 0, 0, 0, 0];
    assert_eq!(read_rollback_file(&bytes), Err(BadRollbackFile { offset: 4 }));
}
