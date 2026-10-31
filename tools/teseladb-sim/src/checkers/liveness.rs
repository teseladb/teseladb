//! Progress: once the faults heal, every group of the history gets at least one write
//! answered for sure (applied or failed on its expected value, both go through the log) within the
//! window; a write refused before reaching the log is no progress.

use std::collections::BTreeMap;

use teseladb_types::Nanos;

use crate::checkers::Violation;
use crate::history::{History, OpKind, OpResult};

/// Default window after the heal.
pub const WINDOW: Nanos = Nanos::from_secs(5);

/// Checks every group that has operations; without a heal (no faults) there is nothing to check.
pub fn check(history: &History, heal: Option<Nanos>, window: Nanos) -> Result<(), Violation> {
    let Some(heal) = heal else { return Ok(()) };
    let end = heal + window;
    let mut progress: BTreeMap<&str, bool> = BTreeMap::new();
    for op in &history.ops {
        let answered = op.kind != OpKind::Read
            && !matches!(op.result, OpResult::Unknown | OpResult::Rejected)
            && op.complete.is_some_and(|c| c >= heal && c <= end);
        *progress.entry(&op.group).or_default() |= answered;
    }
    match progress.into_iter().find(|(_, p)| !p) {
        Some((group, _)) => Err(Violation::liveness(
            "liveness",
            format!("{group}: no write answered within {} ms of the heal", window.as_millis()),
        )),
        None => Ok(()),
    }
}
