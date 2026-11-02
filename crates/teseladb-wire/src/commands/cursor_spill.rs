//! Open cursors within the node's part of memory for them (012 FR-013, D-105): a cursor whose
//! computed results would take the open cursors past that part writes them to the node's
//! temporary store as one run, and its `getMore`s read them back a piece at a time. The run is
//! removed when the cursor ends, is killed or expires.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

use teseladb_bson::{Document, RawDoc};
use teseladb_exec::pipeline::spill::SpillStore;

use super::mutation::from_mql;
use crate::error::CommandError;

/// The store of the cursors' runs.
pub type Store = Arc<dyn SpillStore + Send + Sync>;

/// Bytes read back from a run per refill.
pub const REFILL_BYTES: usize = 1 << 20;

/// A run of a cursor's results in the store, removed when the last copy of the cursor goes.
pub struct SpilledRun {
    store: Store,
    run: u64,
}

impl Drop for SpilledRun {
    fn drop(&mut self) {
        self.store.remove(self.run);
    }
}

impl fmt::Debug for SpilledRun {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpilledRun").field("run", &self.run).finish()
    }
}

impl PartialEq for SpilledRun {
    fn eq(&self, other: &Self) -> bool {
        self.run == other.run
    }
}

/// Bytes `docs` take encoded.
pub fn bytes_of(docs: &VecDeque<Document>) -> usize {
    docs.iter().map(|d| d.to_bytes().map_or(0, |b| b.len())).sum()
}

/// Writes `docs` to `store` as one run and empties them.
pub fn spill(
    store: &Store,
    docs: &mut VecDeque<Document>,
) -> Result<Arc<SpilledRun>, CommandError> {
    let mut bytes = Vec::new();
    for d in docs.drain(..) {
        bytes.extend_from_slice(&d.to_bytes().map_err(|e| {
            CommandError::new(crate::error::INTERNAL_ERROR, format!("a cursor's result: {e:?}"))
        })?);
    }
    let run = store.write(&bytes).map_err(from_mql)?;
    Ok(Arc::new(SpilledRun { store: store.clone(), run }))
}

/// Reads documents of `run` from `offset` into `out` until about [`REFILL_BYTES`] or the run's
/// end: the new offset (`None` at the end) and the bytes read.
pub fn refill(
    run: &SpilledRun,
    offset: u64,
    out: &mut VecDeque<Document>,
) -> Result<(Option<u64>, usize), CommandError> {
    let mut at = offset;
    let mut read = 0usize;
    while read < REFILL_BYTES {
        let head = run.store.read(run.run, at, 4).map_err(from_mql)?;
        let Some(len) = head.get(..4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])) else {
            return Ok((None, read));
        };
        let len = len as usize;
        let body = run.store.read(run.run, at, len).map_err(from_mql)?;
        let doc = RawDoc::from_bytes(&body).map_err(|e| {
            CommandError::new(crate::error::INTERNAL_ERROR, format!("a cursor's run: {e}"))
        })?;
        out.push_back(Document::from(doc));
        at += len as u64;
        read += len;
    }
    Ok((Some(at), read))
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use teseladb_exec::pipeline::spill::MemorySpill;

    #[test]
    fn a_run_reads_back_in_order_and_goes_with_its_last_copy() {
        let mem = Arc::new(MemorySpill::default());
        let store: Store = mem.clone();
        let mut docs: VecDeque<Document> =
            (0..5).map(|i| Document::new().with("i", i).with("s", "x".repeat(300_000))).collect();
        let total = bytes_of(&docs);
        assert!(total > REFILL_BYTES);
        let run = spill(&store, &mut docs).unwrap();
        assert!(docs.is_empty());
        let copy = run.clone();
        let mut out = VecDeque::new();
        let (next, read) = refill(&run, 0, &mut out).unwrap();
        assert!(out.len() < 5 && read >= REFILL_BYTES);
        let mut at = next;
        while let Some(o) = at {
            at = refill(&run, o, &mut out).unwrap().0;
        }
        let got: Vec<i32> = out
            .iter()
            .map(|d| match d.get("i") {
                Some(teseladb_bson::Value::Int32(n)) => *n,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(got, [0, 1, 2, 3, 4]);
        assert_eq!(*run, *copy);
        assert!(format!("{run:?}").contains("run"));
        drop(run);
        assert_eq!(mem.len(), 1, "a copy still holds it");
        drop(copy);
        assert!(mem.is_empty());
    }
}
