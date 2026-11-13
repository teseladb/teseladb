//! The LSM of a core (`lsm-api.md`): writes into memtables, reads over views, and background jobs
//! that write through [`Lsm::poll_io`] and [`Lsm::on_io_done`] with no I/O of their own (FR-001).
//!
//! Jobs run one at a time, each with one operation in flight; reads of blocks go out before
//! them. A full disk keeps the write that failed and retries it every second, and meanwhile the
//! core is read-only (FR-015); any other I/O error stops the core.

mod io;
mod requests;

pub use io::Polled;
mod schedule;
mod write;

use std::collections::BTreeMap;
use std::sync::Arc;

use teseladb_keys::Hlc;
use teseladb_raftlog::{Disk, IoOp};
use teseladb_types::{GroupId, Nanos};

use crate::compaction::{CompactJob, ExportSpec};
use crate::config::LsmConfig;
use crate::entry::Kind;
use crate::error::LsmError;
use crate::gc::GcTable;
use crate::ingest::{FileRef, IngestJob};
use crate::jobs::{FlushJob, ManifestFile, Script};
use crate::manifest::State;
use crate::memtable::Memtable;
use crate::read::Reader;
use crate::recovery;
use crate::sst::MAX_FILE_NUMBER;
use crate::sst::reader::Table;
use crate::version::{Version, View};

/// The commit of a resolved intent: the version 008 takes out of the intent's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Commit<'a> {
    /// Commit timestamp.
    pub ts: Hlc,
    /// [`Kind::Put`] or [`Kind::Delete`].
    pub kind: Kind,
    /// The value written.
    pub value: &'a [u8],
}

/// One version of an entry written with [`Lsm::write_entry`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EntryWrite<'a> {
    /// Logical key.
    pub key: &'a [u8],
    /// [`Kind::Put`] or [`Kind::Delete`].
    pub kind: Kind,
    /// The value written (empty for a deletion).
    pub value: &'a [u8],
}

/// One version of an entry written with [`Lsm::write_versions`], at its own timestamp: 008's
/// entries write intents, retract them and write their commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VersionWrite<'a> {
    /// Logical key.
    pub key: &'a [u8],
    /// Timestamp.
    pub ts: Hlc,
    /// Any [`Kind`].
    pub kind: Kind,
    /// The value written (empty for a deletion or a `Retract`).
    pub value: &'a [u8],
}

/// Write pressure for the leader (FR-012).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pressure {
    /// Nothing to do.
    Normal,
    /// Slow down new proposals.
    Slow,
    /// Reject new proposals with `Overloaded`.
    Reject,
}

/// Timestamp of the tombstones of a clearing: above every version.
pub(crate) const CLEARED: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };

/// What finished since the last call.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Completions {
    /// Ranges whose flushed index moved, with the new index.
    pub flushed: Vec<(GroupId, u64)>,
    /// Finished exports: their identifier and the files written.
    pub exported: Vec<(u64, Vec<FileRef>)>,
    /// Finished ingestions: their identifier and the result.
    pub ingested: Vec<(u64, Result<(), LsmError>)>,
    /// Ranges cleared by [`Lsm::clear_range`]: their identifier.
    pub cleared: Vec<u64>,
    /// Compactions of everything asked by [`Lsm::compact_now`] that ended: their identifier.
    pub compacted: Vec<u64>,
}

impl Completions {
    fn extend(&mut self, other: Completions) {
        self.flushed.extend(other.flushed);
        self.exported.extend(other.exported);
        self.ingested.extend(other.ingested);
        self.cleared.extend(other.cleared);
        self.compacted.extend(other.compacted);
    }
}

/// Counters behind [`Lsm::metrics`].
#[derive(Clone, Debug, Default)]
pub(crate) struct Counters {
    pub(crate) user_bytes: u64,
    pub(crate) written_bytes: u64,
    pub(crate) compactions: u64,
    pub(crate) stalls: u64,
    pub(crate) rejects: u64,
    /// Data blocks written by compression code.
    pub(crate) blocks: [u64; 5],
    /// Bytes of data blocks written, before and after compression.
    pub(crate) block_bytes: [u64; 2],
    /// Compactions that made room for an ingestion, and the bytes they read and wrote (FR-024).
    pub(crate) ingest_compactions: u64,
    pub(crate) ingest_compaction_bytes: u64,
}

impl Counters {
    /// Counts the data blocks of `t`, a file this LSM just wrote.
    pub(crate) fn note_codes(&mut self, t: &Table) {
        for (n, written) in self.blocks.iter_mut().zip(t.written.codes) {
            *n += written;
        }
        self.block_bytes[0] += t.written.raw;
        self.block_bytes[1] += t.written.stored;
    }
}

