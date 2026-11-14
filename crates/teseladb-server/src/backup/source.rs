//! The leader's side of a backup and a restore of a data range (007 FR-021, plan «Copias de
//! seguridad»), on the range's core:
//!
//! - A backup at mark `T` first reads the range's backed-up interval at `T` like a read with
//!   `atClusterTime` (a scan of one document): the read waits for the writes in flight at or
//!   below `T` and enters the read cache at `T` over the whole interval, so no write lands at or
//!   below `T` afterwards. Then the LSM exports the interval's versions up to `T` and the files
//!   are opened and read piece by piece ([`CHUNK_BYTES`]) for the node that coordinates the
//!   backup. The export goes after its last piece, or after [`EXPORT_IDLE`] without a request.
//! - A restore gives every replica of the range its files piece by piece, in order; once they
//!   are all here the replica writes them on its disk and the LSM ingests them into the range's
//!   interval, which must hold nothing. What a restore cut before it ingested (its coordinator
//!   died or stopped waiting) is the only data a cluster without collections has there: the
//!   LSM clears the interval and the ingestion goes again, once. The data is outside the log on
//!   every replica alike, as before any write to the restored collections.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use teseladb_hlc::Hlc;
use teseladb_lsm::{FileRef, Lsm, LsmError};
use teseladb_multiraft::MultiRaftCore;
use teseladb_raft::Role;
use teseladb_raftlog::{FileId, IoDone, IoOp};
use teseladb_range::host::{RangeError, RangeHost, ReadSpec, Reply, ReqId};
use teseladb_range::push::TxnView;
use teseladb_range::read::ReadConcern;
use teseladb_range::snapshot::{CHUNK_BYTES, write_ops};
use teseladb_types::{GroupId, Nanos, NodeId};

use super::{BACKUP_END, BACKUP_START};
use crate::coresm::CoreSm;
use crate::data_wire::{RemoteReply, RemoteReq};
use crate::transfer::EXPORT_IDLE;

/// File identifiers of this module on the core's disk thread, below the transfers'.
const FILE_BASE: u32 = 0x3f00_0000;
/// Identifiers this module cycles through.
const FILE_SPAN: u32 = 0x00ff_0000;

/// A backup of a range at a mark.
type ExportKey = (GroupId, Hlc);

/// A request's answer.
pub type Answer = Result<RemoteReply, RangeError>;

enum ExportState {
    /// The read at the mark is under way.
    Barrier(ReqId),
    /// The LSM exports the interval.
    Exporting(u64),
    /// The exported files are being opened.
    Opening { files: Vec<(FileId, FileRef)>, left: usize, failed: bool },
    /// Ready to be read.
    Ready(Vec<(FileId, FileRef)>),
    /// The read at the mark failed (a mark below the range's GC threshold, say).
    Failed(RangeError),
}

struct Export {
    serial: u64,
    interval: (Vec<u8>, Vec<u8>),
    state: ExportState,
    touched: Nanos,
}

enum Restore {
    /// The pieces received, file by file.
    Staging(BTreeMap<u32, Vec<u8>>),
    /// The files are written on the disk: operations left. `serial` tells its writes from those
    /// of a restore that a new one replaced.
    Writing { serial: u64, refs: Vec<FileRef>, left: usize, failed: bool },
    /// The LSM ingests them into `interval`; `cleared` once a clearing went first.
    Ingesting(Ingestion),
    /// The LSM clears the interval of what a cut restore left; then the ingestion goes again,
    /// with `flushed` as the range's flushed index, which the clearing takes back to 0.
    Clearing { id: u64, ingestion: Ingestion, flushed: u64 },
    /// The end: ingested, or why not.
    Done(Result<(), String>),
}

/// The files of a restore and where they go.
struct Ingestion {
    id: u64,
    refs: Vec<FileRef>,
    interval: (Vec<u8>, Vec<u8>),
    cleared: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    Open(ExportKey, u64),
    Read { id: u64, key: ExportKey, serial: u64, file: u32, offset: u64 },
    Write(GroupId, u64),
    Cleanup,
}

/// The backups and restores of a core's ranges.
pub struct Backups {
    dir: PathBuf,
    nonce: u64,
    next: u64,
    next_file: u32,
    exports: BTreeMap<ExportKey, Export>,
    restores: BTreeMap<GroupId, Restore>,
    ops: VecDeque<(IoOp, Tag)>,
    inflight: VecDeque<Tag>,
    replies: Vec<(u64, Answer)>,
}

fn not_ready() -> Answer {
    Ok(RemoteReply::Piece { ready: false, sizes: Vec::new(), data: Vec::new() })
}

fn failed(why: &str) -> RangeError {
    RangeError::Mutation(teseladb_range::host::MutationError(why.to_owned()))
}

/// The part of `[start, end)` a backup copies: documents, index entries and transaction records.
pub fn backed_up(start: &[u8], end: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let s = start.max(&BACKUP_START[..]).to_vec();
    let e = match end.is_empty() {
        true => BACKUP_END.to_vec(),
        false => end.min(&BACKUP_END[..]).to_vec(),
    };
    (s, e)
}

