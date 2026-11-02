//! `reshard` workload (018 T002, FR-002, FR-013): a collection of [`DOCS`] documents rewritten in a
//! new key order while clients write it, with the steps of `teseladb-wire`'s resharding
//! (`reshard::next_action`, the order of `spec/tla/Reshard.tla`) driven by one client over the
//! transaction workloads' two ranges. The old order lives in the first range and the new one in
//! the second, with the collection's state (`old`, `critical`, `new`) next to it.
//!
//! - Writers read the state and write one document in the order it names; in the critical section
//!   they write nothing. Each write also rewrites the state, as a donor range refuses the writes
//!   of the critical section: a writer that read `old` cannot commit after the critical section
//!   began.
//! - The resharder copies the old order at its read mark (the clone), catches up by copying the
//!   documents whose old version changed since its last copy (the double write), starts the
//!   critical section, catches up to its mark, renames (state `new`) and catches up to the
//!   rename's mark.
//! - Readers read the documents in the order the state names.
//!
//! The `reshard` checker: every committed read of the new order, and the last read after the
//! rename, shows for each document the value of its last committed write at or below the read's
//! mark (an unknown commit may or may not be there); and the resharding gets past its copy.

use std::collections::BTreeMap;

use teseladb_hlc::Hlc;
use teseladb_txn::coordinator::Outcome;
use teseladb_wire::commands::reshard::{Action, Phase, next_action};

use crate::checkers::Violation;
use crate::workloads::Workload;
use crate::workloads::txn::{Finished, Op, Script, Step, TxnWorkload};
use crate::world::World;

/// Name of the checker.
pub const CHECKER: &str = "reshard";
/// Documents.
const DOCS: usize = 4;
/// The old order's keys (first range) and the new one's (second range).
const OLD: [&[u8]; DOCS] = [b"\x01a", b"\x01b", b"\x01c", b"\x01d"];
const NEW: [&[u8]; DOCS] = [b"\x01n", b"\x01o", b"\x01p", b"\x01q"];
/// The collection's state.
const STATE: &[u8] = b"\x01z";
const CRITICAL: &[u8] = b"critical";
const RENAMED: &[u8] = b"new";
const OLD_STATE: &[u8] = b"old";
/// Clients: the last one reshards.
const CLIENTS: u32 = 4;
const RESHARDER: u32 = CLIENTS - 1;
/// Writes this close to a broken read's mark (ms) go in its detail.
const NEAR_MS: u64 = 300;
/// Share of the writers' turns that read.
const READ_SHARE: f64 = 0.3;

/// What a client's transaction is doing.
#[derive(Clone, Debug)]
enum Turn {
    /// A writer's write of document `doc` with value `value`.
    Write { doc: usize, value: Vec<u8> },
    /// A read of every document.
    Read,
    /// The resharder's step.
    Step(Action),
}

/// The script of `reshard`.
#[derive(Debug)]
pub struct Reshard {
    turns: BTreeMap<u32, Turn>,
    phase: Phase,
    /// The read mark of the resharder's last copy, and the mark it must reach.
    mark: Hlc,
    target: Hlc,
    /// The old versions the copy holds.
    copied: [Option<Vec<u8>>; DOCS],
    /// Committed writes per document: mark and value; and the documents with a write whose
    /// commit is unknown.
    writes: Vec<Vec<(Hlc, Vec<u8>)>>,
    unknown: Vec<Vec<Vec<u8>>>,
    /// Reads of the new order to check: mark and values.
    reads: Vec<(Hlc, Vec<Option<Vec<u8>>>)>,
    seq: u64,
}

impl Default for Reshard {
    fn default() -> Reshard {
        Reshard {
            turns: BTreeMap::new(),
            phase: Phase::Clone,
            mark: Hlc { physical: 0, logical: 0 },
            target: Hlc { physical: 0, logical: 0 },
            copied: Default::default(),
            writes: vec![Vec::new(); DOCS],
            unknown: vec![Vec::new(); DOCS],
            reads: Vec::new(),
            seq: 0,
        }
    }
}

