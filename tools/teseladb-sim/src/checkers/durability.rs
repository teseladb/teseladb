//! Durability: every acknowledged write is reflected in a later linearizable read, or
//! superseded by writes that followed it. Every written value is unique, so a value read names the
//! write that produced it, and following each write's expected value back gives the chain of values
//! the key went through: every write acknowledged before the read started has to be in that chain.
//! Unlike the linearizability search it has no size limit, so it also covers long histories.

use std::collections::BTreeMap;

use teseladb_types::Nanos;

use crate::checkers::Violation;
use crate::history::{History, OpKind, OpRecord, OpResult};

/// Checks, for every key, its last successful read against the writes acknowledged before it. A
/// write is accounted for if its value is in the chain of the value read, or if the values written
/// over it reach one that a deletion may have removed. A write whose predecessor is not known (a
/// `Write` without an answer) ends the chain, and nothing older is judged; a deletion with no known
/// predecessor may have removed anything, and no loss is claimed for that key.
pub fn check(history: &History) -> Result<(), Violation> {
    let acknowledged = |w: &OpRecord| match (&w.result, w.complete) {
        (OpResult::Ok(_), Some(c)) if w.kind != OpKind::Read => Some(c),
        _ => None,
    };
    let Some((w, read)) = lost(history, acknowledged)?.into_iter().next() else { return Ok(()) };
    Err(Violation::safety(
        "durability",
        format!(
            "{} {}: write {}/{} of {} was acknowledged and is not in the value read by {}/{}",
            w.group,
            w.key,
            w.client,
            w.op_id,
            w.new.as_deref().unwrap_or("-"),
            read.client,
            read.op_id
        ),
    ))
}

/// The writes `acknowledged` names (with the instant of their acknowledgement) that are lost, each
/// with the last successful read of its key: acknowledged before that read started, and their value
/// is neither in the chain of the value read nor superseded, as [`check`] judges. In key order, and
/// in history order within a key.
pub fn lost(
    history: &History,
    acknowledged: impl Fn(&OpRecord) -> Option<Nanos>,
) -> Result<Vec<(&OpRecord, &OpRecord)>, Violation> {
    let mut found = Vec::new();
    let mut by_key: BTreeMap<(&str, &str), Vec<&OpRecord>> = BTreeMap::new();
    for op in &history.ops {
        by_key.entry((&op.group, &op.key)).or_default().push(op);
    }
    for ((group, key), ops) in by_key {
        let last_read = ops
            .iter()
            .filter_map(|o| match (&o.kind, &o.result) {
                (OpKind::Read, OpResult::Ok(v)) => Some((*o, v)),
                _ => None,
            })
            .max_by_key(|(o, _)| o.complete);
        let Some((read, value)) = last_read else { continue };
        let effective = |o: &&&OpRecord| {
            o.kind != OpKind::Read && !matches!(o.result, OpResult::Fail | OpResult::Rejected)
        };
        // Value → value it replaced (`None` for a write whose predecessor is not known), for
        // every write that may have taken effect.
        let mut prev: BTreeMap<&str, Option<Option<&str>>> = BTreeMap::new();
        // Value → values written over it, and values a deletion may have removed.
        let mut next: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        let mut deleted: Vec<&str> = Vec::new();
        // Values of writes whose predecessor is not known: they may follow any value.
        let mut blind: Vec<&str> = Vec::new();
        let mut blind_delete = false;
        for w in ops.iter().filter(effective) {
            let known = (w.kind == OpKind::Cas).then_some(w.expected.as_deref());
            match (&w.new, known) {
                (Some(new), _) => {
                    prev.insert(new, known);
                    match known {
                        Some(Some(old)) => next.entry(old).or_default().push(new),
                        Some(None) => {}
                        None => blind.push(new),
                    }
                }
                (None, Some(Some(old))) => deleted.push(old),
                (None, Some(None)) => {}
                (None, None) => blind_delete = true,
            }
        }
        let mut chain = Vec::new();
        // The chain reached a value whose predecessor is not known: nothing older can be judged.
        let mut uncertain = false;
        let mut cur = value.as_deref();
        while let Some(v) = cur {
            if chain.len() > prev.len() {
                return Err(Violation::safety(
                    "durability",
                    format!("{group} {key}: the chain of values loops at {v}"),
                ));
            }
            let Some(&p) = prev.get(v) else {
                return Err(Violation::safety(
                    "durability",
                    format!("{group} {key}: read {v}, which no write produced"),
                ));
            };
            chain.push(v);
            match p {
                Some(p) => cur = p,
                None => {
                    uncertain = true;
                    cur = None;
                }
            }
        }
        // A write superseded by values that a deletion may have removed is accounted for.
        let superseded = |v: &str| {
            let mut seen = vec![v];
            let mut i = 0;
            while let Some(&x) = seen.get(i) {
                if deleted.contains(&x) {
                    return true;
                }
                for &n in next.get(x).map_or(&[][..], Vec::as_slice).iter().chain(&blind) {
                    if !seen.contains(&n) {
                        seen.push(n);
                    }
                }
                i += 1;
            }
            false
        };
        if uncertain || blind_delete {
            continue;
        }
        found.extend(
            ops.iter()
                .filter(|w| {
                    acknowledged(w).is_some_and(|c| c < read.invoke)
                        && w.new.as_deref().is_some_and(|n| !chain.contains(&n) && !superseded(n))
                })
                .map(|w| (*w, read)),
        );
    }
    Ok(found)
}
