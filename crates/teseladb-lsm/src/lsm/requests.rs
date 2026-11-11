//! Requests the embedder hands to [`Lsm`]: GC thresholds, a full compaction, exports, ingestions
//! and clearings. Exports, ingestions and clearings queue up and become background jobs in their
//! turn, and their outcome arrives in [`Completions`](super::Completions).

use std::sync::Arc;

use teseladb_keys::Hlc;
use teseladb_types::GroupId;

use super::{CLEARED, Job, Lsm, Request};
use crate::compaction::{CompactJob, ExportSpec};
use crate::gc::GcTable;
use crate::ingest::{FileRef, IngestJob};
use crate::jobs::Script;
use crate::sst::reader::Table;

impl Lsm {
    /// Empties `intervals` of `range` so that its snapshot can be ingested (005 FR-012): once
    /// every frozen memtable is flushed, an edit takes the range's flushed index back to 0; then
    /// tombstones cover the intervals, their GC threshold goes above them and every file is
    /// compacted, which removes the data and the tombstones. The identifier comes back in
    /// [`Completions::cleared`](super::Completions::cleared) once that compaction ends, and then
    /// [`Lsm::ingest_range`] finds the intervals empty. Nothing may be written to the range meanwhile.
    pub fn clear_range(&mut self, range: GroupId, intervals: Vec<(Vec<u8>, Vec<u8>)>) -> u64 {
        self.freeze();
        let id = self.next_request;
        self.next_request += 1;
        self.requests.push_back(Request::Clear { id, range, intervals });
        id
    }

    /// Sets the GC threshold of each range, replacing every one handed before; compactions apply
    /// them from now on (FR-007). The intervals of a clearing keep its own threshold until their
    /// snapshot is ingested, and then take the one handed for them.
    pub fn set_gc_thresholds(&mut self, table: GcTable) {
        self.gc_handed = table;
        self.refresh_gc();
    }

    /// Sets `threshold` as the GC threshold of one range's `intervals`, leaving the thresholds
    /// handed for other keys as they are; like [`Lsm::set_gc_thresholds`], it waits for the
    /// ingestion over the intervals of a clearing.
    pub fn set_range_gc_threshold(&mut self, intervals: &[(Vec<u8>, Vec<u8>)], threshold: Hlc) {
        self.gc_handed = self.gc_handed.with_intervals(intervals, threshold);
        self.refresh_gc();
    }

    /// The thresholds compactions apply: the ones handed, with [`CLEARED`] over the clearings.
    pub(super) fn refresh_gc(&mut self) {
        self.gc = self.gc_handed.with_intervals(&self.clearing, CLEARED);
    }

    /// The bytes of the live versions of the data this LSM holds, which the embedder counts: once
    /// the files take more than [`SPACE_AMP`](crate::lsm::SPACE_AMP) times that, or times what
    /// they took after the last compaction of everything, plus
    /// [`SPACE_FLOOR`](crate::lsm::SPACE_FLOOR), every file is compacted into the last level.
    /// Leveled compaction alone leaves a level within its budget, and the last level, as they
    /// are: the versions below the GC threshold and the keys a clearing removed stay on disk and
    /// in the caches (found by the sustained load of 002 on 11-11-2026).
    pub fn set_live_bytes(&mut self, bytes: u64) {
        self.live_bytes = Some(bytes);
    }

    /// Asks for one compaction of every file into the last level (before an export, or to apply
    /// a new GC threshold everywhere).
    pub fn compact_all(&mut self) {
        self.compact_all = true;
    }

    /// Exports every version of `[start, end)` up to `ts` as SST files in `dir` (FR-009). The
    /// memtable is flushed first; the files come in
    /// [`Completions::exported`](super::Completions::exported) with the returned identifier. Writes up to `ts` must all be applied before.
    pub fn export(&mut self, start: &[u8], end: &[u8], ts: Hlc, dir: std::path::PathBuf) -> u64 {
        self.freeze();
        let id = self.next_request;
        self.next_request += 1;
        let spec = ExportSpec { id, start: start.to_vec(), end: end.to_vec(), ts, dir };
        self.requests.push_back(Request::Export(spec));
        id
    }

