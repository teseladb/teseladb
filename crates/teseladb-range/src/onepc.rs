//! The one-phase commit (008 FR-006): a transaction whose keys are all in one range commits with
//! one `Write` at the entry's mark, after the checks `PutIntents` makes. It leaves no record and no
//! intents, so no reader ever has to push it.

use teseladb_hlc::Hlc;

use crate::command::{Command, Put};
use crate::intents::{Conflict, IntentOp, Latest, TxnId, check_put};
use crate::mutate::live_size_of;

/// The statement of the idempotence record a client transaction's one-phase commit leaves with
/// its session and `txnNumber` (008 FR-022): no statement of a retryable write has it.
pub const COMMIT_STMT: u32 = u32::MAX;
/// The result of that record when the transaction committed in this range.
pub const COMMITTED: &[u8] = &[1];
/// The result of that record when a recovery refused the commit before it came
/// (`TxnOp::DecideOnePhase`).
pub const REFUSED: &[u8] = &[0];

/// The newest version of a key as the leader read it, with its value stored as `type · value`.
pub type Newest = Option<Latest>;

/// The `Write` that commits `writes` of `txn`, read at `read_ts`, over the newest versions of
/// their keys (`newest`, in the same order), or the first conflict.
pub fn one_phase(
    txn: TxnId,
    read_ts: Hlc,
    writes: &[(Vec<u8>, IntentOp)],
    newest: &[Newest],
) -> Result<Command, Conflict> {
    let mut puts = Vec::new();
    let mut deletes = Vec::new();
    let mut live_delta = 0;
    for ((key, op), latest) in writes.iter().zip(newest) {
        check_put(key, latest.as_ref(), txn, read_ts)?;
        // `check_put` let through only values: a version that is no intent.
        let old = latest
            .as_ref()
            .filter(|l| !l.intent)
            .and_then(|l| l.value.split_first())
            .map_or(0, |(t, v)| live_size_of(key, *t, v));
        match op {
            IntentOp::Put { value_type, value } => {
                live_delta += live_size_of(key, *value_type, value) - old;
                puts.push(Put { key: key.clone(), value_type: *value_type, value: value.clone() });
            }
            IntentOp::Delete => {
                live_delta -= old;
                deletes.push(key.clone());
            }
        }
    }
    Ok(Command::Write { puts, deletes, dedup: None, live_delta, docs: None })
}
