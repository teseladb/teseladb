//! A data range's snapshot between nodes (005 FR-012, plan «Instantáneas de un rango»). The
//! snapshot that `teseladb-multiraft` sends only carries the range's local state; a replica
//! without the data parks it and asks for the data (`Actions::snapshot_data`). This module is
//! both ends of that request on the range's core:
//!
//! - The replica that waits asks another replica of the range (any live one, in turn) for the
//!   export, piece by piece, with forwarded requests ([`Fetch`]): at most 1 MiB per answer. It
//!   writes the files whole on its disk, ingests them with the snapshot's index as the range's
//!   flushed index (clearing first an older copy of the range, which the ingestion refuses), and
//!   tells the core that the data is there.
//! - A replica asked for it exports the range's two intervals (local keys and data) once it has
//!   applied the snapshot's index, opens the files and answers each piece with a read. The export
//!   goes once the last piece was read, or after [`EXPORT_IDLE`] without a request.
//!
//! - The segment files of the search indexes over the range (011 FR-013) travel behind the SST
//!   files, as `teseladb_range::snapshot` bundles them: the source takes them from its node when
//!   the export starts and holds them in memory with the bundle's last file; the replica hands
//!   them to its node once the SST files are ingested.
//!
//! An answer "not ready", an error or a deadline sends the replica to the next source after
//! [`FETCH_RETRY`]: a replica that waits for data does not vote, so it never waits for a leader.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use teseladb_hlc::Hlc;
use teseladb_lsm::{FileRef, Lsm, LsmError};
use teseladb_multiraft::{Done, MultiRaftCore, OpError, OpId};
use teseladb_raftlog::{FileId, IoDone, IoOp};
use teseladb_range::sm::RangeSm;
use teseladb_range::snapshot::{Assembler, CHUNK_BYTES, Chunk, trailer, unbundle, write_ops};
use teseladb_types::{Bytes, GroupId, Nanos, NodeId};

use crate::coresm::CoreSm;
use crate::forward::{Fetch, FetchReply, Request};

/// Pause before asking again after "not ready" or a failure.
pub const FETCH_RETRY: Nanos = Nanos::from_millis(100);
/// An export nobody asked for in this long is removed.
pub const EXPORT_IDLE: Nanos = Nanos::from_secs(30);
/// File identifiers of this module on the core's disk thread: between the log's (from 1) and the
/// LSM's (from `0x8000_0000`).
const FILE_BASE: u32 = 0x4000_0000;
/// Identifiers this module cycles through.
const FILE_SPAN: u32 = 0x3fff_0000;
/// The newest mark: an export carries everything the source holds.
const NEWEST: Hlc = Hlc { physical: u64::MAX, logical: u32::MAX };

/// Where a core takes the segment files of the search indexes for a range snapshot and gives
/// those it received (011 FR-013): the node's search indexes.
pub trait SegmentExchange: Send + Sync {
    /// The segment files of the indexes with a segment over `intervals`.
    fn ship(&self, intervals: &[(Vec<u8>, Vec<u8>)]) -> Vec<Vec<u8>>;
    /// Takes the segment files of an ingested snapshot.
    fn receive(&self, files: Vec<Vec<u8>>);
}

impl SegmentExchange for teseladb_wire::commands::Shared {
    fn ship(&self, intervals: &[(Vec<u8>, Vec<u8>)]) -> Vec<Vec<u8>> {
        teseladb_wire::commands::lock(self).search.ship_over(intervals)
    }

    fn receive(&self, files: Vec<Vec<u8>>) {
        let mut tables = teseladb_wire::commands::lock(self);
        // A damaged file is left out: its index is built from the documents.
        for f in &files {
            tables.search.restore(f);
        }
    }
}

/// An export: the replica that asked, its range and the snapshot's index. Two ranges of a core
/// can have snapshots at the same index, so the range is part of it (007).
type ExportKey = (NodeId, GroupId, u64);

/// What a disk operation of this module is for. Sinks and exports carry a serial number, so that
/// a result for one that was replaced in the meantime is not taken for its successor's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    /// Writing a received file of sink `serial`.
    Write(u64),
    /// Opening an exported file of the export of `key` with this serial.
    Open(ExportKey, u64),
    /// Reading a piece of export `key` (`serial`) for request `req` of `from`.
    Read { from: NodeId, req: u64, key: ExportKey, serial: u64, file: u32, offset: u64 },
    /// Closing or removing what is no longer needed.
    Cleanup,
}

enum SinkState {
    /// Asks a source at this instant.
    Idle(Nanos),
    /// Waiting for piece `next` of source `from`.
    Fetching { from: NodeId, op: OpId, next: (u32, u64), asm: Assembler },
    /// Writing the files; `left` operations to go.
    Writing { refs: Vec<FileRef>, left: usize, failed: bool },
    /// Ingesting them.
    Ingesting { id: u64, refs: Vec<FileRef> },
    /// Clearing an older copy of the range before ingesting.
    Clearing { id: u64, refs: Vec<FileRef> },
}

/// The snapshot this replica waits for.
struct Sink {
    serial: u64,
    group: GroupId,
    index: u64,
    /// End of the range in the snapshot.
    end: Vec<u8>,
    /// The segment files that came with the SST files, for the node once these are ingested.
    segments: Vec<Vec<u8>>,
    state: SinkState,
}

enum SourceState {
    /// The LSM writes the exports with these identifiers.
    Exporting { pending: Vec<u64>, files: Vec<FileRef> },
    /// The files are being opened.
    Opening { files: Vec<(FileId, FileRef)>, left: usize, failed: bool },
    /// Open: pieces are read on request.
    Ready(Vec<(FileId, FileRef)>),
}

/// An export for another replica, by [`ExportKey`].
struct Source {
    serial: u64,
    group: GroupId,
    state: SourceState,
    /// The files sent after the SST files, held in memory: the segment files and, once the
    /// export is written, the bundle's last file.
    tail: Vec<Vec<u8>>,
    /// Last request.
    touched: Nanos,
}

/// The snapshot transfers of a core.
pub struct Transfers {
    me: NodeId,
    dir: PathBuf,
    nonce: u64,
    next_name: u64,
    next_file: u32,
    turn: usize,
    ingested: u64,
    sink: Option<Sink>,
    /// Snapshots of other ranges of the core that wait for theirs to end, by range (007: one
    /// transfer at a time per core).
    queued: BTreeMap<GroupId, (u64, Vec<u8>, Nanos)>,
    sources: BTreeMap<ExportKey, Source>,
    /// Ranges whose awaited snapshot a source called stale, for the core to reject.
    stale: Vec<GroupId>,
    ops: VecDeque<(IoOp, Tag)>,
    inflight: VecDeque<Tag>,
    /// The node's search indexes; without them a snapshot carries no segment files.
    segments: Option<Arc<dyn SegmentExchange>>,
}

fn not_ready() -> Bytes {
    Bytes::from_vec(FetchReply::NotReady.encode())
}

impl Transfers {
    /// The transfers of node `me`, with their files under `dir`; `nonce` tells this life's file
    /// names from those an earlier one may have left.
    pub fn new(me: NodeId, dir: PathBuf, nonce: u64) -> Transfers {
        Transfers {
            me,
            dir,
            nonce,
            next_name: 0,
            next_file: 0,
            turn: 0,
            ingested: 0,
            sink: None,
            queued: BTreeMap::new(),
            sources: BTreeMap::new(),
            stale: Vec::new(),
            ops: VecDeque::new(),
            inflight: VecDeque::new(),
            segments: None,
        }
    }

    /// The transfers sending and receiving the segment files of `segments` with the snapshots.
    pub fn with_segments(self, segments: Arc<dyn SegmentExchange>) -> Transfers {
        Transfers { segments: Some(segments), ..self }
    }

    /// Whether a snapshot is waiting for its data here.
    pub fn waiting(&self) -> bool {
        self.sink.is_some()
    }

    /// Whether a snapshot of range `group` is waiting for its data here.
    pub fn waiting_for(&self, group: GroupId) -> bool {
        self.sink.as_ref().is_some_and(|s| s.group == group) || self.queued.contains_key(&group)
    }

    /// The range whose snapshot is being received.
    pub fn sink_group(&self) -> Option<GroupId> {
        self.sink.as_ref().map(|s| s.group)
    }