impl Backups {
    /// The backups of a core, with their files under `dir`; `nonce` tells this life's file names
    /// from those an earlier one may have left.
    pub fn new(dir: PathBuf, nonce: u64) -> Backups {
        Backups {
            dir,
            nonce,
            next: 0,
            next_file: 0,
            exports: BTreeMap::new(),
            restores: BTreeMap::new(),
            ops: VecDeque::new(),
            inflight: VecDeque::new(),
            replies: Vec::new(),
        }
    }

    fn name(&mut self, what: &str) -> PathBuf {
        self.next += 1;
        self.dir.join(format!("{what}-{:016x}-{}", self.nonce, self.next))
    }

    fn file_id(&mut self) -> FileId {
        let id = FileId(FILE_BASE + self.next_file);
        self.next_file = (self.next_file + 1) % FILE_SPAN;
        id
    }

    /// The answers ready, with the identifiers [`Backups::request`] gave.
    pub fn take_replies(&mut self) -> Vec<(u64, Answer)> {
        std::mem::take(&mut self.replies)
    }

    /// Takes request `req` for range `g`; its answer comes in [`Backups::take_replies`] with `id`.
    /// A backup's piece needs the range's leader (another replica answers `NotLeader`); a
    /// restore's request goes to every replica, each of which ingests the files.
    pub fn request(
        &mut self,
        (core, host): (&mut MultiRaftCore<CoreSm>, &mut RangeHost),
        id: u64,
        (g, req): (GroupId, RemoteReq),
        (now, wall_ms): (Nanos, u64),
    ) {
        let status = core.status(g);
        let leads = status.as_ref().is_some_and(|s| s.role == Role::Leader);
        let bounds = core.state_machine(g).and_then(CoreSm::range).map(|sm| {
            let (s, e) = sm.bounds();
            backed_up(s, e)
        });
        let answer = match (req, bounds) {
            (RemoteReq::BackupFetch { at, file, offset }, Some(interval)) if leads => {
                self.fetch(core, host, id, (g, at), (file, offset), interval, (now, wall_ms))
            }
            (RemoteReq::RestorePiece { file, offset, data }, Some(_)) => {
                self.piece(g, file, offset, data)
            }
            (RemoteReq::RestoreIngest { files }, Some(_)) => self.ingest(g, files),
            _ => {
                let hint = status.map_or(NodeId(0), |s| s.leader);
                Some(Err(RangeError::NotLeader { hint }))
            }
        };
        if let Some(a) = answer {
            self.replies.push((id, a));
        }
    }

    fn fetch(
        &mut self,
        core: &mut MultiRaftCore<CoreSm>,
        host: &mut RangeHost,
        id: u64,
        key: ExportKey,
        (file, offset): (u32, u64),
        interval: (Vec<u8>, Vec<u8>),
        (now, wall_ms): (Nanos, u64),
    ) -> Option<Answer> {
        let Some(export) = self.exports.get_mut(&key) else {
            // Only the first piece starts a backup; a later one finds its export gone.
            if (file, offset) != (0, 0) {
                return Some(Err(failed("the backup's export is gone: start it again")));
            }
            let spec = ReadSpec {
                g: key.0,
                key: interval.0.clone(),
                scan: Some((interval.1.clone(), 1)),
                backward: false,
                at: Some(key.1),
                concern: ReadConcern::Linearizable,
                view: TxnView::default(),
                uncertain_to: None,
            };
            let read = host.read_with(core, spec, now, wall_ms);
            self.next += 1;
            let export = Export {
                serial: self.next,
                interval,
                state: ExportState::Barrier(read),
                touched: now,
            };
            self.exports.insert(key, export);
            return Some(not_ready());
        };
        export.touched = now;
        match &export.state {
            ExportState::Failed(e) => {
                let e = e.clone();
                self.exports.remove(&key);
                Some(Err(e))
            }
            ExportState::Ready(files) if files.is_empty() => {
                Some(Ok(RemoteReply::Piece { ready: true, sizes: Vec::new(), data: Vec::new() }))
            }
            ExportState::Ready(files) => {
                let Some((fid, r)) = files.get(file as usize) else {
                    return Some(Err(failed("no such file in the backup")));
                };
                let len = r.size.saturating_sub(offset).min(CHUNK_BYTES as u64) as u32;
                let (fid, serial) = (*fid, export.serial);
                let op = IoOp::ReadAt { file: fid, offset, len, token: id };
                self.ops.push_back((op, Tag::Read { id, key, serial, file, offset }));
                None
            }
            ExportState::Barrier(_) | ExportState::Exporting(_) | ExportState::Opening { .. } => {
                Some(not_ready())
            }
        }
    }

    fn piece(&mut self, g: GroupId, file: u32, offset: u64, data: Vec<u8>) -> Option<Answer> {
        // The first piece of the first file starts a restore again.
        if (file, offset) == (0, 0) {
            self.restores.insert(g, Restore::Staging(BTreeMap::new()));
        }
        let Some(Restore::Staging(files)) = self.restores.get_mut(&g) else {
            return Some(Err(failed("no restore of this range is receiving files")));
        };
        let buf = files.entry(file).or_default();
        if buf.len() as u64 != offset {
            return Some(Err(failed("a restore's piece came out of order")));
        }
        buf.extend_from_slice(&data);
        Some(Ok(RemoteReply::Restored { done: false }))
    }

