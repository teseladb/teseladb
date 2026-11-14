//! `CoreStore`: the engine of one core, with no I/O of its own.
//!
//! Writes accumulate in an open batch; `poll_io` closes the batch and emits it in phases:
//!
//! 1. **Prepare**: create new files with their space reserved. If it fails for lack of space,
//!    nothing has been written: the core goes `DiskFull` and retries the phase every second.
//! 2. **Write**: headers of new files, snapshot files and the WAL region.
//! 3. **Sync**: snapshot files and WAL (with its directory if it is new).
//! 4. **After**: rename snapshot files into place and sync their directories.
//!
//! Maintenance (deleting old WAL segments, snapshots and markers, and syncing their directory)
//! goes apart, one operation at a time in the order asked: an operation waits until the batch
//! open when it was asked for, and every batch before it, is durable (a segment is released
//! because that batch holds the checkpoints that replace it), and no batch waits for maintenance.
//! A directory sync takes as long as the drive's flush, and a batch that carried one held every
//! acknowledgement of the core behind it.
//!
//! Up to `max_inflight_batches` batches are in flight: a batch that only appends to the current WAL
//! segment is written while the batches before it wait for their syncs; any other batch waits until
//! every batch before it finished. Batches finish in order, so a ticket is acknowledged only once
//! its batch and every one before it are durable. Any other I/O error stops the core
//! (fail-stop): the tickets of every batch in flight and waiting fail and no more writes are
//! accepted.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use teseladb_types::{Bytes, Durability, Nanos};

use crate::config::StorageConfig;
use crate::disk::{FileId, IoDone, IoErrno, IoOp};
use crate::error::StorageError;
use crate::format::wal::{WalEntry, WalPayload, WalRecord, record_len};
use crate::group_log::{GroupLog, HardState, LogEntry, LogGap, SnapshotMeta};
use crate::metrics::StorageMetrics;
use crate::release::{ReleaseFloors, ReleaseProvider};
use crate::wal::WalWriter;
use std::collections::BTreeMap;
use teseladb_types::GroupId;

use crate::flush::{FlushScheduler, OpenInfo};

/// Write request waiting for acknowledgement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ticket(pub u64);

/// Read waiting on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ReadToken(pub u64);

/// A finished snapshot read: its token and the snapshot or why it could not be read.
pub type SnapshotRead = (ReadToken, Result<(SnapshotMeta, Bytes), StorageError>);

/// What completed while processing an I/O result.
#[derive(Debug, Default, PartialEq)]
pub struct Completions {
    /// Finished tickets.
    pub tickets: Vec<(Ticket, Result<(), StorageError>)>,
    /// Finished snapshot reads (`load_snapshot`).
    pub snapshots: Vec<SnapshotRead>,
}

impl Completions {
    /// Tells whether there is nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tickets.is_empty() && self.snapshots.is_empty()
    }
}

#[derive(Debug, Default)]
struct OpenBatch {
    seq: u64,
    tickets: Vec<Ticket>,
    prepare: Vec<IoOp>,
    writes: Vec<IoOp>,
    wal: Vec<WalRecord>,
    syncs: Vec<IoOp>,
    after: Vec<IoOp>,
    snaps: Vec<PendingSnap>,
    bytes: u64,
    strict: bool,
    opened_at: Option<Nanos>,
}

impl OpenBatch {
    fn new(seq: u64) -> OpenBatch {
        OpenBatch { seq, ..Default::default() }
    }

    fn is_empty(&self) -> bool {
        self.tickets.is_empty()
            && self.prepare.is_empty()
            && self.wal.is_empty()
            && self.after.is_empty()
    }
}

/// Group snapshot written in a batch.
#[derive(Debug, Clone)]
struct PendingSnap {
    group: GroupId,
    meta: SnapshotMeta,
    path: PathBuf,
    len: u64,
    ticket: Ticket,
}

/// Group snapshot durable on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnapFile {
    pub(crate) index: u64,
    pub(crate) term: u64,
    pub(crate) path: PathBuf,
    pub(crate) len: u64,
}

/// Snapshots kept per group.
pub const SNAPSHOTS_KEPT: usize = 2;

/// State of an operation within a phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpState {
    Waiting,
    Delivered,
    Done,
}

/// A maintenance operation: it goes out once batch `after` is durable.
#[derive(Debug)]
struct Chore {
    after: u64,
    op: IoOp,
    state: OpState,
    /// The directory sync of this spare segment: once done, a batch may open it without one.
    spare: Option<u64>,
}

#[derive(Debug)]
struct InFlight {
    seq: u64,
    phases: VecDeque<Vec<(IoOp, OpState)>>,
    tickets: Vec<Ticket>,
    snaps: Vec<PendingSnap>,
    started_at: Nanos,
    /// It only writes the WAL in its current segment and syncs it: it may go out while the
    /// batches before it wait for their syncs.
    appends_only: bool,
}

