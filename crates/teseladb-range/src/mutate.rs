//! Evaluating a mutation on the leader (FR-007, FR-008): the write takes the latches of its
//! documents, the caller reads their current versions, and inside `MultiRaftCore::propose_at` the
//! upper layer's function gets them with the write's mark and returns the new versions, the index
//! changes and the change description for 014 and 018. The keys it touches join the latch; if
//! another write holds one, everything is given back so that no write waits while it holds keys.
//! If a key it writes was read at or above the mark, it is evaluated again at a higher mark.

use teseladb_hlc::Hlc;

use crate::command::{Command, DedupKey, DedupRecord, Put};
use crate::latch::{LatchId, Latches};
use crate::tscache::TsCache;

/// What a mutation function returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// New versions: the document and its index entries.
    pub puts: Vec<Put>,
    /// Keys deleted: the document or its old index entries.
    pub deletes: Vec<Vec<u8>>,
    /// Description of the change for change streams (014) and the oplog (018).
    pub change: Vec<u8>,
    /// What the driver gets back (`n`, `nModified`, `upserted`, `findAndModify`'s document),
    /// encoded by the upper layer; kept in the idempotence record of a retryable write (FR-011).
    pub result: Vec<u8>,
}

/// A mutation ready to propose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Evaluated {
    /// The `Write` to propose at the mark it was evaluated at.
    pub command: Command,
    /// Description of the change.
    pub change: Vec<u8>,
    /// What the driver gets back.
    pub result: Vec<u8>,
    /// Latch held until the entry is applied.
    pub latch: LatchId,
}

/// Why a mutation was not evaluated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Refused<E> {
    /// Another write holds one of its keys: its latch is released; retry once that write is
    /// applied.
    Busy(LatchId),
    /// A key it writes was read at or above the mark: it keeps its latch and is evaluated again
    /// at a mark above this one.
    Below(Hlc),
    /// The upper layer refused it; its latch is released.
    Failed(E),
}

/// The current versions of a mutation's keys, `(value type, value)` or `None` where there is
/// none, in the order of its keys.
pub type Current<'a> = [Option<(u8, &'a [u8])>];

/// Evaluates the mutation `f` at mark `ts`, holding latch `id` over the current versions of its
/// keys (`(value type, value)`, `None` where there is none). It runs inside
/// `MultiRaftCore::propose_at`, which gives the mark above the reads of the keys latched so far.
pub fn evaluate<E, F>(
    latches: &mut Latches,
    cache: &TsCache,
    id: LatchId,
    (read, current): (&[Vec<u8>], &Current<'_>),
    ts: Hlc,
    f: F,
    dedup: Option<DedupKey>,
) -> Result<Evaluated, Refused<E>>
where
    F: FnOnce(&[Option<(u8, &[u8])>], Hlc) -> Result<Outcome, E>,
{
    let outcome = match f(current, ts) {
        Ok(o) => o,
        Err(e) => {
            latches.release(id);
            return Err(Refused::Failed(e));
        }
    };
    let keys: Vec<Vec<u8>> =
        outcome.puts.iter().map(|p| p.key.clone()).chain(outcome.deletes.iter().cloned()).collect();
    if let Err(holder) = latches.extend(id, &keys) {
        latches.release(id);
        return Err(Refused::Busy(holder));
    }
    let floor = cache.floor_of(&keys);
    // Faulty variant (005 T017): see `RangeHost::propose`.
    #[cfg(teseladb_bug = "write_below_read")]
    let floor = Hlc { physical: 0, logical: 0 }.min(floor);
    if floor >= ts {
        return Err(Refused::Below(floor));
    }
    let Outcome { puts, deletes, change, result } = outcome;
    let live_delta = live_delta(read, current, &puts, &deletes);
    let dedup = dedup.map(|k| DedupRecord {
        lsid: k.lsid,
        txn_number: k.txn_number,
        stmt_id: k.stmt_id,
        result: result.clone(),
    });
    let command = Command::Write { puts, deletes, dedup, live_delta, docs: None };
    Ok(Evaluated { command, change, result, latch: id })
}

/// Bytes a live version of `key` with `value` takes: its key, its type and its value.
pub fn live_size(key: &[u8], value: &[u8]) -> i64 {
    i64::try_from(key.len() + 1 + value.len()).unwrap_or(i64::MAX)
}

/// Bytes a live version of `key` with a value of type `value_type` takes: none for a version that
/// only serves the history ([`crate::HISTORY_VALUE`], 014), which is not live data.
pub fn live_size_of(key: &[u8], value_type: u8, value: &[u8]) -> i64 {
    if value_type == crate::HISTORY_VALUE { 0 } else { live_size(key, value) }
}

/// The change of the range's live bytes that a write makes (007 FR-005, D-068): the new
/// versions' sizes minus the sizes of the versions they replace or delete, which the leader read
/// (`current`, in the order of `read`).
fn live_delta(
    read: &[Vec<u8>],
    current: &[Option<(u8, &[u8])>],
    puts: &[crate::command::Put],
    deletes: &[Vec<u8>],
) -> i64 {
    let data = |k: &[u8]| k.first().is_some_and(|b| *b != 0);
    let old = |k: &[u8]| {
        let at = read.iter().position(|r| r.as_slice() == k);
        at.and_then(|i| current.get(i).copied().flatten()).map_or(0, |(t, v)| live_size_of(k, t, v))
    };
    let added: i64 = puts
        .iter()
        .filter(|p| data(&p.key))
        .map(|p| live_size_of(&p.key, p.value_type, &p.value) - old(&p.key))
        .sum();
    let removed: i64 = deletes.iter().filter(|k| data(k)).map(|k| old(k)).sum();
    added - removed
}