/// Where a job runs: flushes beside one background job, one operation in flight at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slot {
    Flush,
    Background,
}

/// A background job.
#[derive(Debug)]
enum Job {
    /// Deleting unused files.
    Delete(Script),
    /// Manifest rotation: no flush starts while it runs.
    Rotate(Script),
    Compact(Box<CompactJob>),
    Ingest(Box<IngestJob>),
    /// The flushed index of a range going back to 0 before its data is cleared.
    Reset(Script),
}

/// An export or an ingestion waiting for its turn.
#[derive(Debug)]
enum Request {
    Export(ExportSpec),
    Ingest {
        id: u64,
        files: Vec<FileRef>,
        intervals: Vec<(Vec<u8>, Vec<u8>)>,
        flushed: Option<(GroupId, u64)>,
        /// The compaction that makes room for it already ran, or was not needed.
        room: bool,
    },
    Clear {
        id: u64,
        range: GroupId,
        intervals: Vec<(Vec<u8>, Vec<u8>)>,
        /// Whether the intervals must be empty when it ends (an ingest follows).
        compact: bool,
    },
}

/// How many times the live data the files may take before every file is compacted
/// ([`Lsm::set_live_bytes`]).
pub const SPACE_AMP: u64 = 2;
/// Bytes the files may take above that, so that a small LSM does not compact everything for a
/// few megabytes.
pub const SPACE_FLOOR: u64 = 4 << 20;

/// The share of [`LsmConfig::cache_bytes`], in percent, of the cache of the readers on other
/// threads, which serve most reads (005 FR-028); the core's own cache, with every open file's
/// index and filter blocks, has the rest. The two caches share the core's budget (012 FR-013):
/// each with the whole of it, a core used twice its share of the node's memory.
pub const SHARED_CACHE_PERCENT: u64 = 75;

/// The least the core's own cache keeps of a small budget, up to all of it: it holds every open
/// file's index and filter blocks with the data blocks of one read, and below that its reads stop
/// making progress (the simulator's caches of 64 KiB, of which a quarter is one data block).
pub const MIN_CORE_CACHE: u64 = 256 << 10;

/// The budgets of the core's cache and of the shared one for a block cache of `total` bytes:
/// together never more than `total`.
pub fn cache_split(total: u64) -> (u64, u64) {
    let shared = total / 100 * SHARED_CACHE_PERCENT + total % 100 * SHARED_CACHE_PERCENT / 100;
    let core = (total - shared).max(MIN_CORE_CACHE.min(total));
    (core, total - core)
}

/// The LSM of a core.
#[derive(Debug)]
pub struct Lsm {
    pub(crate) cfg: LsmConfig,
    pub(crate) version: Arc<Version>,
    pub(crate) reader: Reader,
    pub(crate) state: State,
    pub(crate) manifest: ManifestFile,
    pub(crate) next_file: u64,
    flush_job: Option<FlushJob>,
    bg_job: Option<Job>,
    /// The compaction set aside, between two of its disk operations, while an export runs
    /// (FR-028); it goes on when no export waits.
    parked: Option<Job>,
    inflight: Option<(Slot, IoOp)>,
    retry: Option<(Slot, IoOp)>,
    full_until: Option<Nanos>,
    /// The GC thresholds compactions apply: those handed, with [`CLEARED`] over `clearing`.
    gc: GcTable,
    /// The GC thresholds handed by the embedder (FR-007).
    gc_handed: GcTable,
    /// Intervals of clearings whose snapshot is not ingested yet: their threshold stays
    /// [`CLEARED`] whatever is handed meanwhile.
    clearing: Vec<(Vec<u8>, Vec<u8>)>,
    /// Intervals of the ingestion that runs, by its identifier.
    ingesting: BTreeMap<u64, Vec<(Vec<u8>, Vec<u8>)>>,
    pointers: Vec<Vec<u8>>,
    compact_all: bool,
    /// Bytes of the live versions of the data this LSM holds, as the embedder last counted them
    /// (`None`: never told): the space of the files is bounded by it ([`Lsm::set_live_bytes`]).
    pub(crate) live_bytes: Option<u64>,
    /// Bytes of the files after the last compaction of everything.
    pub(crate) full_after: u64,
    /// A compaction of everything that the space of the files asked for runs.
    full_running: bool,
    /// The highest GC threshold handed and the newest version in the files when the last such
    /// compaction started: what it could not collect then lay between the two, so the next one
    /// waits for the threshold to pass half of that span (see `space_amplified`).
    full_ts: Option<(Option<teseladb_keys::Hlc>, teseladb_keys::Hlc)>,
    /// Clearings whose tombstones wait for their flush.
    clears: Vec<u64>,
    /// Clearings whose compaction is running.
    clears_compacting: Vec<u64>,
    /// Compactions asked by [`Lsm::compact_now`] whose memtables wait for their flush.
    manual: Vec<u64>,
    /// Compactions asked by [`Lsm::compact_now`] that run.
    manual_compacting: Vec<u64>,
    budget: i64,
    /// The budget of the compactions that make room for an ingestion (FR-024).
    ingest_budget: i64,
    budget_at: Option<Nanos>,
    requests: std::collections::VecDeque<Request>,
    next_request: u64,
    pub(crate) obsolete: Vec<Arc<Table>>,
    done: Completions,
    pub(crate) counters: Counters,
    /// Bytes compactions wrote per collection since its dictionary was trained (004 FR-022).
    pub(crate) dict_written: crate::dict::Written,
    /// The version readers on other threads take (FR-026).
    pub(crate) shared: crate::concurrent::SharedLsm,
}