impl InFlight {
    /// Whether what it still has to do is waiting for its syncs (or nothing).
    fn only_syncs_left(&self) -> bool {
        match self.phases.len() {
            0 => true,
            1 => self.phases[0].iter().all(|(op, _)| matches!(op, IoOp::Sync { .. })),
            _ => false,
        }
    }
}

/// Reads in flight.
#[derive(Debug, Clone)]
enum ReadKind {
    Snapshot { group: GroupId, path: PathBuf },
}

/// Engine of one core.
#[derive(Debug)]
pub struct CoreStore {
    cfg: StorageConfig,
    core: u16,
    pub(crate) wal: WalWriter,
    pub(crate) groups: BTreeMap<GroupId, GroupLog>,
    open: OpenBatch,
    queued: VecDeque<OpenBatch>,
    inflight: VecDeque<InFlight>,
    flush: FlushScheduler,
    next_ticket: u64,
    next_token: u64,
    next_file: u32,
    ready: Completions,
    read_queue: VecDeque<IoOp>,
    reads: HashMap<u64, ReadKind>,
    pub(crate) snap_files: BTreeMap<GroupId, Vec<SnapFile>>,
    /// Index of the last snapshot requested per group (durable or still in a batch).
    snap_pending: HashMap<GroupId, u64>,
    /// Highest releasable WAL index per group (set by the state machine that owns the group).
    floors: ReleaseFloors,
    /// Groups with a `GROUP_CHECKPOINT` already requested so that a segment can be released.
    checkpoint_pending: std::collections::BTreeSet<GroupId>,
    /// Maintenance operations (delete old WAL segments and snapshots), in the order asked.
    chores: VecDeque<Chore>,
    /// The last batch that is durable, with every batch before it.
    durable_seq: u64,
    metrics: StorageMetrics,
    now: Nanos,
    /// Latest cluster time seen (for the headers of new files).
    last_cluster_ms: u64,
}

/// A group that holds back the oldest WAL segment ([`CoreStore::wal_holders`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WalHolder {
    /// The group.
    pub group: GroupId,
    /// Its snapshot is below its entries in the segment.
    pub behind_snapshot: bool,
    /// A release floor of its providers is below its entries in the segment.
    pub behind_floor: bool,
}

impl CoreStore {
    /// The key ring of encryption at rest, if any.
    fn ring(&self) -> Option<&teseladb_crypto::at_rest::KeyRing> {
        self.cfg.encryption.as_ref().map(teseladb_crypto::at_rest::Encryption::ring)
    }

    /// Empty engine (new directory). `first_batch` is the first batch number.
    pub(crate) fn empty(cfg: StorageConfig, core: u16, first_batch: u64) -> CoreStore {
        let node_id = cfg.node_id;
        let wal = WalWriter::new(core, node_id, cfg.wal_segment_size)
            .with_encryption(cfg.encryption.clone());
        let flush = FlushScheduler::new(&cfg);
        CoreStore {
            cfg,
            core,
            wal,
            groups: BTreeMap::new(),
            open: OpenBatch::new(first_batch.max(1)),
            queued: VecDeque::new(),
            inflight: VecDeque::new(),
            flush,
            next_ticket: 1,
            next_token: 1,
            next_file: 1,
            ready: Completions::default(),
            read_queue: VecDeque::new(),
            reads: HashMap::new(),
            snap_files: BTreeMap::new(),
            snap_pending: HashMap::new(),
            floors: ReleaseFloors::default(),
            checkpoint_pending: std::collections::BTreeSet::new(),
            chores: VecDeque::new(),
            durable_seq: first_batch.max(1) - 1,
            metrics: StorageMetrics::default(),
            now: Nanos::ZERO,
            last_cluster_ms: 0,
        }
    }

    /// Core.
    pub fn core(&self) -> u16 {
        self.core
    }

    /// Configuration.
    pub fn config(&self) -> &StorageConfig {
        &self.cfg
    }

    /// The rebuilt core rejoined every group as a new replica: its marker goes with the next batch.
    pub fn clear_rebuild_marker(&mut self) {
        self.push_maintenance(IoOp::Remove { path: crate::rebuild::rebuild_marker(self.core) });
        self.push_maintenance(IoOp::SyncDir { path: crate::paths::core_dir(self.core) });
    }

    /// Counters.
    pub fn metrics(&self) -> &StorageMetrics {
        &self.metrics
    }

    /// Sets the number of the next batch (after recovery).
    pub(crate) fn reset_batch_seq(&mut self, seq: u64) {
        self.open.seq = seq;
    }

    /// Tells whether the core stopped after an I/O failure.
    pub fn is_failed(&self) -> bool {
        self.flush.is_failed()
    }

    /// Tells whether the core is out of disk space.
    pub fn is_disk_full(&self) -> bool {
        self.flush.is_disk_full()
    }

    /// Number of the open batch (the next one to be written after the queued ones).
    pub fn next_batch_seq(&self) -> u64 {
        self.open.seq
    }

