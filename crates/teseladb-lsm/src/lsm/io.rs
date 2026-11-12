//! The I/O loop of [`Lsm`]: [`Lsm::poll_io`] hands out one disk operation at a time (block
//! reads first, then the flush, then the background job) and [`Lsm::on_io_done`] takes its
//! result. A full disk keeps the failed operation for a retry; any other error stops the core.
//! A flush or a compaction works in steps and yields between them (FR-030):
//! [`Lsm::poll_io_step`] does one step and says whether more is left.

use teseladb_raftlog::{DISK_FULL_RETRY, Disk, IoDone, IoErrno, IoOp};
use teseladb_types::Nanos;

use super::{Completions, Job, Lsm, Request, Slot};
use crate::compaction::{CompactJob, JOB_TOKEN};
use crate::error::LsmError;
use crate::jobs::{FlushJob, Next};

/// What [`Lsm::poll_io_step`] gives.
#[derive(Debug)]
pub enum Polled {
    /// A disk operation to run.
    Op(IoOp),
    /// No operation yet, and background work left that waits for none: ask again (FR-030).
    More,
    /// Nothing to do until an operation ends or something is written.
    Idle,
}

impl Lsm {
    fn poll_flush(&mut self) -> Polled {
        loop {
            if self.flush_job.is_none() {
                let rotating = matches!(self.bg_job, Some(Job::Rotate(_)));
                if rotating || self.version.memtables.len() < 2 {
                    return Polled::Idle;
                }
                let mem = self.version.memtables.last().expect("a frozen memtable").clone();
                let number = self.alloc_file();
                self.flush_job = Some(FlushJob::new(&self.cfg, mem, number, self.next_file));
            }
            match self.flush_job.as_mut().expect("started").next(&mut self.manifest) {
                Next::Op(op) => return Polled::Op(op),
                Next::More => return Polled::More,
                Next::Done(o) => {
                    self.flush_job = None;
                    self.install(*o);
                }
            }
        }
    }

    /// Whether an export is the next request and may start: every frozen memtable is in a file.
    fn export_waits(&self) -> bool {
        matches!(self.requests.front(), Some(Request::Export(_)))
            && self.version.memtables.len() == 1
    }

    fn poll_background(&mut self) -> Polled {
        loop {
            // An export does not wait for the compaction in progress (FR-028): whoever asked
            // for it serves nothing meanwhile (a range that changes core), and a compaction
            // takes seconds at its pace. No operation of the compaction is in flight here, so
            // it is set aside as it is. The export reads the current version, which still has
            // the compaction's inputs, and writes files of its own.
            let compacting = matches!(&self.bg_job, Some(Job::Compact(c)) if !c.is_export());
            if compacting && self.parked.is_none() && self.export_waits() {
                self.parked = self.bg_job.take();
            }
            if self.bg_job.is_none() {
                let job = match self.parked.is_some() {
                    // Only exports run ahead of it: another compaction, an ingestion or a
                    // clearing would change the files it reads or the levels it writes to.
                    true if self.export_waits() => self.take_request(),
                    true => self.parked.take(),
                    false => self.schedule(),
                };
                let Some(job) = job else { return Polled::Idle };
                self.bg_job = Some(job);
            }
            let next = match self.bg_job.as_mut().expect("scheduled") {
                Job::Delete(s) | Job::Rotate(s) | Job::Reset(s) => s.next(),
                Job::Ingest(j) => j.next(&mut self.manifest, self.next_file, &self.version),
                Job::Compact(c) => {
                    // An export is not held back; a compaction for an ingestion spends its own.
                    let budget = match (c.is_export(), c.is_for_ingest()) {
                        (true, _) => None,
                        (false, true) => Some(&mut self.ingest_budget),
                        (false, false) => Some(&mut self.budget),
                    };
                    if budget.as_ref().is_some_and(|b| **b <= 0) {
                        return Polled::Idle;
                    }
                    match c.next(&mut self.manifest, &mut self.next_file) {
                        Ok(n) => {
                            if let (Next::Op(op), Some(budget)) = (&n, budget) {
                                let cost = CompactJob::takes_budget(op);
                                *budget -= i64::try_from(cost).unwrap_or(i64::MAX);
                                if c.is_for_ingest() {
                                    self.counters.ingest_compaction_bytes += cost;
                                }
                            }
                            n
                        }
                        Err(e) => {
                            self.reader.failure.get_or_insert(e);
                            return Polled::Idle;
                        }
                    }
                }
            };
            match next {
                Next::Op(op) => return Polled::Op(op),
                Next::More => return Polled::More,
                Next::Done(o) => {
                    self.bg_job = None;
                    self.install(*o);
                }
            }
        }
    }

