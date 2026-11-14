//! Recovery of a core at start-up (plan, "Recovery") and handling of `node.meta`.
//!
//! It is the only part of the engine that does synchronous I/O: it reads whole files with
//! [`Disk::read_all`] and runs the clean-up operations directly.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use teseladb_types::{Bytes, GroupId};

use crate::config::StorageConfig;
use crate::core_store::CoreStore;
use crate::disk::{Disk, FileId, IoDone, IoErrno, IoOp};
use crate::error::StorageError;
use crate::format::meta::NodeMeta;
use crate::format::wal::{BATCH_RESUMED, WalPayload, WalRecord};
use crate::format::{BLOCK, FileHeader, FileKind, FormatError};
use crate::group_log::{GroupLog, HardState, LogEntry, SnapshotMeta};
use crate::paths;
use crate::sim_disk::SECTOR;
use crate::wal::WalSegmentInfo;
use crate::wal::{SEALED_REGION_MAGIC, open_region};
use teseladb_crypto::at_rest::{Encryption, FileKey};

/// Recovered group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveredGroup {
    /// Group.
    pub id: GroupId,
    /// Configuration (the one at creation or the one of the latest snapshot).
    pub conf: Bytes,
    /// Hard state.
    pub hard_state: HardState,
    /// Latest known snapshot.
    pub snapshot: Option<SnapshotMeta>,
    /// Last index of the log.
    pub last_index: u64,
}

/// Recovery summary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recovered {
    /// Groups found in the WAL.
    pub groups: Vec<RecoveredGroup>,
    /// WAL records of complete batches, in order.
    pub wal_records: Vec<WalRecord>,
    /// Last complete batch.
    pub last_batch: u64,
}

const META: &str = "node.meta";

fn io_err(op: &'static str) -> impl Fn(IoErrno) -> StorageError {
    move |errno| StorageError::Io { op, errno }
}

fn corrupt(file: &Path, offset: u64) -> StorageError {
    StorageError::Corrupt { file: file.to_path_buf(), offset }
}

fn exec(disk: &mut dyn Disk, op: IoOp) -> Result<(), StorageError> {
    match disk.execute(op) {
        IoDone::Err { op, errno } => Err(StorageError::Io { op: op.name(), errno }),
        _ => Ok(()),
    }
}

/// Writes a small file atomically: `tmp` → sync → rename → directory sync.
///
/// # Errors
///
/// [`StorageError::Io`] or [`StorageError::DiskFull`] when a step fails.
pub fn write_atomic(
    disk: &mut dyn Disk,
    path: &Path,
    data: Vec<u8>,
    file: FileId,
) -> Result<(), StorageError> {
    let tmp = paths::tmp(path);
    let dir = path.parent().map(Path::to_path_buf).unwrap_or_default();
    let _ = disk.execute(IoOp::Remove { path: tmp.clone() });
    exec(disk, IoOp::Create { file, path: tmp.clone(), prealloc: 0 })?;
    exec(disk, IoOp::WriteAt { file, offset: 0, data: Bytes::from_vec(data), batch: 0 })?;
    exec(disk, IoOp::Sync { file, batch: 0 })?;
    exec(disk, IoOp::Close { file })?;
    exec(disk, IoOp::Rename { from: tmp, to: path.to_path_buf() })?;
    exec(disk, IoOp::SyncDir { path: dir })
}

/// Reads `node.meta` or creates it, checks the number of cores and increments `boot_epoch`
/// (rewriting it atomically before any other write).
///
/// # Errors
///
/// [`StorageError::CoreCountMismatch`] when `node.meta` has another number of cores, and the errors of
/// [`read_node_meta`] and [`write_node_meta`].
pub fn open_node(
    disk: &mut dyn Disk,
    cores: u32,
    cluster_id: [u8; 16],
    now_unix_ms: u64,
) -> Result<NodeMeta, StorageError> {
    let mut meta = read_node_meta(disk)?.unwrap_or(NodeMeta {
        cores,
        cluster_id,
        node_id: 0,
        created_unix_ms: now_unix_ms,
        boot_epoch: 0,
        hlc_upper_bound: 0,
        reserved: Vec::new(),
    });
    if meta.cores != cores {
        return Err(StorageError::CoreCountMismatch { meta: meta.cores, requested: cores });
    }
    meta.boot_epoch = meta.boot_epoch.wrapping_add(1);
    write_node_meta(disk, &meta)?;
    Ok(meta)
}