    pub(crate) fn alloc_file(&mut self) -> FileId {
        let id = FileId(self.next_file);
        self.next_file += 1;
        id
    }

    fn new_ticket(&mut self) -> Ticket {
        let t = Ticket(self.next_ticket);
        self.next_ticket += 1;
        t
    }

    fn check_writable(&self) -> Result<(), StorageError> {
        self.flush.check_writable()
    }

    /// Makes sure the open batch does not exceed `max_batch_bytes`: if it would, it is queued.
    fn ensure_room(&mut self, incoming: u64) {
        if self.open.bytes > 0 && self.open.bytes + incoming > self.cfg.max_batch_bytes {
            let next = OpenBatch::new(self.open.seq + 1);
            let full = core::mem::replace(&mut self.open, next);
            self.queued.push_back(full);
        }
    }

    fn touch_batch(&mut self, mode: Durability, ticket: Ticket) {
        if self.open.opened_at.is_none() {
            self.open.opened_at = Some(self.now);
        }
        match mode {
            Durability::Strict => {
                self.open.strict = true;
                self.open.tickets.push(ticket);
            }
            Durability::Memory | Durability::None => {
                self.ready.tickets.push((ticket, Ok(())));
            }
        }
    }

    fn push_wal(&mut self, group: GroupId, payload: WalPayload) {
        let len = record_len(payload.body_len()) as u64;
        self.ensure_room(len);
        self.open.bytes += len;
        self.open.wal.push(WalRecord { group, batch_seq: self.open.seq, flags: 0, payload });
    }

    fn finish_wal_request(&mut self, mode: Durability) -> Ticket {
        let ticket = self.new_ticket();
        if mode == Durability::None {
            self.ready.tickets.push((ticket, Ok(())));
        } else {
            self.touch_batch(mode, ticket);
        }
        ticket
    }

    /// Creates a Raft group in this core with its initial (opaque) configuration.
    ///
    /// # Errors
    ///
    /// [`StorageError::DiskFull`] or [`StorageError::CoreFailed`] while the core cannot write, and [`StorageError::Invalid`] when
    /// the group already exists.
    pub fn create_group(
        &mut self,
        g: GroupId,
        conf: Bytes,
        mode: Durability,
    ) -> Result<Ticket, StorageError> {
        self.check_writable()?;
        if self.groups.contains_key(&g) {
            return Err(StorageError::Invalid("the group already exists"));
        }
        self.groups.insert(g, GroupLog::new(g, conf.clone()));
        if mode != Durability::None {
            self.push_wal(g, WalPayload::GroupCreate { conf });
        }
        Ok(self.finish_wal_request(mode))
    }

    /// Removes a group from this core.
    ///
    /// # Errors
    ///
    /// [`StorageError::DiskFull`] or [`StorageError::CoreFailed`] while the core cannot write, and [`StorageError::Invalid`] when
    /// the group does not exist.
    pub fn delete_group(&mut self, g: GroupId) -> Result<Ticket, StorageError> {
        self.check_writable()?;
        if self.groups.remove(&g).is_none() {
            return Err(StorageError::Invalid("the group does not exist"));
        }
        self.snap_pending.remove(&g);
        self.checkpoint_pending.remove(&g);
        self.floors.forget(g);
        for f in self.snap_files.remove(&g).unwrap_or_default() {
            self.push_maintenance(IoOp::Remove { path: f.path });
        }
        self.push_wal(g, WalPayload::GroupDelete);
        Ok(self.finish_wal_request(Durability::Strict))
    }

    /// Appends entries (and, if given, the hard state) to a group's log. If the first entry
    /// overwrites a suffix, a `TRUNCATE` is recorded first.
    ///
    /// # Errors
    ///
    /// [`StorageError::DiskFull`] or [`StorageError::CoreFailed`] while the core cannot write, and [`StorageError::Invalid`] when
    /// the group does not exist, the entries do not follow its log, or they are larger than one WAL record
    /// holds.
    pub fn append_raft(
        &mut self,
        g: GroupId,
        entries: &[LogEntry],
        hs: Option<HardState>,
        mode: Durability,
    ) -> Result<Ticket, StorageError> {
        self.check_writable()?;
        if !crate::format::wal::entries_fit(entries.iter().map(|e| e.data.len())) {
            return Err(StorageError::Invalid("the entries are larger than one WAL record holds"));
        }
        let log =
            self.groups.get_mut(&g).ok_or(StorageError::Invalid("the group does not exist"))?;
        let truncated = log.append(entries).map_err(StorageError::Invalid)?;
        if let Some(hs) = hs {
            log.hard = hs;
        }
        if mode != Durability::None {
            if truncated {
                let from = entries[0].index;
                self.push_wal(g, WalPayload::Truncate { from_index: from });
            }
            if let Some(first) = entries.first() {
                let entries = entries
                    .iter()
                    .map(|e| WalEntry { term: e.term, kind: e.kind, data: e.data.clone() })
                    .collect();
                self.push_wal(g, WalPayload::Entries { first_index: first.index, entries });
            }
            if let Some(hs) = hs {
                self.push_wal(
                    g,
                    WalPayload::HardState { term: hs.term, vote: hs.vote, commit: hs.commit },
                );
            }
        }
        Ok(self.finish_wal_request(mode))
    }

