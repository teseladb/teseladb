//! Writes that span ranges (009 FR-003, T007, contract `index-maintenance.md`): the leader of the
//! document's range refuses a mutation whose outcome reaches a key of another range (an index
//! entry, almost always). The node then reads the current versions of the write's keys at a mark,
//! evaluates the mutation with them as the leader would, and writes everything it gives as one
//! internal transaction of 008 reading at that mark; a version committed after it is a
//! `WriteConflict`, and the statement starts again from the read. A retryable statement leaves
//! its result in the same transaction, under a key that comes from the statement: a retry finds
//! it and answers with it. Each round runs its own transaction: a range's record that a conflict
//! aborted refuses every later commit with its identifier.

use teseladb_hlc::Hlc;
use teseladb_range::host::{CROSSES_RANGES, Mutation};
use teseladb_range::intents::{IntentOp, TxnId, record_key};
use teseladb_range::mutate::Outcome;
use teseladb_session::retry::StatementKey;

use super::Backend;
use super::mutation::Bound;
use crate::error::{BSON_OBJECT_TOO_LARGE, CommandError, ENTRY_TOO_LARGE, WRITE_CONFLICT};

/// Times a statement starts again after a conflict before it answers `WriteConflict`: as a
/// `refnode` retries a write conflict, it waits for the other transaction; the node pauses between
/// rounds, so this is about its request deadline.
pub const MAX_CONFLICT_ROUNDS: u32 = 1_500;
/// Value type of a statement's result.
pub const STATEMENT_RESULT: u8 = 4;
/// What follows the record key of a statement's transaction in the key of its result.
const RESULT_SUFFIX: u8 = 1;
/// Bytes of the mark at the start of a stored result.
const MARK_LEN: usize = 12;

/// Whether the range refused `e`'s write because it reaches another range.
pub fn crosses(e: &CommandError) -> bool {
    e.errmsg.starts_with(CROSSES_RANGES)
}

/// Whether the range refused `e`'s write because its entry would pass 64 MiB (005 FR-023): a
/// document whose index entries take more than that goes as an internal transaction in parts
/// (008 FR-006).
pub fn too_large(e: &CommandError) -> bool {
    e.code == BSON_OBJECT_TOO_LARGE && e.errmsg == ENTRY_TOO_LARGE
}

/// The transaction of retryable statement `s`: the same for every send of the statement.
pub fn statement_txn(s: &StatementKey) -> TxnId {
    let mut text = Vec::with_capacity(36);
    text.extend_from_slice(&s.lsid.to_be_bytes());
    text.extend_from_slice(&s.txn_number.to_be_bytes());
    text.extend_from_slice(&s.stmt_id.to_be_bytes());
    TxnId(u128::from_be_bytes(teseladb_hash::md5(&text)))
}

/// The key of the result of the statement whose transaction is `txn`.
pub fn result_key(txn: TxnId) -> Vec<u8> {
    let mut k = record_key(txn);
    k.push(RESULT_SUFFIX);
    k
}

/// A stored result: its mark and the result.
pub fn stored_result(mark: Hlc, result: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(MARK_LEN + result.len());
    v.extend_from_slice(&mark.physical.to_be_bytes());
    v.extend_from_slice(&mark.logical.to_be_bytes());
    v.extend_from_slice(result);
    v
}

/// The mark and result of a stored result, if it is one.
pub fn read_result(stored: &[u8]) -> Option<(Hlc, &[u8])> {
    let (mark, result) = stored.split_at_checked(MARK_LEN)?;
    let physical = u64::from_be_bytes(mark[..8].try_into().ok()?);
    let logical = u32::from_be_bytes(mark[8..].try_into().ok()?);
    Some((Hlc { physical, logical }, result))
}

/// The result an earlier send of retryable statement `s` left, if one went across ranges.
pub fn earlier_result<B: Backend + ?Sized>(
    backend: &mut B,
    s: &StatementKey,
) -> Result<Option<Vec<u8>>, CommandError> {
    let key = result_key(statement_txn(s));
    let (_, found) = backend.read_versions(std::slice::from_ref(&key))?;
    let stored = found.into_iter().next().flatten();
    Ok(stored.and_then(|(_, v)| read_result(&v).map(|(_, r)| r.to_vec())))
}

/// Runs `bound` as a transaction across ranges; the mutation's encoded result.
pub fn write<B: Backend + ?Sized>(
    backend: &mut B,
    bound: &Bound,
    statement: Option<StatementKey>,
) -> Result<Vec<u8>, CommandError> {
    // The result's key: the same for every send of the statement, which looks for it at the mark
    // it evaluates at, so a send that committed meanwhile answers this one.
    let result_at = statement.as_ref().map(|s| result_key(statement_txn(s)));
    let mut rounds = 0;
    loop {
        let ((read_ts, mut writes, result), earlier) =
            evaluate_with(backend, bound, result_at.as_deref())?;
        if let Some(earlier) = earlier {
            return Ok(earlier);
        }
        if let Some(key) = &result_at {
            let value = stored_result(read_ts, &result);
            writes.push((key.clone(), IntentOp::Put { value_type: STATEMENT_RESULT, value }));
        }
        // A fresh transaction each round: the one a conflict aborted can never commit.
        match backend.transact(read_ts, writes, None) {
            Ok(_) => return Ok(result),
            Err(e) if e.code == WRITE_CONFLICT && rounds < MAX_CONFLICT_ROUNDS => rounds += 1,
            Err(e) => return Err(e),
        }
    }
}