impl Reshard {
    /// The workload over `nodes` nodes of `cores` cores.
    pub fn boxed(nodes: u32, cores: u16) -> Box<dyn Workload> {
        Box::new(TxnWorkload::new("reshard", (nodes, cores), CLIENTS, Reshard::default()))
    }

    fn resharder_plan(&mut self) -> Vec<Step> {
        let action = next_action(self.phase, self.mark >= self.target);
        self.turns.insert(RESHARDER, Turn::Step(action));
        match action {
            Action::Copy | Action::CatchUp => {
                let mut steps: Vec<Step> = OLD.iter().map(|k| Step::Read(k.to_vec())).collect();
                steps.push(Step::Expand);
                steps.push(Step::Commit);
                steps
            }
            Action::EnterCritical | Action::Rename => {
                vec![Step::Read(STATE.to_vec()), Step::Write(STATE.to_vec()), Step::Commit]
            }
            Action::Done => {
                let mut steps = vec![Step::Read(STATE.to_vec())];
                steps.extend(NEW.iter().map(|k| Step::Read(k.to_vec())));
                steps.push(Step::Commit);
                steps
            }
        }
    }

    /// The new order's values in a committed read, when it read them.
    fn read_new(ops: &[Op]) -> Option<Vec<Option<Vec<u8>>>> {
        let values: BTreeMap<&[u8], &Option<Vec<u8>>> = ops
            .iter()
            .filter_map(|op| match op {
                Op::Read(k, v) => Some((k.as_slice(), v)),
                Op::Write(..) => None,
            })
            .collect();
        NEW.iter().map(|k| values.get(k).map(|v| (*v).clone())).collect()
    }

    fn check_read(&self, at: Hlc, values: &[Option<Vec<u8>>]) -> Result<(), Violation> {
        for (doc, got) in values.iter().enumerate() {
            let last =
                self.writes[doc].iter().filter(|(ts, _)| *ts <= at).max_by_key(|(ts, _)| *ts);
            let want = last.map(|(_, v)| v.clone());
            let maybe = got.as_ref().is_some_and(|g| self.unknown[doc].contains(g));
            if *got != want && !maybe {
                // The writes around the read's mark, to tell which one the copy missed.
                let near: Vec<_> = self.writes[doc]
                    .iter()
                    .filter(|(ts, _)| ts.physical.abs_diff(at.physical) <= NEAR_MS)
                    .collect();
                return Err(Violation::safety(
                    CHECKER,
                    format!(
                        "a read of the new order at {at:?} shows {got:?} for document {doc}, whose \
                         last committed write at or below it is {want:?}; writes near it: {near:?}"
                    ),
                ));
            }
        }
        Ok(())
    }
}