    /// Discards the entries with index ≥ `from_index`.
    ///
    /// # Errors
    ///
    /// [`StorageError::DiskFull`] or [`StorageError::CoreFailed`] while the core cannot write, and [`StorageError::Invalid`] when
    /// the group does not exist or `from_index` is covered by its snapshot.
    pub fn truncate_raft(&mut self, g: GroupId, from_index: u64) -> Result<Ticket, StorageError> {
        self.check_writable()?;
        let log =
            self.groups.get_mut(&g).ok_or(StorageError::Invalid("the group does not exist"))?;
        if from_index <= log.snapshot_index {
            return Err(StorageError::Invalid("truncating entries covered by the snapshot"));
        }
        log.truncate(from_index);
        self.push_wal(g, WalPayload::Truncate { from_index });
        Ok(self.finish_wal_request(Durability::Strict))
    }

    /// Entries `[lo, hi)` of a group up to `max_bytes` (at least one). Never touches disk.
    ///
    /// # Errors
    ///
    /// [`LogGap::NoGroup`] for a group that does not exist, and the gaps of [`GroupLog::entries`].
    pub fn entries(
        &self,
        g: GroupId,
        lo: u64,
        hi: u64,
        max_bytes: u64,
    ) -> Result<&[LogEntry], LogGap> {
        self.groups.get(&g).ok_or(LogGap::NoGroup)?.entries(lo, hi, max_bytes)
    }

    /// Hard state of a group.
    pub fn hard_state(&self, g: GroupId) -> Option<HardState> {
        self.groups.get(&g).map(|l| l.hard)
    }

    /// Groups of the core, in order.
    pub fn group_ids(&self) -> Vec<GroupId> {
        self.groups.keys().copied().collect()
    }

    /// Log of a group (for Raft: terms, last index, snapshot).
    pub fn group(&self, g: GroupId) -> Option<&GroupLog> {
        self.groups.get(&g)
    }

    /// Next disk operation, if any.
    pub fn poll_io(&mut self, now: Nanos) -> Option<IoOp> {
        self.now = now;
        if let Some(op) = self.read_queue.pop_front() {
            return Some(op);
        }
        if !self.flush.may_write(now) {
            return None;
        }
        if let Some(op) = self.next_op() {
            return Some(op);
        }
        if let Some(op) = self.next_chore() {
            return Some(op);
        }
        let room = self.inflight.len() < self.cfg.max_inflight_batches
            && self.inflight.iter().all(InFlight::only_syncs_left);
        if room && self.start_batch(now) {
            return self.next_op();
        }
        None
    }

    /// Hands out the next operation, not handed out yet, of the current phase of a batch that may
    /// go: the first one in flight, or one that only appends while every batch before it only
    /// waits for its syncs.
    fn next_op(&mut self) -> Option<IoOp> {
        let mut before_only_sync = true;
        for (k, f) in self.inflight.iter_mut().enumerate() {
            if (k == 0 || (f.appends_only && before_only_sync))
                && let Some(phase) = f.phases.front_mut()
                && let Some((op, state)) = phase.iter_mut().find(|(_, st)| *st == OpState::Waiting)
            {
                *state = OpState::Delivered;
                return Some(op.clone());
            }
            before_only_sync = before_only_sync && f.only_syncs_left();
        }
        None
    }

    fn open_info(&self) -> OpenInfo {
        OpenInfo {
            bytes: self.open.bytes,
            strict: self.open.strict,
            opened_at: self.open.opened_at,
        }
    }