    /// Snapshots received and ingested in this life.
    pub fn ingested(&self) -> u64 {
        self.ingested
    }

    /// Ranges whose awaited snapshot no replica serves any more, since the last call: the caller
    /// rejects it (`MultiRaftCore::reject_awaited`) so that the leader sends a newer one.
    pub fn take_stale(&mut self) -> Vec<GroupId> {
        std::mem::take(&mut self.stale)
    }

    /// Exports kept for other replicas.
    pub fn exports(&self) -> usize {
        self.sources.len()
    }

    fn name(&mut self, what: &str) -> PathBuf {
        self.next_name += 1;
        self.dir.join(format!("{what}-{:016x}-{}", self.nonce, self.next_name))
    }

    fn serial(&mut self) -> u64 {
        self.next_name += 1;
        self.next_name
    }

    fn file_id(&mut self) -> FileId {
        let id = FileId(FILE_BASE + self.next_file);
        self.next_file = (self.next_file + 1) % FILE_SPAN;
        id
    }

    /// The core parked the snapshot `index` of `group`, whose range ends at `end`, until its
    /// data is here; index 0 asks for the range as its split left it, for a replica that waits
    /// for a snapshot no leader sends.
    pub fn want(&mut self, group: GroupId, index: u64, end: Vec<u8>, now: Nanos) {
        if self.sink.as_ref().is_some_and(|s| s.group == group && s.index == index) {
            return;
        }
        // Another range's transfer goes on; this one waits for it.
        if self.sink.as_ref().is_some_and(|s| s.group != group) {
            self.queued.insert(group, (index, end, now));
            return;
        }
        // The files of a replaced sink still being written go once written; those of one being
        // ingested or cleared belong to the LSM's job until it ends.
        if let Some(Sink { state: SinkState::Writing { refs, .. }, .. }) = self.sink.take() {
            self.cleanup(&[], &refs);
        }
        let serial = self.serial();
        let state = SinkState::Idle(now);
        self.sink = Some(Sink { serial, group, index, end, segments: Vec::new(), state });
    }

    /// Forgets the snapshots the core no longer waits for (a newer one replaced one, and its
    /// data was here): asking for them would hold back the other ranges' transfers. One whose
    /// files are being written or ingested ends by itself.
    pub fn forget_unawaited(&mut self, core: &MultiRaftCore<CoreSm>) {
        // The range as its split left it (index 0) is wanted while the replica waits for a
        // snapshot no leader sends.
        let awaited = |g: &GroupId, index: u64| {
            core.awaited_snapshot(*g).is_some()
                || (index == 0
                    && core.state_machine(*g).is_some_and(|sm| {
                        use teseladb_multiraft::StateMachine;
                        sm.needs_snapshot()
                    }))
        };
        self.queued.retain(|g, (index, ..)| awaited(g, *index));
        let obsolete = self.sink.as_ref().is_some_and(|s| {
            matches!(s.state, SinkState::Idle(_) | SinkState::Fetching { .. })
                && !awaited(&s.group, s.index)
        });
        if obsolete {
            self.sink = None;
        }
    }

    /// Next disk operation, if any.
    pub fn poll_io(&mut self) -> Option<IoOp> {
        let (op, tag) = self.ops.pop_front()?;
        self.inflight.push_back(tag);
        Some(op)
    }

    /// Asks a source for the next piece, and removes the exports nobody asks for. `nodes` are the
    /// range's replicas this node knows of.
    pub fn tick(&mut self, core: &mut MultiRaftCore<CoreSm>, nodes: &[NodeId], now: Nanos) {
        let idle: Vec<ExportKey> = self
            .sources
            .iter()
            .filter(|(_, s)| now.saturating_since(s.touched) >= EXPORT_IDLE)
            .map(|(k, _)| *k)
            .collect();
        for key in idle {
            self.drop_source(key);
        }
        if self.sink.is_none()
            && let Some((group, (index, end, at))) = self.queued.pop_first()
        {
            let serial = self.serial();
            let state = SinkState::Idle(at);
            self.sink = Some(Sink { serial, group, index, end, segments: Vec::new(), state });
        }
        let Some(sink) = &self.sink else { return };
        let SinkState::Idle(at) = sink.state else { return };
        if now < at {
            return;
        }
        let (group, index) = (sink.group, sink.index);
        // A snapshot whose range covers another range this core holds is older than a split
        // this core already applied or received: its data would land over that range's (an
        // ingestion clears what is there first). It needs a newer snapshot (D-067).
        if covers_another(core, group, &sink.end) {
            let sink = self.sink.as_mut().expect("checked above");
            sink.state = SinkState::Idle(now + FETCH_RETRY);
            self.stale.push(group);
            return;
        }
        let mut candidates: Vec<NodeId> = nodes.to_vec();
        let conf = core.conf(group).cloned().unwrap_or_default();
        candidates.extend(conf.voters.iter().chain(&conf.learners));
        candidates.retain(|n| *n != self.me);
        candidates.sort_unstable();
        candidates.dedup();
        let sink = self.sink.as_mut().expect("checked above");
        let Some(&from) = candidates.get(self.turn % candidates.len().max(1)) else {
            sink.state = SinkState::Idle(now + FETCH_RETRY);
            return;
        };
        self.turn = self.turn.wrapping_add(1);
        let op = fetch(core, Fetch { group, index, file: 0, offset: 0 }, from, now);
        let asm = Assembler::new(group, index);
        sink.state = SinkState::Fetching { from, op, next: (0, 0), asm };
    }

    /// The end of one of the core's operations; `false` if it was not one of this module's.
    pub fn on_done(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        lsm: &mut Lsm,
        op: OpId,
        r: &Result<Done<crate::coresm::CoreOutput>, OpError>,
        now: Nanos,
    ) -> bool {
        let Some(sink) = &mut self.sink else { return false };
        let (group, index) = (sink.group, sink.index);
        let SinkState::Fetching { from, op: waiting, next, asm } = &mut sink.state else {
            return false;
        };
        if *waiting != op {
            return false;
        }
        let reply = match r {
            Ok(Done::Forwarded(body)) => FetchReply::decode(body).ok(),
            _ => None,
        };
        let chunk = match reply {
            Some(FetchReply::Chunk(c)) => c,
            // No replica will serve this snapshot: the core asks the leader for a newer one, and
            // the sink keeps asking until it replaces this one.
            Some(FetchReply::Stale) => {
                sink.state = SinkState::Idle(now + FETCH_RETRY);
                self.stale.push(group);
                return true;
            }
            // Not ready, an error or no answer: the next source, a little later.
            _ => {
                sink.state = SinkState::Idle(now + FETCH_RETRY);
                return true;
            }
        };
        let piece_end = chunk.offset + chunk.data.len() as u64;
        let after =
            if piece_end >= chunk.size { (chunk.file + 1, 0) } else { (chunk.file, piece_end) };
        match asm.add(chunk) {
            Ok(None) => {
                *next = after;
                let f = Fetch { group, index, file: after.0, offset: after.1 };
                *waiting = fetch(core, f, *from, now);
            }
            Ok(Some(files)) => match unbundle(files) {
                Some(b) => {
                    sink.segments = b.segments;
                    self.write(core, lsm, &b.ssts);
                }
                None => sink.state = SinkState::Idle(now + FETCH_RETRY),
            },
            Err(_) => sink.state = SinkState::Idle(now + FETCH_RETRY),
        }
        true
    }

    /// Writes the files received whole.
    fn write(&mut self, core: &MultiRaftCore<CoreSm>, lsm: &mut Lsm, files: &[Vec<u8>]) {
        let dir = self.name("incoming");
        let first = self.file_id();
        // The identifiers the files take after the first.
        for _ in 1..files.len() {
            self.file_id();
        }
        let (ops, refs) = write_ops(files, &dir, first);
        // An empty range has no files to write: its index is ingested at once.
        if ops.is_empty() {
            self.ingest(core, lsm, refs);
            return;
        }
        let left = ops.len();
        let sink = self.sink.as_mut().expect("a sink that writes");
        self.ops.extend(ops.into_iter().map(|op| (op, Tag::Write(sink.serial))));
        sink.state = SinkState::Writing { refs, left, failed: false };
    }