/// Reads `node.meta` without changing it (`None` if the directory is new).
///
/// A damaged `node.meta` next to a whole temporary file of its next write is that write: [`write_atomic`] only renames
/// the temporary file after syncing it, so one whose checksum holds was written completely. A disk that acknowledged a
/// sync it did not keep (a virtual machine stopped with a full disk) leaves `node.meta` zeroed that way, and the node
/// starts from the temporary file; the bound of its clock there is at least the one it replaces.
///
/// # Errors
///
/// [`StorageError::Corrupt`] or [`StorageError::FormatVersion`] for a damaged or unknown `node.meta` without a whole
/// temporary file, and [`StorageError::Io`] when it cannot be read.
pub fn read_node_meta(disk: &mut dyn Disk) -> Result<Option<NodeMeta>, StorageError> {
    let path = PathBuf::from(META);
    match disk.read_all(&path) {
        Ok(b) => NodeMeta::decode(&b).map(Some).or_else(|e| match e {
            FormatError::Version(v) => Err(StorageError::FormatVersion { found: v }),
            _ => disk
                .read_all(&paths::tmp(&path))
                .ok()
                .and_then(|t| NodeMeta::decode(&t).ok())
                .map(Some)
                .ok_or_else(|| corrupt(&path, 0)),
        }),
        Err(IoErrno::NotFound) => Ok(None),
        Err(e) => Err(StorageError::Io { op: "read", errno: e }),
    }
}

/// Rewrites `node.meta` atomically (for example, to record the `node_id` Group 0 assigned).
///
/// # Errors
///
/// The errors of [`write_atomic`].
pub fn write_node_meta(disk: &mut dyn Disk, meta: &NodeMeta) -> Result<(), StorageError> {
    write_atomic(disk, &PathBuf::from(META), meta.encode().to_vec(), FileId(u32::MAX))
}

/// A segment whose header write never reached the disk whole: its creation did but not its first
/// batch, so nothing in it was acknowledged (the header is synced together with that batch). A
/// header only has content in its first sector (fields) and its last one (CRC), so a torn header
/// is invalid with one of those two still zero (or the file is shorter than a block). An invalid
/// header with both written is corruption.
fn unborn(data: &[u8]) -> bool {
    let Some(h) = data.get(..BLOCK) else {
        return true;
    };
    let zero = |s: &[u8]| s.iter().all(|&b| b == 0);
    FileHeader::decode(data).is_err() && (zero(&h[..SECTOR]) || zero(&h[BLOCK - SECTOR..]))
}

/// Tells whether `[lo, hi)` contains a whole zero 512 B sector (aligned to the file) or goes past
/// the end of the file (bytes that were never written).
fn has_zero_sector(file: &[u8], lo: usize, hi: usize) -> bool {
    if hi > file.len() {
        return true;
    }
    let mut s = lo.next_multiple_of(SECTOR);
    while s + SECTOR <= hi {
        if file[s..s + SECTOR].iter().all(|&b| b == 0) {
            return true;
        }
        s += SECTOR;
    }
    false
}

/// Result of walking a file batch by batch.
struct Walk {
    /// Complete batches: (`batch_seq`, offset of the start of their region, record bytes).
    batches: Vec<(u64, usize, Vec<u8>)>,
    /// Last `batch_seq` of a valid header (even if its region is broken).
    max_seen: u64,
}

