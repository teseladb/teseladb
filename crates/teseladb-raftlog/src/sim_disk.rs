//! Simulated disk with the durability model of a POSIX file system.
//!
//! - Written contents are visible right away but only durable after a `Sync` of the file.
//! - Create, rename and delete are visible right away but only durable after a `SyncDir` of the
//!   directory holding the entry. A file system that journals metadata may still keep a new name
//!   whose `SyncDir` never came: on a crash each such creation survives with
//!   [`SimDiskConfig::unsynced_create_probability`], with only its durable contents (the zeros of
//!   its reservation unless a `Sync` of the file came).
//! - [`SimDisk::crash`] drops what is not durable. Of each file's last unsynced write an arbitrary
//!   subset of its 512 B sectors may persist (torn write); the sectors that do not persist keep
//!   their previous durable contents.
//! - `ENOSPC` (maximum capacity) and errors of a specific operation can be injected.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};

use teseladb_types::{Bytes, Nanos, Rng};

use crate::disk::{Disk, FileId, IoDone, IoErrno, IoOp};

/// Sector size: the model's unit of atomic write.
pub const SECTOR: usize = 512;

/// Parameters of the simulated disk.
#[derive(Clone, Debug)]
pub struct SimDiskConfig {
    /// Total capacity in bytes (sum of the lengths of all visible files).
    pub capacity: u64,
    /// Probability that a file's last unsynced write partially persists on a
    /// crash.
    pub torn_write_probability: f64,
    /// Probability that a created name without its `SyncDir` survives a crash.
    pub unsynced_create_probability: f64,
    /// Base latency of a write.
    pub write_latency: Nanos,
    /// Base latency of a sync.
    pub sync_latency: Nanos,
    /// Base latency of a read.
    pub read_latency: Nanos,
    /// Maximum variation added to each latency (uniform).
    pub jitter: Nanos,
}

impl Default for SimDiskConfig {
    fn default() -> Self {
        SimDiskConfig {
            capacity: u64::MAX,
            torn_write_probability: 0.5,
            unsynced_create_probability: 0.5,
            write_latency: Nanos::from_micros(10),
            sync_latency: Nanos::from_micros(100),
            read_latency: Nanos::from_micros(50),
            jitter: Nanos::from_micros(20),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Inode {
    /// Visible contents.
    data: Vec<u8>,
    /// Durable contents.
    durable: Vec<u8>,
    /// Unsynced writes, in order: (offset, length).
    unsynced: Vec<(u64, usize)>,
}

impl Inode {
    /// Goes back to the durable contents undoing only the pending ranges (visible and durable
    /// only differ there and in the length).
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the simulated files live in memory, so their offsets and lengths fit usize"
    )]
    fn revert(&mut self) {
        let durable_len = self.durable.len();
        for &(off, len) in &self.unsynced {
            let lo = (off as usize).min(durable_len).min(self.data.len());
            let hi = (off as usize + len).min(durable_len).min(self.data.len());
            self.data[lo..hi].copy_from_slice(&self.durable[lo..hi]);
        }
        // What grew beyond the durable part is cut; what shrank comes back with zeros the previous
        // loop no longer reaches, so the rest of the durable contents is copied.
        let visible = self.data.len();
        self.data.resize(durable_len, 0);
        if visible < durable_len {
            self.data[visible..].copy_from_slice(&self.durable[visible..]);
        }
        self.unsynced.clear();
    }

    /// Makes the pending part durable copying only its ranges.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the simulated files live in memory, so their offsets and lengths fit usize"
    )]
    fn sync(&mut self) {
        self.durable.resize(self.data.len(), 0);
        for &(off, len) in &self.unsynced {
            let lo = (off as usize).min(self.data.len());
            let hi = (off as usize + len).min(self.data.len());
            self.durable[lo..hi].copy_from_slice(&self.data[lo..hi]);
        }
        self.unsynced.clear();
    }
}

