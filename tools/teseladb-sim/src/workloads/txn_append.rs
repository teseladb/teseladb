//! `txn-append` workload (008 T004, FR-016, SC-001): transactions across two ranges append unique
//! values to lists and read whole lists, the history Elle's list-append test reads (Kingsbury and
//! Alvaro, VLDB 2020). Each transaction touches each of its keys once; an append reads the list
//! at the transaction's mark and writes it back with its value at the end. The snapshot isolation
//! checker looks for G0, G1a, G1b, G1c and G-SI in the history, and with the
//! real-time order of the transactions begun while the clocks are within the uncertainty window,
//! for G1c-realtime and G-SI-realtime (a transaction that misses one acknowledged before it began).

use std::collections::BTreeMap;

use teseladb_txn::coordinator::Outcome as TxnOutcome;

use crate::checkers::Violation;
use crate::checkers::snapshot_isolation::{self, MicroOp, Outcome, RealTime, Txn};
use crate::workloads::Workload;
use crate::workloads::txn::{Finished, Op, Script, Step, TxnWorkload};
use crate::world::World;

/// Keys: three in each range.
const KEYS: [&[u8]; 6] = [b"\x01a", b"\x01b", b"\x01c", b"\x01n", b"\x01o", b"\x01p"];
/// Clients.
const CLIENTS: u32 = 4;
/// Most keys a transaction touches.
const MAX_OPS: u64 = 4;

/// The lists, encoded as their values separated by commas.
fn decode(v: Option<&[u8]>) -> Vec<i64> {
    v.map(|b| String::from_utf8_lossy(b).split(',').filter_map(|x| x.parse().ok()).collect())
        .unwrap_or_default()
}

fn encode(list: &[i64]) -> Vec<u8> {
    list.iter().map(i64::to_string).collect::<Vec<_>>().join(",").into_bytes()
}

fn name(key: &[u8]) -> String {
    String::from_utf8_lossy(&key[1..]).into_owned()
}

/// The script of `txn-append`.
#[derive(Debug, Default)]
pub struct Append {
    next: BTreeMap<Vec<u8>, i64>,
    history: Vec<Txn>,
}

impl Append {
    /// The workload over `nodes` nodes of `cores` cores.
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn Workload> {
        Box::new(TxnWorkload::new("txn-append", (nodes, cores), CLIENTS, Append::default()))
    }
}

impl Script for Append {
    fn plan(&mut self, w: &mut World, _client: u32) -> Vec<Step> {
        let n = w.rng.range(1, MAX_OPS + 1) as usize;
        let mut keys: Vec<&[u8]> = KEYS.to_vec();
        let mut steps = Vec::new();
        for _ in 0..n {
            let k = keys.remove(w.rng.range(0, keys.len() as u64) as usize).to_vec();
            steps.push(Step::Read(k.clone()));
            if w.rng.chance(0.6) {
                steps.push(Step::Write(k));
            }
        }
        steps.push(Step::Commit);
        steps
    }

    fn value(
        &mut self,
        _client: u32,
        key: &[u8],
        read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> Option<Vec<u8>> {
        let mut list = decode(read.get(key).and_then(|v| v.as_deref()));
        let next = self.next.entry(key.to_vec()).or_insert(0);
        *next += 1;
        list.push(*next);
        Some(encode(&list))
    }

    fn ended(&mut self, _client: u32, done: Finished) {
        let ops = done
            .ops
            .iter()
            .map(|op| match op {
                Op::Read(k, v) => MicroOp::Read { key: name(k), list: decode(v.as_deref()) },
                Op::Write(k, v) => MicroOp::Append {
                    key: name(k),
                    value: decode(v.as_deref()).last().copied().unwrap_or(0),
                },
            })
            .collect();
        let outcome = match done.outcome {
            TxnOutcome::Committed(_) => Outcome::Committed,
            TxnOutcome::Aborted | TxnOutcome::Failed(_) => Outcome::Aborted,
            TxnOutcome::Unknown(_) => Outcome::Unknown,
        };
        let realtime = done
            .in_window
            .then(|| RealTime { began: done.began.0, acked: done.acked.map(|a| a.0) });
        let id = self.history.len() as u64;
        self.history.push(Txn { id, ops, outcome, realtime });
    }

    fn finish(&mut self) -> Result<(), Violation> {
        if !self.history.iter().any(|t| t.outcome == Outcome::Committed) {
            return Err(Violation::liveness(snapshot_isolation::NAME, "no transaction committed"));
        }
        snapshot_isolation::check(&self.history)
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn a_run_without_a_committed_transaction_fails_liveness() {
        let v = Append::default().finish().expect_err("nothing committed");
        assert_eq!((v.checker, v.liveness), (snapshot_isolation::NAME, true));
    }
}