/// Walks the regions of a file (WAL or object segment) starting at `BLOCK`.
/// `header` decodes the batch header at a position: returns (`batch_seq`, `prev_seq`, resumed,
/// header length, `region_len`, `region_crc`). `tail_len` gives, from the complete batches, how many
/// bytes the last write that does not start with a batch header may take (in an object segment,
/// the footer written when sealing it), and `sealed_at` tells whether a complete, valid footer
/// starts at a position (the segment was sealed and only the rename was lost).
fn walk_regions(
    file: &[u8],
    path: &Path,
    last_segment: bool,
    mut prev: u64,
    chain_global: bool,
    header: impl Fn(&[u8]) -> Option<(u64, u64, bool, usize, usize, u32)>,
    tail_len: impl Fn(&[(u64, usize, Vec<u8>)]) -> usize,
    sealed_at: impl Fn(usize) -> bool,
) -> Result<Walk, StorageError> {
    let mut walk = Walk { batches: Vec::new(), max_seen: 0 };
    let mut pos = BLOCK;
    while pos + 8 <= file.len() {
        let parsed = header(&file[pos..]);
        let complete = parsed.and_then(|(seq, p, resumed, hlen, rlen, crc)| {
            let start = pos + hlen;
            let region = file.get(start..start + rlen)?;
            let chained = if chain_global {
                p == prev && (seq == p + 1 || (resumed && seq > p))
            } else {
                p == prev && seq > p
            };
            (chained && teseladb_hash::crc32c(region) == crc).then(|| (seq, start, region.to_vec()))
        });
        if let Some((seq, start, region)) = complete {
            walk.max_seen = walk.max_seen.max(seq);
            prev = seq;
            let next = start + region.len();
            walk.batches.push((seq, start, region));
            pos = next.next_multiple_of(BLOCK);
            continue;
        }
        if parsed.is_none() && sealed_at(pos) {
            break;
        }
        // Incomplete batch: unused space, broken end or corruption (plan, step 3).
        // A batch write reaches the block boundary (with non-zero padding).
        let span_end = parsed
            .map_or_else(
                || pos + tail_len(&walk.batches).max(BLOCK),
                |(_, _, _, hlen, rlen, _)| pos + hlen + rlen,
            )
            .next_multiple_of(BLOCK);
        if let Some((seq, ..)) = parsed {
            walk.max_seen = walk.max_seen.max(seq);
        }
        let later_header =
            (pos + BLOCK..file.len()).step_by(BLOCK).any(|q| header(&file[q..]).is_some());
        if later_header {
            return Err(corrupt(path, pos as u64));
        }
        let block_unused = file[pos..(pos + BLOCK).min(file.len())].iter().all(|&b| b == 0);
        if block_unused || (last_segment && has_zero_sector(file, pos, span_end)) {
            break;
        }
        return Err(corrupt(path, pos as u64));
    }
    Ok(walk)
}

fn wal_header(b: &[u8]) -> Option<(u64, u64, bool, usize, usize, u32)> {
    match WalRecord::decode(b) {
        Ok((
            WalRecord {
                batch_seq,
                payload: WalPayload::Batch { region_len, region_crc, prev_seq, flags },
                ..
            },
            n,
        )) => Some((
            batch_seq,
            prev_seq,
            flags & BATCH_RESUMED != 0,
            n,
            region_len as usize,
            region_crc,
        )),
        _ => None,
    }
}

/// The data key of a WAL segment from its header: `None` for a clear segment; an error for an
/// encrypted one without the key ring or with another master key.
fn segment_key(
    data: &[u8],
    path: &Path,
    encryption: Option<&Encryption>,
) -> Result<Option<FileKey>, StorageError> {
    let header = FileHeader::decode(data).map_err(|_| corrupt(path, 0))?;
    let Some(wrapped) = header.wrapped_key() else {
        return Ok(None);
    };
    let ring = encryption.ok_or(StorageError::Invalid("an encrypted WAL needs the master key"))?;
    ring.ring()
        .open_file_key(&wrapped)
        .map(Some)
        .map_err(|_| StorageError::Invalid("the master key does not open the WAL"))
}