    /// The next disk operation, if any (`now` paces compactions and disk-full retries). A job
    /// that yields goes on at once: whoever drives the LSM this way (tests, the simulator, the
    /// recovery tools) serves nothing between two of its steps.
    pub fn poll_io(&mut self, now: Nanos) -> Option<IoOp> {
        loop {
            match self.poll_io_step(now) {
                Polled::Op(op) => return Some(op),
                Polled::More => {}
                Polled::Idle => return None,
            }
        }
    }

    /// As [`Lsm::poll_io`], with at most one step of background work (FR-030): a core asks once
    /// a turn and serves its requests between two steps.
    pub fn poll_io_step(&mut self, now: Nanos) -> Polled {
        if let Some(op) = self.reader.pop_op() {
            return Polled::Op(op);
        }
        if self.reader.failure.is_some() || self.inflight.is_some() {
            return Polled::Idle;
        }
        if self.full_until.is_some_and(|until| now < until) {
            return Polled::Idle;
        }
        self.refill(now);
        if let Some((slot, op)) = self.retry.take() {
            self.full_until = None;
            self.inflight = Some((slot, op.clone()));
            return Polled::Op(op);
        }
        match self.poll_flush() {
            Polled::Op(op) => return self.hand_out(Slot::Flush, op),
            Polled::More => return Polled::More,
            Polled::Idle => {}
        }
        match self.poll_background() {
            Polled::Op(op) => return self.hand_out(Slot::Background, op),
            Polled::More => return Polled::More,
            Polled::Idle => {}
        }
        // A rotation that just ended may have held a flush back.
        match self.poll_flush() {
            Polled::Op(op) => self.hand_out(Slot::Flush, op),
            rest @ (Polled::More | Polled::Idle) => rest,
        }
    }

    /// Notes operation `op` of `slot` as the one in flight and gives it.
    fn hand_out(&mut self, slot: Slot, op: IoOp) -> Polled {
        self.inflight = Some((slot, op.clone()));
        Polled::Op(op)
    }

    /// Takes the result of an operation from [`Lsm::poll_io`].
    pub fn on_io_done(&mut self, done: IoDone, now: Nanos) -> Completions {
        match done {
            IoDone::Read { token: JOB_TOKEN, data } => {
                self.inflight = None;
                match &mut self.bg_job {
                    Some(Job::Compact(c)) => {
                        if let Err(e) = c.on_read(data) {
                            self.reader.failure.get_or_insert(e);
                        }
                    }
                    Some(Job::Ingest(j)) => j.on_read(data),
                    _ => unreachable!("only compactions and ingestions read with the job token"),
                }
            }
            IoDone::Read { token, data } => {
                self.reader.on_read(token, data);
            }
            IoDone::Ok(_) => self.inflight = None,
            IoDone::Err { op: IoOp::ReadAt { token: JOB_TOKEN, .. }, errno } => {
                self.inflight = None;
                self.reader.failure.get_or_insert(LsmError::Stopped(errno));
            }
            IoDone::Err { op: IoOp::ReadAt { token, .. }, errno } => {
                self.reader.on_read(token, Err(errno));
            }
            IoDone::Err { errno, .. } => {
                let failed = self.inflight.take();
                if errno == IoErrno::NoSpace {
                    self.retry = failed;
                    self.full_until = Some(now + DISK_FULL_RETRY);
                } else {
                    self.reader.failure.get_or_insert(LsmError::Stopped(errno));
                }
            }
        }
        std::mem::take(&mut self.done)
    }

    /// What finished inside [`Lsm::poll_io`] since the last [`Lsm::on_io_done`] (a job whose last
    /// step needs no I/O, such as an export): an embedder that runs the I/O itself calls it after
    /// `poll_io`, or an idle LSM keeps those completions.
    pub fn take_completions(&mut self) -> Completions {
        std::mem::take(&mut self.done)
    }

    /// Runs every pending operation on `disk` (tests, the simulator and recovery tools).
    pub fn run_io(&mut self, disk: &mut dyn Disk, now: Nanos) -> Completions {
        let mut all = std::mem::take(&mut self.done);
        while let Some(op) = self.poll_io(now) {
            let done = disk.execute(op);
            all.extend(self.on_io_done(done, now));
        }
        all.extend(std::mem::take(&mut self.done));
        all
    }
}
