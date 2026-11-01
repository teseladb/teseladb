//! A transaction's requests to a range as they travel to its leader, and the leader's answers
//! (008, for the writes across ranges of 009 FR-003): [`TxnOp`] encoded for
//! [`crate::data_wire::RemoteReq::Txn`].

use teseladb_hlc::Hlc;
use teseladb_range::host::TxnOp;
use teseladb_range::intents::{IntentOp, Push, RecordOp, TxnId, TxnStatus};
use teseladb_types::{DecodeError, Decoder, Encoder};

const PUT_INTENTS: u8 = 0;
const END_TXN: u8 = 1;
const PUSH_TXN: u8 = 2;
const ONE_PHASE: u8 = 3;
const RESOLVE: u8 = 4;
/// Tag of `TxnOp::FindIntents`.
const FIND_INTENTS: u8 = 5;
/// Tag of `TxnOp::ForgetRecord`.
const FORGET_RECORD: u8 = 6;
/// Tag of `TxnOp::ReadRecord`.
const READ_RECORD: u8 = 7;

const OP_PUT: u8 = 0;
const OP_DELETE: u8 = 1;

const BEGIN: u8 = 0;
const HEARTBEAT: u8 = 1;
const COMMIT: u8 = 2;
const ABORT: u8 = 3;
/// Code of `RecordOp::Resolved`.
const RESOLVED: u8 = 4;

const PUSH_TO: u8 = 0;
const ABORT_IF_ABANDONED: u8 = 1;
/// Code of `Push::Abort`.
const PUSH_ABORT: u8 = 2;

fn put_hlc(e: &mut Encoder, ts: Hlc) {
    e.u64(ts.physical).u32(ts.logical);
}

fn get_hlc(d: &mut Decoder<'_>) -> Result<Hlc, DecodeError> {
    Ok(Hlc { physical: d.u64()?, logical: d.u32()? })
}

/// A count read from the input, refused when the input cannot hold that many items.
fn count(d: &mut Decoder<'_>) -> Result<u64, DecodeError> {
    let at = d.position();
    let n = d.var()?;
    if n > d.remaining() as u64 {
        return Err(DecodeError::InvalidValue { offset: at });
    }
    Ok(n)
}

fn put_writes(e: &mut Encoder, writes: &[(Vec<u8>, IntentOp)]) {
    e.var(writes.len() as u64);
    for (k, op) in writes {
        e.bytes(k);
        match op {
            IntentOp::Put { value_type, value } => {
                e.u8(OP_PUT).u8(*value_type).bytes(value);
            }
            IntentOp::Delete => {
                e.u8(OP_DELETE);
            }
        }
    }
}

fn get_writes(d: &mut Decoder<'_>) -> Result<Vec<(Vec<u8>, IntentOp)>, DecodeError> {
    let n = count(d)?;
    (0..n)
        .map(|_| {
            let key = d.bytes()?.to_vec();
            let at = d.position();
            let op = match d.u8()? {
                OP_PUT => IntentOp::Put { value_type: d.u8()?, value: d.bytes()?.to_vec() },
                OP_DELETE => IntentOp::Delete,
                _ => return Err(DecodeError::InvalidValue { offset: at }),
            };
            Ok((key, op))
        })
        .collect()
}

fn put_keys(e: &mut Encoder, keys: &[Vec<u8>]) {
    e.var(keys.len() as u64);
    for k in keys {
        e.bytes(k);
    }
}

fn get_keys(d: &mut Decoder<'_>) -> Result<Vec<Vec<u8>>, DecodeError> {
    let n = count(d)?;
    (0..n).map(|_| d.bytes().map(<[u8]>::to_vec)).collect()
}

fn put_spans(e: &mut Encoder, spans: &[(Vec<u8>, Vec<u8>)]) {
    e.var(spans.len() as u64);
    for (s, end) in spans {
        e.bytes(s).bytes(end);
    }
}

fn get_spans(d: &mut Decoder<'_>) -> Result<Vec<(Vec<u8>, Vec<u8>)>, DecodeError> {
    let n = count(d)?;
    (0..n).map(|_| Ok((d.bytes()?.to_vec(), d.bytes()?.to_vec()))).collect()
}

