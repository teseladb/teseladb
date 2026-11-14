//! Commands of a range (005 FR-001, contract `range-commands.md`): each one survives its
//! encoding, and an unknown or later `kind`, a cut payload or trailing bytes do not decode, so
//! the state machine stops instead of skipping an entry.

use teseladb_hlc::Hlc;
use teseladb_range::command::{
    Absorbed, Command, CopiedRecord, DedupKey, DedupRecord, KIND_ADJUST_COUNTS, KIND_ADJUST_SIZED,
    KIND_COMPUTE_CHECKSUM, KIND_FENCE, KIND_MERGE, KIND_MERGE_FREEZE, KIND_MERGE_REACHING,
    KIND_NOOP, KIND_SET_GC_THRESHOLD, KIND_SPLIT, KIND_SPLIT_REACHING, KIND_WRITE,
    KIND_WRITE_COUNTED, KIND_WRITE_REACHING, KIND_WRITE_SIZED, Put,
};
use teseladb_range::doc_count::Change;
use teseladb_types::GroupId;

fn samples() -> Vec<Command> {
    let ts = Hlc { physical: 1_700_000_000_000, logical: (5 << 10) | 7 };
    vec![
        Command::Noop,
        Command::Write { puts: vec![], deletes: vec![], dedup: None, live_delta: 0, docs: None },
        Command::Write {
            puts: vec![
                Put { key: b"doc/1".to_vec(), value_type: 1, value: b"{a:1}".to_vec() },
                Put { key: b"idx/a/1".to_vec(), value_type: 2, value: vec![] },
            ],
            deletes: vec![b"idx/a/0".to_vec()],
            dedup: Some(DedupRecord {
                lsid: u128::MAX - 3,
                txn_number: -1,
                stmt_id: 9,
                result: b"n=1".to_vec(),
                reaching: false,
            }),
            live_delta: 0,
            docs: None,
        },
        Command::SetGcThreshold { ts },
        Command::ComputeChecksum { at: ts },
        Command::MergeFreeze { merge: 41 },
        Command::MergeFreeze { merge: 0 },
        Command::Merge {
            right: GroupId(7),
            merge: 41,
            absorb: Some(Absorbed {
                end: b"z".to_vec(),
                gc: Hlc { physical: 3, logical: 1 },
                mark: ts,
                live: 1 << 40,
                records: vec![CopiedRecord {
                    key: DedupKey { lsid: 5, txn_number: -2, stmt_id: 1 },
                    mark: ts,
                    result: b"ok".to_vec(),
                    reach: Some(b"q".to_vec()),
                }],
                fences: vec![(b"a".to_vec(), b"c".to_vec())],
            }),
        },
        Command::Merge { right: GroupId(7), merge: 41, absorb: None },
        Command::Fence { start: b"a".to_vec(), end: b"c".to_vec(), on: true },
        Command::Fence { start: vec![], end: vec![], on: false },
    ]
}

#[test]
fn every_command_survives_its_encoding() {
    let kinds: Vec<u16> = samples().iter().map(Command::kind).collect();
    assert_eq!(
        kinds,
        [
            KIND_NOOP,
            KIND_WRITE,
            KIND_WRITE,
            KIND_SET_GC_THRESHOLD,
            KIND_COMPUTE_CHECKSUM,
            KIND_MERGE_FREEZE,
            KIND_MERGE_FREEZE,
            KIND_MERGE_REACHING,
            KIND_MERGE,
            KIND_FENCE,
            KIND_FENCE,
        ]
    );
    for c in samples() {
        let b = c.encode();
        assert_eq!(u16::from_le_bytes([b[0], b[1]]), c.kind(), "{c:?}");
        assert_eq!(Command::decode(&b).unwrap(), c);
    }
}

#[test]
fn the_live_delta_is_a_signed_varint_at_the_end_of_a_write() {
    let write = |live_delta| Command::Write {
        puts: vec![],
        deletes: vec![],
        dedup: None,
        live_delta,
        docs: None,
    };
    // Kind, no puts, no deletes, no record, then the delta in zigzag LEB128 (D-068).
    for (delta, tail) in [
        (0i64, vec![0u8]),
        (-1, vec![1]),
        (1, vec![2]),
        (-64, vec![127]),
        (64, vec![128, 1]),
        (i64::MAX, vec![0xfe, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1]),
        (i64::MIN, vec![0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1]),
    ] {
        let b = write(delta).encode();
        assert_eq!(b, [&KIND_WRITE.to_le_bytes()[..], &[0, 0, 0], &tail].concat(), "{delta}");
        assert_eq!(Command::decode(&b), Ok(write(delta)));
    }
}

