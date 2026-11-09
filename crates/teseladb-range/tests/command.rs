//! Commands of a range (005 FR-001, contract `range-commands.md`): each one survives its
//! encoding, and an unknown or later `kind`, a cut payload or trailing bytes do not decode, so
//! the state machine stops instead of skipping an entry.

use teseladb_hlc::Hlc;
use teseladb_range::command::{
    Absorbed, Command, CopiedRecord, DedupKey, DedupRecord, KIND_COMPUTE_CHECKSUM, KIND_FENCE,
    KIND_MERGE, KIND_MERGE_FREEZE, KIND_NOOP, KIND_SET_GC_THRESHOLD, KIND_WRITE, Put,
};
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
            KIND_MERGE,
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