/// The records of a region, opened with the segment's key when it is sealed.
fn region_records(
    region: &[u8],
    path: &Path,
    base: usize,
    key: Option<(&FileKey, u64, u64)>,
) -> Result<Vec<WalRecord>, StorageError> {
    let sealed = region.get(..2) == Some(&SEALED_REGION_MAGIC.to_le_bytes()[..]);
    match (sealed, key) {
        (false, _) => wal_records(region, path, base),
        (true, Some((k, segment, batch))) => {
            let plain =
                open_region(k, segment, batch, region).ok_or_else(|| corrupt(path, base as u64))?;
            wal_records(&plain, path, base)
        }
        (true, None) => Err(corrupt(path, base as u64)),
    }
}

/// Records of a WAL region (up to the zero padding).
fn wal_records(region: &[u8], path: &Path, base: usize) -> Result<Vec<WalRecord>, StorageError> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 2 <= region.len()
        && crate::format::le_u16(&region[pos..pos + 2]) == crate::format::wal::RECORD_MAGIC
    {
        let (rec, n) =
            WalRecord::decode(&region[pos..]).map_err(|_| corrupt(path, (base + pos) as u64))?;
        out.push(rec);
        pos += n;
    }
    Ok(out)
}

struct Builder<'a> {
    store: CoreStore,
    disk: &'a mut dyn Disk,
}

impl Builder<'_> {
    fn open(&mut self, path: &Path) -> Result<FileId, StorageError> {
        let file = self.store.alloc_file();
        exec(self.disk, IoOp::Open { file, path: path.to_path_buf() })?;
        Ok(file)
    }

    fn read(&mut self, path: &Path) -> Result<Vec<u8>, StorageError> {
        self.disk.read_all(path).map_err(io_err("read"))
    }

    fn check_header(data: &[u8], path: &Path, kind: FileKind) -> Result<(), StorageError> {
        match FileHeader::decode(data) {
            Ok(h) if h.kind == kind => Ok(()),
            Err(FormatError::Version(v)) => Err(StorageError::FormatVersion { found: v }),
            _ => Err(corrupt(path, 0)),
        }
    }
}