impl Lsm {
    /// Opens the LSM of `cfg.dir`, creating it if it does not exist. Returns it with the flushed
    /// index of each range: the log of each range is replayed from the next one (FR-010).
    pub fn recover(
        cfg: &LsmConfig,
        disk: &mut dyn Disk,
    ) -> Result<(Lsm, BTreeMap<GroupId, u64>), LsmError> {
        assert!(
            (4096..=32 << 10).contains(&cfg.block_bytes),
            "data blocks are 4 to 32 KiB (FR-003)"
        );
        let r = recovery::recover(cfg, disk)?;
        let mut v = Version::empty(cfg.levels);
        let (core_cache, shared_cache) = cache_split(cfg.cache_bytes);
        let mut reader = Reader::new(core_cache);
        for (level, t) in r.tables {
            reader.cache.pin_meta(t.number, t.meta.clone());
            if let Some(h) = &t.history {
                reader.cache.pin_meta(h.number, h.meta.clone());
            }
            let level = usize::from(level).min(cfg.levels);
            v.levels[level].push(Arc::new(t));
        }
        v.levels[0].sort_by_key(|t| std::cmp::Reverse(t.number));
        for level in &mut v.levels[1..] {
            level.sort_by(|a, b| a.props.lo.cmp(&b.props.lo));
        }
        for f in v.value_files().values() {
            reader.cache.pin_meta(f.number, f.meta.clone());
        }
        let flushed = r.state.flushed.clone();
        // The clearings under way when the node stopped, from their tombstones (004 FR-032): a
        // range dropped from this core has no one to clear it again, and what its removed files
        // did not take is collected under their threshold as before the restart.
        let clearing: Vec<(Vec<u8>, Vec<u8>)> = v
            .levels
            .iter()
            .flatten()
            .flat_map(|t| t.props.tombstones.iter())
            .filter(|t| t.ts == CLEARED)
            .map(|t| (t.start.clone(), t.end.clone()))
            .collect();
        let version = Arc::new(v);
        let shared =
            crate::concurrent::SharedLsm::new(version.clone(), shared_cache, cfg.dir.clone());
        let mut lsm = Lsm {
            cfg: cfg.clone(),
            version,
            reader,
            next_file: r.state.next_file,
            state: r.state,
            manifest: ManifestFile {
                number: r.manifest,
                tail: r.manifest_tail,
                encryption: cfg.encryption.clone(),
            },
            flush_job: None,
            bg_job: None,
            parked: None,
            inflight: None,
            retry: None,
            full_until: None,
            gc: GcTable::default(),
            gc_handed: GcTable::default(),
            clearing,
            ingesting: BTreeMap::new(),
            pointers: vec![Vec::new(); cfg.levels + 1],
            compact_all: false,
            live_bytes: None,
            full_after: 0,
            full_running: false,
            full_ts: None,
            clears: Vec::new(),
            clears_compacting: Vec::new(),
            manual: Vec::new(),
            manual_compacting: Vec::new(),
            budget: i64::try_from(cfg.compaction_bytes_per_sec).unwrap_or(i64::MAX),
            ingest_budget: i64::try_from(cfg.ingest_compaction_bytes_per_sec).unwrap_or(i64::MAX),
            budget_at: None,
            requests: std::collections::VecDeque::new(),
            next_request: 1,
            obsolete: Vec::new(),
            done: Completions::default(),
            counters: Counters::default(),
            dict_written: crate::dict::Written::new(),
            shared,
        };
        lsm.refresh_gc();
        Ok((lsm, flushed))
    }