fn state_of(read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> Vec<u8> {
    read.get(STATE).cloned().flatten().unwrap_or_else(|| OLD_STATE.to_vec())
}

impl Script for Reshard {
    fn plan(&mut self, w: &mut World, client: u32) -> Vec<Step> {
        if client == RESHARDER {
            return self.resharder_plan();
        }
        if w.rng.chance(READ_SHARE) {
            self.turns.insert(client, Turn::Read);
        } else {
            let doc = w.rng.range(0, DOCS as u64) as usize;
            self.seq += 1;
            let value = format!("{client}:{}", self.seq).into_bytes();
            self.turns.insert(client, Turn::Write { doc, value });
        }
        vec![Step::Read(STATE.to_vec()), Step::Expand, Step::Commit]
    }

    fn expand(&mut self, client: u32, read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>) -> Vec<Step> {
        let state = state_of(read);
        match self.turns.get(&client) {
            Some(Turn::Write { doc, .. }) if state.as_slice() == OLD_STATE => {
                vec![Step::Write(STATE.to_vec()), Step::Write(OLD[*doc].to_vec())]
            }
            Some(Turn::Write { doc, .. }) if state.as_slice() == RENAMED => {
                vec![Step::Write(NEW[*doc].to_vec())]
            }
            // The critical section refuses the write.
            Some(Turn::Write { .. }) => Vec::new(),
            Some(Turn::Read) => {
                let keys = if state.as_slice() == RENAMED { NEW } else { OLD };
                keys.iter().map(|k| Step::Read(k.to_vec())).collect()
            }
            Some(Turn::Step(_)) => {
                // A copy writes the documents whose old version changed since the last one.
                (0..DOCS)
                    .filter(|d| read.get(OLD[*d]).cloned().flatten() != self.copied[*d])
                    .map(|d| Step::Write(NEW[d].to_vec()))
                    .collect()
            }
            None => Vec::new(),
        }
    }

    fn value(
        &mut self,
        client: u32,
        key: &[u8],
        read: &BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    ) -> Option<Vec<u8>> {
        match self.turns.get(&client) {
            Some(Turn::Write { value, .. }) if key != STATE => Some(value.clone()),
            Some(Turn::Write { .. }) => Some(state_of(read)),
            Some(Turn::Step(Action::EnterCritical)) => Some(CRITICAL.to_vec()),
            Some(Turn::Step(Action::Rename)) => Some(RENAMED.to_vec()),
            Some(Turn::Step(_)) => {
                let doc = NEW.iter().position(|k| *k == key)?;
                read.get(OLD[doc]).cloned().flatten()
            }
            _ => None,
        }
    }

    fn ended(&mut self, client: u32, done: Finished) {
        let turn = self.turns.remove(&client);
        let written = |key: &[u8]| {
            done.ops.iter().find_map(|op| match op {
                Op::Write(k, v) if k == key => Some(v.clone()),
                _ => None,
            })
        };
        let wrote_doc = done.ops.iter().any(|op| {
            matches!(op, Op::Write(k, _) if OLD.contains(&k.as_slice()) || NEW.contains(&k.as_slice()))
        });
        if let (Some(Turn::Write { doc, value }), true) = (&turn, wrote_doc) {
            match done.outcome {
                Outcome::Committed(ts) => self.writes[*doc].push((ts, value.clone())),
                Outcome::Unknown(_) => self.unknown[*doc].push(value.clone()),
                Outcome::Aborted | Outcome::Failed(_) => {}
            }
        }
        let Outcome::Committed(ts) = done.outcome else { return };
        if let Some(values) = Self::read_new(&done.ops) {
            self.reads.push((done.read_ts, values));
        }
        let Some(Turn::Step(action)) = turn else { return };
        match action {
            Action::Copy | Action::CatchUp => {
                for (d, k) in OLD.iter().enumerate() {
                    let old = done.ops.iter().find_map(|op| match op {
                        Op::Read(r, v) if r.as_slice() == *k => Some(v.clone()),
                        _ => None,
                    });
                    if written(NEW[d]).is_some()
                        && let Some(v) = old
                    {
                        self.copied[d] = v;
                    }
                }
                self.mark = done.read_ts;
                if action == Action::Copy {
                    self.phase = Phase::Apply;
                    self.target = ts;
                }
            }
            Action::EnterCritical => {
                self.phase = Phase::Critical;
                self.target = ts;
            }
            Action::Rename => {
                self.phase = Phase::Committed;
                self.target = ts;
            }
            Action::Done => {}
        }
    }

    fn finish(&mut self) -> Result<(), Violation> {
        for (at, values) in &self.reads {
            self.check_read(*at, values)?;
        }
        // A short run may end before the rename; it must at least have copied.
        if self.phase == Phase::Clone {
            return Err(Violation::liveness(CHECKER, "the resharding never copied the collection"));
        }
        Ok(())
    }
}