    /// Closes the next batch if it is time. Returns whether a batch is in flight.
    fn start_batch(&mut self, now: Nanos) -> bool {
        let take_queued = !self.queued.is_empty();
        let maintenance_only = self.open.tickets.is_empty() && !self.open.after.is_empty();
        if !take_queued
            && (self.open.is_empty()
                || (!maintenance_only && !self.flush.due(self.open_info(), now)))
        {
            return false;
        }
        let batch = if take_queued {
            self.queued.pop_front().expect("queued batch")
        } else {
            let next = OpenBatch::new(self.open.seq + 1);
            core::mem::replace(&mut self.open, next)
        };
        let seq = batch.seq;
        let now_ms = self.last_cluster_ms;
        let mut alloc = {
            let next_file = &mut self.next_file;
            move || {
                let id = FileId(*next_file);
                *next_file += 1;
                id
            }
        };
        for r in &batch.wal {
            self.wal.push(r);
        }
        let wal = self.wal.close_batch(seq, &mut alloc, now_ms);
        if let Some(spare) = wal.spare_created {
            self.chores.push_back(Chore {
                after: seq,
                op: IoOp::SyncDir { path: crate::paths::wal_dir(self.core) },
                state: OpState::Waiting,
                spare: Some(spare),
            });
        }
        let mut prepare = batch.prepare;
        prepare.extend(wal.prepare);
        let mut writes = batch.writes;
        writes.extend(wal.writes);
        let mut syncs = batch.syncs;
        syncs.extend(wal.syncs);
        let after = batch.after;
        let appends_only = !wal.opens_segment
            && prepare.is_empty()
            && after.is_empty()
            && batch.snaps.is_empty()
            && writes.iter().all(|op| matches!(op, IoOp::WriteAt { .. }))
            && syncs.iter().all(|op| matches!(op, IoOp::Sync { .. }));
        let phases: VecDeque<Vec<(IoOp, OpState)>> = [prepare, writes, syncs, after]
            .into_iter()
            .filter(|p| !p.is_empty())
            .map(|p| p.into_iter().map(|op| (op, OpState::Waiting)).collect())
            .collect();
        self.metrics.batches += 1;
        self.metrics.batched_requests += batch.tickets.len() as u64;
        self.metrics.wal_bytes_written += wal.bytes;
        let tickets = batch.tickets;
        // Faulty variant: acknowledges the batch before writing and syncing it.
        #[cfg(teseladb_bug = "ack_before_fsync")]
        let tickets = {
            for t in tickets {
                self.ready.tickets.push((t, Ok(())));
            }
            Vec::new()
        };
        self.inflight.push_back(InFlight {
            seq,
            phases,
            tickets,
            snaps: batch.snaps,
            started_at: now,
            appends_only,
        });
        true
    }

    /// Processes the result of a disk operation.
    pub fn on_io_done(&mut self, done: IoDone, now: Nanos) -> Completions {
        self.now = now;
        match done {
            IoDone::Read { token, data } => self.read_done(token, data),
            IoDone::Ok(op) => self.op_done(&op, None, now),
            IoDone::Err { op, errno } => self.op_done(&op, Some(errno), now),
        }
        self.take_completions()
    }

    /// Completions waiting to be delivered (`memory` and `none` modes, and those of `on_io_done`).
    pub fn take_completions(&mut self) -> Completions {
        core::mem::take(&mut self.ready)
    }

    fn read_done(&mut self, token: u64, data: Result<Bytes, IoErrno>) {
        let Some(ReadKind::Snapshot { group, path }) = self.reads.remove(&token) else {
            return;
        };
        let r = match data {
            Err(errno) => Err(StorageError::Io { op: "read", errno }),
            Ok(buf) => crate::format::snapshot::GroupSnapshot::decode_with(&buf, self.ring())
                .ok()
                .filter(|(_, snap)| snap.group == group)
                .map(|(_, snap)| {
                    (
                        SnapshotMeta { index: snap.index, term: snap.term, conf: snap.conf },
                        snap.data,
                    )
                })
                .ok_or(StorageError::Corrupt { file: path, offset: 0 }),
        };
        self.ready.snapshots.push((ReadToken(token), r));
    }

    fn op_done(&mut self, op: &IoOp, err: Option<IoErrno>, now: Nanos) {
        if let Some(chore) = self
            .chores
            .pop_front_if(|c| c.state == OpState::Delivered && c.op.without_data() == *op)
        {
            match err {
                Some(errno) => self.fail(op.name(), errno),
                None => {
                    if let Some(seq) = chore.spare {
                        self.wal.spare_dir_synced(seq);
                    }
                }
            }
            return;
        }
        let delivered = |phase: &Vec<(IoOp, OpState)>| {
            phase.iter().position(|(o, st)| *st == OpState::Delivered && o.without_data() == *op)
        };
        let Some((f, pos)) = self.inflight.iter_mut().find_map(|f| {
            let pos = f.phases.front().and_then(delivered)?;
            Some((f, pos))
        }) else {
            return;
        };
        // An operation was handed out from the batch's current phase, so it has one.
        let phase = f.phases.front_mut().expect("phase in progress");
        match err {
            None => {
                phase[pos].1 = OpState::Done;
                if phase.iter().all(|(_, st)| *st == OpState::Done) {
                    f.phases.pop_front();
                }
                // Batches finish in order: a later one that is done waits for those before it.
                while self.inflight.front().is_some_and(|f| f.phases.is_empty()) {
                    let f = self.inflight.pop_front().expect("checked");
                    self.finish_batch(f, now);
                }
            }
            Some(IoErrno::NoSpace) if matches!(op, IoOp::Create { .. }) => {
                // Failed reservation: nothing of this batch has been written. The operations of
                // the phase that did not finish are retried.
                for (_, st) in phase.iter_mut() {
                    if *st == OpState::Delivered {
                        *st = OpState::Waiting;
                    }
                }
                self.flush.disk_full(now);
            }
            // Faulty variant: after a failed `Sync` it carries on and retries it, trusting
            // that the retry works.
            #[cfg(teseladb_bug = "fsync_retry_success")]
            Some(_) if matches!(op, IoOp::Sync { .. }) => {
                phase[pos].1 = OpState::Waiting;
            }
            Some(errno) => self.fail(op.name(), errno),
        }
    }

