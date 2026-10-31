//! Change stream delivery (014 FR-005, FR-010, FR-015): every committed change in a cursor's
//! window reaches it once, in the order of the merge (commit mark, transaction, position in the
//! transaction, key), with the changes of a transaction one after another and complete, and never
//! above the resolved mark the cursor had when it emitted it.

use std::collections::{BTreeMap, BTreeSet};

use teseladb_hlc::Hlc;

use crate::checkers::Violation;

/// Name of the checker.
pub const CHECKER: &str = "changestream_delivery";

/// A committed change, as the merge orders it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Change {
    /// Commit mark.
    pub ts: Hlc,
    /// Transaction (`None` for a write outside one); its changes share `ts`.
    pub txn: Option<u128>,
    /// Position in the transaction (0 outside one).
    pub op: u32,
    /// Key of the document.
    pub key: Vec<u8>,
}

/// A change the cursor emitted, with the resolved mark it had then.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Emitted {
    /// The change.
    pub change: Change,
    /// The cursor's resolved mark when it emitted it.
    pub resolved: Hlc,
}

/// What one cursor saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CursorLog {
    /// Name, for the report.
    pub name: String,
    /// It started after this mark (its start, or the mark of the token it resumed from).
    pub after: Hlc,
    /// The changes it emitted, in order.
    pub emitted: Vec<Emitted>,
    /// Its last resolved mark: every committed change at or below it has to be emitted.
    pub resolved: Hlc,
}

/// Checks the cursors against the committed changes of their scope.
pub fn check(committed: &[Change], cursors: &[CursorLog]) -> Result<(), Violation> {
    let known: BTreeSet<&Change> = committed.iter().collect();
    cursors.iter().try_for_each(|c| check_cursor(&known, c))
}

fn check_cursor(known: &BTreeSet<&Change>, c: &CursorLog) -> Result<(), Violation> {
    let fail = |what: String| Violation::safety(CHECKER, format!("cursor {}: {what}", c.name));
    for (i, e) in c.emitted.iter().enumerate() {
        let ch = &e.change;
        if !known.contains(ch) {
            return Err(fail(format!("{ch:?} was never committed")));
        }
        if ch.ts <= c.after {
            return Err(fail(format!("{ch:?} is not after its start {:?}", c.after)));
        }
        if ch.ts > e.resolved {
            return Err(fail(format!("{ch:?} emitted above the resolved mark {:?}", e.resolved)));
        }
        if let Some(prev) = i.checked_sub(1).map(|p| &c.emitted[p].change) {
            if prev == ch {
                return Err(fail(format!("{ch:?} emitted twice")));
            }
            if prev > ch {
                return Err(fail(format!("{ch:?} emitted after {prev:?}: out of order")));
            }
        }
    }
    check_transactions(known, c).map_err(fail)?;
    let emitted: BTreeSet<&Change> = c.emitted.iter().map(|e| &e.change).collect();
    let owed = known.iter().filter(|k| k.ts > c.after && k.ts <= c.resolved);
    match owed.into_iter().find(|k| !emitted.contains(*k)) {
        Some(k) => Err(fail(format!(
            "{k:?} at or below the resolved mark {:?} was not emitted",
            c.resolved
        ))),
        None => Ok(()),
    }
}

/// The changes of each transaction the cursor emitted come one after another, and all of them.
fn check_transactions(known: &BTreeSet<&Change>, c: &CursorLog) -> Result<(), String> {
    let mut at: BTreeMap<u128, Vec<usize>> = BTreeMap::new();
    for (i, e) in c.emitted.iter().enumerate() {
        if let Some(t) = e.change.txn {
            at.entry(t).or_default().push(i);
        }
    }
    for (t, idx) in at {
        if idx.windows(2).any(|w| w[1] != w[0] + 1) {
            return Err(format!("transaction {t} split by other changes"));
        }
        let all = known.iter().filter(|k| k.txn == Some(t)).count();
        if all != idx.len() {
            return Err(format!(
                "transaction {t} emitted in part ({} of {all} changes)",
                idx.len()
            ));
        }
    }
    Ok(())
}