    fn ingest(&mut self, g: GroupId, count: u32) -> Option<Answer> {
        let answer = match self.restores.get(&g) {
            None => Err(failed("no restore of this range is under way")),
            Some(Restore::Done(Ok(()))) => Ok(RemoteReply::Restored { done: true }),
            Some(Restore::Done(Err(why))) => Err(failed(why)),
            Some(Restore::Writing { .. } | Restore::Ingesting(_) | Restore::Clearing { .. }) => {
                Ok(RemoteReply::Restored { done: false })
            }
            Some(Restore::Staging(files)) => {
                if !files.keys().copied().eq(0..count) {
                    return Some(Err(failed("the restore's files did not all come")));
                }
                let bufs: Vec<Vec<u8>> = files.values().cloned().collect();
                let dir = self.name("restore");
                let first = self.file_id();
                // The files take consecutive identifiers from `first`.
                for _ in 1..bufs.len() {
                    self.file_id();
                }
                let (ops, refs) = write_ops(&bufs, &dir, first);
                self.next += 1;
                let serial = self.next;
                // No file at all still waits for one write to end.
                let left = ops.len().max(1);
                self.ops.extend(ops.into_iter().map(|op| (op, Tag::Write(g, serial))));
                let state = Restore::Writing { serial, refs, left, failed: false };
                self.restores.insert(g, state);
                Ok(RemoteReply::Restored { done: false })
            }
        };
        Some(answer)
    }

    /// The host answered read `id`; `true` if it was the read at the mark of a backup, which then
    /// exports its interval (or fails with the read's error).
    pub fn on_host_reply(
        &mut self,
        lsm: &mut Lsm,
        id: ReqId,
        r: &Result<Reply, RangeError>,
    ) -> bool {
        let Some((key, _)) = self
            .exports
            .iter()
            .find(|(_, e)| matches!(e.state, ExportState::Barrier(b) if b == id))
        else {
            return false;
        };
        let key = *key;
        let dir = self.name("export");
        let e = self.exports.get_mut(&key).expect("found above");
        e.state = match r {
            // The read entered the cache at the mark; an intent met at the mark is exported as it
            // is, with its transaction's record, which the backup copies too.
            Ok(_) | Err(RangeError::ReadIntent { .. }) => {
                ExportState::Exporting(lsm.export(&e.interval.0, &e.interval.1, key.1, dir))
            }
            Err(err) => ExportState::Failed(err.clone()),
        };
        true
    }

    /// The LSM finished export `id`; the files come back if it is not one of this module's.
    pub fn on_exported(&mut self, id: u64, files: Vec<FileRef>) -> Result<(), Vec<FileRef>> {
        let Some((key, _)) = self
            .exports
            .iter()
            .find(|(_, e)| matches!(e.state, ExportState::Exporting(x) if x == id))
        else {
            return Err(files);
        };
        let key = *key;
        let ids: Vec<(FileId, FileRef)> = files.into_iter().map(|f| (self.file_id(), f)).collect();
        let e = self.exports.get_mut(&key).expect("found above");
        for (fid, f) in &ids {
            let op = IoOp::Open { file: *fid, path: f.path.clone() };
            self.ops.push_back((op, Tag::Open(key, e.serial)));
        }
        e.state = match ids.len() {
            0 => ExportState::Ready(Vec::new()),
            left => ExportState::Opening { files: ids, left, failed: false },
        };
        Ok(())
    }

    /// The LSM finished ingestion `id`: the range of a restore it ended well, which the caller
    /// snapshots at once, so that a replica added later receives the data in a snapshot and not
    /// by replaying entries that lack it (as a range born from a split). An interval that holds
    /// data is cleared, once, and ingested again. Its result comes back if it is not one of this
    /// module's.
    pub fn on_ingested(
        &mut self,
        lsm: &mut Lsm,
        id: u64,
        r: Result<(), LsmError>,
    ) -> Result<Option<GroupId>, Result<(), LsmError>> {
        let Some(g) = self
            .restores
            .iter()
            .find(|(_, s)| matches!(s, Restore::Ingesting(i) if i.id == id))
            .map(|(g, _)| *g)
        else {
            return Err(r);
        };
        let Some(Restore::Ingesting(ingestion)) = self.restores.remove(&g) else {
            unreachable!("found above")
        };
        let done = match r {
            Ok(()) => {
                // The LSM moved the files in: their directory is empty.
                self.remove_dirs(&ingestion.refs);
                self.restores.insert(g, Restore::Done(Ok(())));
                return Ok(Some(g));
            }
            // What a restore cut before this one ingested: it goes first.
            Err(LsmError::IngestOverlap) if !ingestion.cleared => {
                let flushed = lsm.flushed_index(g);
                let id = lsm.clear_range(g, vec![ingestion.interval.clone()]);
                self.restores.insert(g, Restore::Clearing { id, ingestion, flushed });
                return Ok(None);
            }
            Err(LsmError::IngestOverlap) => {
                "the range is not empty: a restore needs an empty cluster".to_owned()
            }
            Err(other) => format!("the ingestion failed: {other}"),
        };
        self.fail(g, &ingestion.refs, done);
        Ok(None)
    }