    fn finish_batch(&mut self, f: InFlight, now: Nanos) {
        self.durable_seq = f.seq;
        self.metrics.syncs += 1;
        self.metrics.sync_latency.record(now.saturating_since(f.started_at).0);
        for t in f.tickets {
            self.ready.tickets.push((t, Ok(())));
        }
        for snap in f.snaps {
            self.snapshot_durable(snap);
        }
        self.maybe_release_wal();
    }

    /// A snapshot file is now durable: the log is compacted, `SNAPSHOT_MARK` is recorded (the
    /// ticket completes when the mark is durable) and the deletion of the old ones is scheduled.
    fn snapshot_durable(&mut self, snap: PendingSnap) {
        let PendingSnap { group, meta, path, len, ticket } = snap;
        let files = self.snap_files.entry(group).or_default();
        files.retain(|f| f.index != meta.index);
        files.push(SnapFile { index: meta.index, term: meta.term, path, len });
        files.sort_by_key(|f| f.index);
        let surplus = files.len().saturating_sub(SNAPSHOTS_KEPT);
        let old: Vec<SnapFile> = files.drain(..surplus).collect();
        for f in old {
            self.push_maintenance(IoOp::Remove { path: f.path });
        }
        if let Some(log) = self.groups.get_mut(&group) {
            // `compact` does nothing with an index that does not advance.
            log.compact(meta.index, meta.term);
            log.conf = meta.conf.clone();
            self.push_wal(group, WalPayload::SnapshotMark { index: meta.index, term: meta.term });
            self.open.strict = true;
            self.open.opened_at.get_or_insert(self.now);
            self.open.tickets.push(ticket);
        } else {
            // The group was deleted while the snapshot was being written.
            self.ready.tickets.push((ticket, Ok(())));
        }
    }

    /// Releases the prefix of WAL segments that are no longer needed (data-model §8): all their
    /// entries covered by snapshots (and by the group's release floor) and no live group with its
    /// latest hard state in them. For the latter, a `GROUP_CHECKPOINT` is written first in the
    /// active segment.
    fn maybe_release_wal(&mut self) {
        // After any batch there is at least one WAL segment (every batch writes its `BATCH`).
        let active = *self.wal.segments.keys().next_back().expect("active segment");
        let mut release = Vec::new();
        let mut checkpoints = Vec::new();
        for (&seq, seg) in &self.wal.segments {
            if seq == active {
                break;
            }
            let covered = seg.groups.iter().all(|(g, &max)| match self.groups.get(g) {
                None => true,
                Some(log) => {
                    let floor = self.floors.floor(*g);
                    // Faulty variant: releases what the LSM has not flushed.
                    #[cfg(teseladb_bug = "release_log_before_flush")]
                    let floor: Option<u64> = floor.and(None);
                    max <= log.snapshot_index.min(floor.unwrap_or(u64::MAX))
                }
            });
            if !covered {
                break;
            }
            let holders: Vec<GroupId> =
                seg.bases.iter().copied().filter(|g| self.groups.contains_key(g)).collect();
            // Faulty variant: releases segments holding a live group's base.
            #[cfg(teseladb_bug = "wal_release_hardstate")]
            let holders: Vec<GroupId> = {
                drop(holders);
                Vec::new()
            };
            if !holders.is_empty() {
                checkpoints.extend(holders);
                break;
            }
            release.push(seq);
        }
        for g in checkpoints {
            if self.checkpoint_pending.insert(g) {
                let log = &self.groups[&g];
                let payload = WalPayload::Checkpoint {
                    term: log.hard.term,
                    vote: log.hard.vote,
                    commit: log.hard.commit,
                    snapshot_index: log.snapshot_index,
                    snapshot_term: log.snapshot_term,
                    last_truncate: log.last_truncate,
                    conf: log.conf.clone(),
                };
                self.push_wal(g, payload);
                self.open.opened_at.get_or_insert(self.now);
            }
        }
        for seq in release {
            let seg = self.wal.segments.remove(&seq).expect("segment walked above");
            self.push_maintenance(IoOp::Close { file: seg.file });
            self.push_maintenance(IoOp::Remove { path: crate::paths::wal_segment(self.core, seq) });
            self.push_maintenance(IoOp::SyncDir { path: crate::paths::wal_dir(self.core) });
        }
        // A checkpoint stops being pending once the group's base is already in the active segment
        // (or the group no longer exists).
        let segments = &self.wal.segments;
        self.checkpoint_pending
            .retain(|g| segments.iter().any(|(s, seg)| *s != active && seg.bases.contains(g)));
    }

    /// Sets the highest index of group `g` that `provider` no longer needs, or `None` when it
    /// holds nothing back. The WAL holding a group's entries is released only up to
    /// the lowest floor of its providers and its snapshot, and a snapshot past that floor is
    /// refused: it would compact entries a provider still needs.
    pub fn set_release_floor(&mut self, g: GroupId, provider: ReleaseProvider, floor: Option<u64>) {
        self.floors.set(g, provider, floor);
    }