    /// The failure that stopped this LSM's reads, flushes and compactions, if any: the embedder
    /// reports it, since nothing it holds is read or written to disk any more.
    #[must_use]
    pub fn failure(&self) -> Option<&LsmError> {
        self.reader.failure.as_ref()
    }

    fn writable(&self) -> Result<(), LsmError> {
        match &self.reader.failure {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    /// Freezes the active memtable so the next jobs flush it.
    fn freeze(&mut self) {
        let active = self.version.memtables[0].clone();
        if active.is_empty() && active.applied().is_empty() {
            return;
        }
        active.freeze();
        let mut v = self.version.clone_shallow();
        v.memtables.insert(0, Arc::new(Memtable::default()));
        self.version = Arc::new(v);
    }

    /// Bytes in the memtables, frozen ones included, and those of the active one alone.
    pub fn memtables_bytes(&self) -> (u64, u64) {
        let all = self.version.memtables.iter().map(|m| m.bytes()).sum();
        (all, self.version.memtables[0].bytes())
    }

    /// Freezes the active memtable now (before a shutdown, an export or a test).
    pub fn flush(&mut self) {
        self.freeze();
    }

    /// Keeps the blocks its caches take from now on in `memory` (012 FR-032): the core's cache
    /// and the one readers on other threads share.
    pub fn place_blocks(&mut self, memory: std::sync::Arc<dyn crate::BlockMemory>) {
        self.reader.cache.set_memory(memory.clone());
        self.shared.place_blocks(&memory);
    }

    /// The handle readers on other threads keep (FR-026).
    pub fn shared(&self) -> crate::concurrent::SharedLsm {
        self.shared.clone()
    }

    /// Publishes the current version to readers on other threads; the core calls it after each
    /// batch it applies. Writes to the active memtable are seen without it; a new memtable or new
    /// files are not.
    pub fn publish(&self) {
        self.shared.publish(self.version.clone());
    }

    /// A view of the current memtables and files.
    pub fn view(&self) -> View {
        View { version: self.version.clone() }
    }

    /// The flushed index of `range`: its log is replayed from the next entry after a restart.
    pub fn flushed_index(&self, range: GroupId) -> u64 {
        self.state.flushed.get(&range).copied().unwrap_or(0)
    }

    /// The highest log index of `range` the LSM no longer needs, or `None` if every entry it
    /// applied is in a file (FR-011). Entries up to the flushed index are in a file even when a
    /// memtable holds them again (a range replays its log after a restart, 005 FR-015).
    pub fn release_floor(&self, range: GroupId) -> Option<u64> {
        let flushed = self.flushed_index(range);
        self.version
            .memtables
            .iter()
            .filter_map(|m| m.applied_of(range))
            .filter(|a| a.last > flushed)
            .map(|a| a.first)
            .min()
            .map(|first| first.saturating_sub(1).max(flushed))
    }

    /// Write pressure (FR-012).
    pub fn pressure(&self) -> Pressure {
        let l0 = self.version.levels[0].len();
        let frozen = self.version.memtables.len() - 1;
        if self.full_until.is_some()
            || l0 >= self.cfg.l0_reject
            || frozen >= 2 * self.cfg.max_frozen
        {
            Pressure::Reject
        } else if l0 >= self.cfg.l0_slow || frozen >= self.cfg.max_frozen {
            Pressure::Slow
        } else {
            Pressure::Normal
        }
    }

    /// The debt the leader's write rate follows (005 FR-038): files in level 0 plus frozen
    /// memtables.
    pub fn write_debt(&self) -> u64 {
        (self.version.levels[0].len() + self.version.memtables.len() - 1) as u64
    }

    /// What a read that just missed blocks waits for: [`Lsm::block_reads_back`] tells when the
    /// block reads asked so far, its own among them, came back.
    pub fn block_reads_asked(&self) -> u64 {
        self.reader.asked()
    }

    /// Whether the block reads asked up to `due` ([`Lsm::block_reads_asked`] then) came back, or
    /// a read failed. A request that missed blocks reads again once this holds: before, it finds
    /// the same blocks missing.
    pub fn block_reads_back(&self, due: u64) -> bool {
        self.reader.back(due)
    }

    /// `Ok` while the core works; the error when it is read-only, corrupt or stopped.
    pub fn health(&self) -> Result<(), LsmError> {
        self.writable()?;
        match self.full_until {
            Some(_) => Err(LsmError::ReadOnly),
            None => Ok(()),
        }
    }

    pub(crate) fn alloc_file(&mut self) -> u64 {
        let n = self.next_file;
        assert!(n <= MAX_FILE_NUMBER, "file numbers exhausted");
        self.next_file += 1;
        n
    }
}
