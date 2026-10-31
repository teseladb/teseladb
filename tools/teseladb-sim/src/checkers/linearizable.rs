//! Linearizability of a CAS register per key: backtracking search over the real-time
//! order (Wing–Gong, with Lowe's pruning) and memoization of the already explored states
//! `(linearized operations, value)`. An operation without a response (`Unknown`) may take effect at
//! any point after its invocation or never.

use std::collections::{BTreeMap, HashSet};

use crate::checkers::Violation;
use crate::history::{History, OpKind, OpRecord, OpResult};

/// Maximum operations per key and seed.
pub const MAX_OPS_PER_KEY: usize = 300;

/// Checks every key of the history separately.
pub fn check(history: &History) -> Result<(), Violation> {
    let mut by_key: BTreeMap<(&str, &str), Vec<&OpRecord>> = BTreeMap::new();
    for op in &history.ops {
        by_key.entry((&op.group, &op.key)).or_default().push(op);
    }
    for ((group, key), ops) in by_key {
        if ops.len() > MAX_OPS_PER_KEY {
            return Err(Violation::safety(
                "linearizable",
                format!("{group} {key}: {} operations (maximum {MAX_OPS_PER_KEY})", ops.len()),
            ));
        }
        if !linearizable(&ops) {
            return Err(Violation::safety(
                "linearizable",
                format!("{group} {key}: not linearizable ({} operations)", ops.len()),
            ));
        }
    }
    Ok(())
}

/// Applies `op` to value `cur` if its result is possible there; returns the next value.
fn step(op: &OpRecord, cur: &Option<String>) -> Option<Option<String>> {
    match (&op.kind, &op.result) {
        (OpKind::Read, OpResult::Ok(v)) => (v == cur).then(|| cur.clone()),
        // A read without a response or a failed one changes nothing and imposes nothing.
        (OpKind::Read, _) => Some(cur.clone()),
        // Its predecessor is not known: it applies whatever the value is.
        (OpKind::Write, OpResult::Ok(_) | OpResult::Unknown) => Some(op.new.clone()),
        (OpKind::Write, OpResult::Fail | OpResult::Rejected) => Some(cur.clone()),
        (_, OpResult::Ok(_)) => (op.expected == *cur).then(|| op.new.clone()),
        (_, OpResult::Fail) => (op.expected != *cur).then(|| cur.clone()),
        // Refused before it was evaluated: it changes nothing and imposes nothing.
        (_, OpResult::Rejected) => Some(cur.clone()),
        (_, OpResult::Unknown) => (op.expected == *cur).then(|| op.new.clone()),
    }
}

fn linearizable(ops: &[&OpRecord]) -> bool {
    // A read without a response, and an operation refused before it was evaluated, impose nothing:
    // they are left out.
    let ops: Vec<&OpRecord> = ops
        .iter()
        .copied()
        .filter(|o| o.result != OpResult::Rejected)
        .filter(|o| o.kind != OpKind::Read || o.result != OpResult::Unknown)
        .collect();
    let n = ops.len();
    // Values some operation needs to see: read, or expected by a write that may have been applied.
    // Values are unique per write, so a write without a response whose value nobody needs can only
    // take effect once every required operation left is a rejected one (after it, nothing else
    // could succeed), or before a failed cas, which it may be what made fail; applying it at any
    // other point is a dead branch and is not explored.
    let needed: HashSet<&str> = ops
        .iter()
        .filter_map(|o| match (&o.kind, &o.result) {
            (OpKind::Read, OpResult::Ok(v)) => v.as_deref(),
            (OpKind::Read | OpKind::Write, _) | (_, OpResult::Fail) => None,
            _ => o.expected.as_deref(),
        })
        .collect();
    // Absence is needed too when a read found nothing or a write expects it: then a deletion
    // without a response may be what they saw.
    let absence_needed = ops.iter().any(|o| match (&o.kind, &o.result) {
        (OpKind::Read, OpResult::Ok(v)) => v.is_none(),
        (OpKind::Read | OpKind::Write, _) | (_, OpResult::Fail) => false,
        _ => o.expected.is_none(),
    });
    let unseen_write = |o: &OpRecord| {
        o.kind != OpKind::Read
            && o.result == OpResult::Unknown
            && o.new.as_deref().map_or(!absence_needed, |v| !needed.contains(v))
    };
    let ops = &ops[..];
    let words = n.div_ceil(64);
    // Operations without a response may stay unlinearized.
    let must: Vec<bool> =
        ops.iter().map(|o| o.complete.is_some() && o.result != OpResult::Unknown).collect();
    let end = |o: &OpRecord| o.complete.map_or(u64::MAX, |c| c.0);
    let mut seen: HashSet<(Vec<u64>, Option<String>)> = HashSet::new();
    let mut stack: Vec<(Vec<u64>, Option<String>)> = vec![(vec![0; words], None)];
    while let Some((done, cur)) = stack.pop() {
        let is_done = |i: usize| done[i / 64] >> (i % 64) & 1 == 1;
        if (0..n).all(|i| is_done(i) || !must[i]) {
            return true;
        }
        if !seen.insert((done.clone(), cur.clone())) {
            continue;
        }
        // Only those that started before some pending one finished may go now.
        let horizon =
            (0..n).filter(|&i| !is_done(i)).map(|i| end(ops[i])).min().unwrap_or(u64::MAX);
        let only_rejected_left =
            (0..n).all(|i| is_done(i) || !must[i] || ops[i].result == OpResult::Fail);
        for i in (0..n).filter(|&i| !is_done(i) && ops[i].invoke.0 <= horizon) {
            // A failed cas that can still go after it may have seen the value it wrote: then it
            // may go now too.
            let a_failure_needs_it = (0..n).any(|j| {
                !is_done(j) && ops[j].result == OpResult::Fail && end(ops[j]) >= ops[i].invoke.0
            });
            if unseen_write(ops[i]) && !only_rejected_left && !a_failure_needs_it {
                continue;
            }
            if let Some(next) = step(ops[i], &cur) {
                let mut d = done.clone();
                d[i / 64] |= 1 << (i % 64);
                stack.push((d, next));
            }
        }
    }
    false
}