    /// The lowest release floor of a group's providers, if any holds it back.
    pub fn release_floor_of(&self, g: GroupId) -> Option<u64> {
        self.floors.floor(g)
    }

    /// Bytes the WAL's segments take on disk: each one its whole preallocated size, the active
    /// one and the one made ahead included.
    #[must_use]
    pub fn wal_disk_bytes(&self) -> u64 {
        (self.wal.files() as u64).saturating_mul(self.cfg.wal_segment_size)
    }

    /// Bytes of the entries of every group held in memory.
    #[must_use]
    pub fn entries_bytes(&self) -> u64 {
        self.groups.values().map(GroupLog::bytes).sum()
    }

    /// What holds back the oldest WAL segment, when there is one besides the active one: each
    /// group with entries in it past its snapshot or past its release floor. A consumer that
    /// wants the WAL shorter flushes what the floor waits for and takes the snapshot.
    #[must_use]
    pub fn wal_holders(&self) -> Vec<WalHolder> {
        if self.wal.segments.len() < 2 {
            return Vec::new();
        }
        let Some(oldest) = self.wal.segments.values().next() else {
            return Vec::new();
        };
        oldest
            .groups
            .iter()
            .filter_map(|(g, &max)| {
                let log = self.groups.get(g)?;
                let behind_snapshot = max > log.snapshot_index;
                let behind_floor = self.floors.floor(*g).is_some_and(|f| max > f);
                (behind_snapshot || behind_floor).then_some(WalHolder {
                    group: *g,
                    behind_snapshot,
                    behind_floor,
                })
            })
            .collect()
    }

    /// Stores a group snapshot atomically (tmp → sync → rename → directory sync). Once durable,
    /// the log is compacted up to `meta.index` and the group's last two snapshots are kept.
    ///
    /// # Errors
    ///
    /// [`StorageError::DiskFull`] or [`StorageError::CoreFailed`] while the core cannot write, and [`StorageError::Invalid`] when
    /// the group does not exist, the snapshot is not newer than the latest one (durable or in
    /// progress), or it is past the release floor of the group's providers.
    pub fn save_snapshot(
        &mut self,
        g: GroupId,
        meta: SnapshotMeta,
        data: Bytes,
    ) -> Result<Ticket, StorageError> {
        self.check_writable()?;
        let log = self.groups.get(&g).ok_or(StorageError::Invalid("the group does not exist"))?;
        let latest = self.snap_pending.get(&g).copied().unwrap_or(0).max(log.snapshot_index);
        if meta.index <= latest {
            return Err(StorageError::Invalid(
                "snapshot not newer than the latest (durable or in progress)",
            ));
        }
        // A snapshot compacts the log: not past what a provider still needs.
        let floor = self.floors.floor(g);
        // Faulty variant: compacts what the LSM has not flushed.
        #[cfg(teseladb_bug = "release_log_before_flush")]
        let floor: Option<u64> = floor.and(None);
        if floor.is_some_and(|f| meta.index > f) {
            return Err(StorageError::Invalid(
                "snapshot past the release floor of the group's providers",
            ));
        }
        self.snap_pending.insert(g, meta.index);
        let header = crate::format::FileHeader::new(
            crate::format::FileKind::Snapshot,
            u32::from(self.core),
            meta.index,
            self.cfg.node_id,
            self.last_cluster_ms,
        );
        let body = crate::format::snapshot::GroupSnapshot {
            group: g,
            index: meta.index,
            term: meta.term,
            conf: meta.conf.clone(),
            data,
        }
        .encode_with(&header, self.ring());
        let path = crate::paths::group_snapshot(self.core, g, meta.index);
        let len = body.len() as u64;
        self.queue_file_atomic(path.clone(), body);
        let ticket = self.new_ticket();
        self.open.snaps.push(PendingSnap { group: g, meta, path, len, ticket });
        self.open.strict = true;
        self.open.opened_at.get_or_insert(self.now);
        Ok(ticket)
    }

    /// Adds to the open batch the atomic write of a file (tmp → sync → rename → directory
    /// sync).
    fn queue_file_atomic(&mut self, path: PathBuf, body: Vec<u8>) {
        let tmp = crate::paths::tmp(&path);
        let dir = path.parent().map(PathBuf::from).unwrap_or_default();
        let file = self.alloc_file();
        let len = body.len() as u64;
        self.ensure_room(len);
        self.open.prepare.push(IoOp::Create { file, path: tmp.clone(), prealloc: len });
        self.open.writes.push(IoOp::WriteAt {
            file,
            offset: 0,
            data: Bytes::from_vec(body),
            batch: self.open.seq,
        });
        self.open.syncs.push(IoOp::Sync { file, batch: self.open.seq });
        self.open.after.push(IoOp::Close { file });
        self.open.after.push(IoOp::Rename { from: tmp, to: path });
        self.open.after.push(IoOp::SyncDir { path: dir });
        self.open.bytes += len;
    }

