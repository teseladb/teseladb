//! 007 T039 (FR-026, contract `peer-frames.md`): the bodies of the requests one node forwards to
//! another (`Forward`, 0x30) and of their answers read back what was written, and bytes damaged on
//! the way give a body or an error, never a panic. The frames around them are fuzzed in
//! `teseladb-io` (`frames_survive_mutated_bytes`). A few seconds with fixed seeds;
//! `TESELADB_FUZZ_SECS=<n>` runs `n` seconds with seeds that change every round.

use teseladb_hlc::Hlc;
use teseladb_range::host::{RangeError, TxnOp};
use teseladb_range::intents::{IntentOp, TxnId, TxnStatus};
use teseladb_range::read::ReadConcern;
use teseladb_server::data_wire::{
    RemoteReply, RemoteReq, decode_for, decode_reply, encode_for, encode_reply,
};
use teseladb_server::forward::{Fetch, FetchReply, Request};
use teseladb_types::{GroupId, NodeId, Rng};

fn requests() -> Vec<RemoteReq> {
    let at = Hlc { physical: 7, logical: 1 };
    vec![
        RemoteReq::Scan { start: vec![1], end: vec![2], limit: 10, concern: ReadConcern::Majority },
        RemoteReq::FollowerScan {
            start: vec![1],
            end: vec![],
            limit: 3,
            max_stale_ms: Some(90_000),
            nodes: vec![(NodeId(2), false), (NodeId(1), true)],
        },
        RemoteReq::Read { key: vec![1, 2, 3], at: Some(at) },
        RemoteReq::Read { key: vec![], at: None },
        RemoteReq::BackupFetch { at, file: 2, offset: 4096 },
        RemoteReq::RestorePiece { file: 0, offset: 0, data: vec![9; 40] },
        RemoteReq::RestoreIngest { files: 3 },
        RemoteReq::Txn {
            op: TxnOp::PutIntents {
                txn: TxnId(42),
                read_ts: at,
                writes: vec![(vec![1, 4], IntentOp::Put { value_type: 3, value: vec![1] })],
            },
            deadline: Some(99),
        },
        RemoteReq::Txn {
            op: TxnOp::ResolveIntents {
                txn: TxnId(42),
                status: TxnStatus::Committed,
                commit_ts: at,
                keys: vec![vec![1, 4]],
            },
            deadline: None,
        },
    ]
}

fn replies() -> Vec<Result<RemoteReply, RangeError>> {
    vec![
        Ok(RemoteReply::Rows(vec![(vec![1, 1], vec![2, 2, 2]), (vec![1, 2], vec![])])),
        Ok(RemoteReply::Piece { ready: true, sizes: vec![10, 0], data: vec![5; 10] }),
        Ok(RemoteReply::Restored { done: false }),
        Err(RangeError::Timeout),
        Err(RangeError::NotLeader { hint: NodeId(3) }),
    ]
}

fn forwards() -> Vec<Request> {
    let data = encode_for(GroupId(4), &RemoteReq::RestoreIngest { files: 1 });
    vec![
        Request::Fetch(Fetch { group: GroupId(4), index: 77, file: 1, offset: 512 }),
        Request::Data(data),
        Request::Catalog(vec![3, 0, 1]),
        Request::Rebuild(GroupId(9)),
        Request::MergeProbe { range: GroupId(5), merge: 12 },
        Request::Meta2 { group: GroupId(7), entry: vec![1, 2, 3] },
    ]
}

#[test]
fn every_body_reads_back_as_written() {
    for r in requests() {
        assert_eq!(decode_for(&encode_for(GroupId(4), &r)).unwrap(), (GroupId(4), r));
    }
    for r in replies() {
        assert_eq!(decode_reply(&encode_reply(&r)).unwrap(), r);
    }
    for r in forwards() {
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
    }
    for r in [FetchReply::NotReady, FetchReply::Stale] {
        assert_eq!(FetchReply::decode(&r.encode()).unwrap(), r);
    }
}

/// `input` damaged: a byte changed, cut short, grown, or a run of bytes replaced.
fn mutate(rng: &mut Rng, input: &[u8]) -> Vec<u8> {
    let mut out = input.to_vec();
    for _ in 0..rng.range(1, 4) {
        let len = out.len() as u64;
        match rng.range(0, 4) {
            0 if len > 0 => out.truncate(rng.range(0, len) as usize),
            1 => out.extend((0..rng.range(1, 9)).map(|_| rng.range(0, 256) as u8)),
            2 if len >= 8 => {
                let i = rng.range(0, len - 7) as usize;
                let v = [0u64, 1, u64::MAX, u64::from(u32::MAX)][rng.range(0, 4) as usize];
                out[i..i + 8].copy_from_slice(&v.to_be_bytes());
            }
            _ if len > 0 => {
                let i = rng.range(0, len) as usize;
                out[i] = rng.range(0, 256) as u8;
            }
            _ => out.push(rng.range(0, 256) as u8),
        }
    }
    out
}

#[test]
fn damaged_bodies_give_a_body_or_an_error() {
    let mut bodies: Vec<Vec<u8>> = Vec::new();
    bodies.extend(requests().iter().map(|r| encode_for(GroupId(4), r)));
    bodies.extend(replies().iter().map(encode_reply));
    bodies.extend(forwards().iter().map(Request::encode));
    bodies.extend([FetchReply::NotReady, FetchReply::Stale].iter().map(FetchReply::encode));
    let secs: Option<u64> = std::env::var("TESELADB_FUZZ_SECS").ok().and_then(|s| s.parse().ok());
    let start = std::time::Instant::now();
    let mut seed = 0x0f0d_0026_u64;
    let mut rounds = 0u64;
    loop {
        let mut rng = Rng::new(seed);
        for b in &bodies {
            let input = mutate(&mut rng, b);
            // Whatever reads must write again without a panic.
            if let Ok((g, r)) = decode_for(&input) {
                let _ = encode_for(g, &r);
            }
            if let Ok(r) = decode_reply(&input) {
                let _ = encode_reply(&r);
            }
            if let Ok(r) = Request::decode(&input) {
                let _ = r.encode();
            }
            if let Ok(r) = FetchReply::decode(&input) {
                let _ = r.encode();
            }
            rounds += 1;
        }
        match secs {
            Some(s) if start.elapsed() < std::time::Duration::from_secs(s) => {
                seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15)
            }
            Some(_) => break,
            None if rounds < 50_000 => seed += 1,
            None => break,
        }
    }
    assert!(rounds >= 50_000 || secs.is_some());
}
