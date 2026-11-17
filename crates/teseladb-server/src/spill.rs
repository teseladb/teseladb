//! The node's temporary store for aggregation stages that pass their memory (010 FR-010, T024):
//! each run is a file `spill-<n>.bson` under `<data>/tmp`, written, read and removed through the
//! engine's disk executor of the shard whose command asks (023 FR-010), so a spill never blocks
//! the shard; what a previous process left there goes at startup.
//!
//! Each operation opens the run's file, works on it and closes it: a cursor's runs may be read on
//! another shard than the one that wrote them (its `getMore` on another connection), and no shard
//! keeps a file of a run another shard removes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use teseladb::engine::core::reactor::internal::at_destroy;
use teseladb_exec::pipeline::spill::SpillStore;
use teseladb_io::engine::disk::{DiskPlace, ShardDisk};
use teseladb_mql::{MqlError, codes};
use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp};

/// The directory of the runs, inside the data directory.
pub const SPILL_DIR: &str = "tmp";

/// Gives the disk the store works on where it is called.
pub trait SpillDisk: std::fmt::Debug + Send + Sync {
    /// The disk of the caller, a handle that waits only for this operation.
    ///
    /// # Errors
    ///
    /// When there is no disk where it is called.
    fn disk(&self) -> Result<Box<dyn Disk>, MqlError>;
}

/// The disk executor of the calling shard, rooted at the directory of the runs; made on the
/// shard's first spill.
#[derive(Debug)]
pub struct ShardSpillDisk {
    root: PathBuf,
}

impl ShardSpillDisk {
    /// The executors rooted at `root`.
    #[must_use]
    pub fn new(root: PathBuf) -> ShardSpillDisk {
        ShardSpillDisk { root }
    }
}

thread_local! {
    /// The executors of this shard, by root.
    static SHARD_DISKS: RefCell<HashMap<PathBuf, ShardDisk>> = RefCell::new(HashMap::new());
}

impl SpillDisk for ShardSpillDisk {
    fn disk(&self) -> Result<Box<dyn Disk>, MqlError> {
        if !teseladb_io::engine::wait::in_engine_thread() {
            return Err(failed("reach", IoErrno::Io));
        }
        let kept = SHARD_DISKS.with(|d| d.borrow().get(&self.root).cloned());
        let disk = match kept {
            Some(d) => d,
            None => {
                // Opened without holding the map: the open waits for the engine.
                let d = DiskPlace::Files(self.root.clone())
                    .on_shard()
                    .map_err(|_| failed("open the directory of", IoErrno::Io))?;
                SHARD_DISKS.with(|m| {
                    let mut m = m.borrow_mut();
                    if m.is_empty() {
                        // The executors go while the reactor still runs, not with the thread's
                        // locals once it is gone.
                        at_destroy(|| {
                            drop(SHARD_DISKS.with(|m| std::mem::take(&mut *m.borrow_mut())))
                        });
                    }
                    m.insert(self.root.clone(), d.clone());
                });
                d
            }
        };
        Ok(Box::new(disk))
    }
}

/// A store over the disks `S` gives.
#[derive(Debug)]
pub struct DiskSpill<S: SpillDisk> {
    disks: S,
    next: AtomicU64,
}

fn failed(what: &str, errno: IoErrno) -> MqlError {
    MqlError::new(codes::INTERNAL_ERROR, format!("{what} a spilled run: {errno:?}"))
}

fn path(run: u64) -> PathBuf {
    PathBuf::from(format!("spill-{run}.bson"))
}

fn file(run: u64) -> FileId {
    FileId(u32::try_from(run % u64::from(u32::MAX)).unwrap_or(0))
}

fn done(d: IoDone, what: &str) -> Result<(), MqlError> {
    match d {
        IoDone::Err { errno, .. } => Err(failed(what, errno)),
        IoDone::Ok(_) | IoDone::Read { .. } => Ok(()),
    }
}

/// Removes the runs a previous process left on `disk` (the directory of the runs, read before
/// the engine runs).
///
/// # Errors
///
/// When the directory cannot be read or a run removed.
pub fn clear(disk: &mut dyn Disk) -> Result<(), MqlError> {
    // A directory not made yet holds no runs; the first one makes it.
    let left = match disk.list(&PathBuf::new()) {
        Ok(left) => left,
        Err(IoErrno::NotFound) => Vec::new(),
        Err(e) => return Err(failed("list", e)),
    };
    for p in left {
        done(disk.execute(IoOp::Remove { path: p }), "remove")?;
    }
    Ok(())
}

impl<S: SpillDisk> DiskSpill<S> {
    /// The store over the disks of `disks`.
    pub fn new(disks: S) -> DiskSpill<S> {
        DiskSpill { disks, next: AtomicU64::new(0) }
    }
}