    /// Ingests the files of the sink's snapshot with its index as the range's flushed index.
    fn ingest(&mut self, core: &MultiRaftCore<CoreSm>, lsm: &mut Lsm, refs: Vec<FileRef>) {
        let sink = self.sink.as_mut().expect("a sink that ingests");
        let intervals = intervals(core, sink.group, &sink.end);
        let id = lsm.ingest_range(refs.clone(), intervals, (sink.group, sink.index));
        sink.state = SinkState::Ingesting { id, refs };
    }

    /// A request of another replica for a piece of the export of a range this core holds.
    pub fn on_fetch(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        lsm: &mut Lsm,
        (from, req): (NodeId, u64),
        f: Fetch,
        now: Nanos,
    ) {
        let key = (from, f.group, f.index);
        // A replica that waits for a snapshot has nothing of its range to give.
        let applied = core.state_machine(f.group).and_then(CoreSm::range).filter(|sm| {
            use teseladb_multiraft::StateMachine;
            !sm.needs_snapshot()
        });
        let applied = applied.map(|sm| {
            use teseladb_multiraft::StateMachine;
            (sm.applied_index(), sm.snapshot_floor(), sm.intervals())
        });
        let Some((applied, floor, intervals)) = applied else {
            core.answer_forward(from, req, not_ready());
            return;
        };
        // A snapshot from before the range's last split is never served (D-067): the export is
        // the range as it is now, without the part that split gave away, and the replica that
        // asks would replay the split over nothing. It needs a newer snapshot.
        // Index 0 is a range as its split left it (a replica that never applied its parent's
        // cut, 007): only one that applied nothing of its own log has it.
        if f.index < floor || (f.index == 0 && applied > 0) {
            core.answer_forward(from, req, Bytes::from_vec(FetchReply::Stale.encode()));
            return;
        }
        // A request for the first piece with no export yet, or after an earlier one ended,
        // starts one.
        if !self.sources.contains_key(&key) {
            if (f.file, f.offset) == (0, 0) && applied >= f.index {
                let dir = self.name("export");
                let pending = intervals
                    .iter()
                    .map(|(start, end)| lsm.export(start, end, NEWEST, dir.clone()))
                    .collect();
                let state = SourceState::Exporting { pending, files: Vec::new() };
                let serial = self.serial();
                let tail = self.segments.as_ref().map_or_else(Vec::new, |s| s.ship(&intervals));
                let source = Source { serial, group: f.group, state, tail, touched: now };
                self.sources.insert(key, source);
            }
            core.answer_forward(from, req, not_ready());
            return;
        }
        let source = self.sources.get_mut(&key).expect("checked above");
        source.touched = now;
        let SourceState::Ready(files) = &source.state else {
            core.answer_forward(from, req, not_ready());
            return;
        };
        let Some((id, r)) = files.get(f.file as usize) else {
            // A file of the tail, from memory.
            let at = f.file as usize - files.len();
            let Some(whole) = source.tail.get(at) else {
                core.answer_forward(from, req, not_ready());
                return;
            };
            let start = (f.offset as usize).min(whole.len());
            let data = whole[start..whole.len().min(start + CHUNK_BYTES)].to_vec();
            let (file, size) = (f.file, whole.len() as u64);
            let total = files.len() + source.tail.len();
            let last = at + 1 == source.tail.len() && f.offset + data.len() as u64 >= size;
            let c = Chunk {
                range: f.group,
                index: f.index,
                file,
                files: total as u32,
                offset: f.offset,
                size,
                data,
            };
            core.answer_forward(from, req, Bytes::from_vec(FetchReply::Chunk(c).encode()));
            if last {
                self.drop_source(key);
            }
            return;
        };
        let len = r.size.saturating_sub(f.offset).min(CHUNK_BYTES as u64) as u32;
        let tag =
            Tag::Read { from, req, key, serial: source.serial, file: f.file, offset: f.offset };
        let op = IoOp::ReadAt { file: *id, offset: f.offset, len, token: req };
        self.ops.push_back((op, tag));
    }

    /// Closes and removes the files of an export.
    fn drop_source(&mut self, key: ExportKey) {
        let s = self.sources.remove(&key).expect("an export of this module");
        let (open, files): (Vec<FileId>, Vec<FileRef>) = match s.state {
            SourceState::Exporting { files, .. } => (Vec::new(), files),
            SourceState::Opening { files, .. } | SourceState::Ready(files) => {
                files.into_iter().unzip()
            }
        };
        self.cleanup(&open, &files);
    }

    /// Closes and removes `files`, then their directories, which only held them.
    fn cleanup(&mut self, open: &[FileId], files: &[FileRef]) {
        for &file in open {
            self.ops.push_back((IoOp::Close { file }, Tag::Cleanup));
        }
        for f in files {
            self.ops.push_back((IoOp::Remove { path: f.path.clone() }, Tag::Cleanup));
        }
        self.remove_dirs(files);
    }

    /// Removes the directories of `files` once the files are gone from them (removed, or moved
    /// into the LSM by an ingestion).
    fn remove_dirs(&mut self, files: &[FileRef]) {
        let dirs: BTreeSet<PathBuf> =
            files.iter().filter_map(|f| f.path.parent().map(Path::to_path_buf)).collect();
        for path in dirs {
            self.ops.push_back((IoOp::RemoveDir { path }, Tag::Cleanup));
        }
    }

    /// The LSM finished export `id` with these files.
    pub fn on_exported(&mut self, id: u64, written: Vec<FileRef>) {
        let mut finished = None;
        for (key, s) in &mut self.sources {
            let SourceState::Exporting { pending, files } = &mut s.state else { continue };
            let Some(at) = pending.iter().position(|p| *p == id) else { continue };
            pending.remove(at);
            files.extend(written);
            if pending.is_empty() {
                finished = Some((*key, std::mem::take(files)));
            }
            break;
        }
        let Some((key, files)) = finished else { return };
        let source = self.sources.get_mut(&key).expect("found above");
        let segments = source.tail.len();
        source.tail.push(trailer(files.len(), segments));
        let serial = source.serial;
        // The files are opened to be read piece by piece; an empty range has none.
        let ids: Vec<(FileId, FileRef)> = files.into_iter().map(|f| (self.file_id(), f)).collect();
        for (id, f) in &ids {
            self.ops.push_back((
                IoOp::Open { file: *id, path: f.path.clone() },
                Tag::Open(key, serial),
            ));
        }
        let s = self.sources.get_mut(&key).expect("found above");
        s.state = match ids.len() {
            0 => SourceState::Ready(Vec::new()),
            left => SourceState::Opening { files: ids, left, failed: false },
        };
    }

    /// The LSM finished ingestion `id`. Returns the range whose data is now here: the caller tells
    /// the core (`MultiRaftCore::snapshot_data_ready`) once it no longer holds the LSM, which the
    /// range's restore reads.
    pub fn on_ingested(
        &mut self,
        core: &MultiRaftCore<CoreSm>,
        lsm: &mut Lsm,
        id: u64,
        r: Result<(), LsmError>,
        now: Nanos,
    ) -> Option<GroupId> {
        let sink = self.sink.as_mut()?;
        let SinkState::Ingesting { id: waiting, refs } = &mut sink.state else { return None };
        if *waiting != id {
            return None;
        }
        let group = sink.group;
        match r {
            Ok(()) => {
                // The LSM moved the files in: their directory is empty.
                let refs = std::mem::take(refs);
                let segments = std::mem::take(&mut sink.segments);
                self.remove_dirs(&refs);
                self.sink = None;
                if let Some(exchange) = &self.segments {
                    exchange.receive(segments);
                }
                self.ingested += 1;
                return Some(group);
            }
            // An older copy of the range is still here: it goes first.
            Err(LsmError::IngestOverlap) => {
                let refs = std::mem::take(refs);
                let id = lsm.clear_range(group, intervals(core, group, &sink.end));
                sink.state = SinkState::Clearing { id, refs };
            }
            // The files stay unused: they go, and the data is asked for again.
            Err(_) => {
                let refs = std::mem::take(refs);
                sink.state = SinkState::Idle(now + FETCH_RETRY);
                self.cleanup(&[], &refs);
            }
        }
        None
    }