/// Simulated disk (see the module documentation).
#[derive(Debug)]
pub struct SimDisk {
    cfg: SimDiskConfig,
    inodes: HashMap<u64, Inode>,
    next_inode: u64,
    /// Visible directory entries.
    names: BTreeMap<PathBuf, u64>,
    /// Durable directory entries.
    durable_names: BTreeMap<PathBuf, u64>,
    /// Names that changed (created, renamed or deleted) since the last `SyncDir` of their
    /// directory: the only thing a `SyncDir` has to make durable.
    dirty_names: std::collections::BTreeSet<PathBuf>,
    open: HashMap<FileId, u64>,
    injected: VecDeque<(&'static str, IoErrno)>,
    stats: SimDiskStats,
}

/// Counters of the simulated disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimDiskStats {
    /// Operations executed.
    pub ops: u64,
    /// `Sync`s executed.
    pub syncs: u64,
    /// Bytes written.
    pub bytes_written: u64,
    /// Simulated crashes.
    pub crashes: u64,
    /// Sectors of a last unsynced write that a crash kept (torn writes).
    pub torn_sectors: u64,
    /// Operations refused because the disk was full.
    pub no_space: u64,
    /// Operations that failed with an injected error.
    pub injected_errors: u64,
}

impl Default for SimDisk {
    fn default() -> Self {
        SimDisk::new(SimDiskConfig::default())
    }
}

fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or_else(|| Path::new(""))
}

/// Entries of a directory from a path-ordered map: the paths under `dir` are contiguous in
/// component order, so a range is enough (without walking the rest of the disk).
fn entries_in<'a, V>(
    map: &'a BTreeMap<PathBuf, V>,
    dir: &'a Path,
) -> impl Iterator<Item = (&'a PathBuf, &'a V)> + 'a {
    map.range::<Path, _>((std::ops::Bound::Included(dir), std::ops::Bound::Unbounded))
        .take_while(move |(p, _)| p.starts_with(dir))
        .filter(move |(p, _)| parent(p) == dir)
}