impl<S: SpillDisk> SpillStore for DiskSpill<S> {
    fn write(&self, bytes: &[u8]) -> Result<u64, MqlError> {
        let mut d = self.disks.disk()?;
        let run = self.next.fetch_add(1, Ordering::Relaxed);
        let f = file(run);
        done(d.execute(IoOp::Create { file: f, path: path(run), prealloc: 0 }), "create")?;
        let data = teseladb_types::Bytes::from(bytes.to_vec());
        let op = IoOp::WriteAt { file: f, offset: 0, data, batch: 0 };
        let written = done(d.execute(op), "write");
        let _closed = d.execute(IoOp::Close { file: f });
        written.map(|()| run)
    }

    fn read(&self, run: u64, offset: u64, len: usize) -> Result<Vec<u8>, MqlError> {
        let mut d = self.disks.disk()?;
        let f = file(run);
        done(d.execute(IoOp::Open { file: f, path: path(run) }), "open")?;
        let len = u32::try_from(len).unwrap_or(u32::MAX);
        let read = match d.execute(IoOp::ReadAt { file: f, offset, len, token: run }) {
            IoDone::Read { data: Ok(b), .. } => Ok(b.to_vec()),
            IoDone::Read { data: Err(e), .. } | IoDone::Err { errno: e, .. } => {
                Err(failed("read", e))
            }
            IoDone::Ok(_) => Ok(Vec::new()),
        };
        let _closed = d.execute(IoOp::Close { file: f });
        read
    }

    fn remove(&self, run: u64) {
        // A run that cannot go stays until the next start removes it.
        if let Ok(mut d) = self.disks.disk() {
            let _removed = d.execute(IoOp::Remove { path: path(run) });
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use std::path::Path;
    use std::sync::{Arc, Mutex, PoisonError};

    use super::*;
    use teseladb_raftlog::MemDisk;

    /// One disk in memory behind every handle, for the tests (no engine runs here).
    #[derive(Debug, Clone)]
    struct Shared(Arc<Mutex<MemDisk>>);

    impl Default for Shared {
        fn default() -> Shared {
            Shared(Arc::new(Mutex::new(MemDisk::new())))
        }
    }

    impl Disk for Shared {
        fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).list(dir)
        }

        fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).read_all(path)
        }

        fn execute(&mut self, op: IoOp) -> IoDone {
            self.0.lock().unwrap_or_else(PoisonError::into_inner).execute(op)
        }
    }

    impl SpillDisk for Shared {
        fn disk(&self) -> Result<Box<dyn Disk>, MqlError> {
            Ok(Box::new(self.clone()))
        }
    }

    /// The names of the runs on the store's disk.
    fn names(d: &Shared) -> Vec<PathBuf> {
        d.clone().list(&PathBuf::new()).unwrap_or_default()
    }

    /// A disk in memory with a run a previous process left.
    fn left_behind() -> Shared {
        let mut d = Shared::default();
        let left = PathBuf::from("spill-9.bson");
        let ops = [
            IoOp::Create { file: FileId(1), path: left.clone(), prealloc: 0 },
            IoOp::WriteAt { file: FileId(1), offset: 0, data: b"left".as_slice().into(), batch: 0 },
            IoOp::Close { file: FileId(1) },
        ];
        for op in ops {
            assert!(matches!(d.execute(op), IoDone::Ok(_)));
        }
        d
    }

    #[test]
    fn runs_are_files_read_in_pieces_and_removed() {
        let d = left_behind();
        clear(&mut d.clone()).unwrap();
        assert!(names(&d).is_empty(), "what a previous process left goes");
        let s = DiskSpill::new(d.clone());
        let a = s.write(b"hello world").unwrap();
        let b = s.write(b"xyz").unwrap();
        assert_eq!(s.read(a, 6, 100).unwrap(), b"world");
        assert_eq!(s.read(b, 0, 2).unwrap(), b"xy");
        s.remove(a);
        assert_eq!(names(&d), [PathBuf::from("spill-1.bson")]);
        assert!(s.read(a, 0, 1).is_err());
    }

    #[test]
    fn a_sort_past_its_limit_goes_through_the_files() {
        use teseladb_bson::{Document, RawDoc};
        let d = Shared::default();
        let s = DiskSpill::new(d.clone());
        let spec = Document::new().with("s", Document::new().with("k", -1));
        let bytes = spec.to_bytes().unwrap();
        let raw = RawDoc::from_bytes(&bytes).unwrap();
        let spec = teseladb_exec::pipeline::sort::SortSpec::parse(raw.get("s").unwrap()).unwrap();
        let docs: Vec<Document> =
            (0..3_000).map(|i: i32| Document::new().with("k", (i * 31) % 1000)).collect();
        let expected = spec.sort(docs.clone(), None).unwrap();
        let out = teseladb_exec::pipeline::spill::sort_runs(&spec, docs, None, 4_096, &s).unwrap();
        assert_eq!(out, expected);
        assert!(names(&d).is_empty(), "every run is removed");
    }

    #[test]
    fn outside_an_engine_thread_the_shards_disk_is_not_there() {
        let s = DiskSpill::new(ShardSpillDisk::new(PathBuf::from("/nonexistent")));
        assert!(s.write(b"x").is_err());
        assert!(s.read(0, 0, 1).is_err());
        s.remove(0);
    }
}