fn put_record_op(e: &mut Encoder, op: &RecordOp) {
    match op {
        RecordOp::Begin { lsid, txn_number, read_ts } => {
            e.u8(BEGIN).u128(*lsid).i64(*txn_number);
            put_hlc(e, *read_ts);
        }
        RecordOp::Heartbeat { spans } => {
            e.u8(HEARTBEAT);
            put_spans(e, spans);
        }
        RecordOp::Commit { ts, spans } => {
            e.u8(COMMIT);
            put_hlc(e, *ts);
            put_spans(e, spans);
        }
        RecordOp::Abort { spans } => {
            e.u8(ABORT);
            put_spans(e, spans);
        }
        RecordOp::Resolved => {
            e.u8(RESOLVED);
        }
    }
}

fn get_record_op(d: &mut Decoder<'_>) -> Result<RecordOp, DecodeError> {
    let at = d.position();
    Ok(match d.u8()? {
        BEGIN => RecordOp::Begin { lsid: d.u128()?, txn_number: d.i64()?, read_ts: get_hlc(d)? },
        HEARTBEAT => RecordOp::Heartbeat { spans: get_spans(d)? },
        COMMIT => RecordOp::Commit { ts: get_hlc(d)?, spans: get_spans(d)? },
        ABORT => RecordOp::Abort { spans: get_spans(d)? },
        RESOLVED => RecordOp::Resolved,
        _ => return Err(DecodeError::InvalidValue { offset: at }),
    })
}

/// Writes `op`.
pub fn put_op(e: &mut Encoder, op: &TxnOp) {
    match op {
        TxnOp::PutIntents { txn, read_ts, writes } | TxnOp::OnePhase { txn, read_ts, writes } => {
            let tag = if matches!(op, TxnOp::PutIntents { .. }) { PUT_INTENTS } else { ONE_PHASE };
            e.u8(tag).u128(txn.0);
            put_hlc(e, *read_ts);
            put_writes(e, writes);
        }
        TxnOp::EndTxn { txn, op } => {
            e.u8(END_TXN).u128(txn.0);
            put_record_op(e, op);
        }
        TxnOp::PushTxn { txn, push } => {
            e.u8(PUSH_TXN).u128(txn.0);
            match push {
                Push::To(ts) => {
                    e.u8(PUSH_TO);
                    put_hlc(e, *ts);
                }
                Push::AbortIfAbandoned { intent_ts } => {
                    e.u8(ABORT_IF_ABANDONED);
                    put_hlc(e, *intent_ts);
                }
                Push::Abort { lsid, txn_number } => {
                    e.u8(PUSH_ABORT).u128(*lsid).i64(*txn_number);
                }
            }
        }
        TxnOp::ResolveIntents { txn, status, commit_ts, keys } => {
            e.u8(RESOLVE).u128(txn.0).u8(status.code());
            put_hlc(e, *commit_ts);
            put_keys(e, keys);
        }
        TxnOp::FindIntents { txn, start, end } => {
            e.u8(FIND_INTENTS).u128(txn.0).bytes(start).bytes(end);
        }
        TxnOp::ForgetRecord { txn, before } => {
            e.u8(FORGET_RECORD).u128(txn.0);
            put_hlc(e, *before);
        }
        TxnOp::ReadRecord { txn } => {
            e.u8(READ_RECORD).u128(txn.0);
        }
    }
}