impl CoreStore {
    /// Recovers core `core` by reading `disk`. Blocking; only at start-up.
    ///
    /// # Panics
    ///
    /// Never on what it reads: the checks behind each `expect` (a last segment, a registered one, a
    /// snapshot read before) hold by the order of recovery.
    ///
    /// # Errors
    ///
    /// [`StorageError::Corrupt`] for damaged data that recovery cannot skip, [`StorageError::FormatVersion`]
    /// for an unknown format, and [`StorageError::Io`] when a file cannot be read.
    #[expect(
        clippy::too_many_lines,
        reason = "recovery reads the files in the order of its specification, one step after another"
    )]
    pub fn recover(
        cfg: &StorageConfig,
        core: u16,
        disk: &mut dyn Disk,
    ) -> Result<(CoreStore, Recovered), StorageError> {
        cfg.validate().map_err(|_| StorageError::Invalid("configuration"))?;
        let mut b = Builder { store: CoreStore::empty(cfg.clone(), core, 1), disk };
        let mut rec = Recovered::default();

        // --- WAL ---
        let wal_dir = paths::wal_dir(core);
        let mut wal_files: Vec<(u64, PathBuf)> = Vec::new();
        for p in b.disk.list(&wal_dir).unwrap_or_default() {
            match paths::classify_wal(&p) {
                Some(seq) => wal_files.push((seq, p)),
                None if p.extension().is_some_and(|e| e == "tmp") => {
                    exec(b.disk, IoOp::Remove { path: p })?;
                }
                None => {}
            }
        }
        wal_files.sort();
        // The segments are read one after another, never all at once: a WAL of gigabytes (many
        // small batches, each padded to its block) did not fit in the memory of the node that had
        // written it, and the node could not start again. Only the current segment and the next
        // one, whose first batch says whether the current may end torn, are in memory.
        // The newest segment may have been created by a run that crashed before its first batch
        // reached the disk (its header may or may not have): nothing in it was acknowledged, and
        // it must not hide that the previous segment is the last one with batches. "Nothing" means
        // no complete batch and, at most, a broken tail; a damaged batch is still corruption.
        let empty_last = |d: &Vec<u8>, path: &Path| {
            unborn(d)
                || (matches!(FileHeader::decode(d), Ok(h) if h.kind == FileKind::Wal) && {
                    // Its first batch is checked on its own: it chains to whatever it claims.
                    let claimed = wal_header(&d[BLOCK..]).map_or(0, |h| h.1);
                    walk_regions(d, path, true, claimed, true, wal_header, |_| BLOCK, |_| false)
                        .is_ok_and(|w| w.batches.is_empty())
                })
        };
        if let Some((_, path)) = wal_files.last() {
            let last = b.read(path)?;
            if empty_last(&last, path) {
                let (_, path) = wal_files.pop().expect("last segment");
                exec(b.disk, IoOp::Remove { path })?;
                exec(b.disk, IoOp::SyncDir { path: wal_dir.clone() })?;
            }
        }
        // The first remaining WAL batch chains to one from an already released segment: its
        // `prev_seq` is accepted as it is.
        let mut prev: Option<u64> = None;
        let mut max_seen = 0u64;
        let mut last_base: BTreeMap<GroupId, u64> = BTreeMap::new();
        let mut next_data: Option<Vec<u8>> = None;
        for (i, (seq, path)) in wal_files.iter().enumerate() {
            let data = if let Some(d) = next_data.take() {
                d
            } else {
                let d = b.read(path)?;
                Builder::check_header(&d, path, FileKind::Wal)?;
                d
            };
            let prev_seq =
                *prev.get_or_insert_with(|| wal_header(&data[BLOCK..]).map_or(0, |h| h.1));
            if let Some((_, next_path)) = wal_files.get(i + 1) {
                let d = b.read(next_path)?;
                Builder::check_header(&d, next_path, FileKind::Wal)?;
                next_data = Some(d);
            }
            // A segment may end in a broken batch if it was the last one of a run: it is the last
            // one now, or the next segment's first batch is a resumption.
            let torn_allowed = next_data.as_ref().is_none_or(|next| {
                wal_header(&next[BLOCK..]).is_some_and(|(.., resumed, _, _, _)| resumed)
            });
            let walk = walk_regions(
                &data,
                path,
                torn_allowed,
                prev_seq,
                true,
                wal_header,
                |_| BLOCK,
                |_| false,
            )?;
            max_seen = max_seen.max(walk.max_seen);
            // Every existing segment is registered (with each group's highest index and the bases
            // it holds) so that release stays prefix-based after a restart too.
            let mut seg_groups: BTreeMap<GroupId, u64> = BTreeMap::new();
            let key = segment_key(&data, path, cfg.encryption.as_ref())?;
            for (bseq, start, region) in walk.batches {
                let opener = key.as_ref().map(|k| (k, *seq, bseq));
                let recs = region_records(&region, path, start, opener)?;
                for r in &recs {
                    match &r.payload {
                        WalPayload::Entries { first_index, entries } => {
                            // An empty `ENTRIES` is never written; if there were one, it would not raise the maximum.
                            let last = (first_index + entries.len() as u64).saturating_sub(1);
                            let m = seg_groups.entry(r.group).or_insert(0);
                            *m = (*m).max(last);
                        }
                        WalPayload::GroupCreate { .. } | WalPayload::Checkpoint { .. } => {
                            last_base.insert(r.group, *seq);
                        }
                        _ => {}
                    }
                }
                rec.wal_records.extend(recs);
                prev = Some(bseq);
            }
            let file = b.open(path)?;
            b.store.wal.segments.insert(
                *seq,
                WalSegmentInfo {
                    seq: *seq,
                    file,
                    size_used: data.len() as u64,
                    groups: seg_groups,
                    bases: BTreeSet::new(),
                },
            );
            b.store.wal.set_next_file_seq(seq + 1);
        }
        for (g, seq) in last_base {
            b.store.wal.segments.get_mut(&seq).expect("registered segment").bases.insert(g);
        }
        let prev = prev.unwrap_or(0);
        rec.last_batch = prev;

        // --- state to keep writing ---
        let next_batch = max_seen.max(prev) + 1;
        b.store.reset_batch_seq(next_batch);
        b.store.wal.last_batch = prev;
        b.store.wal.resumed = prev > 0 || max_seen > 0;
        b.store.wal.force_new = true;

        // --- latest snapshot of each group (before replaying: WAL release may have deleted
        // entries that only that snapshot covers, newer than the group's last base) ---
        let ring = cfg.encryption.as_ref().map(teseladb_crypto::at_rest::Encryption::ring);
        let mut by_group: BTreeMap<GroupId, Vec<(u64, PathBuf)>> = BTreeMap::new();
        for p in b.disk.list(&paths::snap_dir(core)).unwrap_or_default() {
            match paths::classify_snap(&p) {
                Some(paths::SnapFileName::Group(g, i)) => {
                    by_group.entry(g).or_default().push((i, p));
                }
                Some(paths::SnapFileName::Tmp) => exec(b.disk, IoOp::Remove { path: p })?,
                _ => {}
            }
        }
        let mut latest: BTreeMap<GroupId, (u64, PathBuf, Vec<u8>)> = BTreeMap::new();
        let mut snap_floor: BTreeMap<GroupId, (u64, u64)> = BTreeMap::new();
        for (g, files) in &mut by_group {
            files.sort();
            let (index, path) = files.last().cloned().expect("at least one");
            let data = b.read(&path)?;
            // A damaged or foreign one covers nothing (and if the group exists, recovery fails below).
            let valid = crate::format::snapshot::GroupSnapshot::decode_with(&data, ring)
                .ok()
                .filter(|(_, snap)| (snap.group == *g) && (snap.index == index));
            if let Some((_, snap)) = valid {
                snap_floor.insert(*g, (snap.index, snap.term));
            }
            latest.insert(*g, (index, path, data));
        }

        // --- groups: the records of the complete batches are replayed in order ---
        let mut groups = replay_all(&rec.wal_records, &snap_floor)
            .map_err(|_| StorageError::Corrupt { file: wal_dir.clone(), offset: 0 })?;
        // --- group snapshots: the newest of each group has to be valid ---
        for (g, files) in by_group {
            let Some(log) = groups.get_mut(&g) else {
                // Deleted group: its snapshots are no longer useful.
                for (_, p) in files {
                    exec(b.disk, IoOp::Remove { path: p })?;
                }
                continue;
            };
            let (index, path, data) = latest.remove(&g).expect("read before");
            let (_, snap) = crate::format::snapshot::GroupSnapshot::decode_with(&data, ring)
                .ok()
                .filter(|(_, s)| s.group == g && s.index == index)
                .ok_or_else(|| corrupt(&path, 0))?;
            if snap.index > log.snapshot_index {
                log.compact(snap.index, snap.term);
            }
            log.conf = snap.conf.clone();
            let mut kept = Vec::new();
            for (i, p) in files {
                let len = if i == index { data.len() as u64 } else { b.read(&p)?.len() as u64 };
                let term = if i == index { snap.term } else { 0 };
                kept.push(crate::core_store::SnapFile { index: i, term, path: p, len });
            }
            b.store.snap_files.insert(g, kept);
        }
        // A compacted log needs its snapshot: without it there is nowhere to get the state from.
        for (g, log) in &groups {
            let newest = b.store.snap_files.get(g).and_then(|f| f.last()).map_or(0, |f| f.index);
            if log.snapshot_index > newest {
                return Err(corrupt(&paths::snap_dir(core), 0));
            }
        }
        rec.groups = groups
            .values()
            .map(|g| RecoveredGroup {
                id: g.id,
                conf: g.conf.clone(),
                hard_state: g.hard,
                snapshot: (g.snapshot_index > 0).then(|| SnapshotMeta {
                    index: g.snapshot_index,
                    term: g.snapshot_term,
                    conf: g.conf.clone(),
                }),
                last_index: g.last_index(),
            })
            .collect();
        b.store.groups = groups;
        Ok((b.store, rec))
    }
}

