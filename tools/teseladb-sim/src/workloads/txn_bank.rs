//! `txn-bank` workload (008 T004, FR-016, SC-001): accounts in two ranges, transactions that move
//! money between two of them and transactions that read every account. Snapshot isolation keeps
//! the total: every read of all the accounts in a committed transaction adds up to it.

use std::collections::BTreeMap;

use teseladb_txn::coordinator::Outcome;

use crate::checkers::Violation;
use crate::workloads::Workload;
use crate::workloads::txn::{Finished, Op, Script, Step, TxnWorkload};
use crate::world::World;

/// Accounts: three in each range.
const ACCOUNTS: [&[u8]; 6] = [b"\x01a", b"\x01b", b"\x01c", b"\x01n", b"\x01o", b"\x01p"];
/// The balance of an account nobody wrote yet.
const OPENING: i64 = 100;
/// Clients.
const CLIENTS: u32 = 4;
/// Transfers committed this close to a broken read's mark go in its detail.
const NEAR_MS: u64 = 2_000;
/// Name of the checker in its violations.
pub const NAME: &str = "bank-total";

fn balance(v: Option<&[u8]>) -> i64 {
    v.and_then(|b| String::from_utf8_lossy(b).parse().ok()).unwrap_or(OPENING)
}

/// The script of `txn-bank`.
#[derive(Debug, Default)]
pub struct Bank {
    /// The account each client's transfer takes from and the amount, while it runs.
    transfers: BTreeMap<u32, (Vec<u8>, i64)>,
    /// The first read of every account whose total was wrong.
    broken: Option<String>,
    committed: u64,
    /// Committed transfers: commit mark and what they wrote.
    done: Vec<(teseladb_hlc::Hlc, Vec<Op>)>,
}

impl Bank {
    /// The workload over `nodes` nodes of `cores` cores.
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn Workload> {
        Box::new(TxnWorkload::new("txn-bank", (nodes, cores), CLIENTS, Bank::default()))
    }
}

impl Script for Bank {
    fn plan(&mut self, w: &mut World, client: u32) -> Vec<Step> {
        if w.rng.chance(0.3) {
            // A read-only transaction holds no intents: before reading, it rewrites an account
            // outside the transaction, a plain write that waits for the transfers holding it.
            let touched = ACCOUNTS[w.rng.range(0, ACCOUNTS.len() as u64) as usize].to_vec();
            let mut steps = vec![Step::Touch(touched)];
            steps.extend(ACCOUNTS.iter().map(|a| Step::Read(a.to_vec())));
            steps.push(Step::Commit);
            return steps;
        }
        let from = w.rng.range(0, ACCOUNTS.len() as u64) as usize;
        let to = (from + 1 + w.rng.range(0, ACCOUNTS.len() as u64 - 1) as usize) % ACCOUNTS.len();
        let (a, b) = (ACCOUNTS[from].to_vec(), ACCOUNTS[to].to_vec());
        self.transfers.insert(client, (a.clone(), w.rng.range(1, 11) as i64));
        vec![
            Step::Read(a.clone()),
            Step::Read(b.clone()),
            Step::Write(a),
            Step::Write(b),
            Step::Commit,
        ]
    }

    fn value(
        &mut self,
        client: u32,
        key: &[u8],
        read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> Option<Vec<u8>> {
        let now = balance(read.get(key).and_then(|v| v.as_deref()));
        let next = match self.transfers.get(&client) {
            Some((from, amount)) if from.as_slice() == key => now - amount,
            Some((_, amount)) => now + amount,
            None => now,
        };
        Some(next.to_string().into_bytes())
    }

    fn ended(&mut self, client: u32, done: Finished) {
        self.transfers.remove(&client);
        let reads: Vec<(&Vec<u8>, i64)> = done
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Read(k, v) => Some((k, balance(v.as_deref()))),
                Op::Write(..) => None,
            })
            .collect();
        let committed = matches!(done.outcome, Outcome::Committed(_));
        if let Outcome::Committed(ts) = done.outcome
            && done.ops.iter().any(|op| matches!(op, Op::Write(..)))
        {
            self.done.push((ts, done.ops.clone()));
        }
        self.committed += u64::from(committed);
        let total: i64 = reads.iter().map(|(_, b)| b).sum();
        let all = reads.len() == ACCOUNTS.len();
        let want = OPENING * ACCOUNTS.len() as i64;
        if committed && all && total != want && self.broken.is_none() {
            // The transfers that committed near its mark, to tell which one it saw in part.
            let near: Vec<_> = self
                .done
                .iter()
                .filter(|(ts, _)| ts.physical.abs_diff(done.read_ts.physical) <= NEAR_MS)
                .collect();
            self.broken = Some(format!(
                "a committed read of every account at {:?} adds up to {total}, not {want}: \
                 {reads:?}; transfers committed near it: {near:?}",
                done.read_ts
            ));
        }
    }

    fn finish(&mut self) -> Result<(), Violation> {
        if let Some(d) = self.broken.take() {
            return Err(Violation::safety(NAME, d));
        }
        if self.committed == 0 {
            return Err(Violation::liveness(NAME, "no transaction committed"));
        }
        Ok(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use teseladb_hlc::Hlc;
    use teseladb_types::Nanos;

    use super::*;

    /// A read of every account: the opening balance but `moved` more on the first two.
    fn read_all(moved: (i64, i64), outcome: Outcome) -> Finished {
        let ops = ACCOUNTS
            .iter()
            .enumerate()
            .map(|(i, k)| {
                let b = OPENING + [moved.0, moved.1].get(i).copied().unwrap_or(0);
                Op::Read(k.to_vec(), Some(b.to_string().into_bytes()))
            })
            .collect();
        let read_ts = Hlc { physical: 10_000, logical: 0 };
        Finished { ops, read_ts, outcome, began: Nanos::ZERO, acked: None, in_window: true }
    }

    #[test]
    fn a_committed_read_of_every_account_must_add_up_to_the_opening_total() {
        let mut bank = Bank::default();
        assert!(bank.finish().expect_err("nothing committed").liveness);
        let at = |p| Outcome::Committed(Hlc { physical: p, logical: 0 });
        // A transfer near the read, one far from it, and a read that did not commit.
        let mut transfer = read_all((-5, 5), at(10_500));
        transfer.ops.push(Op::Write(ACCOUNTS[0].to_vec(), Some(b"1".to_vec())));
        bank.ended(0, transfer.clone());
        transfer.outcome = at(50_000);
        bank.ended(0, transfer);
        bank.ended(1, read_all((0, 1), Outcome::Aborted));
        bank.finish().expect("what committed adds up");
        bank.ended(1, read_all((0, 1), at(10_000)));
        bank.ended(1, read_all((0, 2), at(10_000)));
        let v = bank.finish().expect_err("one more than the total");
        let total = OPENING * ACCOUNTS.len() as i64 + 1;
        assert!(v.detail.contains(&format!("adds up to {total}")), "{}", v.detail);
        assert!(v.detail.contains("10500") && !v.detail.contains("50000"), "{}", v.detail);
    }
}