    /// The LSM finished clearing `id`: the ingestion goes again.
    pub fn on_cleared(&mut self, core: &MultiRaftCore<CoreSm>, lsm: &mut Lsm, id: u64) {
        let Some(sink) = &mut self.sink else { return };
        let SinkState::Clearing { id: waiting, refs } = &mut sink.state else { return };
        if *waiting != id {
            return;
        }
        let refs = std::mem::take(refs);
        self.ingest(core, lsm, refs);
    }

    /// The result of one of this module's disk operations.
    pub fn on_io_done(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        lsm: &mut Lsm,
        done: IoDone,
        now: Nanos,
    ) {
        let tag = self.inflight.pop_front().expect("an operation of this module in flight");
        let ok = !matches!(done, IoDone::Err { .. } | IoDone::Read { data: Err(_), .. });
        match tag {
            Tag::Cleanup => {}
            Tag::Write(serial) => self.written(core, lsm, serial, ok, now),
            Tag::Open(key, serial) => self.opened(key, serial, ok),
            Tag::Read { from, req, key, serial, file, offset } => {
                let data = match done {
                    IoDone::Read { data: Ok(d), .. } => Some(d.to_vec()),
                    _ => None,
                };
                self.read(core, (from, req), (key, serial), (file, offset), data);
            }
        }
    }

    fn written(
        &mut self,
        core: &MultiRaftCore<CoreSm>,
        lsm: &mut Lsm,
        serial: u64,
        ok: bool,
        now: Nanos,
    ) {
        // A sink replaced by a newer snapshot meanwhile ignores the writes of the old one.
        let Some(Sink { state: SinkState::Writing { refs, left, failed }, .. }) =
            self.sink.as_mut().filter(|s| s.serial == serial)
        else {
            return;
        };
        *left -= 1;
        *failed |= !ok;
        if *left > 0 {
            return;
        }
        let (refs, failed) = (std::mem::take(refs), *failed);
        if failed {
            self.sink.as_mut().expect("matched above").state = SinkState::Idle(now + FETCH_RETRY);
            self.cleanup(&[], &refs);
            return;
        }
        self.ingest(core, lsm, refs);
    }

    fn opened(&mut self, key: ExportKey, serial: u64, ok: bool) {
        // An export removed (or replaced) meanwhile ignores the opens of the old one.
        let Some(Source { state: SourceState::Opening { files, left, failed }, .. }) =
            self.sources.get_mut(&key).filter(|s| s.serial == serial)
        else {
            return;
        };
        *left -= 1;
        *failed |= !ok;
        if *left > 0 {
            return;
        }
        if *failed {
            self.drop_source(key);
            return;
        }
        let files = std::mem::take(files);
        self.sources.get_mut(&key).expect("matched above").state = SourceState::Ready(files);
    }

    fn read(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        (from, req): (NodeId, u64),
        (key, serial): (ExportKey, u64),
        (file, offset): (u32, u64),
        data: Option<Vec<u8>>,
    ) {
        // The export may have gone (and another started) while the piece was read.
        let source = self
            .sources
            .get(&key)
            .filter(|s| s.serial == serial)
            .map(|s| (s.group, &s.state, s.tail.len()));
        let (group, files, tail, data) = match (source, data) {
            (Some((group, SourceState::Ready(files), tail)), Some(data)) => {
                (group, files, tail, data)
            }
            _ => {
                core.answer_forward(from, req, not_ready());
                return;
            }
        };
        // The tail always follows (at least the bundle's last file): a file of the disk is never
        // the last piece.
        let size = files[file as usize].1.size;
        let c = Chunk {
            range: group,
            index: key.2,
            file,
            files: (files.len() + tail) as u32,
            offset,
            size,
            data,
        };
        core.answer_forward(from, req, Bytes::from_vec(FetchReply::Chunk(c).encode()));
    }
}

/// Directory of a core's transfer files.
pub fn transfer_dir(core_dir: &Path) -> PathBuf {
    core_dir.join("transfer")
}

/// The intervals of a snapshot of range `group` that ends at `end` (empty: where the replica
/// ends): its local keys and its data.
fn intervals(core: &MultiRaftCore<CoreSm>, group: GroupId, end: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let own = core.state_machine(group).and_then(CoreSm::range).map(RangeSm::intervals);
    let mut own = own.unwrap_or_default();
    if let Some((_, e)) = own.last_mut().filter(|_| !end.is_empty()) {
        *e = end.to_vec();
    }
    own
}

/// Whether the data of a snapshot of range `group` that ends at `end` (empty: where the replica
/// ends) overlaps another range of the core.
fn covers_another(core: &MultiRaftCore<CoreSm>, group: GroupId, end: &[u8]) -> bool {
    let Some(sm) = core.state_machine(group).and_then(CoreSm::range) else { return false };
    let start = sm.bounds().0;
    let end = if end.is_empty() { sm.bounds().1 } else { end };
    core.group_ids()
        .into_iter()
        .filter(|g| *g != group)
        .filter_map(|g| core.state_machine(g).and_then(CoreSm::range))
        .any(|other| other.bounds().0 < end && start < other.bounds().1)
}