    /// The LSM finished clearing `id`; `false` if it is not one of this module's. The restore's
    /// files are ingested again, with the range's flushed index of before the clearing.
    pub fn on_cleared(&mut self, lsm: &mut Lsm, id: u64) -> bool {
        let Some(g) = self
            .restores
            .iter()
            .find(|(_, s)| matches!(s, Restore::Clearing { id: x, .. } if *x == id))
            .map(|(g, _)| *g)
        else {
            return false;
        };
        let Some(Restore::Clearing { ingestion, flushed, .. }) = self.restores.remove(&g) else {
            unreachable!("found above")
        };
        let intervals = vec![ingestion.interval.clone()];
        let id = lsm.ingest_range(ingestion.refs.clone(), intervals, (g, flushed));
        self.restores.insert(g, Restore::Ingesting(Ingestion { id, cleared: true, ..ingestion }));
        true
    }

    /// Removes the directories of `files`, once the files are gone from them.
    fn remove_dirs(&mut self, files: &[FileRef]) {
        let dirs: std::collections::BTreeSet<PathBuf> =
            files.iter().filter_map(|f| f.path.parent().map(PathBuf::from)).collect();
        for path in dirs {
            self.ops.push_back((IoOp::RemoveDir { path }, Tag::Cleanup));
        }
    }

    /// Next disk operation.
    pub fn poll_io(&mut self) -> Option<IoOp> {
        let (op, tag) = self.ops.pop_front()?;
        self.inflight.push_back(tag);
        Some(op)
    }

    /// The result of an operation of [`Backups::poll_io`].
    pub fn on_io_done(&mut self, core: &MultiRaftCore<CoreSm>, lsm: &mut Lsm, done: IoDone) {
        let tag = self.inflight.pop_front().expect("an operation of this module in flight");
        let ok = !matches!(done, IoDone::Err { .. } | IoDone::Read { data: Err(_), .. });
        match tag {
            Tag::Cleanup => {}
            Tag::Open(key, serial) => self.opened(key, serial, ok),
            Tag::Read { id, key, serial, file, offset } => {
                let data = match done {
                    IoDone::Read { data: Ok(d), .. } => Some(d.to_vec()),
                    _ => None,
                };
                self.read(id, (key, serial), (file, offset), data);
            }
            Tag::Write(g, serial) => self.written(core, lsm, (g, serial), ok),
        }
    }

    fn opened(&mut self, key: ExportKey, serial: u64, ok: bool) {
        let Some(Export { state: ExportState::Opening { files, left, failed: f }, .. }) =
            self.exports.get_mut(&key).filter(|e| e.serial == serial)
        else {
            return;
        };
        *left -= 1;
        *f |= !ok;
        if *left > 0 {
            return;
        }
        if *f {
            self.drop_export(key);
            return;
        }
        let files = std::mem::take(files);
        self.exports.get_mut(&key).expect("matched above").state = ExportState::Ready(files);
    }

    fn read(
        &mut self,
        id: u64,
        (key, serial): (ExportKey, u64),
        (file, offset): (u32, u64),
        data: Option<Vec<u8>>,
    ) {
        let ready =
            self.exports.get(&key).filter(|e| e.serial == serial).and_then(|e| match &e.state {
                ExportState::Ready(files) => {
                    Some(files.iter().map(|(_, f)| f.size).collect::<Vec<_>>())
                }
                _ => None,
            });
        let (Some(sizes), Some(data)) = (ready, data) else {
            self.replies.push((id, Err(failed("reading the backup's file failed"))));
            return;
        };
        let last =
            file as usize + 1 == sizes.len() && offset + data.len() as u64 >= sizes[file as usize];
        self.replies.push((id, Ok(RemoteReply::Piece { ready: true, sizes, data })));
        if last {
            self.drop_export(key);
        }
    }

    fn written(
        &mut self,
        core: &MultiRaftCore<CoreSm>,
        lsm: &mut Lsm,
        (g, serial): (GroupId, u64),
        ok: bool,
    ) {
        let Some(Restore::Writing { serial: of, refs, left, failed: f }) =
            self.restores.get_mut(&g)
        else {
            return;
        };
        // A write of a restore that a new one replaced counts for nothing.
        if *of != serial {
            return;
        }
        *left -= 1;
        *f |= !ok;
        if *left > 0 {
            return;
        }
        let (refs, f) = (std::mem::take(refs), *f);
        let interval = core.state_machine(g).and_then(CoreSm::range).map(|sm| {
            let (s, e) = sm.bounds();
            backed_up(s, e)
        });
        match (f, interval) {
            (false, Some((s, e))) => {
                let id = lsm.ingest(refs.clone(), &s, &e);
                let ingestion = Ingestion { id, refs, interval: (s, e), cleared: false };
                self.restores.insert(g, Restore::Ingesting(ingestion));
            }
            (true, _) => self.fail(g, &refs, "writing the restore's files failed".to_owned()),
            (false, None) => self.fail(g, &refs, "the range left this core".to_owned()),
        }
    }