    /// Ingests `files` (from an export) into `[start, end)`, which must hold no local data
    /// (FR-009). The result comes in [`Completions::ingested`](super::Completions::ingested) with
    /// the returned identifier. When every level below level 0 has a file that spans the interval,
    /// a compaction of its own budget first makes room in the last level (FR-024).
    pub fn ingest(&mut self, files: Vec<FileRef>, start: &[u8], end: &[u8]) -> u64 {
        self.push_ingest(files, vec![(start.to_vec(), end.to_vec())], None)
    }

    /// Ingests a range's snapshot (005 FR-012): `files` into `intervals` (its local keys and its
    /// data), with `flushed.1` as the flushed index of range `flushed.0`, in one manifest edit.
    /// The outcome arrives in [`Completions::ingested`](super::Completions::ingested).
    pub fn ingest_range(
        &mut self,
        files: Vec<FileRef>,
        intervals: Vec<(Vec<u8>, Vec<u8>)>,
        flushed: (GroupId, u64),
    ) -> u64 {
        self.push_ingest(files, intervals, Some(flushed))
    }

    fn push_ingest(
        &mut self,
        files: Vec<FileRef>,
        intervals: Vec<(Vec<u8>, Vec<u8>)>,
        flushed: Option<(GroupId, u64)>,
    ) -> u64 {
        let id = self.next_request;
        self.next_request += 1;
        self.requests.push_back(Request::Ingest { id, files, intervals, flushed, room: false });
        id
    }

    /// The next export or ingestion, when it can start.
    pub(super) fn take_request(&mut self) -> Option<Job> {
        // An export waits for every frozen memtable, which may still be there when a rotation that
        // held flushes back has just ended.
        let ready = match self.requests.front()? {
            // A clearing waits too: no entry of the range may raise its index after the reset.
            Request::Export(_) | Request::Clear { .. } => self.version.memtables.len() == 1,
            Request::Ingest { .. } => true,
        };
        if !ready {
            return None;
        }
        match self.requests.pop_front().expect("checked") {
            Request::Export(spec) => {
                let inputs: Vec<(u8, Arc<Table>)> = self
                    .version
                    .levels
                    .iter()
                    .enumerate()
                    .flat_map(|(l, ts)| ts.iter().map(move |t| (l as u8, t.clone())))
                    .filter(|(_, t)| {
                        t.props.hi.as_slice() >= spec.start.as_slice()
                            && t.props.lo.as_slice() < spec.end.as_slice()
                    })
                    .collect();
                Some(Job::Compact(Box::new(CompactJob::export(&self.cfg, inputs, spec))))
            }
            Request::Ingest { id, files, intervals, flushed, room: false } => {
                let room = crate::compaction::room_for_ingest(&self.version, &intervals);
                self.requests.push_front(Request::Ingest {
                    id,
                    files,
                    intervals,
                    flushed,
                    room: true,
                });
                match room {
                    Some(plan) => Some(Job::Compact(Box::new(self.compaction(plan).for_ingest()))),
                    None => self.take_request(),
                }
            }
            Request::Ingest { id, files, intervals, flushed, room: true } => {
                self.ingesting.insert(id, intervals.clone());
                let job = IngestJob::new(
                    &self.cfg,
                    &self.version,
                    id,
                    files,
                    intervals,
                    flushed,
                    &mut self.next_file,
                );
                Some(Job::Ingest(Box::new(job)))
            }
            Request::Clear { id, range, intervals } => {
                let next = self.next_file;
                Some(Job::Reset(Script::reset(&mut self.manifest, id, range, intervals, next)))
            }
        }
    }
}