impl SimDisk {
    /// Empty disk.
    #[must_use]
    pub fn new(cfg: SimDiskConfig) -> SimDisk {
        SimDisk {
            cfg,
            inodes: HashMap::new(),
            next_inode: 1,
            names: BTreeMap::new(),
            durable_names: BTreeMap::new(),
            dirty_names: std::collections::BTreeSet::new(),
            open: HashMap::new(),
            injected: VecDeque::new(),
            stats: SimDiskStats::default(),
        }
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> SimDiskStats {
        self.stats
    }

    /// Changes the capacity (to simulate filling up or freeing space).
    pub fn set_capacity(&mut self, capacity: u64) {
        self.cfg.capacity = capacity;
    }

    /// Makes the next operation named `op` (see [`IoOp::name`]) fail with `errno`.
    pub fn inject_error(&mut self, op: &'static str, errno: IoErrno) {
        self.injected.push_back((op, errno));
    }

    /// Bytes used (visible lengths).
    #[must_use]
    pub fn used(&self) -> u64 {
        self.names.values().map(|i| self.inodes[i].data.len() as u64).sum()
    }

    /// Simulated latency of `op`.
    pub fn latency(&self, op: &IoOp, rng: &mut Rng) -> Nanos {
        let base = match op {
            IoOp::Sync { .. } | IoOp::SyncDir { .. } => self.cfg.sync_latency,
            IoOp::ReadAt { .. } => self.cfg.read_latency,
            _ => self.cfg.write_latency,
        };
        let jitter = if self.cfg.jitter.0 == 0 { 0 } else { rng.range(0, self.cfg.jitter.0 + 1) };
        base + Nanos(jitter)
    }

    /// Damages byte `offset` of the durable file `path` (bit rot): its visible and durable contents
    /// both change, so a crash does not repair it. `false` if `path` does not exist durably or is
    /// shorter (nothing is changed).
    ///
    /// # Panics
    ///
    /// When the simulator's own bookkeeping is inconsistent, which would be a defect of the simulator.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the simulated files live in memory, so their offsets and lengths fit usize"
    )]
    pub fn corrupt(&mut self, path: &Path, offset: u64) -> bool {
        let Some(&ino) = self.durable_names.get(path) else {
            return false;
        };
        let inode = self.inodes.get_mut(&ino).expect("a durable name has its inode");
        let at = offset as usize;
        if at >= inode.durable.len() {
            return false;
        }
        inode.durable[at] ^= 0xff;
        if let Some(b) = inode.data.get_mut(at) {
            *b ^= 0xff;
        }
        true
    }

    /// Tells whether `path` exists durably.
    #[must_use]
    pub fn is_durable(&self, path: &Path) -> bool {
        self.durable_names.contains_key(path)
    }

    /// Crash: everything not durable is lost except, with some probability, part of each file's
    /// last unsynced write. Open files are closed.
    ///
    /// # Panics
    ///
    /// When the simulator's own bookkeeping is inconsistent, which would be a defect of the simulator.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the simulated files live in memory, so their offsets and lengths fit usize"
    )]
    pub fn crash(&mut self, rng: &mut Rng) {
        self.stats.crashes += 1;
        self.open.clear();
        self.injected.clear();
        let mut names = self.durable_names.clone();
        for path in &self.dirty_names {
            if let Some(&ino) = self.names.get(path)
                && !names.contains_key(path)
                && rng.chance(self.cfg.unsynced_create_probability)
            {
                names.insert(path.clone(), ino);
            }
        }
        self.durable_names.clone_from(&names);
        self.names = names;
        self.dirty_names.clear();
        let live: std::collections::HashSet<u64> = self.names.values().copied().collect();
        self.inodes.retain(|ino, _| live.contains(ino));
        let mut inos: Vec<u64> = self.inodes.keys().copied().collect();
        inos.sort_unstable();
        for ino in inos {
            let inode = self.inodes.get_mut(&ino).expect("live inode");
            // Without pending writes, the visible contents are already the durable ones.
            let Some(&(off, len)) = inode.unsynced.last() else {
                continue;
            };
            let off = off as usize;
            let lo_all = off.min(inode.data.len());
            let hi_all = (off + len).min(inode.data.len());
            let last_write = inode.data[lo_all..hi_all].to_vec();
            inode.revert();
            if rng.chance(self.cfg.torn_write_probability) {
                let first = off / SECTOR;
                let last = (off + len).div_ceil(SECTOR);
                for s in first..last {
                    if !rng.chance(0.5) {
                        continue;
                    }
                    let lo = (s * SECTOR).max(off);
                    let hi = ((s + 1) * SECTOR).min(off + len).min(hi_all);
                    if lo >= hi {
                        continue;
                    }
                    if inode.durable.len() < hi {
                        inode.durable.resize(hi, 0);
                    }
                    inode.durable[lo..hi].copy_from_slice(&last_write[lo - lo_all..hi - lo_all]);
                    self.stats.torn_sectors += 1;
                }
                inode.data.clone_from(&inode.durable);
            }
        }
    }

    fn take_injected(&mut self, op: &IoOp) -> Option<IoErrno> {
        let pos = self.injected.iter().position(|(name, _)| *name == op.name())?;
        self.injected.remove(pos).map(|(_, e)| e)
    }

    fn inode_of(&mut self, file: FileId) -> Result<&mut Inode, IoErrno> {
        let ino = *self.open.get(&file).ok_or(IoErrno::BadFile)?;
        self.inodes.get_mut(&ino).ok_or(IoErrno::BadFile)
    }

    /// Frees an inode no name (visible or durable) or descriptor points to any more.
    fn forget_if_unused(&mut self, ino: u64) {
        let used = self.names.values().any(|&i| i == ino)
            || self.durable_names.values().any(|&i| i == ino)
            || self.open.values().any(|&i| i == ino);
        if !used {
            self.inodes.remove(&ino);
        }
    }

    fn grow_ok(&self, extra: u64) -> bool {
        // Without a limit there is no need to add up the size of every file.
        self.cfg.capacity == u64::MAX || self.used().saturating_add(extra) <= self.cfg.capacity
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::too_many_lines,
        reason = "the simulated files live in memory, so their offsets and lengths fit usize; one arm per operation of the disk"
    )]
    fn run(&mut self, op: &IoOp) -> Result<Option<Bytes>, IoErrno> {
        match op {
            IoOp::Create { file, path, prealloc } => {
                if self.names.contains_key(path) {
                    return Err(IoErrno::AlreadyExists);
                }
                if !self.grow_ok(*prealloc) {
                    return Err(IoErrno::NoSpace);
                }
                let ino = self.next_inode;
                self.next_inode += 1;
                let data = vec![0u8; *prealloc as usize];
                // The reservation is considered durable together with the directory entry.
                self.inodes
                    .insert(ino, Inode { durable: data.clone(), data, unsynced: Vec::new() });
                self.names.insert(path.clone(), ino);
                self.dirty_names.insert(path.clone());
                self.open.insert(*file, ino);
                Ok(None)
            }
            IoOp::Open { file, path } => {
                let ino = *self.names.get(path).ok_or(IoErrno::NotFound)?;
                self.open.insert(*file, ino);
                Ok(None)
            }
            IoOp::WriteAt { file, offset, data, .. } => {
                let end = *offset as usize + data.len();
                let cur = self.inode_of(*file)?.data.len();
                if end > cur && !self.grow_ok((end - cur) as u64) {
                    return Err(IoErrno::NoSpace);
                }
                let inode = self.inode_of(*file)?;
                if inode.data.len() < end {
                    inode.data.resize(end, 0);
                }
                inode.data[*offset as usize..end].copy_from_slice(data);
                inode.unsynced.push((*offset, data.len()));
                self.stats.bytes_written += data.len() as u64;
                Ok(None)
            }
            IoOp::ReadAt { file, offset, len, .. } => {
                let inode = self.inode_of(*file)?;
                let lo = (*offset as usize).min(inode.data.len());
                let hi = (lo + *len as usize).min(inode.data.len());
                Ok(Some(Bytes::copy_from_slice(&inode.data[lo..hi])))
            }
            IoOp::Sync { file, .. } => {
                self.inode_of(*file)?.sync();
                self.stats.syncs += 1;
                Ok(None)
            }
            IoOp::SyncDir { path } => {
                let changed: Vec<PathBuf> = self
                    .dirty_names
                    .iter()
                    .filter(|p| parent(p) == path.as_path())
                    .cloned()
                    .collect();
                for p in changed {
                    let gone = match self.names.get(&p) {
                        Some(&ino) => self.durable_names.insert(p.clone(), ino),
                        None => self.durable_names.remove(&p),
                    };
                    self.dirty_names.remove(&p);
                    if let Some(ino) = gone {
                        self.forget_if_unused(ino);
                    }
                }
                self.stats.syncs += 1;
                Ok(None)
            }
            IoOp::Truncate { file, len } => {
                let len = *len as usize;
                let cur = self.inode_of(*file)?.data.len();
                if len > cur && !self.grow_ok((len - cur) as u64) {
                    return Err(IoErrno::NoSpace);
                }
                let inode = self.inode_of(*file)?;
                inode.data.resize(len, 0);
                // Modelled as a write of the part that changes (durable after `Sync`).
                inode.unsynced.push((len.min(cur) as u64, len.abs_diff(cur)));
                Ok(None)
            }
            IoOp::Rename { from, to } => {
                let ino = self.names.remove(from).ok_or(IoErrno::NotFound)?;
                self.names.insert(to.clone(), ino);
                self.dirty_names.insert(from.clone());
                self.dirty_names.insert(to.clone());
                Ok(None)
            }
            IoOp::Link { from, to } => {
                let ino = *self.names.get(from).ok_or(IoErrno::NotFound)?;
                if self.names.contains_key(to) {
                    return Err(IoErrno::AlreadyExists);
                }
                self.names.insert(to.clone(), ino);
                self.dirty_names.insert(to.clone());
                Ok(None)
            }
            IoOp::Remove { path } => {
                self.names.remove(path).ok_or(IoErrno::NotFound)?;
                self.dirty_names.insert(path.clone());
                Ok(None)
            }
            // A directory is there while a name is under it: an empty one has nothing to remove.
            IoOp::RemoveDir { path } => {
                if self.names.keys().any(|p| p != path && p.starts_with(path)) {
                    return Err(IoErrno::NotEmpty);
                }
                Ok(None)
            }
            IoOp::Close { file } => {
                let ino = self.open.remove(file).ok_or(IoErrno::BadFile)?;
                self.forget_if_unused(ino);
                Ok(None)
            }
        }
    }
}

