//! The writes of a capped collection on the range's leader (015 FR-003, T007, research R5).
//!
//! The write's first key is the collection's state ([`teseladb_range::capped::state_key`]),
//! which every write of the collection latches, so they are evaluated one after another. Then
//! come the oldest documents the command scanned ([`CappedRule::oldest`]) and the write's own
//! keys. An insert takes the next `seq`, and the documents the limits evict go in the same write
//! with their index entries; an update or a deletion changes the state's bytes and documents. An
//! insert whose scan fell short, or read a document deleted since, is refused with
//! [`is_refusal`] and the command scans again.

use teseladb_bson::RawDoc;
use teseladb_hlc::Hlc;
use teseladb_range::capped::{
    self as cap, CappedError, CappedState, plan_delete, plan_insert, plan_update,
};
use teseladb_range::command::Put;

use super::{
    CappedRule, Checker, DocMutation, Evaluated, Lookup, MutationResult, Rules, Stored, WriteError,
    fill_timestamps, modify, put, stored_bytes,
};
use crate::error::{CommandError, INTERNAL_ERROR, WRITE_CONFLICT};

/// Value type of a capped collection's state (documents are 1, index entries 2, guards 3).
pub const STATE: u8 = 4;

/// The error that refuses a write whose scan of the oldest documents does not do.
pub fn refusal(e: CappedError) -> CommandError {
    CommandError::new(WRITE_CONFLICT, e.message())
}

/// The refusal `e` carries, if it is one.
pub fn refused(e: &CommandError) -> Option<CappedError> {
    (e.code.code == WRITE_CONFLICT.code).then(|| CappedError::from_message(&e.errmsg)).flatten()
}

/// Whether `e` refuses a write for its scan of the oldest documents.
pub fn is_refusal(e: &CommandError) -> bool {
    refused(e).is_some()
}

fn internal(msg: &str) -> CommandError {
    CommandError::new(INTERNAL_ERROR, msg)
}