    /// Ends the restore of `g` with `why`, removing its files.
    fn fail(&mut self, g: GroupId, refs: &[FileRef], why: String) {
        for f in refs {
            self.ops.push_back((IoOp::Remove { path: f.path.clone() }, Tag::Cleanup));
        }
        self.remove_dirs(refs);
        self.restores.insert(g, Restore::Done(Err(why)));
    }

    /// Closes and removes the files of an export.
    fn drop_export(&mut self, key: ExportKey) {
        let Some(e) = self.exports.remove(&key) else { return };
        let files = match e.state {
            ExportState::Opening { files, .. } | ExportState::Ready(files) => files,
            ExportState::Barrier(_) | ExportState::Exporting(_) | ExportState::Failed(_) => {
                Vec::new()
            }
        };
        for (fid, f) in &files {
            self.ops.push_back((IoOp::Close { file: *fid }, Tag::Cleanup));
            self.ops.push_back((IoOp::Remove { path: f.path.clone() }, Tag::Cleanup));
        }
        let refs: Vec<FileRef> = files.into_iter().map(|(_, f)| f).collect();
        self.remove_dirs(&refs);
    }

    /// Removes the exports nobody asked for in [`EXPORT_IDLE`].
    pub fn tick(&mut self, now: Nanos) {
        let idle: Vec<ExportKey> = self
            .exports
            .iter()
            .filter(|(_, e)| now.saturating_since(e.touched) >= EXPORT_IDLE)
            .map(|(k, _)| *k)
            .collect();
        for key in idle {
            self.drop_export(key);
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use teseladb_lsm::LsmConfig;
    use teseladb_meta::bootstrap::{DATA_END, DATA_RANGE, DATA_START};
    use teseladb_multiraft::{CoreConfig, GroupDesc, NodeClock};
    use teseladb_raftlog::{CoreStore, Disk, MemDisk, StorageConfig};
    use teseladb_range::sm::RangeSm;

    use super::*;

    const ME: NodeId = NodeId(1);
    const AT: Hlc = Hlc { physical: 10, logical: 0 };
    const NOW: (Nanos, u64) = (Nanos::ZERO, 0);

    /// A core with the data range, which this node does not lead yet, and its host and LSM.
    fn world() -> (MultiRaftCore<CoreSm>, RangeHost, Rc<RefCell<Lsm>>, MemDisk) {
        let mut disk = MemDisk::new();
        let (store, _) = CoreStore::recover(&StorageConfig::default(), 0, &mut disk).unwrap();
        let mut core = MultiRaftCore::new(CoreConfig::new(ME, 0), store, 1);
        let lcfg = LsmConfig { dir: "lsm".into(), ..LsmConfig::default() };
        let lsm = Rc::new(RefCell::new(Lsm::recover(&lcfg, &mut disk).unwrap().0));
        let (s, e) = (DATA_START.to_vec(), DATA_END.to_vec());
        let sm = RangeSm::open(DATA_RANGE, s.clone(), e.clone(), lsm.clone(), &mut disk).unwrap();
        let desc = GroupDesc { group: DATA_RANGE, voters: vec![ME, NodeId(2)], learners: vec![] };
        core.create_group(desc, CoreSm::Range(sm)).unwrap();
        let mut host = RangeHost::new(lsm.clone(), NodeClock::for_node(ME));
        host.add_range(DATA_RANGE, s, e);
        (core, host, lsm, disk)
    }

    fn ask(
        b: &mut Backups,
        w: &mut (MultiRaftCore<CoreSm>, RangeHost),
        g: GroupId,
        r: RemoteReq,
    ) -> Answer {
        b.request((&mut w.0, &mut w.1), 1, (g, r), NOW);
        b.take_replies().pop().expect("an answer").1
    }

    fn piece(file: u32, offset: u64, data: &[u8]) -> RemoteReq {
        RemoteReq::RestorePiece { file, offset, data: data.to_vec() }
    }

    #[test]
    fn the_copied_interval_is_documents_and_records() {
        assert_eq!(backed_up(&[0], &[]), (vec![1], vec![3]));
        assert_eq!(backed_up(&[1, 5], &[2]), (vec![1, 5], vec![2]));
        assert_eq!(backed_up(&[2], &[9]), (vec![2], vec![3]));
    }

    #[test]
    fn a_backup_needs_the_leader_and_a_restore_any_replica_of_a_range_here() {
        let (core, host, _lsm, _disk) = world();
        let mut w = (core, host);
        let mut b = Backups::new("b".into(), 1);
        let fetch = RemoteReq::BackupFetch { at: AT, file: 0, offset: 0 };
        assert!(matches!(
            ask(&mut b, &mut w, DATA_RANGE, fetch),
            Err(RangeError::NotLeader { .. })
        ));
        let other = GroupId(99);
        assert!(matches!(
            ask(&mut b, &mut w, other, piece(0, 0, b"x")),
            Err(RangeError::NotLeader { .. })
        ));
        // Not a backup's request at all.
        let scan = RemoteReq::Scan {
            start: vec![],
            end: vec![],
            limit: 1,
            concern: ReadConcern::Majority,
            backward: false,
        };
        assert!(matches!(ask(&mut b, &mut w, DATA_RANGE, scan), Err(RangeError::NotLeader { .. })));
        assert!(b.take_replies().is_empty());
    }

    #[test]
    fn a_restores_pieces_come_in_order_and_its_files_all_before_the_ingestion() {
        let (core, host, lsm, mut disk) = world();
        let mut w = (core, host);
        let mut b = Backups::new("b".into(), 1);
        let ingest = |n| RemoteReq::RestoreIngest { files: n };
        // Nothing staged.
        assert!(ask(&mut b, &mut w, DATA_RANGE, ingest(1)).is_err());
        // A piece that is not the first of a restore, then pieces out of order.
        assert!(ask(&mut b, &mut w, DATA_RANGE, piece(0, 3, b"x")).is_err());
        assert!(ask(&mut b, &mut w, DATA_RANGE, piece(0, 0, b"ab")).is_ok());
        assert!(ask(&mut b, &mut w, DATA_RANGE, piece(0, 5, b"x")).is_err());
        assert!(ask(&mut b, &mut w, DATA_RANGE, piece(0, 2, b"cd")).is_ok());
        // Two files expected, one came.
        assert!(ask(&mut b, &mut w, DATA_RANGE, ingest(2)).is_err());
        let started = ask(&mut b, &mut w, DATA_RANGE, ingest(1));
        assert_eq!(started, Ok(RemoteReply::Restored { done: false }));
        // A piece while the files are written belongs to no staging.
        assert!(ask(&mut b, &mut w, DATA_RANGE, piece(1, 4, b"x")).is_err());
        // The files are written; a failed write ends the restore.
        let mut n = 0;
        while let Some(op) = b.poll_io() {
            let done = match n {
                0 => disk.execute(op),
                _ => IoDone::Err { op, errno: teseladb_raftlog::IoErrno::NoSpace },
            };
            n += 1;
            b.on_io_done(&w.0, &mut lsm.borrow_mut(), done);
        }
        assert!(n > 1);
        let failed = ask(&mut b, &mut w, DATA_RANGE, ingest(1));
        assert!(
            matches!(&failed, Err(RangeError::Mutation(m)) if m.0.contains("writing")),
            "{failed:?}"
        );
    }

    #[test]
    fn ingestions_and_exports_of_others_pass_through_and_a_refused_one_ends_the_restore() {
        let (core, host, lsm, mut disk) = world();
        let mut w = (core, host);
        let mut b = Backups::new("b".into(), 1);
        assert_eq!(b.on_ingested(&mut lsm.borrow_mut(), 77, Ok(())), Err(Ok(())));
        assert!(!b.on_cleared(&mut lsm.borrow_mut(), 77));
        assert!(b.on_exported(77, Vec::new()).is_err());
        assert!(!b.on_host_reply(&mut lsm.borrow_mut(), 77, &Err(RangeError::Timeout)));
        assert!(ask(&mut b, &mut w, DATA_RANGE, piece(0, 0, b"not an sst")).is_ok());
        assert!(ask(&mut b, &mut w, DATA_RANGE, RemoteReq::RestoreIngest { files: 1 }).is_ok());
        while let Some(op) = b.poll_io() {
            let done = disk.execute(op);
            b.on_io_done(&w.0, &mut lsm.borrow_mut(), done);
        }
        let Some(Restore::Ingesting(i)) = b.restores.get(&DATA_RANGE) else { panic!("ingesting") };
        let id = i.id;
        let still = Ok(RemoteReply::Restored { done: false });
        assert_eq!(ask(&mut b, &mut w, DATA_RANGE, RemoteReq::RestoreIngest { files: 1 }), still);
        // Data there: the interval is cleared, and the files are ingested again once.
        assert_eq!(
            b.on_ingested(&mut lsm.borrow_mut(), id, Err(LsmError::IngestOverlap)),
            Ok(None)
        );
        let Some(Restore::Clearing { id: clearing, .. }) = b.restores.get(&DATA_RANGE) else {
            panic!("clearing")
        };
        let clearing = *clearing;
        assert_eq!(ask(&mut b, &mut w, DATA_RANGE, RemoteReq::RestoreIngest { files: 1 }), still);
        assert!(!b.on_cleared(&mut lsm.borrow_mut(), clearing + 1));
        assert!(b.on_cleared(&mut lsm.borrow_mut(), clearing));
        let Some(Restore::Ingesting(i)) = b.restores.get(&DATA_RANGE) else { panic!("again") };
        assert!(i.cleared);
        let id = i.id;
        assert_eq!(
            b.on_ingested(&mut lsm.borrow_mut(), id, Err(LsmError::IngestOverlap)),
            Ok(None)
        );
        let r = ask(&mut b, &mut w, DATA_RANGE, RemoteReq::RestoreIngest { files: 1 });
        assert!(matches!(&r, Err(RangeError::Mutation(m)) if m.0.contains("not empty")), "{r:?}");
        let ingesting = |id| {
            let refs = vec![FileRef::new("r/a.sst".into(), 1)];
            Restore::Ingesting(Ingestion { id, refs, interval: (vec![1], vec![3]), cleared: false })
        };
        b.restores.insert(DATA_RANGE, ingesting(5));
        assert_eq!(b.on_ingested(&mut lsm.borrow_mut(), 5, Err(LsmError::ReadOnly)), Ok(None));
        let r = ask(&mut b, &mut w, DATA_RANGE, RemoteReq::RestoreIngest { files: 1 });
        assert!(matches!(&r, Err(RangeError::Mutation(m)) if m.0.contains("failed")), "{r:?}");
        b.restores.insert(DATA_RANGE, ingesting(6));
        assert_eq!(b.on_ingested(&mut lsm.borrow_mut(), 6, Ok(())), Ok(Some(DATA_RANGE)));
        assert_eq!(
            ask(&mut b, &mut w, DATA_RANGE, RemoteReq::RestoreIngest { files: 1 }),
            Ok(RemoteReply::Restored { done: true })
        );
        // A range gone from the core when its files are written.
        let writing = Restore::Writing { serial: 4, refs: Vec::new(), left: 1, failed: false };
        b.restores.insert(GroupId(99), writing);
        // A write of a restore replaced since counts for nothing.
        b.written(&w.0, &mut lsm.borrow_mut(), (GroupId(99), 3), true);
        assert!(matches!(b.restores.get(&GroupId(99)), Some(Restore::Writing { left: 1, .. })));
        b.written(&w.0, &mut lsm.borrow_mut(), (GroupId(99), 4), true);
        assert!(matches!(b.restores.get(&GroupId(99)), Some(Restore::Done(Err(_)))));
        b.written(&w.0, &mut lsm.borrow_mut(), (GroupId(98), 4), true);
    }

    #[test]
    fn a_restore_of_several_files_or_of_none_writes_them_and_answers_go_to_their_own_job() {
        let (core, host, lsm, _disk) = world();
        let mut w = (core, host);
        let mut b = Backups::new("b".into(), 1);
        assert!(ask(&mut b, &mut w, DATA_RANGE, piece(0, 0, b"ab")).is_ok());
        assert!(ask(&mut b, &mut w, DATA_RANGE, piece(1, 0, b"cd")).is_ok());
        let ingest = |n| RemoteReq::RestoreIngest { files: n };
        assert!(ask(&mut b, &mut w, DATA_RANGE, ingest(2)).is_ok());
        let Some(Restore::Writing { refs, .. }) = b.restores.get(&DATA_RANGE) else {
            panic!("writing")
        };
        assert_eq!(refs.len(), 2);
        // No file at all: the restore still waits for one write to end.
        b.restores.insert(DATA_RANGE, Restore::Staging(BTreeMap::new()));
        assert!(ask(&mut b, &mut w, DATA_RANGE, ingest(0)).is_ok());
        assert!(matches!(b.restores.get(&DATA_RANGE), Some(Restore::Writing { left: 1, .. })));
        // Another job's ingestion or export, while one of this module's is under way.
        let refs = Vec::new();
        let ingestion = Ingestion { id: 5, refs, interval: (vec![1], vec![3]), cleared: false };
        b.restores.insert(DATA_RANGE, Restore::Ingesting(ingestion));
        assert_eq!(b.on_ingested(&mut lsm.borrow_mut(), 6, Ok(())), Err(Ok(())));
        b.exports.insert((DATA_RANGE, AT), export(ExportState::Exporting(8)));
        assert!(b.on_exported(9, Vec::new()).is_err());
        assert!(matches!(b.exports[&(DATA_RANGE, AT)].state, ExportState::Exporting(8)));
    }

    #[test]
    fn opens_and_reads_of_an_export_that_changed_meanwhile_answer_nothing_or_fail() {
        let mut b = Backups::new("b".into(), 1);
        let key = (DATA_RANGE, AT);
        // Opened for an export that is gone, or that is no longer opening.
        b.opened(key, 1, true);
        b.exports.insert(key, export(ExportState::Exporting(8)));
        b.opened(key, 1, true);
        assert!(matches!(b.exports[&key].state, ExportState::Exporting(8)));
        // A read of an export not ready, or that read nothing.
        b.read(3, (key, 1), (0, 0), Some(b"ab".to_vec()));
        let f = FileRef::new("x".into(), 4);
        b.exports.insert(key, export(ExportState::Ready(vec![(FileId(1), f)])));
        b.read(4, (key, 1), (0, 0), None);
        let replies = b.take_replies();
        assert_eq!(replies.len(), 2);
        assert!(replies.iter().all(|(_, r)| r.is_err()));
        assert!(b.exports.contains_key(&key), "a failed read leaves the export");
    }

    fn export(state: ExportState) -> Export {
        Export { serial: 1, interval: (vec![1], vec![3]), state, touched: Nanos::ZERO }
    }

    #[test]
    fn exports_answer_by_their_state_and_go_when_idle_or_read_whole() {
        let mut b = Backups::new("b".into(), 1);
        let key = (DATA_RANGE, AT);
        let path = std::path::PathBuf::from("b/f");
        // The barrier's read fails: the export fails with its error.
        b.exports.insert(key, export(ExportState::Barrier(3)));
        let lsm = Rc::new(RefCell::new(
            Lsm::recover(
                &LsmConfig { dir: "l".into(), ..LsmConfig::default() },
                &mut MemDisk::new(),
            )
            .unwrap()
            .0,
        ));
        assert!(b.on_host_reply(&mut lsm.borrow_mut(), 3, &Err(RangeError::SnapshotTooOld)));
        assert!(matches!(b.exports[&key].state, ExportState::Failed(RangeError::SnapshotTooOld)));
        // Exported: an empty export is ready at once, files are opened.
        b.exports.insert(key, export(ExportState::Exporting(8)));
        assert!(b.on_exported(8, Vec::new()).is_ok());
        assert!(matches!(&b.exports[&key].state, ExportState::Ready(f) if f.is_empty()));
        b.exports.insert(key, export(ExportState::Exporting(9)));
        let f = FileRef::new(path.clone(), 4);
        assert!(b.on_exported(9, vec![f.clone(), f.clone()]).is_ok());
        // One open fails: the export goes, its files are closed and removed.
        let ok = IoDone::Ok(IoOp::Close { file: FileId(1) });
        let bad = IoDone::Err {
            op: IoOp::Close { file: FileId(1) },
            errno: teseladb_raftlog::IoErrno::NoSpace,
        };
        let mut dones = vec![ok, bad].into_iter();
        let mut disk = MemDisk::new();
        let (core, _, _, _) = world();
        while let Some(_op) = b.poll_io() {
            let done = dones.next().unwrap_or(IoDone::Ok(IoOp::Close { file: FileId(1) }));
            b.on_io_done(&core, &mut lsm.borrow_mut(), done);
        }
        assert!(!b.exports.contains_key(&key));
        // Reads of a ready export: a failed one answers an error; the last piece ends it.
        b.exports.insert(key, export(ExportState::Ready(vec![(FileId(1), f)])));
        b.ops.push_back((
            IoOp::Close { file: FileId(1) },
            Tag::Read { id: 4, key, serial: 1, file: 0, offset: 0 },
        ));
        b.poll_io();
        b.on_io_done(&core, &mut lsm.borrow_mut(), IoDone::Ok(IoOp::Close { file: FileId(1) }));
        assert!(b.take_replies()[0].1.is_err());
        b.ops.push_back((
            IoOp::Close { file: FileId(1) },
            Tag::Read { id: 5, key, serial: 1, file: 0, offset: 0 },
        ));
        b.poll_io();
        let read =
            IoDone::Read { token: 5, data: Ok(teseladb_types::Bytes::from_vec(b"abcd".to_vec())) };
        b.on_io_done(&core, &mut lsm.borrow_mut(), read);
        assert!(matches!(&b.take_replies()[0].1, Ok(RemoteReply::Piece { ready: true, .. })));
        assert!(!b.exports.contains_key(&key), "read whole");
        // Idle exports go; a fresh one stays.
        b.exports.insert(key, export(ExportState::Exporting(1)));
        b.tick(Nanos::from_secs(1));
        assert!(b.exports.contains_key(&key));
        b.tick(EXPORT_IDLE);
        assert!(!b.exports.contains_key(&key));
        b.drop_export(key);
        let _ = disk.execute(IoOp::Close { file: FileId(1) });
    }

    #[test]
    fn fetches_answer_from_the_exports_state() {
        let (mut core, mut host, _lsm, _disk) = world();
        let mut b = Backups::new("b".into(), 1);
        let key = (DATA_RANGE, AT);
        let interval = (vec![1], vec![3]);
        let fetch = |b: &mut Backups,
                     core: &mut MultiRaftCore<CoreSm>,
                     host: &mut RangeHost,
                     file,
                     offset| {
            b.fetch(core, host, 1, key, (file, offset), interval.clone(), NOW)
        };
        // A later piece of an export that is gone.
        assert!(matches!(fetch(&mut b, &mut core, &mut host, 1, 0), Some(Err(_))));
        // The first starts the read at the mark.
        assert_eq!(fetch(&mut b, &mut core, &mut host, 0, 0), Some(not_ready()));
        assert_eq!(fetch(&mut b, &mut core, &mut host, 0, 0), Some(not_ready()));
        b.exports.insert(key, export(ExportState::Failed(RangeError::SnapshotTooOld)));
        assert_eq!(
            fetch(&mut b, &mut core, &mut host, 0, 0),
            Some(Err(RangeError::SnapshotTooOld))
        );
        b.exports.insert(key, export(ExportState::Ready(Vec::new())));
        let empty =
            Some(Ok(RemoteReply::Piece { ready: true, sizes: Vec::new(), data: Vec::new() }));
        assert_eq!(fetch(&mut b, &mut core, &mut host, 0, 0), empty);
        let f = FileRef::new("x".into(), 4);
        b.exports.insert(key, export(ExportState::Ready(vec![(FileId(1), f)])));
        assert!(matches!(fetch(&mut b, &mut core, &mut host, 3, 0), Some(Err(_))));
        assert_eq!(fetch(&mut b, &mut core, &mut host, 0, 0), None, "read from the disk");
        assert!(b.poll_io().is_some());
    }
}