impl Disk for SimDisk {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        let v: Vec<PathBuf> = entries_in(&self.names, dir).map(|(p, _)| p.clone()).collect();
        if v.is_empty() && !self.names.keys().any(|p| p.starts_with(dir)) {
            return Err(IoErrno::NotFound);
        }
        Ok(v)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        let ino = self.names.get(path).ok_or(IoErrno::NotFound)?;
        Ok(self.inodes[ino].data.clone())
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        self.stats.ops += 1;
        let result = match self.take_injected(&op) {
            Some(errno) => {
                // As on Linux after a failed `fsync`: dirty pages are considered clean and their
                // contents may be lost (here they are lost right away); a later `Sync` that works
                // does not bring them back.
                if let IoOp::Sync { file, .. } = &op
                    && let Ok(inode) = self.inode_of(*file)
                {
                    inode.revert();
                }
                self.stats.injected_errors += 1;
                Err(errno)
            }
            None => self.run(&op),
        };
        if result == Err(IoErrno::NoSpace) {
            self.stats.no_space += 1;
        }
        match (op, result) {
            (IoOp::ReadAt { token, .. }, Ok(data)) => {
                IoDone::Read { token, data: Ok(data.unwrap_or_default()) }
            }
            (IoOp::ReadAt { token, .. }, Err(e)) => IoDone::Read { token, data: Err(e) },
            (op, Ok(_)) => IoDone::Ok(op.without_data()),
            (op, Err(errno)) => IoDone::Err { op: op.without_data(), errno },
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn ok(d: &mut SimDisk, op: IoOp) {
        let r = d.execute(op);
        assert!(matches!(r, IoDone::Ok(_)), "{r:?}");
    }

    fn create(d: &mut SimDisk, id: u32, path: &str, prealloc: u64) {
        ok(d, IoOp::Create { file: FileId(id), path: path.into(), prealloc });
    }

    fn write(d: &mut SimDisk, id: u32, offset: u64, data: &[u8]) {
        ok(
            d,
            IoOp::WriteAt {
                file: FileId(id),
                offset,
                data: Bytes::copy_from_slice(data),
                batch: 1,
            },
        );
    }

    #[test]
    fn a_link_gives_a_file_a_second_name_durable_after_its_directory_sync() {
        let mut d = SimDisk::new(SimDiskConfig::default());
        let mut rng = Rng::new(3);
        create(&mut d, 1, "a/f", 0);
        write(&mut d, 1, 0, b"data");
        ok(&mut d, IoOp::Sync { file: FileId(1), batch: 0 });
        ok(&mut d, IoOp::SyncDir { path: "a".into() });
        ok(&mut d, IoOp::Close { file: FileId(1) });
        ok(&mut d, IoOp::Link { from: "a/f".into(), to: "b/g".into() });
        assert_eq!(
            d.execute(IoOp::Link { from: "a/f".into(), to: "b/g".into() }),
            IoDone::Err {
                op: IoOp::Link { from: "a/f".into(), to: "b/g".into() },
                errno: IoErrno::AlreadyExists
            }
        );
        assert_eq!(
            d.execute(IoOp::Link { from: "a/none".into(), to: "b/h".into() }),
            IoDone::Err {
                op: IoOp::Link { from: "a/none".into(), to: "b/h".into() },
                errno: IoErrno::NotFound
            }
        );
        // Both names read the same file, and removing one keeps the other.
        ok(&mut d, IoOp::Remove { path: "a/f".into() });
        assert_eq!(d.read_all(Path::new("b/g")).expect("the link"), b"data");
        ok(&mut d, IoOp::SyncDir { path: "b".into() });
        d.crash(&mut rng);
        assert_eq!(d.read_all(Path::new("b/g")).expect("durable link"), b"data");
    }

    #[test]
    fn unsynced_data_and_names_are_lost_on_crash() {
        let mut d =
            SimDisk::new(SimDiskConfig { torn_write_probability: 0.0, ..Default::default() });
        let mut rng = Rng::new(1);
        create(&mut d, 1, "dir/a", 0);
        write(&mut d, 1, 0, b"ping");
        assert_eq!(d.read_all(Path::new("dir/a")).expect("read_all succeeds"), b"ping");
        d.crash(&mut rng);
        assert_eq!(d.read_all(Path::new("dir/a")), Err(IoErrno::NotFound), "without SyncDir");

        create(&mut d, 1, "dir/a", 0);
        ok(&mut d, IoOp::SyncDir { path: "dir".into() });
        write(&mut d, 1, 0, b"ping");
        d.crash(&mut rng);
        assert_eq!(d.read_all(Path::new("dir/a")).expect("read_all succeeds"), b"", "without Sync");

        ok(&mut d, IoOp::Open { file: FileId(2), path: "dir/a".into() });
        write(&mut d, 2, 0, b"ping");
        ok(&mut d, IoOp::Sync { file: FileId(2), batch: 1 });
        write(&mut d, 2, 4, b" world");
        d.crash(&mut rng);
        assert_eq!(d.read_all(Path::new("dir/a")).expect("read_all succeeds"), b"ping");
        assert!(d.is_durable(Path::new("dir/a")));
        assert_eq!(d.stats().crashes, 3);
    }

    #[test]
    fn torn_write_keeps_a_subset_of_sectors() {
        let mut seen_partial = false;
        for seed in 0..50 {
            let mut d =
                SimDisk::new(SimDiskConfig { torn_write_probability: 1.0, ..Default::default() });
            let mut rng = Rng::new(seed);
            create(&mut d, 1, "f", 8 * SECTOR as u64);
            ok(&mut d, IoOp::SyncDir { path: "".into() });
            write(&mut d, 1, 0, &[0xab; 8 * SECTOR]);
            d.crash(&mut rng);
            let data = d.read_all(Path::new("f")).expect("read_all succeeds");
            assert_eq!(data.len(), 8 * SECTOR);
            let written: Vec<bool> =
                data.chunks(SECTOR).map(|s| s.iter().all(|&b| b == 0xab)).collect();
            for s in data.chunks(SECTOR) {
                assert!(s.iter().all(|&b| b == 0xab) || s.iter().all(|&b| b == 0), "mixed sector");
            }
            if written.iter().any(|&w| w) && written.iter().any(|&w| !w) {
                seen_partial = true;
            }
        }
        assert!(seen_partial);
    }

    #[test]
    fn torn_write_can_extend_a_file_and_skips_empty_sectors() {
        let mut d =
            SimDisk::new(SimDiskConfig { torn_write_probability: 1.0, ..Default::default() });
        create(&mut d, 1, "f", 0);
        ok(&mut d, IoOp::SyncDir { path: "".into() });
        write(&mut d, 1, 0, &[1; 3 * SECTOR]);
        ok(&mut d, IoOp::Truncate { file: FileId(1), len: 10 });
        let mut rng = Rng::new(4);
        d.crash(&mut rng);
        assert!(d.read_all(Path::new("f")).expect("read_all succeeds").len() <= 3 * SECTOR);
    }

    #[test]
    fn torn_append_beyond_durable_length_extends_the_file() {
        let mut extended = false;
        for seed in 0..20 {
            let mut d =
                SimDisk::new(SimDiskConfig { torn_write_probability: 1.0, ..Default::default() });
            create(&mut d, 1, "f", 0);
            ok(&mut d, IoOp::SyncDir { path: "".into() });
            write(&mut d, 1, 0, &[7; 4 * SECTOR]);
            d.crash(&mut Rng::new(seed));
            let len = d.read_all(Path::new("f")).expect("read_all succeeds").len();
            assert!(len <= 4 * SECTOR);
            extended |= len > 0;
        }
        assert!(extended);
    }

    #[test]
    fn rename_remove_and_sync_dir() {
        let mut d = SimDisk::default();
        let mut rng = Rng::new(2);
        create(&mut d, 1, "s/x.tmp", 0);
        write(&mut d, 1, 0, b"snap");
        ok(&mut d, IoOp::Sync { file: FileId(1), batch: 0 });
        ok(&mut d, IoOp::Close { file: FileId(1) });
        ok(&mut d, IoOp::Rename { from: "s/x.tmp".into(), to: "s/x".into() });
        ok(&mut d, IoOp::SyncDir { path: "s".into() });
        create(&mut d, 2, "s/y", 0);
        ok(&mut d, IoOp::Remove { path: "s/y".into() });
        assert_eq!(d.list(Path::new("s")).expect("list succeeds"), vec![PathBuf::from("s/x")]);
        d.crash(&mut rng);
        assert_eq!(d.read_all(Path::new("s/x")).expect("read_all succeeds"), b"snap");
        assert_eq!(d.list(Path::new("missing")), Err(IoErrno::NotFound));
        assert_eq!(d.list(Path::new("")).expect("list succeeds"), Vec::<PathBuf>::new());
    }

    #[test]
    fn errors_and_reads() {
        let mut d = SimDisk::new(SimDiskConfig { capacity: 100, ..Default::default() });
        assert_eq!(
            d.execute(IoOp::Create { file: FileId(1), path: "big".into(), prealloc: 200 }),
            IoDone::Err {
                op: IoOp::Create { file: FileId(1), path: "big".into(), prealloc: 200 },
                errno: IoErrno::NoSpace
            }
        );
        create(&mut d, 1, "f", 50);
        assert!(matches!(
            d.execute(IoOp::Create { file: FileId(2), path: "f".into(), prealloc: 0 }),
            IoDone::Err { errno: IoErrno::AlreadyExists, .. }
        ));
        assert!(matches!(
            d.execute(IoOp::WriteAt {
                file: FileId(1),
                offset: 90,
                data: Bytes::from("0123456789abc"),
                batch: 0
            }),
            IoDone::Err { errno: IoErrno::NoSpace, .. }
        ));
        assert!(matches!(
            d.execute(IoOp::Truncate { file: FileId(1), len: 1000 }),
            IoDone::Err { errno: IoErrno::NoSpace, .. }
        ));
        // Growing within the capacity is allowed.
        assert!(matches!(d.execute(IoOp::Truncate { file: FileId(1), len: 60 }), IoDone::Ok(_)));
        assert!(matches!(d.execute(IoOp::Truncate { file: FileId(1), len: 50 }), IoDone::Ok(_)));
        d.set_capacity(u64::MAX);
        write(&mut d, 1, 0, b"abcdef");
        assert_eq!(
            d.execute(IoOp::ReadAt { file: FileId(1), offset: 2, len: 3, token: 9 }),
            IoDone::Read { token: 9, data: Ok(Bytes::from("cde")) }
        );
        assert_eq!(
            d.execute(IoOp::ReadAt { file: FileId(1), offset: 1000, len: 3, token: 9 }),
            IoDone::Read { token: 9, data: Ok(Bytes::new()) }
        );
        assert_eq!(
            d.execute(IoOp::ReadAt { file: FileId(7), offset: 0, len: 3, token: 1 }),
            IoDone::Read { token: 1, data: Err(IoErrno::BadFile) }
        );
        d.inject_error("sync", IoErrno::Io);
        assert!(matches!(
            d.execute(IoOp::Sync { file: FileId(1), batch: 3 }),
            IoDone::Err { errno: IoErrno::Io, .. }
        ));
        ok(&mut d, IoOp::Sync { file: FileId(1), batch: 3 });
        for op in [
            IoOp::Open { file: FileId(3), path: "no".into() },
            IoOp::Rename { from: "no".into(), to: "x".into() },
            IoOp::Remove { path: "no".into() },
        ] {
            assert!(matches!(d.execute(op), IoDone::Err { errno: IoErrno::NotFound, .. }));
        }
        assert!(matches!(
            d.execute(IoOp::Close { file: FileId(42) }),
            IoDone::Err { errno: IoErrno::BadFile, .. }
        ));
        assert_eq!(d.read_all(Path::new("no")), Err(IoErrno::NotFound));
        assert!(d.stats().ops > 10);
        assert!(d.stats().bytes_written >= 6);
        assert_eq!(d.used(), 50);
    }

    #[test]
    fn failed_sync_loses_the_unsynced_writes() {
        let mut d = SimDisk::default();
        create(&mut d, 1, "f", 0);
        ok(&mut d, IoOp::SyncDir { path: "".into() });
        write(&mut d, 1, 0, b"older");
        ok(&mut d, IoOp::Sync { file: FileId(1), batch: 0 });
        write(&mut d, 1, 0, b"newer");
        d.inject_error("sync", IoErrno::Io);
        assert!(matches!(d.execute(IoOp::Sync { file: FileId(1), batch: 0 }), IoDone::Err { .. }));
        // The retry "works" but what was written is already lost.
        ok(&mut d, IoOp::Sync { file: FileId(1), batch: 0 });
        d.crash(&mut Rng::new(1));
        assert_eq!(d.read_all(Path::new("f")).expect("read_all succeeds"), b"older");
        // An error injected in the `Sync` of a file that is not open breaks nothing.
        d.inject_error("sync", IoErrno::Io);
        assert!(matches!(d.execute(IoOp::Sync { file: FileId(9), batch: 0 }), IoDone::Err { .. }));
    }

    #[test]
    fn latency_is_bounded() {
        let d = SimDisk::default();
        let mut rng = Rng::new(3);
        for op in [
            IoOp::Sync { file: FileId(1), batch: 0 },
            IoOp::ReadAt { file: FileId(1), offset: 0, len: 1, token: 0 },
            IoOp::Close { file: FileId(1) },
        ] {
            let l = d.latency(&op, &mut rng);
            assert!(l >= Nanos::from_micros(10) && l <= Nanos::from_micros(120));
        }
        let d = SimDisk::new(SimDiskConfig { jitter: Nanos::ZERO, ..Default::default() });
        assert_eq!(d.latency(&IoOp::Close { file: FileId(1) }, &mut rng), Nanos::from_micros(10));
    }
}
