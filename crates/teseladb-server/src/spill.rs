//! The node's temporary store for aggregation stages that pass their memory (010 FR-010, T024):
//! each run is a file `spill-<n>.bson` under `<data>/tmp`, written, read and removed through the
//! engine's disk operations ([`Disk`]); what a previous process left there goes at startup.

use std::path::PathBuf;
use std::sync::Mutex;

use teseladb_exec::pipeline::spill::SpillStore;
use teseladb_mql::{MqlError, codes};
use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp};

/// The directory of the runs, inside the data directory.
pub const SPILL_DIR: &str = "tmp";

/// A store over disk `D`.
#[derive(Debug)]
pub struct DiskSpill<D: Disk> {
    inner: Mutex<(D, u64)>,
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

impl<D: Disk> DiskSpill<D> {
    /// The store over `disk`, rooted at its directory of runs, without the runs a previous
    /// process left.
    pub fn open(mut disk: D) -> Result<DiskSpill<D>, MqlError> {
        let left = disk.list(&PathBuf::new()).map_err(|e| failed("list", e))?;
        for p in left {
            done(disk.execute(IoOp::Remove { path: p }), "remove")?;
        }
        Ok(DiskSpill { inner: Mutex::new((disk, 0)) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (D, u64)> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl<D: Disk + std::fmt::Debug> SpillStore for DiskSpill<D> {
    fn write(&self, bytes: &[u8]) -> Result<u64, MqlError> {
        let mut g = self.lock();
        let run = g.1;
        g.1 += 1;
        let f = file(run);
        done(g.0.execute(IoOp::Create { file: f, path: path(run), prealloc: 0 }), "create")?;
        let data = teseladb_types::Bytes::from(bytes.to_vec());
        done(g.0.execute(IoOp::WriteAt { file: f, offset: 0, data, batch: 0 }), "write")?;
        Ok(run)
    }

    fn read(&self, run: u64, offset: u64, len: usize) -> Result<Vec<u8>, MqlError> {
        let len = u32::try_from(len).unwrap_or(u32::MAX);
        let op = IoOp::ReadAt { file: file(run), offset, len, token: run };
        match self.lock().0.execute(op) {
            IoDone::Read { data: Ok(d), .. } => Ok(d.to_vec()),
            IoDone::Read { data: Err(e), .. } | IoDone::Err { errno: e, .. } => {
                Err(failed("read", e))
            }
            IoDone::Ok(_) => Ok(Vec::new()),
        }
    }

    fn remove(&self, run: u64) {
        let mut g = self.lock();
        // A run that cannot go stays until the next start removes it.
        let _closed = g.0.execute(IoOp::Close { file: file(run) });
        let _removed = g.0.execute(IoOp::Remove { path: path(run) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("teseladb-spill-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn runs_are_files_read_in_pieces_and_removed() {
        let d = dir("runs");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("spill-9.bson"), b"left").unwrap();
        let s = DiskSpill::open(teseladb_io::FileDisk::open(&d).unwrap()).unwrap();
        assert!(!d.join("spill-9.bson").exists());
        let a = s.write(b"hello world").unwrap();
        let b = s.write(b"xyz").unwrap();
        assert_eq!(s.read(a, 6, 100).unwrap(), b"world");
        assert_eq!(s.read(b, 0, 2).unwrap(), b"xy");
        s.remove(a);
        assert!(!d.join("spill-0.bson").exists());
        assert!(s.read(a, 0, 1).is_err());
        assert!(d.join("spill-1.bson").exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_sort_past_its_limit_goes_through_the_files() {
        use teseladb_bson::{Document, RawDoc};
        let d = dir("sort");
        let s = DiskSpill::open(teseladb_io::FileDisk::open(&d).unwrap()).unwrap();
        let spec = Document::new().with("s", Document::new().with("k", -1));
        let bytes = spec.to_bytes().unwrap();
        let raw = RawDoc::from_bytes(&bytes).unwrap();
        let spec = teseladb_exec::pipeline::sort::SortSpec::parse(raw.get("s").unwrap()).unwrap();
        let docs: Vec<Document> =
            (0..3_000).map(|i: i32| Document::new().with("k", (i * 31) % 1000)).collect();
        let expected = spec.sort(docs.clone(), None).unwrap();
        let out = teseladb_exec::pipeline::spill::sort_runs(&spec, docs, None, 4_096, &s).unwrap();
        assert_eq!(out, expected);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 0);
        std::fs::remove_dir_all(&d).unwrap();
    }
}
