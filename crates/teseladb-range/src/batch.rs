//! A batch of writes outside a transaction (FR-023): `insertMany`, or a bulk write of 006, on one
//! range. Each document is its own `Write` entry, so it gets its own mark, above the one before it
//! in the log (with `succ`, FR-025), as each insert of the reference server gets its own optime;
//! Raft carries many of them in one `AppendReq`, and a batch of any size spreads over as many
//! entries as it has documents.
//!
//! An unordered batch keeps up to [`BATCH_WINDOW`] documents in flight and goes on after a
//! failure. An ordered one proposes a document only once every document before it in flight is
//! proposed in the leader's current term, so the log holds them in the batch's order and a
//! document is never committed without the ones before it; it stops at the first failure, and
//! the documents after it never run. `Overloaded` (a full proposal queue, or the clock that ran
//! out of marks in its millisecond and waits for the next one) holds the document back to the
//! next [`Batch::pump`]; only when the batch makes no progress for [`OVERLOADED_PATIENCE`] is it
//! the document's answer.
//!
//! The driver is sans-I/O: the embedder calls [`Batch::pump`] after each poll of the host and
//! hands it the host's answers with [`Batch::take_replies`].

use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use teseladb_hlc::Hlc;
use teseladb_multiraft::{MultiRaftCore, StateMachine};
use teseladb_types::{GroupId, Nanos};

use crate::ack::WriteConcern;
use crate::command::DedupKey;
use crate::host::{Mutation, MutationError, RangeError, RangeHost, Reply, ReqId, WriteReq};
use crate::mutate::Outcome;
use crate::sm::RangeState;

/// Documents of a batch in flight at most.
pub const BATCH_WINDOW: usize = 256;
/// How long a batch keeps proposing a document again after `Overloaded` without any document
/// answered in between.
pub const OVERLOADED_PATIENCE: Nanos = Nanos::from_secs(2);

/// A document of a batch.
pub struct BatchWrite {
    /// Keys of the document (latched before it is read).
    pub keys: Vec<Vec<u8>>,
    /// The mutation (kept to propose it again after `Overloaded`).
    pub m: Rc<dyn Mutation>,
    /// The statement of a retryable write (FR-011).
    pub dedup: Option<DedupKey>,
}

/// The semantics of a batch (the reference server's `ordered`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    /// In order, stopping at the first failure.
    Ordered,
    /// Every document, whatever fails.
    Unordered,
}

/// A document's answer: `None` if it never ran (after the failure of an ordered batch).
pub type DocResult = Option<Result<Reply, RangeError>>;

/// A mutation the batch keeps after handing it to the host.
struct Shared(Rc<dyn Mutation>);

impl Mutation for Shared {
    fn eval(&self, current: &[Option<(u8, &[u8])>], ts: Hlc) -> Result<Outcome, MutationError> {
        self.0.eval(current, ts)
    }
}

/// A batch of writes on range `g`.
pub struct Batch {
    g: GroupId,
    order: Order,
    wc: WriteConcern,
    writes: Vec<BatchWrite>,
    /// Next document never handed to the host.
    next: usize,
    /// Documents to hand again, after `Overloaded`.
    again: VecDeque<usize>,
    /// The host's requests in flight and their documents (in the order they were handed).
    in_flight: BTreeMap<ReqId, usize>,
    /// Since when every answer is `Overloaded`.
    overloaded_since: Option<Nanos>,
    /// Time of the last pump.
    now: Nanos,
    /// An ordered batch saw a failure.
    stopped: bool,
    results: Vec<DocResult>,
}

impl Batch {
    /// A batch of `writes` on range `g`; each document is answered at write concern `wc`.
    pub fn new(g: GroupId, writes: Vec<BatchWrite>, order: Order, wc: WriteConcern) -> Batch {
        let results = writes.iter().map(|_| None).collect();
        Batch {
            g,
            order,
            wc,
            writes,
            next: 0,
            again: VecDeque::new(),
            in_flight: BTreeMap::new(),
            overloaded_since: None,
            now: Nanos::ZERO,
            stopped: false,
            results,
        }
    }

    /// Whether every document that runs has its answer.
    pub fn is_done(&self) -> bool {
        self.in_flight.is_empty()
            && (self.stopped || (self.next == self.writes.len() && self.again.is_empty()))
    }

    /// The answers by document (call it once the batch is done).
    pub fn results(self) -> Vec<DocResult> {
        self.results
    }

    /// Whether an ordered batch may hand its next document: every one in flight is proposed in
    /// the current term of the range's leader.
    fn in_order<S: StateMachine>(&self, host: &RangeHost, core: &MultiRaftCore<S>) -> bool {
        let Some(term) = core.status(self.g).map(|st| st.hard_state.term) else { return false };
        self.in_flight.keys().all(|&id| host.proposed(id).is_some_and(|(_, t)| t == term))
    }

    /// Hands the host the documents that may go now.
    pub fn pump<S: StateMachine + RangeState>(
        &mut self,
        host: &mut RangeHost,
        core: &mut MultiRaftCore<S>,
        now: Nanos,
        wall_ms: u64,
    ) {
        self.now = now;
        while !self.stopped && self.in_flight.len() < BATCH_WINDOW {
            if self.order == Order::Ordered && !self.in_order(host, core) {
                return;
            }
            let i = match self.again.pop_front() {
                Some(i) => i,
                None if self.next < self.writes.len() => {
                    self.next += 1;
                    self.next - 1
                }
                None => return,
            };
            let w = &self.writes[i];
            let req = WriteReq {
                g: self.g,
                keys: w.keys.clone(),
                m: Box::new(Shared(w.m.clone())),
                dedup: w.dedup,
                wc: self.wc,
            };
            let id = host.mutate(core, req, now, wall_ms);
            self.in_flight.insert(id, i);
        }
    }

    /// Takes the answers of the batch's documents and gives back the others.
    pub fn take_replies(
        &mut self,
        replies: Vec<(ReqId, Result<Reply, RangeError>)>,
    ) -> Vec<(ReqId, Result<Reply, RangeError>)> {
        let mut others = Vec::new();
        for (id, r) in replies {
            match self.in_flight.remove(&id) {
                Some(i) => self.answered(i, r),
                None => others.push((id, r)),
            }
        }
        others
    }

    fn answered(&mut self, i: usize, r: Result<Reply, RangeError>) {
        if r == Err(RangeError::Overloaded) {
            let since = *self.overloaded_since.get_or_insert(self.now);
            if self.now.saturating_since(since) < OVERLOADED_PATIENCE {
                self.again.push_back(i);
                return;
            }
        } else {
            self.overloaded_since = None;
        }
        if r.is_err() && self.order == Order::Ordered {
            self.stopped = true;
        }
        self.results[i] = Some(r);
    }
}