/// Reads what [`put_op`] wrote.
pub fn get_op(d: &mut Decoder<'_>) -> Result<TxnOp, DecodeError> {
    let at = d.position();
    let tag = d.u8()?;
    let txn = TxnId(d.u128()?);
    Ok(match tag {
        PUT_INTENTS => TxnOp::PutIntents { txn, read_ts: get_hlc(d)?, writes: get_writes(d)? },
        ONE_PHASE => TxnOp::OnePhase { txn, read_ts: get_hlc(d)?, writes: get_writes(d)? },
        END_TXN => TxnOp::EndTxn { txn, op: get_record_op(d)? },
        PUSH_TXN => {
            let at = d.position();
            let push = match d.u8()? {
                PUSH_TO => Push::To(get_hlc(d)?),
                ABORT_IF_ABANDONED => Push::AbortIfAbandoned { intent_ts: get_hlc(d)? },
                PUSH_ABORT => Push::Abort { lsid: d.u128()?, txn_number: d.i64()? },
                _ => return Err(DecodeError::InvalidValue { offset: at }),
            };
            TxnOp::PushTxn { txn, push }
        }
        RESOLVE => {
            let at = d.position();
            let status =
                TxnStatus::from_code(d.u8()?).ok_or(DecodeError::InvalidValue { offset: at })?;
            TxnOp::ResolveIntents { txn, status, commit_ts: get_hlc(d)?, keys: get_keys(d)? }
        }
        FIND_INTENTS => {
            TxnOp::FindIntents { txn, start: d.bytes()?.to_vec(), end: d.bytes()?.to_vec() }
        }
        FORGET_RECORD => TxnOp::ForgetRecord { txn, before: get_hlc(d)? },
        READ_RECORD => TxnOp::ReadRecord { txn },
        _ => return Err(DecodeError::InvalidValue { offset: at }),
    })
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn back(op: &TxnOp) -> TxnOp {
        let mut e = Encoder::new();
        put_op(&mut e, op);
        let bytes = e.finish();
        let mut d = Decoder::new(&bytes);
        let r = get_op(&mut d).expect("decode");
        d.finish().expect("whole");
        r
    }

    #[test]
    fn every_request_reads_back() {
        let (txn, ts) = (TxnId(7), Hlc { physical: 5, logical: 2 });
        let writes = vec![
            (vec![1], IntentOp::Put { value_type: 2, value: vec![3] }),
            (vec![4], IntentOp::Delete),
        ];
        let spans = vec![(vec![1], vec![9])];
        let ops = [
            TxnOp::PutIntents { txn, read_ts: ts, writes: writes.clone() },
            TxnOp::OnePhase { txn, read_ts: ts, writes },
            TxnOp::EndTxn { txn, op: RecordOp::Begin { lsid: 3, txn_number: -1, read_ts: ts } },
            TxnOp::EndTxn { txn, op: RecordOp::Heartbeat { spans: spans.clone() } },
            TxnOp::EndTxn { txn, op: RecordOp::Commit { ts, spans: spans.clone() } },
            TxnOp::EndTxn { txn, op: RecordOp::Abort { spans } },
            TxnOp::PushTxn { txn, push: Push::To(ts) },
            TxnOp::PushTxn { txn, push: Push::AbortIfAbandoned { intent_ts: ts } },
            TxnOp::PushTxn { txn, push: Push::Abort { lsid: 9, txn_number: -2 } },
            TxnOp::EndTxn { txn, op: RecordOp::Resolved },
            TxnOp::FindIntents { txn, start: vec![1, 2], end: vec![3] },
            TxnOp::ForgetRecord { txn, before: ts },
            TxnOp::ReadRecord { txn },
            TxnOp::ResolveIntents {
                txn,
                status: TxnStatus::Committed,
                commit_ts: ts,
                keys: vec![vec![1], vec![2]],
            },
        ];
        for op in ops {
            assert_eq!(back(&op), op);
        }
    }

    #[test]
    fn bad_tags_and_counts_are_refused() {
        let bad = |bytes: &[u8]| get_op(&mut Decoder::new(bytes)).is_err();
        let mut e = Encoder::new();
        e.u8(9).u128(1);
        assert!(bad(&e.finish()), "unknown request");
        let mut e = Encoder::new();
        e.u8(PUSH_TXN).u128(1).u8(9);
        assert!(bad(&e.finish()), "unknown push");
        let mut e = Encoder::new();
        e.u8(END_TXN).u128(1).u8(9);
        assert!(bad(&e.finish()), "unknown record change");
        let mut e = Encoder::new();
        e.u8(RESOLVE).u128(1).u8(9);
        assert!(bad(&e.finish()), "unknown status");
        let mut e = Encoder::new();
        e.u8(PUT_INTENTS).u128(1).u64(0).u32(0).var(1).bytes(&[1]).u8(9);
        assert!(bad(&e.finish()), "unknown intent");
        let mut e = Encoder::new();
        e.u8(PUT_INTENTS).u128(1).u64(0).u32(0).var(1_000);
        assert!(bad(&e.finish()), "more writes than bytes");
    }
}