/// Replays the retained WAL records. The first retained segment may hold records of a group older
/// than its first base (`GROUP_CREATE` or `GROUP_CHECKPOINT`): their entries and truncations are
/// applied right after that base (entries covered by its snapshot are dropped); their partial state
/// (`HARD_STATE`, `SNAPSHOT_MARK`) is already inside the checkpoint.
fn replay_all(
    records: &[WalRecord],
    snaps: &BTreeMap<GroupId, (u64, u64)>,
) -> Result<BTreeMap<GroupId, GroupLog>, &'static str> {
    let mut first_base: BTreeMap<GroupId, usize> = BTreeMap::new();
    for (i, r) in records.iter().enumerate() {
        if matches!(r.payload, WalPayload::GroupCreate { .. } | WalPayload::Checkpoint { .. }) {
            first_base.entry(r.group).or_insert(i);
        }
    }
    // A record of a life of its group that ends in a `GroupDelete` (no `GroupCreate` between
    // them) describes nothing that survives: the deletion removed the group's snapshots, and once
    // it was durable the segments with the group's base or entries could go, so what is left of
    // that life may have gaps. Only the deletion itself is replayed.
    let mut ends_deleted = vec![false; records.len()];
    let mut next_is_delete: BTreeMap<GroupId, bool> = BTreeMap::new();
    for (i, r) in records.iter().enumerate().rev() {
        ends_deleted[i] = next_is_delete.get(&r.group) == Some(&true);
        match r.payload {
            WalPayload::GroupDelete => {
                next_is_delete.insert(r.group, true);
            }
            WalPayload::GroupCreate { .. } => {
                next_is_delete.insert(r.group, false);
            }
            _ => {}
        }
    }
    let mut groups: BTreeMap<GroupId, GroupLog> = BTreeMap::new();
    let mut early: BTreeMap<GroupId, Vec<&WalRecord>> = BTreeMap::new();
    for (i, r) in records.iter().enumerate() {
        let g = r.group;
        if ends_deleted[i] && !matches!(r.payload, WalPayload::GroupDelete) {
            continue;
        }
        let before_base = first_base.get(&g).is_some_and(|&b| i < b) && !groups.contains_key(&g);
        if before_base {
            early.entry(g).or_default().push(r);
            continue;
        }
        replay(&mut groups, r, snaps)?;
        if first_base.get(&g) == Some(&i)
            && let (Some(pending), WalPayload::Checkpoint { .. }) = (early.remove(&g), &r.payload)
        {
            for p in pending {
                if matches!(p.payload, WalPayload::Entries { .. } | WalPayload::Truncate { .. }) {
                    replay(&mut groups, p, snaps)?;
                }
            }
        }
    }
    Ok(groups)
}