/// What every capped write reads first: the state and the oldest documents.
struct Read<'k, 'a> {
    tenant: u32,
    collection: u32,
    rule: CappedRule,
    state: CappedState,
    /// The oldest documents, `(seq, BSON)`: their sizes are those of their BSON (003 FR-021).
    oldest: Vec<(u64, &'a [u8])>,
    /// The write's own keys.
    own: &'k [Vec<u8>],
}

fn read<'k, 'a>(
    keys: &'k [Vec<u8>],
    rules: &Rules,
    lookup: &Lookup<'a>,
) -> Result<Read<'k, 'a>, CommandError> {
    let rule = rules.capped.ok_or_else(|| internal("not a capped write"))?;
    let (tenant, collection) = (rules.tenant, rules.collection);
    let Some((state_key, rest)) = keys.split_first() else {
        return Err(internal("a capped write without its state"));
    };
    if *state_key != cap::state_key(tenant, collection) || rest.len() < rule.oldest {
        return Err(internal("malformed capped write"));
    }
    let state = match lookup(state_key) {
        Some((_, v)) => CappedState::decode(v).map_err(|e| internal(&e.to_string()))?,
        None => CappedState::default(),
    };
    let (oldest_keys, own) = rest.split_at(rule.oldest);
    let mut oldest = Vec::with_capacity(oldest_keys.len());
    for k in oldest_keys {
        // A document deleted since the scan.
        let Some((_, doc)) = lookup(k) else { return Err(refusal(CappedError::Stale)) };
        let seq = cap::seq_of(tenant, collection, k).ok_or_else(|| internal("not a capped key"))?;
        oldest.push((seq, teseladb_keys::stored::bson(doc)?));
    }
    Ok(Read { tenant, collection, rule, state, oldest, own })
}

/// Evaluates an insert, update or delete of a capped collection.
pub(super) fn eval(
    keys: &[Vec<u8>],
    m: &DocMutation,
    ts: Hlc,
    checker: &Checker,
    lookup: &Lookup<'_>,
) -> Result<Evaluated, CommandError> {
    match m {
        DocMutation::Insert { docs, rules, .. } => {
            let r = read(keys, rules, lookup)?;
            let [doc] = docs.as_slice() else {
                return Err(internal("a capped insert writes one document"));
            };
            let filled = fill_timestamps(doc, Some(ts))?;
            let doc = filled.as_ref().unwrap_or(doc);
            let written = stored_bytes(doc, Stored::Insert)
                .and_then(|b| checker.check(&b, None).map(|()| b))
                .and_then(|b| admit(&r, r.state, b, checker, lookup));
            match written {
                Ok((puts, deletes)) => {
                    Ok((puts, deletes, MutationResult { n: 1, ..MutationResult::default() }))
                }
                Err(e) if is_refusal(&e) => Err(e),
                Err(error) => {
                    let r = MutationResult {
                        errors: vec![WriteError { index: 0, error }],
                        ..MutationResult::default()
                    };
                    Ok((Vec::new(), Vec::new(), r))
                }
            }
        }
        DocMutation::Update { rules, .. } | DocMutation::Delete { rules, .. } => {
            let r = read(keys, rules, lookup)?;
            change(&r, m, ts, checker, lookup)
        }
        DocMutation::Backfill { .. } | DocMutation::Erase { .. } => {
            Err(internal("not a capped write"))
        }
    }
}

/// The size a capped collection counts for stored document `stored`: its BSON's, without its
/// table (003 FR-021).
fn bson_len(stored: &[u8]) -> Result<u64, CommandError> {
    Ok(teseladb_keys::stored::bson(stored)?.len() as u64)
}

fn state_put((tenant, collection): (u32, u32), state: CappedState) -> Put {
    Put { key: cap::state_key(tenant, collection), value_type: STATE, value: state.encode() }
}

/// The puts and deletes that add document `bytes` at the next `seq` of state `state`, evict the
/// oldest documents the limits leave out, and write the new state.
fn admit(
    r: &Read<'_, '_>,
    state: CappedState,
    bytes: Vec<u8>,
    checker: &Checker,
    lookup: &Lookup<'_>,
) -> Result<(Vec<Put>, Vec<Vec<u8>>), CommandError> {
    let key = cap::doc_key(r.tenant, r.collection, state.next);
    // The new document first: a unique value an evicted one holds is still a duplicate.
    let (mut puts, mut deletes) = checker.entries_at(Some(&key), None, Some(&bytes), lookup)?;
    let size = bytes.len() as u64;
    puts.push(put(&key, bytes, checker, None));
    let (p, d) = evict(r, state, size, checker, lookup)?;
    puts.extend(p);
    deletes.extend(d);
    Ok((puts, deletes))
}

/// The insert of a document of `size` bytes at the next `seq` of `state`: the deletion of the
/// oldest documents the limits leave out, with their entries, and the new state.
fn evict(
    r: &Read<'_, '_>,
    state: CappedState,
    size: u64,
    checker: &Checker,
    lookup: &Lookup<'_>,
) -> Result<(Vec<Put>, Vec<Vec<u8>>), CommandError> {
    let sizes: Vec<(u64, u64)> = r.oldest.iter().map(|(s, d)| (*s, d.len() as u64)).collect();
    let plan = plan_insert(state, r.rule.limits, size, &sizes).map_err(refusal)?;
    let (mut puts, mut deletes) = (Vec::new(), Vec::new());
    for (seq, doc) in &r.oldest[..plan.evict.len()] {
        let k = cap::doc_key(r.tenant, r.collection, *seq);
        let old = RawDoc::from_bytes(doc)?;
        let (p, d) = checker.entries_at(Some(&k), Some(old), None, lookup)?;
        puts.extend(p);
        deletes.push(k);
        deletes.extend(d);
    }
    puts.push(state_put((r.tenant, r.collection), plan.state));
    Ok((puts, deletes))
}

/// An update or a deletion: the state follows the bytes and documents it changes, and an upsert
/// is an insert at the next `seq`.
fn change(
    r: &Read<'_, '_>,
    m: &DocMutation,
    ts: Hlc,
    checker: &Checker,
    lookup: &Lookup<'_>,
) -> Result<Evaluated, CommandError> {
    let upsert_key = cap::doc_key(r.tenant, r.collection, r.state.next);
    let mut keys = r.own.to_vec();
    if matches!(m, DocMutation::Update { upsert: Some(_), .. }) {
        keys.push(upsert_key.clone());
    }
    let (mut puts, mut deletes, result) = modify(&keys, m, Some(ts), checker, lookup)?;
    let mut state = r.state;
    for k in r.own {
        let Some((_, old)) = lookup(k) else { continue };
        let old = bson_len(old)?;
        if deletes.contains(k) {
            state = plan_delete(state, old);
        } else if let Some(p) = puts.iter().find(|p| &p.key == k) {
            state = plan_update(state, old, bson_len(&p.value)?);
        }
    }
    if result.upserted.is_none() {
        puts.push(state_put((r.tenant, r.collection), state));
        return Ok((puts, deletes, result));
    }
    // The upsert wrote its document and entries at the next `seq`: it evicts like an insert.
    let size = puts
        .iter()
        .find(|p| p.key == upsert_key)
        .map(|p| bson_len(&p.value))
        .ok_or_else(|| internal("an upsert without its document"))??;
    let (p, d) = evict(r, state, size, checker, lookup)?;
    puts.extend(p);
    deletes.extend(d);
    Ok((puts, deletes, result))
}