/// What the write gives at a mark: the mark, its keys with what each one becomes, and the result.
type Evaluation = (Hlc, Vec<(Vec<u8>, IntentOp)>, Vec<u8>);

/// Reads the write's keys, and those its evaluation asks for, and evaluates it as the leader of
/// its range does.
pub(crate) fn evaluate<B: Backend + ?Sized>(
    backend: &mut B,
    bound: &Bound,
) -> Result<Evaluation, CommandError> {
    evaluate_with(backend, bound, None).map(|(evaluation, _)| evaluation)
}

/// [`evaluate`], reading also the result an earlier send of the statement left under `result_at`,
/// at the same mark: that result, if there is one.
fn evaluate_with<B: Backend + ?Sized>(
    backend: &mut B,
    bound: &Bound,
    result_at: Option<&[u8]>,
) -> Result<(Evaluation, Option<Vec<u8>>), CommandError> {
    let mut keys = bound.latch_keys();
    loop {
        let mut read = keys.clone();
        read.extend(result_at.map(<[u8]>::to_vec));
        let (ts, mut values) = backend.read_versions(&read)?;
        let earlier = match result_at {
            Some(_) => values.pop().flatten(),
            None => None,
        };
        if let Some(result) = earlier.as_ref().and_then(|(_, v)| read_result(v)) {
            return Ok(((ts, Vec::new(), Vec::new()), Some(result.1.to_vec())));
        }
        let current: Vec<Option<(u8, &[u8])>> =
            values.iter().map(|v| v.as_ref().map(|(t, b)| (*t, b.as_slice()))).collect();
        let outcome =
            bound.eval_with(&keys, &current, ts).map_err(|e| CommandError::from_mutation(&e))?;
        let unread: Vec<Vec<u8>> =
            bound.reads(&outcome).into_iter().filter(|k| !keys.contains(k)).collect();
        if !unread.is_empty() {
            keys.extend(unread);
            continue;
        }
        let result = outcome.result.clone();
        return Ok(((ts, intent_writes(outcome), result), None));
    }
}

/// What an evaluated write gives as a transaction's writes: each key once, with what it becomes
/// (the simulator's `indexes` workload writes them too).
pub fn intent_writes(outcome: Outcome) -> Vec<(Vec<u8>, IntentOp)> {
    // Faulty variant (009 T028): a write across ranges leaves the entries of its old version
    // behind in the indexes.
    #[cfg(teseladb_bug = "multirange_write_without_index_delta")]
    let outcome = {
        let mut o = outcome;
        o.deletes.retain(|k| {
            !matches!(teseladb_keys::KeyParts::of(k), Ok(teseladb_keys::KeyParts::Data { index, .. }) if index > 0)
        });
        o
    };
    // A transaction's writes keep no local identifiers (009 FR-023): no leader numbers them.
    let mut puts = outcome.puts;
    teseladb_range::local_ids::settle(&mut puts);
    let mut writes: Vec<(Vec<u8>, IntentOp)> = Vec::new();
    for p in puts {
        writes.retain(|(k, _)| k != &p.key);
        writes.push((p.key, IntentOp::Put { value_type: p.value_type, value: p.value }));
    }
    for k in outcome.deletes {
        writes.retain(|(w, _)| w != &k);
        writes.push((k, IntentOp::Delete));
    }
    writes
}

/// Removes the results of retryable statements older than the retry records of 005
/// (`DEDUP_TTL_MS` of cluster time) at cluster time `now_ms`: how many went.
pub fn purge_results<B: Backend + ?Sized>(
    backend: &mut B,
    now_ms: u64,
) -> Result<u64, CommandError> {
    use teseladb_range::dedup::DEDUP_TTL_MS;
    use teseladb_range::intents::TXN_SPACE;
    let Some(cutoff) = now_ms.checked_sub(DEDUP_TTL_MS) else { return Ok(0) };
    let (mut from, end) = (vec![TXN_SPACE], vec![TXN_SPACE + 1]);
    let result_len = result_key(TxnId(0)).len();
    let mut purged = 0;
    loop {
        let rows = backend.scan(
            &from,
            &end,
            super::crud::MODIFY_CHUNK_KEYS,
            teseladb_range::read::ReadConcern::Local,
        )?;
        let Some((last, _)) = rows.last() else { return Ok(purged) };
        from = [last.as_slice(), &[0]].concat();
        let old: Vec<Vec<u8>> = rows
            .into_iter()
            .filter(|(k, v)| {
                k.len() == result_len
                    && k.last() == Some(&RESULT_SUFFIX)
                    && read_result(v).is_some_and(|(mark, _)| mark.physical <= cutoff)
            })
            .map(|(k, _)| k)
            .collect();
        if old.is_empty() {
            continue;
        }
        // One write each: two results may be in different ranges.
        let mut w = super::crud::Writer::internal();
        for key in old {
            let mutation =
                super::mutation::DocMutation::Erase { rules: super::mutation::Rules::default() };
            w.write(backend, &Bound { mutation, keys: vec![key] }, 0)?;
            purged += 1;
        }
    }
}