/// Applies a WAL record to the in-memory logs. Error if the record is inconsistent with the state
/// (a group that does not exist, a gap in the log): that only happens with a corrupt WAL.
fn replay(
    groups: &mut BTreeMap<GroupId, GroupLog>,
    r: &WalRecord,
    snaps: &BTreeMap<GroupId, (u64, u64)>,
) -> Result<(), &'static str> {
    let g = r.group;
    match &r.payload {
        WalPayload::GroupCreate { conf } => {
            groups.insert(g, GroupLog::new(g, conf.clone()));
        }
        WalPayload::GroupDelete => {
            groups.remove(&g);
        }
        WalPayload::Checkpoint {
            term,
            vote,
            commit,
            snapshot_index,
            snapshot_term,
            last_truncate,
            conf,
        } => {
            let log = groups.entry(g).or_insert_with(|| GroupLog::new(g, conf.clone()));
            log.conf = conf.clone();
            log.hard = HardState { term: *term, vote: *vote, commit: *commit };
            log.compact(*snapshot_index, *snapshot_term);
            log.last_truncate = *last_truncate;
        }
        WalPayload::Entries { first_index, entries } => {
            let log = groups.get_mut(&g).ok_or("nonexistent group")?;
            let new: Vec<LogEntry> = entries
                .iter()
                .enumerate()
                .map(|(i, e)| LogEntry {
                    index: first_index + i as u64,
                    term: e.term,
                    kind: e.kind,
                    data: e.data.clone(),
                })
                .collect();
            // Gap covered by the group's latest durable snapshot: its entries were in segments
            // released after the last base (the checkpoint holds an older snapshot).
            let covering = snaps
                .get(&g)
                .filter(|&&(index, _)| (index + 1 >= *first_index) && (index > log.snapshot_index));
            if *first_index > log.last_index() + 1
                && let Some(&(index, term)) = covering
            {
                log.compact(index, term);
            }
            // Entries already covered by a newer snapshot: the extra ones are dropped.
            let skip = new.iter().take_while(|e| e.index <= log.snapshot_index).count();
            log.append(&new[skip..])?;
        }
        WalPayload::HardState { term, vote, commit } => {
            let log = groups.get_mut(&g).ok_or("nonexistent group")?;
            log.hard = HardState { term: *term, vote: *vote, commit: *commit };
        }
        WalPayload::Truncate { from_index } => {
            groups.get_mut(&g).ok_or("nonexistent group")?.truncate(*from_index);
        }
        WalPayload::SnapshotMark { index, term } => {
            groups.get_mut(&g).ok_or("nonexistent group")?.compact(*index, *term);
        }
        WalPayload::Pad | WalPayload::Batch { .. } => {}
    }
    Ok(())
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;
    use crate::format::wal::{EntryKind, WalEntry};

    fn rec(g: u64, payload: WalPayload) -> WalRecord {
        WalRecord { group: GroupId(g), batch_seq: 1, flags: 0, payload }
    }

    fn entries(first_index: u64) -> WalPayload {
        let e = WalEntry { term: 1, kind: EntryKind::Normal, data: Bytes::from_vec(vec![1]) };
        WalPayload::Entries { first_index, entries: vec![e] }
    }

    #[test]
    fn records_of_a_group_deleted_later_whose_base_was_released_are_skipped() {
        // Group 7 was created and checkpointed in segments released since its deletion; the WAL
        // still has some of its records before the `GroupDelete`, among those of group 8.
        let records = vec![
            rec(8, WalPayload::GroupCreate { conf: Bytes::new() }),
            rec(7, entries(5)),
            rec(7, WalPayload::HardState { term: 1, vote: 0, commit: 5 }),
            rec(7, WalPayload::Truncate { from_index: 5 }),
            rec(7, WalPayload::SnapshotMark { index: 5, term: 1 }),
            rec(8, entries(1)),
            rec(7, WalPayload::GroupDelete),
        ];
        let groups = replay_all(&records, &BTreeMap::new()).expect("a valid WAL");
        assert_eq!(groups.keys().copied().collect::<Vec<_>>(), [GroupId(8)]);
        assert_eq!(groups[&GroupId(8)].last_index(), 1);
        // Records of a group that never existed and is not deleted later are still corruption.
        let orphan = vec![rec(9, entries(1))];
        assert!(replay_all(&orphan, &BTreeMap::new()).is_err());
        // Nor does a deletion earlier in the WAL excuse records after it.
        let after = vec![rec(9, WalPayload::GroupDelete), rec(9, entries(1))];
        assert!(replay_all(&after, &BTreeMap::new()).is_err());
    }
}