#[test]
fn unknown_kinds_cut_payloads_and_trailing_bytes_do_not_decode() {
    for kind in [4u16, 8, 12, 13, u16::MAX] {
        assert!(Command::decode(&kind.to_le_bytes()).is_err(), "kind {kind}");
    }
    for c in samples() {
        let b = c.encode();
        for cut in 0..b.len() {
            assert!(Command::decode(&b[..cut]).is_err(), "{c:?} cut at {cut}");
        }
        let mut longer = b.clone();
        longer.push(0);
        assert!(Command::decode(&longer).is_err(), "{c:?} with a trailing byte");
    }
}

/// 007 FR-022: a split whose records keep their greatest key is of format 3 and carries the keys;
/// one whose records do not is the split nodes of the format before read, and a counted write
/// whose record keeps its key is of format 3 as well.
#[test]
fn records_that_keep_their_greatest_key_go_in_the_commands_of_format_3() {
    let record = |reach: Option<&[u8]>| CopiedRecord {
        key: DedupKey { lsid: 9, txn_number: 3, stmt_id: 2 },
        mark: Hlc { physical: 7, logical: 1 },
        result: b"r".to_vec(),
        reach: reach.map(<[u8]>::to_vec),
    };
    let split = |records| Command::Split {
        at: b"m".to_vec(),
        new_range: GroupId(4),
        records,
        right_live: 12,
    };
    let reaching = split(vec![record(Some(b"q")), record(None)]);
    let older = split(vec![record(None), record(None)]);
    assert_eq!(reaching.kind(), KIND_SPLIT_REACHING);
    assert_eq!(older.kind(), KIND_SPLIT);
    for c in [&reaching, &older] {
        assert_eq!(Command::decode(&c.encode()).as_ref(), Ok(c));
    }
    // The keys follow the split's fields: the same entry without them is the older one.
    let (new, old) = (reaching.encode(), older.encode());
    assert_eq!(new[2..old.len()], old[2..]);
    let write = |reaching, docs: Option<Vec<((u32, u32), Change)>>| Command::Write {
        puts: vec![Put { key: b"k".to_vec(), value_type: 1, value: b"v".to_vec() }],
        deletes: vec![],
        dedup: Some(DedupRecord { lsid: 1, txn_number: 2, stmt_id: 3, result: vec![], reaching }),
        live_delta: 4,
        docs,
    };
    let w = write(true, Some(vec![((0, 1), Change::docs(1))]));
    assert_eq!(w.kind(), KIND_WRITE_REACHING);
    assert_eq!(Command::decode(&w.encode()), Ok(w));
    assert_eq!(write(false, Some(vec![])).kind(), KIND_WRITE_COUNTED);
    // With the bytes of each collection (format 4), reaching or not.
    for reaching in [true, false] {
        let sized = write(reaching, Some(vec![((0, 1), Change::sized(1, 30, -4))]));
        assert_eq!(sized.kind(), KIND_WRITE_SIZED);
        assert_eq!(Command::decode(&sized.encode()), Ok(sized));
    }
    let adjust = |sizes| Command::AdjustCounts { base: 3, counts: vec![((0, 1), 2)], sizes };
    assert_eq!(adjust(None).kind(), KIND_ADJUST_COUNTS);
    assert_eq!(adjust(Some(vec![((0, 1), (60, 8))])).kind(), KIND_ADJUST_SIZED);
    for a in [adjust(None), adjust(Some(vec![((0, 1), (60, 8))]))] {
        assert_eq!(Command::decode(&a.encode()), Ok(a));
    }
    // A write of format 3 without a record does not decode.
    let no_record = Command::Write {
        puts: vec![],
        deletes: vec![],
        dedup: None,
        live_delta: 0,
        docs: Some(vec![]),
    };
    let mut bad = no_record.encode();
    bad[..2].copy_from_slice(&KIND_WRITE_REACHING.to_le_bytes());
    assert!(Command::decode(&bad).is_err());
}