    /// Schedules a maintenance operation, to go out once the open batch is durable.
    pub(crate) fn push_maintenance(&mut self, op: IoOp) {
        self.chores.push_back(Chore {
            after: self.open.seq,
            op,
            state: OpState::Waiting,
            spare: None,
        });
    }

    /// Whether `c` may go out: its batch is durable, or nothing was written since it was asked
    /// for and every batch before it is durable.
    fn chore_ready(&self, c: &Chore) -> bool {
        self.durable_seq >= c.after
            || (c.after == self.open.seq
                && self.open.is_empty()
                && self.inflight.is_empty()
                && self.queued.is_empty())
    }

    /// The next maintenance operation, if the one before is done and it may go out.
    fn next_chore(&mut self) -> Option<IoOp> {
        let front = self.chores.front()?;
        if front.state != OpState::Waiting || !self.chore_ready(front) {
            return None;
        }
        let front = self.chores.front_mut().expect("checked above");
        front.state = OpState::Delivered;
        Some(front.op.clone())
    }

    /// Latest durable snapshot of a group: its metadata and a read in progress whose result
    /// arrives in `Completions::snapshots`.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a snapshot file's length was read from a u32 field"
    )]
    pub fn load_snapshot(&mut self, g: GroupId) -> Option<(SnapshotMeta, ReadToken)> {
        let f = self.snap_files.get(&g)?.last()?.clone();
        let conf = self.groups.get(&g).map(|l| l.conf.clone()).unwrap_or_default();
        let token = self.next_token;
        self.next_token += 1;
        let file = self.alloc_file();
        self.read_queue.push_back(IoOp::Open { file, path: f.path.clone() });
        self.read_queue.push_back(IoOp::ReadAt { file, offset: 0, len: f.len as u32, token });
        self.read_queue.push_back(IoOp::Close { file });
        self.reads.insert(token, ReadKind::Snapshot { group: g, path: f.path });
        Some((SnapshotMeta { index: f.index, term: f.term, conf }, ReadToken(token)))
    }

    /// Whether every write this store took is on disk or cannot get there now: nothing waits in
    /// a batch and no batch is in flight, or the core is out of space or stopped after an I/O
    /// error (fail-stop). A clean stop of the core waits for it. Maintenance, which a later
    /// batch takes, does not count.
    pub fn writes_settled(&self) -> bool {
        self.flush.is_failed()
            || self.flush.is_disk_full()
            || (self.inflight.is_empty()
                && self.queued.is_empty()
                && self.open.is_empty()
                && self.chores.is_empty())
    }

    /// Fail-stop: fails everything pending and accepts no more writes.
    fn fail(&mut self, op: &'static str, errno: IoErrno) {
        self.flush.fail();
        let err = StorageError::Io { op, errno };
        // Every batch in flight fails with it, in order: a later one is durable only after it.
        let mut batches: Vec<(u64, Vec<Ticket>)> =
            self.inflight.drain(..).map(|f| (f.seq, f.tickets)).collect();
        for b in self.queued.drain(..) {
            batches.push((b.seq, b.tickets));
        }
        let open = core::mem::replace(&mut self.open, OpenBatch::new(0));
        batches.push((open.seq, open.tickets));
        for (_, tickets) in batches {
            for t in tickets {
                self.ready.tickets.push((t, Err(err.clone())));
            }
        }
    }

    /// Next instant with work to do even if nothing arrives: `memory` flush, grouping wait or
    /// disk-full retry.
    pub fn next_deadline(&self) -> Option<Nanos> {
        let busy = !self.inflight.is_empty() || !self.queued.is_empty();
        let idle = if busy {
            None
        } else if self.open.is_empty() {
            // Maintenance without requests: goes out as soon as `poll_io` is called.
            self.chores
                .front()
                .is_some_and(|c| c.state == OpState::Waiting && self.chore_ready(c))
                .then_some(OpenInfo {
                    bytes: 0,
                    strict: false,
                    opened_at: Some(self.now.saturating_since(self.cfg.memory_flush_interval)),
                })
        } else {
            Some(self.open_info())
        };
        self.flush.deadline(idle)
    }

    /// Notifies the passage of time (no I/O; the next `poll_io` emits it).
    pub fn on_tick(&mut self, now: Nanos) {
        self.now = now;
    }

    /// Runs synchronously against `disk` every operation available at `now` and returns what
    /// completed. Used by tests and single-threaded tools; the server runs the operations in each
    /// core's disk thread.
    pub fn run_io(&mut self, disk: &mut dyn crate::disk::Disk, now: Nanos) -> Completions {
        let mut all = self.take_completions();
        while let Some(op) = self.poll_io(now) {
            let done = disk.execute(op);
            let c = self.on_io_done(done, now);
            all.tickets.extend(c.tickets);
            all.snapshots.extend(c.snapshots);
        }
        all
    }
}
