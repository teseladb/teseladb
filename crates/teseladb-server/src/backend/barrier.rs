//! The statement barrier of a command of several writes (005 FR-036, D-163): while the command
//! runs, each write is answered once its entry is in the leader's log and the backend keeps, for
//! each range, the last entry of each term it wrote; at the end it asks the leader of each range
//! for a barrier with them at the command's write concern.

use std::collections::BTreeMap;
use std::thread;
use std::time::Instant;

use teseladb_range::ack::{EntryId, W, WriteConcern};
use teseladb_range::host::RangeError;
use teseladb_wire::error::{
    CommandError, INTERNAL_ERROR, RangeContext, UNSATISFIABLE_WRITE_CONCERN, from_range,
};
use teseladb_types::GroupId;

use super::{RETRY_PAUSE, ServerBackend};
use crate::data::{DataReply, DataReq};
use crate::data_wire::{RemoteReply, RemoteReq};
use crate::logic::NodeMsg;

/// The entries a command wrote: for each range, the last index of each term.
#[derive(Debug, Default)]
pub(super) struct Statements {
    ranges: BTreeMap<GroupId, BTreeMap<u64, u64>>,
}

impl Statements {
    /// Keeps entry `e`, which a write of range `g` answered with.
    pub(super) fn note(&mut self, g: GroupId, e: EntryId) {
        let last = self.ranges.entry(g).or_default().entry(e.term).or_insert(e.index);
        *last = (*last).max(e.index);
    }
}

/// Why a range's barrier failed.
enum Failed {
    /// The range's error.
    Range(RangeError),
    /// An answer of another kind.
    Answer,
}

/// The write concern a barrier falls back to when the command's asks for more replicas than a
/// range has, as a single write does (`Writer::write`).
const MAJORITY: WriteConcern = WriteConcern { w: W::Majority, j: true };

impl ServerBackend {
    /// The barrier of every range in `statements` at `wc`, range by range.
    pub(super) fn barriers(
        &mut self,
        statements: Statements,
        wc: WriteConcern,
    ) -> Result<(), CommandError> {
        let mut unsatisfiable = false;
        for (g, terms) in statements.ranges {
            let entries: Vec<EntryId> =
                terms.into_iter().map(|(term, index)| EntryId { term, index }).collect();
            match self.barrier(g, &entries, wc) {
                Err(Failed::Range(RangeError::Unsatisfiable)) => {
                    unsatisfiable = true;
                    self.barrier(g, &entries, MAJORITY).map_err(|e| self.barrier_error(e))?;
                }
                r => r.map_err(|e| self.barrier_error(e))?,
            }
        }
        match unsatisfiable {
            true => Err(CommandError::new(UNSATISFIABLE_WRITE_CONCERN, "unsatisfiable")),
            false => Ok(()),
        }
    }

    /// The barrier of range `g`: through the core of the range on this node, to its leader,
    /// asked again while the range has no leader that answers, until the command's deadline. A
    /// range that is gone (merged into another) took its log with it: its entries are in doubt.
    fn barrier(&mut self, g: GroupId, entries: &[EntryId], wc: WriteConcern) -> Result<(), Failed> {
        let until = self.until();
        loop {
            let Some(route) = self.h.routes.entry(g) else {
                return Err(Failed::Range(RangeError::EntryLost));
            };
            let req = RemoteReq::Barrier { entries: entries.to_vec(), wc };
            let core = usize::from(route.core);
            let e = match self.ask(core, |tx| NodeMsg::Data(DataReq::Remote(g, req), tx), until) {
                Some(Ok(DataReply::Remote(RemoteReply::Settled))) => return Ok(()),
                Some(Ok(_)) => return Err(Failed::Answer),
                Some(Err(e)) => e,
                None => RangeError::Timeout,
            };
            let again = matches!(
                e,
                RangeError::NotLeader { .. }
                    | RangeError::Overloaded
                    | RangeError::Timeout
                    | RangeError::Unavailable
                    | RangeError::OutOfRange
            );
            if !again || Instant::now() >= until {
                // Past the deadline a range that did not answer leaves its entries in doubt.
                return Err(Failed::Range(match e {
                    RangeError::OutOfRange => RangeError::EntryLost,
                    e => e,
                }));
            }
            thread::sleep(RETRY_PAUSE);
        }
    }

    fn barrier_error(&self, e: Failed) -> CommandError {
        let max_time_expired = self.max_until.is_some_and(|m| Instant::now() >= m);
        match e {
            Failed::Answer => {
                CommandError::new(INTERNAL_ERROR, "a barrier answered with no confirmation")
            }
            Failed::Range(e) => {
                from_range(&e, RangeContext { retryable_write: false, max_time_expired })
            }
        }
    }
}