/// Asks `from` for a piece of a snapshot.
fn fetch(core: &mut MultiRaftCore<CoreSm>, f: Fetch, from: NodeId, now: Nanos) -> OpId {
    core.forward(from, Bytes::from_vec(Request::Fetch(f).encode()), now)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use teseladb_lsm::{EntryWrite, Kind, Lookup, LsmConfig};
    use teseladb_meta::bootstrap::{DATA_END, DATA_RANGE, DATA_START};
    use teseladb_multiraft::{ApplyCtx, CoreConfig, GroupDesc, PeerFrame, StateMachine};
    use teseladb_raftlog::{CoreStore, Disk, IoErrno, MemDisk, StorageConfig};
    use teseladb_range::command::Command;
    use teseladb_range::sm::RangeSm;

    use super::*;
    use crate::coresm::CoreOutput;

    const G: GroupId = DATA_RANGE;
    /// Value size: 1 500 documents make a file of more than one 1 MiB piece.
    const VALUE: usize = 1 << 10;

    /// Which kind of this module's disk operation fails once, if any.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fault {
        None,
        Write,
        Open,
        Read,
    }

    /// A node with the range over its LSM, on `MemDisk`, whose disk runs synchronously.
    struct Side {
        core: MultiRaftCore<CoreSm>,
        lsm: Rc<RefCell<Lsm>>,
        disk: MemDisk,
        t: Transfers,
        ready: Vec<GroupId>,
        fault: Fault,
        fail_ingest: bool,
        /// Directories of the files created under the transfer directory, and those removed.
        dirs: (BTreeSet<PathBuf>, BTreeSet<PathBuf>),
    }

    fn key(i: u32) -> Vec<u8> {
        [&DATA_START[..], format!("k{i:05}").as_bytes()].concat()
    }

    /// Node `me` holding `docs` documents written as entries 1..=docs and flushed: its range
    /// applied index `docs`.
    fn side(me: u64, docs: u32) -> Side {
        side_cut(me, docs, None)
    }

    /// [`side`] whose range then applied a `Split` at `at` as entry `docs + 1`.
    fn side_cut(me: u64, docs: u32, at: Option<Vec<u8>>) -> Side {
        let mut disk = MemDisk::new();
        let cfg = LsmConfig {
            memtable_bytes: 4 << 20,
            block_bytes: 4096,
            cache_bytes: 1 << 20,
            ..LsmConfig::default()
        };
        let (lsm, _) = Lsm::recover(&cfg, &mut disk).unwrap();
        let lsm = Rc::new(RefCell::new(lsm));
        let value = vec![7u8; VALUE];
        for i in 1..=docs {
            let k = key(i);
            let w = [EntryWrite { key: &k, kind: Kind::Put, value: &value }];
            let ts = Hlc { physical: u64::from(i), logical: 0 };
            lsm.borrow_mut().write_entry(G, u64::from(i), ts, &w).unwrap();
        }
        lsm.borrow_mut().flush();
        lsm.borrow_mut().run_io(&mut disk, Nanos::ZERO);
        let mut sm =
            RangeSm::open(G, DATA_START.to_vec(), DATA_END.to_vec(), lsm.clone(), &mut disk)
                .unwrap();
        if let Some(at) = at {
            let split =
                Command::Split { at, new_range: GroupId(G.0 + 1), records: vec![], right_live: 0 };
            let ts = Hlc { physical: u64::from(docs) + 1, logical: 0 };
            let ctx = ApplyCtx { group: G, index: u64::from(docs) + 1, term: 1, ts };
            sm.apply(&split.encode(), &ctx).unwrap();
        }
        let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
        let mut core = MultiRaftCore::new(CoreConfig::new(NodeId(me), 0), store, me);
        // Two voters and no ticks: nobody campaigns, so nothing is applied over the seeded state.
        let desc = GroupDesc { group: G, voters: vec![NodeId(1), NodeId(2)], learners: vec![] };
        core.create_group(desc, CoreSm::Range(sm)).unwrap();
        let t = Transfers::new(NodeId(me), PathBuf::from("transfer"), me);
        let mut s = Side {
            core,
            lsm,
            disk,
            t,
            ready: Vec::new(),
            fault: Fault::None,
            fail_ingest: false,
            dirs: (BTreeSet::new(), BTreeSet::new()),
        };
        s.io(Nanos::ZERO);
        s
    }

    impl Side {
        /// Runs `op`, noting the transfer directories it creates and removes.
        fn run(&mut self, op: IoOp) -> IoDone {
            match &op {
                IoOp::Create { path, .. } if path.starts_with(&self.t.dir) => {
                    self.dirs.0.extend(path.parent().map(Path::to_path_buf));
                }
                IoOp::RemoveDir { path } => {
                    self.dirs.1.insert(path.clone());
                }
                _ => {}
            }
            self.disk.execute(op)
        }

        /// Transfer directories made and never removed.
        fn left_dirs(&self) -> Vec<&PathBuf> {
            self.dirs.0.difference(&self.dirs.1).collect()
        }

        fn applied(&self) -> u64 {
            self.core.state_machine(G).map_or(0, StateMachine::applied_index)
        }

        fn lsm_done(&mut self, c: teseladb_lsm::Completions, now: Nanos) {
            let lsm = self.lsm.clone();
            for (id, files) in c.exported {
                self.t.on_exported(id, files);
            }
            for (id, r) in c.ingested {
                let r =
                    if std::mem::take(&mut self.fail_ingest) { Err(LsmError::ReadOnly) } else { r };
                let ready = self.t.on_ingested(&self.core, &mut lsm.borrow_mut(), id, r, now);
                self.ready.extend(ready);
            }
            for id in c.cleared {
                self.t.on_cleared(&self.core, &mut lsm.borrow_mut(), id);
            }
        }

        /// Runs every disk operation, failing the one `fault` names once.
        fn io(&mut self, now: Nanos) {
            loop {
                if let Some(op) = self.core.poll_io(now) {
                    let d = self.disk.execute(op);
                    self.core.on_io_done(d, now);
                    continue;
                }
                let op = self.lsm.borrow_mut().poll_io(now);
                let c = self.lsm.borrow_mut().take_completions();
                self.lsm_done(c, now);
                if let Some(op) = op {
                    let d = self.run(op);
                    let c = self.lsm.borrow_mut().on_io_done(d, now);
                    self.lsm_done(c, now);
                    continue;
                }
                let Some(op) = self.t.poll_io() else { return };
                let hit = matches!(
                    (&op, self.fault),
                    (IoOp::WriteAt { .. }, Fault::Write)
                        | (IoOp::Open { .. }, Fault::Open)
                        | (IoOp::ReadAt { .. }, Fault::Read)
                );
                let d = if hit {
                    self.fault = Fault::None;
                    IoDone::Err { op: op.without_data(), errno: IoErrno::NoSpace }
                } else {
                    self.run(op)
                };
                let lsm = self.lsm.clone();
                self.t.on_io_done(&mut self.core, &mut lsm.borrow_mut(), d, now);
            }
        }

        /// Whether `key` is visible in the LSM.
        fn has(&mut self, k: &[u8]) -> bool {
            let ts = Hlc { physical: u64::MAX, logical: 0 };
            loop {
                let view = self.lsm.borrow().view();
                let found = view.get(&mut self.lsm.borrow_mut(), k, ts);
                match found {
                    Lookup::Hit(_) => return true,
                    Lookup::NotFound | Lookup::Failed(_) => return false,
                    Lookup::Miss(_) => {
                        self.lsm.borrow_mut().run_io(&mut self.disk, Nanos::ZERO);
                    }
                }
            }
        }
    }

    /// Moves what `from` produced to `to`; `true` if there was something.
    fn half(from: &mut Side, to: &mut Side, now: Nanos) -> bool {
        from.io(now);
        let me = from.t.me;
        let acts = from.core.drain_actions();
        let mut moved = false;
        for (op, r) in acts.done {
            moved = true;
            let lsm = from.lsm.clone();
            from.t.on_done(&mut from.core, &mut lsm.borrow_mut(), op, &r, now);
        }
        for f in acts.forwarded {
            moved = true;
            let Ok(Request::Fetch(fetch)) = Request::decode(&f.body) else { panic!("a fetch") };
            let lsm = from.lsm.clone();
            from.t.on_fetch(&mut from.core, &mut lsm.borrow_mut(), (f.from, f.req), fetch, now);
        }
        for (_, frame) in acts.send {
            // Only forwards and their answers matter here, not the range's own Raft.
            if matches!(frame, PeerFrame::Forward { .. } | PeerFrame::ForwardResp { .. }) {
                moved = true;
                to.core.on_peer_message(me, frame, now);
            }
        }
        moved
    }

    /// Moves the frames of `a` and `b` between them, and what they ask of each other's
    /// transfers, until both are quiet.
    fn exchange(a: &mut Side, b: &mut Side, now: Nanos) {
        for _ in 0..1000 {
            let moved = half(a, b, now) | half(b, a, now);
            if !moved {
                return;
            }
        }
        panic!("never quiet");
    }

    /// Ticks and exchanges until the sink has its data, or `steps` rounds of [`FETCH_RETRY`] pass.
    fn transfer(src: &mut Side, dst: &mut Side, index: u64, steps: u32) -> Nanos {
        let mut now = Nanos::from_secs(1);
        dst.t.want(G, index, Vec::new(), now);
        for _ in 0..steps {
            let nodes = [src.t.me];
            dst.t.tick(&mut dst.core, &nodes, now);
            src.t.tick(&mut src.core, &[], now);
            exchange(src, dst, now);
            if !dst.ready.is_empty() {
                return now;
            }
            now = now + FETCH_RETRY;
        }
        now
    }

    #[test]
    fn a_snapshot_travels_in_pieces_and_is_ingested() {
        let mut src = side(1, 1500);
        let mut dst = side(2, 0);
        let index = src.applied();
        assert_eq!(index, 1500);
        transfer(&mut src, &mut dst, index, 20);
        assert_eq!(dst.ready, [G]);
        assert_eq!((dst.t.ingested(), dst.t.waiting()), (1, false));
        assert_eq!(dst.lsm.borrow().flushed_index(G), index);
        assert!(dst.has(&key(1)) && dst.has(&key(1500)));
        // The export went with its last piece, and no transfer directory is left on either side.
        assert_eq!(src.t.exports(), 0);
        assert!(src.left_dirs().is_empty(), "{:?}", src.left_dirs());
        assert!(dst.left_dirs().is_empty(), "{:?}", dst.left_dirs());
    }

    /// Intervals of a range.
    type Intervals = Vec<(Vec<u8>, Vec<u8>)>;

    /// The segment files a node gives and takes, as its search indexes would.
    #[derive(Default)]
    struct Segs {
        gives: Vec<Vec<u8>>,
        asked: std::sync::Mutex<Vec<Intervals>>,
        took: std::sync::Mutex<Vec<Vec<u8>>>,
    }

    impl SegmentExchange for Segs {
        fn ship(&self, intervals: &[(Vec<u8>, Vec<u8>)]) -> Vec<Vec<u8>> {
            self.asked.lock().unwrap().push(intervals.to_vec());
            self.gives.clone()
        }
        fn receive(&self, files: Vec<Vec<u8>>) {
            self.took.lock().unwrap().extend(files);
        }
    }

    fn with_segs(side: &mut Side, segs: &Arc<Segs>) {
        let t = std::mem::replace(&mut side.t, Transfers::new(NodeId(0), PathBuf::new(), 0));
        side.t = t.with_segments(segs.clone());
    }

    #[test]
    fn the_segment_files_travel_behind_the_sst_files() {
        let mut src = side(1, 300);
        let mut dst = side(2, 0);
        let gives = vec![vec![5u8; CHUNK_BYTES + 3], Vec::new(), vec![6]];
        let theirs = Arc::new(Segs { gives: gives.clone(), ..Segs::default() });
        let ours = Arc::new(Segs::default());
        with_segs(&mut src, &theirs);
        with_segs(&mut dst, &ours);
        transfer(&mut src, &mut dst, 300, 30);
        assert_eq!(dst.ready, [G]);
        assert!(dst.has(&key(1)) && dst.has(&key(300)));
        assert_eq!(*ours.took.lock().unwrap(), gives, "handed over once ingested");
        let asked = theirs.asked.lock().unwrap().clone();
        assert_eq!(asked.len(), 1);
        assert!(asked[0].contains(&(DATA_START.to_vec(), DATA_END.to_vec())), "{asked:?}");
        assert_eq!(src.t.exports(), 0, "the last piece of the tail ends the export");
        assert!(src.left_dirs().is_empty() && dst.left_dirs().is_empty());
        // A piece of the tail past its files is not there.
        let mut src = side(1, 3);
        with_segs(&mut src, &theirs);
        let lsm = src.lsm.clone();
        for file in [0, 0, 9] {
            let f = Fetch { group: G, index: 3, file, offset: 0 };
            src.t.on_fetch(&mut src.core, &mut lsm.borrow_mut(), (NodeId(2), 7), f, Nanos::ZERO);
            src.io(Nanos::ZERO);
        }
        let last = src.core.drain_actions().send.into_iter().rev().find_map(|(_, f)| match f {
            PeerFrame::ForwardResp { body, .. } => FetchReply::decode(&body).ok(),
            _ => None,
        });
        assert_eq!(last, Some(FetchReply::NotReady));
    }

    #[test]
    fn the_node_search_indexes_give_and_take_segment_files() {
        use teseladb_wire::commands::search_ops::Shipped;
        let shared = teseladb_wire::commands::Shared::default();
        let all = [(Vec::new(), Vec::new())];
        assert!(SegmentExchange::ship(&shared, &all).is_empty(), "nothing built");
        let id = [4u8; 12];
        let file = Shipped { collection: 1, version: 1, files: Vec::new() }.encode(&id);
        SegmentExchange::receive(&shared, vec![b"damaged".to_vec(), file]);
        // Received files wait for a query to serve them; what was built is shipped.
        assert!(SegmentExchange::ship(&shared, &all).is_empty());
    }

    #[test]
    fn files_that_are_not_a_bundle_start_the_transfer_again() {
        let mut dst = side(2, 0);
        let now = Nanos::ZERO;
        dst.t.want(G, 3, Vec::new(), now);
        dst.t.tick(&mut dst.core, &[NodeId(1)], now);
        let SinkState::Fetching { op, .. } = dst.t.sink.as_ref().unwrap().state else { panic!() };
        let whole =
            Chunk { range: G, index: 3, file: 0, files: 1, offset: 0, size: 1, data: vec![1] };
        let body = Bytes::from_vec(FetchReply::Chunk(whole).encode());
        let lsm = dst.lsm.clone();
        let r = Ok(Done::Forwarded(body));
        assert!(dst.t.on_done(&mut dst.core, &mut lsm.borrow_mut(), op, &r, now));
        assert!(matches!(dst.t.sink.as_ref().unwrap().state, SinkState::Idle(_)));
    }

    #[test]
    fn a_replica_with_an_old_copy_clears_it_before_ingesting() {
        let mut src = side(1, 40);
        let mut dst = side(2, 10);
        transfer(&mut src, &mut dst, 40, 20);
        assert_eq!(dst.ready, [G]);
        assert_eq!(dst.lsm.borrow().flushed_index(G), 40);
        assert!(dst.has(&key(40)));
    }

    #[test]
    fn an_empty_range_travels_as_one_empty_piece() {
        let mut src = side(1, 0);
        let mut dst = side(2, 0);
        transfer(&mut src, &mut dst, 0, 20);
        assert_eq!(dst.ready, [G]);
        assert_eq!(src.t.exports(), 0);
    }

    #[test]
    fn a_source_behind_the_snapshot_or_without_the_range_is_not_ready() {
        let mut src = side(1, 5);
        let mut dst = side(2, 0);
        // The source applied 5: it never exports for 50.
        transfer(&mut src, &mut dst, 50, 5);
        assert!(dst.ready.is_empty() && dst.t.waiting());
        assert_eq!(src.t.exports(), 0);
        // Asked again for the same snapshot, nothing changes.
        dst.t.want(G, 50, Vec::new(), Nanos::ZERO);
        assert!(dst.t.waiting());
        // A core without the range answers "not ready".
        let (store, _) =
            CoreStore::recover(&StorageConfig::default(), 0, &mut MemDisk::new()).unwrap();
        let mut bare: MultiRaftCore<CoreSm> =
            MultiRaftCore::new(CoreConfig::new(NodeId(3), 0), store, 3);
        let f = Fetch { group: G, index: 1, file: 0, offset: 0 };
        let mut t = Transfers::new(NodeId(3), PathBuf::from("t"), 3);
        t.on_fetch(&mut bare, &mut src.lsm.borrow_mut(), (NodeId(2), 9), f.clone(), Nanos::ZERO);
        let sent = bare.drain_actions().send;
        let answer = sent.iter().find_map(|(_, f)| match f {
            PeerFrame::ForwardResp { body, .. } => Some(body.to_vec()),
            _ => None,
        });
        assert_eq!(answer, Some(FetchReply::NotReady.encode()));
    }

    #[test]
    fn a_replica_that_waits_for_a_snapshot_serves_none() {
        // Opened with nothing of its range, it would export what an older life of the range left
        // in its LSM, or nothing at all.
        let mut src = side(1, 0);
        let mut waiting = RangeSm::open(
            G,
            DATA_START.to_vec(),
            DATA_END.to_vec(),
            src.lsm.clone(),
            &mut src.disk,
        )
        .unwrap();
        waiting.expect_snapshot();
        assert!(waiting.needs_snapshot());
        src.core.remove_group(G).unwrap();
        let desc = GroupDesc { group: G, voters: vec![NodeId(1), NodeId(2)], learners: vec![] };
        src.core.create_group(desc, CoreSm::Range(waiting)).unwrap();
        src.io(Nanos::ZERO);
        let mut dst = side(2, 0);
        transfer(&mut src, &mut dst, 0, 5);
        assert!(dst.ready.is_empty() && dst.t.waiting());
        assert_eq!(src.t.exports(), 0);
    }

    #[test]
    fn a_snapshot_from_before_the_last_split_is_never_served() {
        // The source split its range after entry 40: an export now would lack the half it gave
        // away, and the replica would replay the split over nothing (D-067).
        let mut src = side_cut(1, 40, Some(key(20)));
        let mut dst = side(2, 0);
        transfer(&mut src, &mut dst, 40, 5);
        assert!(dst.ready.is_empty() && dst.t.waiting());
        assert_eq!(src.t.exports(), 0);
        // The source says so: the replica rejects that snapshot for a newer one.
        assert!(!dst.t.take_stale().is_empty() && dst.t.take_stale().is_empty());
        // The snapshot taken after the split is served, with the range's half only.
        let mut dst = side(2, 0);
        transfer(&mut src, &mut dst, 41, 20);
        assert_eq!(dst.ready, [G]);
        assert!(dst.has(&key(19)) && !dst.has(&key(20)));
    }

    #[test]
    fn exports_of_two_ranges_at_the_same_index_stay_apart() {
        // An export of several pieces, which stays while they are asked for, and another range of
        // the core past the data space with a snapshot at the same index.
        let mut src = side(1, 1500);
        let other = GroupId(G.0 + 1);
        let (start, end) = (vec![DATA_END[0] + 1], vec![DATA_END[0] + 2]);
        let k = [&start[..], b"x"].concat();
        let w = [EntryWrite { key: &k, kind: Kind::Put, value: b"v" }];
        let ts = Hlc { physical: 1, logical: 0 };
        src.lsm.borrow_mut().write_entry(other, 1500, ts, &w).unwrap();
        src.lsm.borrow_mut().flush();
        src.lsm.borrow_mut().run_io(&mut src.disk, Nanos::ZERO);
        let sm = RangeSm::open(other, start, end, src.lsm.clone(), &mut src.disk).unwrap();
        let desc = GroupDesc { group: other, voters: vec![NodeId(1), NodeId(2)], learners: vec![] };
        src.core.create_group(desc, CoreSm::Range(sm)).unwrap();
        src.io(Nanos::ZERO);
        let answer = |src: &mut Side, group: GroupId, file: u32| {
            let f = Fetch { group, index: 1500, file, offset: 0 };
            let lsm = src.lsm.clone();
            src.t.on_fetch(&mut src.core, &mut lsm.borrow_mut(), (NodeId(2), 7), f, Nanos::ZERO);
            src.io(Nanos::ZERO);
            src.core.drain_actions().send.into_iter().find_map(|(_, f)| match f {
                PeerFrame::ForwardResp { body, .. } => FetchReply::decode(&body).ok(),
                _ => None,
            })
        };
        // The first request starts the export of `G` at 1500, which is then ready.
        assert_eq!(answer(&mut src, G, 0), Some(FetchReply::NotReady));
        let second = answer(&mut src, G, 0);
        assert!(matches!(&second, Some(FetchReply::Chunk(c)) if c.range == G), "{second:?}");
        // The other range at the same index gets an export of its own.
        assert_eq!(answer(&mut src, other, 0), Some(FetchReply::NotReady));
        let third = answer(&mut src, other, 0);
        assert!(matches!(&third, Some(FetchReply::Chunk(c)) if c.range == other), "{third:?}");
        // Its SST file and the bundle's last file: that one ends the export.
        let last = answer(&mut src, other, 1);
        assert!(matches!(&last, Some(FetchReply::Chunk(c)) if c.files == 2), "{last:?}");
        assert_eq!(src.t.exports(), 1, "the other range's last piece went, `G`'s export stays");
    }

    #[test]
    fn a_snapshot_over_another_range_of_the_core_is_stale() {
        // The replica split its range at key 20 and holds the new half too.
        let mut dst = side_cut(2, 0, Some(key(20)));
        let new = GroupId(G.0 + 1);
        let sm = RangeSm::try_open(new, key(20), DATA_END.to_vec(), dst.lsm.clone());
        let desc = GroupDesc { group: new, voters: vec![NodeId(1), NodeId(2)], learners: vec![] };
        dst.core.create_group(desc, CoreSm::Range(sm.unwrap().unwrap())).unwrap();
        let fetches = |dst: &mut Side| {
            dst.core
                .drain_actions()
                .send
                .iter()
                .filter(|(_, f)| matches!(f, PeerFrame::Forward { .. }))
                .count()
        };
        // A snapshot of the whole space, from before that split, is not asked for.
        dst.t.want(G, 5, DATA_END.to_vec(), Nanos::ZERO);
        dst.t.tick(&mut dst.core, &[NodeId(1)], Nanos::ZERO);
        assert_eq!((dst.t.take_stale(), fetches(&mut dst)), (vec![G], 0));
        assert!(dst.t.take_stale().is_empty(), "handed out once");
        // One that ends at the cut is.
        dst.t.want(G, 6, key(20), Nanos::ZERO);
        dst.t.tick(&mut dst.core, &[NodeId(1)], Nanos::from_secs(1));
        assert_eq!((dst.t.take_stale(), fetches(&mut dst)), (vec![], 1));
    }

    #[test]
    fn snapshots_the_core_no_longer_waits_for_are_forgotten() {
        let mut dst = side(2, 0);
        let other = GroupId(G.0 + 1);
        dst.t.want(G, 5, Vec::new(), Nanos::ZERO);
        dst.t.want(other, 7, Vec::new(), Nanos::ZERO);
        assert!(dst.t.waiting_for(G) && dst.t.waiting_for(other));
        // The core parked neither: both go.
        dst.t.forget_unawaited(&dst.core);
        assert!(!dst.t.waiting() && !dst.t.waiting_for(other));
    }

    #[test]
    fn without_another_replica_the_replica_waits() {
        let mut dst = side(2, 0);
        // A group this core does not have: no configuration names another replica.
        let other = GroupId(9);
        dst.t.want(other, 3, Vec::new(), Nanos::ZERO);
        dst.t.tick(&mut dst.core, &[], Nanos::ZERO);
        // Not yet time to ask again.
        dst.t.tick(&mut dst.core, &[NodeId(1)], Nanos::from_millis(1));
        assert!(dst.core.drain_actions().send.is_empty());
        assert!(dst.t.waiting());
    }

    #[test]
    fn answers_that_are_not_a_piece_or_not_awaited_restart_the_request() {
        let mut dst = side(2, 0);
        let now = Nanos::ZERO;
        let r: Result<Done<CoreOutput>, OpError> = Err(OpError::Timeout);
        assert!(
            !dst.t.on_done(&mut dst.core, &mut dst.lsm.clone().borrow_mut(), OpId(1), &r, now),
            "no sink"
        );
        dst.t.want(G, 3, Vec::new(), now);
        assert!(
            !dst.t.on_done(&mut dst.core, &mut dst.lsm.clone().borrow_mut(), OpId(1), &r, now),
            "not fetching"
        );
        dst.t.tick(&mut dst.core, &[NodeId(1)], now);
        let op = dst.core.drain_actions();
        assert!(!op.send.is_empty());
        let SinkState::Fetching { op, .. } = dst.t.sink.as_ref().unwrap().state else { panic!() };
        assert!(
            !dst.t.on_done(
                &mut dst.core,
                &mut dst.lsm.clone().borrow_mut(),
                OpId(op.0 + 1),
                &r,
                now
            ),
            "another operation"
        );
        // A piece of another snapshot is refused and the request starts over.
        let wrong =
            Chunk { range: G, index: 99, file: 0, files: 1, offset: 0, size: 1, data: vec![1] };
        let body = Bytes::from_vec(FetchReply::Chunk(wrong).encode());
        assert!(dst.t.on_done(
            &mut dst.core,
            &mut dst.lsm.clone().borrow_mut(),
            op,
            &Ok(Done::Forwarded(body)),
            now
        ));
        assert!(matches!(dst.t.sink.as_ref().unwrap().state, SinkState::Idle(_)));
        // A failed request too.
        dst.t.tick(&mut dst.core, &[NodeId(1)], now + FETCH_RETRY);
        let SinkState::Fetching { op, .. } = dst.t.sink.as_ref().unwrap().state else { panic!() };
        assert!(dst.t.on_done(&mut dst.core, &mut dst.lsm.clone().borrow_mut(), op, &r, now));
        assert!(matches!(dst.t.sink.as_ref().unwrap().state, SinkState::Idle(_)));
    }

    #[test]
    fn disk_failures_start_the_transfer_again() {
        for fault in [Fault::Write, Fault::Open, Fault::Read] {
            let mut src = side(1, 30);
            let mut dst = side(2, 0);
            match fault {
                Fault::Write => dst.fault = fault,
                _ => src.fault = fault,
            }
            transfer(&mut src, &mut dst, 30, 40);
            assert_eq!(
                dst.ready,
                [G],
                "after a failed {}",
                match fault {
                    Fault::Write => "write",
                    Fault::Open => "open",
                    _ => "read",
                }
            );
            assert!(dst.has(&key(30)));
            assert!(src.left_dirs().is_empty() && dst.left_dirs().is_empty());
        }
    }

    #[test]
    fn a_failed_ingestion_asks_for_the_data_again() {
        let mut src = side(1, 30);
        let mut dst = side(2, 0);
        dst.fail_ingest = true;
        transfer(&mut src, &mut dst, 30, 40);
        assert_eq!(dst.ready, [G]);
        assert_eq!(dst.t.ingested(), 1);
        // The files of the failed ingestion went with their directory.
        assert!(dst.left_dirs().is_empty(), "{:?}", dst.left_dirs());
    }

    /// The answers `side` sent, by request.
    fn answers(side: &mut Side) -> Vec<(u64, Vec<u8>)> {
        side.core
            .drain_actions()
            .send
            .into_iter()
            .filter_map(|(_, f)| match f {
                PeerFrame::ForwardResp { req, body, .. } => Some((req, body.to_vec())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_piece_read_after_its_export_went_is_not_ready() {
        let mut src = side(1, 30);
        let lsm = src.lsm.clone();
        let first = Fetch { group: G, index: 30, file: 0, offset: 0 };
        src.t.on_fetch(
            &mut src.core,
            &mut lsm.borrow_mut(),
            (NodeId(2), 1),
            first.clone(),
            Nanos::ZERO,
        );
        // Exporting: asked again meanwhile, it is not ready.
        src.t.on_fetch(
            &mut src.core,
            &mut lsm.borrow_mut(),
            (NodeId(2), 2),
            first.clone(),
            Nanos::ZERO,
        );
        src.io(Nanos::ZERO);
        // Ready now: the read is queued, and the export goes before it completes.
        src.t.on_fetch(
            &mut src.core,
            &mut lsm.borrow_mut(),
            (NodeId(2), 3),
            first.clone(),
            Nanos::ZERO,
        );
        src.t.tick(&mut src.core, &[], EXPORT_IDLE);
        src.io(EXPORT_IDLE);
        let not_ready = FetchReply::NotReady.encode();
        let got: Vec<(u64, bool)> =
            answers(&mut src).into_iter().map(|(r, b)| (r, b == not_ready)).collect();
        assert_eq!(got, [(1, true), (2, true), (3, true)]);
        assert_eq!(src.t.exports(), 0);
    }

    #[test]
    fn opens_of_an_export_that_went_are_ignored() {
        let mut src = side(1, 30);
        let lsm = src.lsm.clone();
        let first = Fetch { group: G, index: 30, file: 0, offset: 0 };
        src.t.on_fetch(
            &mut src.core,
            &mut lsm.borrow_mut(),
            (NodeId(2), 1),
            first.clone(),
            Nanos::ZERO,
        );
        // Run the LSM alone: the export ends and its files wait to be opened.
        loop {
            let op = lsm.borrow_mut().poll_io(Nanos::ZERO);
            let c = lsm.borrow_mut().take_completions();
            src.lsm_done(c, Nanos::ZERO);
            let Some(op) = op else { break };
            let d = src.disk.execute(op);
            let c = lsm.borrow_mut().on_io_done(d, Nanos::ZERO);
            src.lsm_done(c, Nanos::ZERO);
        }
        src.t.tick(&mut src.core, &[], EXPORT_IDLE);
        assert_eq!(src.t.exports(), 0);
        // A new export of the same key starts before the old opens complete.
        src.t.on_fetch(&mut src.core, &mut lsm.borrow_mut(), (NodeId(2), 2), first, EXPORT_IDLE);
        src.io(EXPORT_IDLE);
        assert_eq!(src.t.exports(), 1);
        let s = src.t.sources.values().next().unwrap();
        assert!(matches!(s.state, SourceState::Ready(_)));
    }

    #[test]
    fn a_newer_snapshot_replaces_the_one_being_written() {
        let mut dst = side(2, 0);
        let lsm = dst.lsm.clone();
        dst.t.want(G, 5, Vec::new(), Nanos::ZERO);
        dst.t.write(&dst.core, &mut lsm.borrow_mut(), &[vec![1, 2, 3]]);
        dst.t.want(G, 9, Vec::new(), Nanos::ZERO);
        dst.io(Nanos::ZERO);
        let sink = dst.t.sink.as_ref().unwrap();
        assert_eq!(sink.index, 9);
        assert!(matches!(sink.state, SinkState::Idle(_)));
        // The old snapshot's file went.
        assert!(
            dst.disk
                .list(Path::new("transfer"))
                .unwrap_or_default()
                .iter()
                .all(|d| { dst.disk.list(d).unwrap_or_default().is_empty() })
        );
    }

    #[test]
    fn snapshots_of_two_ranges_go_one_after_the_other() {
        let mut dst = side(2, 0);
        let other = GroupId(G.0 + 1);
        dst.t.want(G, 5, Vec::new(), Nanos::ZERO);
        dst.t.want(G, 5, Vec::new(), Nanos::ZERO);
        dst.t.want(other, 7, Vec::new(), Nanos::ZERO);
        assert_eq!(dst.t.sink_group(), Some(G));
        assert!(dst.t.waiting_for(G) && dst.t.waiting_for(other));
        assert!(!dst.t.waiting_for(GroupId(99)));
        // Once the first one is done, the next tick starts the other.
        dst.t.sink = None;
        dst.t.tick(&mut dst.core, &[], Nanos::ZERO);
        assert_eq!(dst.t.sink_group(), Some(other));
        assert_eq!(dst.t.sink.as_ref().map(|s| s.index), Some(7));
        assert!(!dst.t.waiting_for(G));
    }

    #[test]
    fn results_of_a_replaced_sink_change_nothing() {
        let mut src = side(1, 30);
        let mut dst = side(2, 10);
        let lsm = dst.lsm.clone();
        // An ingestion and a clearing of another sink end while this one waits.
        dst.t.want(G, 30, Vec::new(), Nanos::ZERO);
        let sink = dst.t.sink.as_mut().unwrap();
        sink.state = SinkState::Ingesting { id: 1000, refs: Vec::new() };
        assert_eq!(
            dst.t.on_ingested(&dst.core, &mut lsm.borrow_mut(), 7, Ok(()), Nanos::ZERO),
            None
        );
        dst.t.sink.as_mut().unwrap().state = SinkState::Clearing { id: 1000, refs: Vec::new() };
        dst.t.on_cleared(&dst.core, &mut lsm.borrow_mut(), 7);
        assert!(matches!(dst.t.sink.as_ref().unwrap().state, SinkState::Clearing { id: 1000, .. }));
        // Nor while it waits to ask.
        dst.t.sink.as_mut().unwrap().state = SinkState::Idle(Nanos::ZERO);
        assert_eq!(
            dst.t.on_ingested(&dst.core, &mut lsm.borrow_mut(), 7, Ok(()), Nanos::ZERO),
            None
        );
        dst.t.on_cleared(&dst.core, &mut lsm.borrow_mut(), 7);
        // Two replicas ask the same source at once: two exports, each ended on its own.
        let lsm = src.lsm.clone();
        for from in [2, 3] {
            let f = Fetch { group: G, index: 30, file: 0, offset: 0 };
            src.t.on_fetch(
                &mut src.core,
                &mut lsm.borrow_mut(),
                (NodeId(from), 1),
                f.clone(),
                Nanos::ZERO,
            );
        }
        // An export that is not one of theirs changes neither.
        src.t.on_exported(99_999, Vec::new());
        src.io(Nanos::ZERO);
        assert_eq!(src.t.exports(), 2);
        assert!(src.t.sources.values().all(|s| matches!(s.state, SourceState::Ready(_))));
        // A piece other than the first, of an export that does not exist, starts nothing.
        let f = Fetch { group: G, index: 30, file: 1, offset: 0 };
        src.t.on_fetch(
            &mut src.core,
            &mut lsm.borrow_mut(),
            (NodeId(4), 1),
            f.clone(),
            Nanos::ZERO,
        );
        assert_eq!(src.t.exports(), 2);
    }

    #[test]
    fn exports_nobody_asks_for_are_removed() {
        let mut src = side(1, 30);
        let mut dst = side(2, 0);
        dst.t.want(G, 30, Vec::new(), Nanos::ZERO);
        dst.t.tick(&mut dst.core, &[NodeId(1)], Nanos::ZERO);
        exchange(&mut src, &mut dst, Nanos::ZERO);
        assert_eq!(src.t.exports(), 1);
        // A piece that is not there is not ready.
        let f = Fetch { group: G, index: 30, file: 9, offset: 0 };
        let lsm = src.lsm.clone();
        src.t.on_fetch(
            &mut src.core,
            &mut lsm.borrow_mut(),
            (NodeId(2), 77),
            f.clone(),
            Nanos::ZERO,
        );
        src.t.tick(&mut src.core, &[], EXPORT_IDLE);
        assert_eq!(src.t.exports(), 0);
        src.io(EXPORT_IDLE);
        // Results that belong to nothing any more change nothing.
        src.t.on_exported(12345, Vec::new());
        assert_eq!(
            src.t.on_ingested(&src.core, &mut lsm.borrow_mut(), 5, Ok(()), Nanos::ZERO),
            None
        );
        src.t.on_cleared(&src.core, &mut lsm.borrow_mut(), 5);
    }
}
