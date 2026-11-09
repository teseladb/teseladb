//! How [`Lsm`] picks its next background job and installs what a job leaves: deletions of unused
//! files first, then queued requests, compactions and a manifest rotation; finished jobs change
//! the version, the manifest state and the completions. Compactions spend a budget refilled at
//! `compaction_bytes_per_sec`.

use std::sync::Arc;

use teseladb_types::Nanos;

use super::{Job, Lsm};
use crate::compaction::{self, CompactJob};
use crate::config::Codec;
use crate::dict::Training;
use crate::jobs::{Outcome, Script};
use crate::sst::reader::Table;

impl Lsm {
    pub(super) fn schedule(&mut self) -> Option<Job> {
        let (free, held): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.obsolete).into_iter().partition(|t| Arc::strong_count(t) == 1);
        self.obsolete = held;
        if !free.is_empty() {
            let numbers: Vec<u64> = free.iter().map(|t| t.number).collect();
            for t in &free {
                self.reader.cache.forget(t.number, t.blocks());
            }
            return Some(Job::Delete(Script::delete(&self.cfg, &numbers)));
        }
        if let Some(job) = self.take_request() {
            return Some(job);
        }
        // Clearings whose tombstones reached a file: one compaction of everything removes them.
        if !self.clears.is_empty() && self.version.memtables.len() == 1 {
            self.compact_all = true;
            self.clears_compacting.append(&mut self.clears);
        }
        let all = std::mem::take(&mut self.compact_all);
        if let Some(plan) = compaction::pick(&self.version, &self.cfg, &mut self.pointers, all) {
            return Some(Job::Compact(Box::new(self.compaction(plan))));
        }
        // Nothing to compact: the clearings are done.
        self.done.cleared.extend(std::mem::take(&mut self.clears_compacting));
        // Background jobs start only when no flush runs (`poll_flush` came back empty).
        if self.manifest.tail > self.cfg.manifest_rotate_bytes {
            let number = self.alloc_file();
            let mut snapshot = self.state.snapshot();
            snapshot.next_file = self.next_file;
            return Some(Job::Rotate(Script::rotate(
                &self.cfg,
                self.manifest.number,
                number,
                &snapshot,
            )));
        }
        None
    }

    /// The compaction of `plan`, which trains the collections' dictionaries with
    /// [`Codec::ZstdDict`].
    pub(super) fn compaction(&self, plan: compaction::Plan) -> CompactJob {
        let job = CompactJob::new(&self.cfg, plan, self.gc.clone());
        match self.cfg.codec {
            Codec::ZstdDict => {
                let written = self.dict_written.clone();
                job.with_training(Training::new(&self.cfg, &self.state, &self.version, written))
            }
            Codec::None | Codec::Zlib | Codec::Zstd => job,
        }
    }

    pub(super) fn install(&mut self, o: Outcome) {
        match o {
            Outcome::Flushed { table, edit } => {
                self.state.apply(&edit);
                let mut v = self.version.clone_shallow();
                v.memtables.pop();
                if let Some(t) = table {
                    self.reader.cache.pin_meta(t.number, t.meta.clone());
                    self.counters.written_bytes += t.size;
                    self.counters.note_codes(&t);
                    v.levels[0].insert(0, Arc::new(*t));
                }
                self.version = Arc::new(v);
                self.done.flushed.extend(edit.flushed);
            }
            Outcome::Rotated { manifest } => self.manifest = manifest,
            Outcome::Deleted => {}
            Outcome::Exported { id, files } => self.done.exported.push((id, files)),
            Outcome::Ingested { id, result: Err(e) } => {
                self.ingesting.remove(&id);
                self.done.ingested.push((id, Err(e)));
            }
            // The cleared intervals it fills take the thresholds handed for them.
            Outcome::Ingested { id, result: Ok((added, edit)) } => {
                let filled = self.ingesting.remove(&id).unwrap_or_default();
                self.clearing = std::mem::take(&mut self.clearing)
                    .iter()
                    .flat_map(|(s, e)| crate::gc::outside(s, e, &filled))
                    .collect();
                self.refresh_gc();
                self.apply_files(added, &[], &edit);
                self.done.ingested.push((id, Ok(())));
            }
            // The index is down on disk: now the data goes, with tombstones that a GC threshold
            // above them lets the compaction remove together with what they cover.
            Outcome::Reset { id, range, intervals, edit } => {
                self.state.apply(&edit);
                for (start, end) in &intervals {
                    self.insert_tombstone(range, start, end);
                }
                self.clearing.extend(intervals);
                self.refresh_gc();
                self.freeze();
                // The compaction waits until the tombstones are in a file.
                self.clears.push(id);
            }
            Outcome::Changed { added, removed, edit, dict_written, ingest } => {
                self.counters.ingest_compactions += u64::from(ingest);
                self.apply_files(added, &removed, &edit);
                if let Some(w) = dict_written {
                    self.dict_written = w;
                }
                self.done.cleared.extend(std::mem::take(&mut self.clears_compacting));
                self.counters.compactions += 1;
            }
        }
    }

    /// Installs a change of files: `removed` leave the version (and the disk once no view holds
    /// them), `added` join their levels.
    fn apply_files(
        &mut self,
        added: Vec<(u8, Table)>,
        removed: &[u64],
        edit: &crate::manifest::Edit,
    ) {
        self.state.apply(edit);
        let mut v = self.version.clone_shallow();
        for level in &mut v.levels {
            let (gone, kept): (Vec<_>, Vec<_>) =
                std::mem::take(level).into_iter().partition(|t| removed.contains(&t.number));
            *level = kept;
            self.obsolete.extend(gone);
        }
        for (level, t) in added {
            self.reader.cache.pin_meta(t.number, t.meta.clone());
            self.counters.written_bytes += t.size;
            self.counters.note_codes(&t);
            v.levels[usize::from(level)].push(Arc::new(t));
        }
        for level in &mut v.levels[1..] {
            level.sort_by(|a, b| a.props.lo.cmp(&b.props.lo));
        }
        self.version = Arc::new(v);
    }

    /// Adds the compaction budgets earned since the last call, up to one second of each.
    pub(super) fn refill(&mut self, now: Nanos) {
        match self.budget_at {
            Some(at) if now > at => {
                let elapsed = now - at;
                self.budget = earn(self.budget, self.cfg.compaction_bytes_per_sec, elapsed);
                let rate = self.cfg.ingest_compaction_bytes_per_sec;
                self.ingest_budget = earn(self.ingest_budget, rate, elapsed);
                self.budget_at = Some(now);
            }
            Some(_) => {}
            None => self.budget_at = Some(now),
        }
    }
}

/// `budget` after `elapsed` at `rate` bytes per second, up to one second of it.
fn earn(budget: i64, rate: u64, elapsed: Nanos) -> i64 {
    let earned = u128::from(rate) * u128::from(elapsed.0) / 1_000_000_000;
    let earned = i64::try_from(earned).unwrap_or(i64::MAX);
    budget.saturating_add(earned).min(i64::try_from(rate).unwrap_or(i64::MAX))
}
