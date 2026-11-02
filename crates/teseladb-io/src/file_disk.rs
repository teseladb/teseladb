//! Executor of [`IoOp`] over real files.
//!
//! Operation paths are relative to the disk's root directory (the data directory). Durability is
//! requested from the system with a full `fsync` (`sync_all`, which on macOS uses `F_FULLFSYNC`)
//! on files and directories.
//!
//! With `O_DIRECT` (Linux, 012 FR-003) each file has a second descriptor opened with it: writes
//! and reads of whole 4 KiB blocks go through it from an aligned buffer, and the rest through the
//! buffered one. Both are the same inode, so a `fsync` on either covers both.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp};
use teseladb_sys::aligned::AlignedBuf;
use teseladb_types::Bytes;

use crate::buffers::is_aligned;

/// `O_DIRECT` on Linux (x86_64 and aarch64 differ).
#[cfg(target_arch = "aarch64")]
pub(crate) const O_DIRECT: i32 = 0o200000;
#[cfg(not(target_arch = "aarch64"))]
pub(crate) const O_DIRECT: i32 = 0o40000;

/// An open file: its buffered descriptor and, with `O_DIRECT`, the direct one.
#[derive(Debug)]
struct DiskFile {
    file: File,
    direct: Option<File>,
}

/// Real disk rooted at a directory.
#[derive(Debug)]
pub struct FileDisk {
    root: PathBuf,
    files: HashMap<FileId, DiskFile>,
    direct_io: bool,
    /// Aligned bounce buffer of the direct accesses.
    scratch: AlignedBuf,
}

/// Whether the file system of `dir` takes `O_DIRECT` writes of whole 4 KiB blocks (012 FR-003):
/// it writes, reads back and removes a probe file. Always `false` outside Linux.
pub fn direct_io_supported(dir: &Path) -> io::Result<bool> {
    if !cfg!(target_os = "linux") {
        return Ok(false);
    }
    let probe = dir.join(".direct-io-probe");
    let mut o = OpenOptions::new();
    o.read(true).write(true).create(true).truncate(true);
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(O_DIRECT);
    }
    let r = o.open(&probe).and_then(|f| {
        let mut b = AlignedBuf::new(4096);
        b[..5].copy_from_slice(b"probe");
        f.write_all_at(&b, 0)?;
        f.read_exact_at(&mut b, 0)?;
        Ok(&b[..5] == b"probe")
    });
    let _ = fs::remove_file(&probe);
    match r {
        Ok(same) => Ok(same),
        // EINVAL: the file system does not take O_DIRECT, or not with 4 KiB blocks.
        Err(e) if e.raw_os_error() == Some(22) => Ok(false),
        Err(e) => Err(e),
    }
}

pub(crate) fn errno(e: &io::Error) -> IoErrno {
    if teseladb_sys::is_no_space(e) {
        return IoErrno::NoSpace;
    }
    match e.kind() {
        io::ErrorKind::NotFound => IoErrno::NotFound,
        io::ErrorKind::AlreadyExists => IoErrno::AlreadyExists,
        io::ErrorKind::DirectoryNotEmpty => IoErrno::NotEmpty,
        _ => match e.raw_os_error() {
            Some(5) => IoErrno::Io,
            Some(code) => IoErrno::Other(code),
            None => IoErrno::Io,
        },
    }
}

/// Reads until `buf` is full or the end of the file is reached; retries interrupted reads.
fn read_up_to(f: &impl FileExt, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match f.read_at(&mut buf[got..], offset + got as u64) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(got)
}

impl FileDisk {
    /// Disk rooted at `root` (created if it does not exist).
    pub fn open(root: impl Into<PathBuf>) -> io::Result<FileDisk> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(FileDisk {
            root,
            files: HashMap::new(),
            direct_io: false,
            scratch: AlignedBuf::new(4096),
        })
    }

    /// Enables `O_DIRECT` for whole-block accesses on the files opened from now on (Linux only;
    /// no effect on other systems).
    pub fn set_direct_io(&mut self, on: bool) {
        self.direct_io = on && cfg!(target_os = "linux");
    }

    /// Whether whole-block accesses use `O_DIRECT`.
    pub fn direct_io(&self) -> bool {
        self.direct_io
    }

    /// Root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn full(&self, p: &Path) -> PathBuf {
        self.root.join(p)
    }

    fn options() -> OpenOptions {
        let mut o = OpenOptions::new();
        o.read(true).write(true);
        o
    }

    /// The direct descriptor of a file just opened at `full`, when `O_DIRECT` is on.
    fn direct(&self, full: &Path) -> io::Result<Option<File>> {
        if !self.direct_io {
            return Ok(None);
        }
        use std::os::unix::fs::OpenOptionsExt;
        Self::options().custom_flags(O_DIRECT).open(full).map(Some)
    }

    fn file(&self, id: FileId) -> Result<&DiskFile, IoErrno> {
        self.files.get(&id).ok_or(IoErrno::BadFile)
    }

    fn write(&mut self, id: FileId, offset: u64, data: &[u8]) -> Result<(), IoErrno> {
        let f = self.files.get(&id).ok_or(IoErrno::BadFile)?;
        match &f.direct {
            Some(d) if is_aligned(offset, data.len() as u64) => {
                if self.scratch.len() < data.len() {
                    self.scratch = AlignedBuf::new(data.len());
                }
                self.scratch[..data.len()].copy_from_slice(data);
                d.write_all_at(&self.scratch[..data.len()], offset).map_err(|e| errno(&e))
            }
            _ => f.file.write_all_at(data, offset).map_err(|e| errno(&e)),
        }
    }

    fn read(&mut self, id: FileId, offset: u64, len: usize) -> Result<Vec<u8>, IoErrno> {
        let f = self.files.get(&id).ok_or(IoErrno::BadFile)?;
        let mut buf = vec![0u8; len];
        let got = match &f.direct {
            Some(d) if is_aligned(offset, len as u64) => {
                if self.scratch.len() < len {
                    self.scratch = AlignedBuf::new(len);
                }
                let got = read_up_to(d, &mut self.scratch[..len], offset).map_err(|e| errno(&e))?;
                buf[..got].copy_from_slice(&self.scratch[..got]);
                got
            }
            _ => read_up_to(&f.file, &mut buf, offset).map_err(|e| errno(&e))?,
        };
        buf.truncate(got);
        Ok(buf)
    }

    fn run(&mut self, op: &IoOp) -> Result<Option<Bytes>, IoErrno> {
        let e = |err: io::Error| errno(&err);
        match op {
            IoOp::Create { file, path, prealloc } => {
                let full = self.full(path);
                fs::create_dir_all(full.parent().unwrap_or(&self.root)).map_err(e)?;
                let f = Self::options().create_new(true).open(&full).map_err(e)?;
                let made = match *prealloc {
                    0 => Ok(()),
                    n => teseladb_sys::preallocate(&f, n),
                };
                let direct = made.and_then(|()| self.direct(&full));
                match direct {
                    Ok(direct) => self.files.insert(*file, DiskFile { file: f, direct }),
                    Err(err) => {
                        // Do not leave a half-made file behind if there is no space.
                        let _ = fs::remove_file(&full);
                        return Err(errno(&err));
                    }
                };
                Ok(None)
            }
            IoOp::Open { file, path } => {
                let full = self.full(path);
                let f = Self::options().open(&full).map_err(e)?;
                let direct = self.direct(&full).map_err(e)?;
                self.files.insert(*file, DiskFile { file: f, direct });
                Ok(None)
            }
            IoOp::WriteAt { file, offset, data, .. } => {
                self.write(*file, *offset, data)?;
                Ok(None)
            }
            IoOp::ReadAt { file, offset, len, .. } => {
                Ok(Some(Bytes::from_vec(self.read(*file, *offset, *len as usize)?)))
            }
            IoOp::Sync { file, .. } => {
                self.file(*file)?.file.sync_all().map_err(e)?;
                Ok(None)
            }
            IoOp::SyncDir { path } => {
                File::open(self.full(path)).and_then(|d| d.sync_all()).map_err(e)?;
                Ok(None)
            }
            IoOp::Truncate { file, len } => {
                self.file(*file)?.file.set_len(*len).map_err(e)?;
                Ok(None)
            }
            IoOp::Rename { from, to } => {
                fs::rename(self.full(from), self.full(to)).map_err(e)?;
                Ok(None)
            }
            IoOp::Remove { path } => {
                fs::remove_file(self.full(path)).map_err(e)?;
                Ok(None)
            }
            IoOp::RemoveDir { path } => {
                fs::remove_dir(self.full(path)).map_err(e)?;
                Ok(None)
            }
            IoOp::Close { file } => {
                self.files.remove(file).ok_or(IoErrno::BadFile)?;
                Ok(None)
            }
        }
    }
}

impl Disk for FileDisk {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        let mut v = Vec::new();
        for entry in fs::read_dir(self.full(dir)).map_err(|e| errno(&e))? {
            let entry = entry.map_err(|e| errno(&e))?;
            v.push(dir.join(entry.file_name()));
        }
        v.sort();
        Ok(v)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        fs::read(self.full(path)).map_err(|e| errno(&e))
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        let result = self.run(&op);
        match (op, result) {
            (IoOp::ReadAt { token, .. }, r) => {
                IoDone::Read { token, data: r.map(|d| d.unwrap_or_default()) }
            }
            (op, Ok(_)) => IoDone::Ok(op.without_data()),
            (op, Err(errno)) => IoDone::Err { op: op.without_data(), errno },
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("teseladb-io-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn ok(d: &mut FileDisk, op: IoOp) {
        let r = d.execute(op);
        assert!(matches!(r, IoDone::Ok(_)), "{r:?}");
    }

    #[test]
    fn full_lifecycle() {
        let root = temp_root("life");
        let mut d = FileDisk::open(&root).unwrap();
        d.set_direct_io(false);
        assert_eq!(d.root(), root.as_path());
        ok(
            &mut d,
            IoOp::Create { file: FileId(1), path: "core-00/wal/a.tmp".into(), prealloc: 8192 },
        );
        ok(
            &mut d,
            IoOp::WriteAt { file: FileId(1), offset: 4096, data: Bytes::from("ping"), batch: 1 },
        );
        ok(&mut d, IoOp::Sync { file: FileId(1), batch: 1 });
        assert_eq!(
            d.execute(IoOp::ReadAt { file: FileId(1), offset: 4096, len: 4, token: 5 }),
            IoDone::Read { token: 5, data: Ok(Bytes::from("ping")) }
        );
        // Read beyond the end: what is there is returned.
        assert_eq!(
            d.execute(IoOp::ReadAt { file: FileId(1), offset: 8190, len: 10, token: 6 }),
            IoDone::Read { token: 6, data: Ok(Bytes::from_vec(vec![0, 0])) }
        );
        ok(&mut d, IoOp::Truncate { file: FileId(1), len: 4100 });
        ok(&mut d, IoOp::Close { file: FileId(1) });
        ok(
            &mut d,
            IoOp::Rename { from: "core-00/wal/a.tmp".into(), to: "core-00/wal/a.wal".into() },
        );
        ok(&mut d, IoOp::SyncDir { path: "core-00/wal".into() });
        assert_eq!(
            d.list(Path::new("core-00/wal")).unwrap(),
            vec![PathBuf::from("core-00/wal/a.wal")]
        );
        let content = d.read_all(Path::new("core-00/wal/a.wal")).unwrap();
        assert_eq!(content.len(), 4100);
        assert_eq!(&content[4096..], b"ping");
        ok(&mut d, IoOp::Open { file: FileId(2), path: "core-00/wal/a.wal".into() });
        ok(&mut d, IoOp::Close { file: FileId(2) });
        ok(&mut d, IoOp::Remove { path: "core-00/wal/a.wal".into() });
        assert!(d.list(Path::new("core-00/wal")).unwrap().is_empty());
        // An empty directory goes; one with entries stays.
        let r = d.execute(IoOp::RemoveDir { path: "core-00".into() });
        assert!(matches!(r, IoDone::Err { errno: IoErrno::NotEmpty, .. }), "{r:?}");
        ok(&mut d, IoOp::RemoveDir { path: "core-00/wal".into() });
        assert_eq!(d.list(Path::new("core-00/wal")), Err(IoErrno::NotFound));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn error_mapping() {
        let root = temp_root("errors");
        let mut d = FileDisk::open(&root).unwrap();
        ok(&mut d, IoOp::Create { file: FileId(1), path: "x".into(), prealloc: 0 });
        assert!(matches!(
            d.execute(IoOp::Create { file: FileId(2), path: "x".into(), prealloc: 0 }),
            IoDone::Err { errno: IoErrno::AlreadyExists, .. }
        ));
        for op in [
            IoOp::Open { file: FileId(3), path: "missing".into() },
            IoOp::Rename { from: "missing".into(), to: "y".into() },
            IoOp::Remove { path: "missing".into() },
            IoOp::SyncDir { path: "missing".into() },
        ] {
            assert!(matches!(d.execute(op), IoDone::Err { errno: IoErrno::NotFound, .. }));
        }
        for op in [
            IoOp::WriteAt { file: FileId(9), offset: 0, data: Bytes::new(), batch: 0 },
            IoOp::Sync { file: FileId(9), batch: 0 },
            IoOp::Truncate { file: FileId(9), len: 0 },
            IoOp::Close { file: FileId(9) },
        ] {
            assert!(matches!(d.execute(op), IoDone::Err { errno: IoErrno::BadFile, .. }));
        }
        assert_eq!(
            d.execute(IoOp::ReadAt { file: FileId(9), offset: 0, len: 1, token: 1 }),
            IoDone::Read { token: 1, data: Err(IoErrno::BadFile) }
        );
        assert_eq!(d.list(Path::new("missing")), Err(IoErrno::NotFound));
        assert_eq!(d.read_all(Path::new("missing")), Err(IoErrno::NotFound));
        // Reserving more than fits on the disk: ENOSPC and no file is left behind.
        let huge = u64::MAX / 4;
        let r = d.execute(IoOp::Create { file: FileId(4), path: "huge".into(), prealloc: huge });
        assert!(matches!(r, IoDone::Err { .. }), "{r:?}");
        assert!(!root.join("huge").exists());
        fs::remove_dir_all(&root).unwrap();
    }

    /// Fake file returning a sequence of `read_at` results.
    struct Script(std::cell::RefCell<Vec<io::Result<usize>>>);

    impl FileExt for Script {
        fn read_at(&self, buf: &mut [u8], _offset: u64) -> io::Result<usize> {
            let r = self.0.borrow_mut().remove(0);
            if let Ok(n) = r {
                buf[..n].fill(7);
            }
            r
        }
        fn write_at(&self, _buf: &[u8], _offset: u64) -> io::Result<usize> {
            Err(io::Error::from(io::ErrorKind::Unsupported))
        }
    }

    #[test]
    fn read_up_to_retries_interrupts_and_reports_errors() {
        let s = Script(std::cell::RefCell::new(vec![
            Ok(2),
            Err(io::Error::from(io::ErrorKind::Interrupted)),
            Ok(2),
        ]));
        let mut buf = [0u8; 4];
        assert_eq!(read_up_to(&s, &mut buf, 0).unwrap(), 4);
        assert_eq!(buf, [7; 4]);
        let s = Script(std::cell::RefCell::new(vec![Err(io::Error::from_raw_os_error(5))]));
        assert!(read_up_to(&s, &mut buf, 0).is_err());
        assert!(s.write_at(b"x", 0).is_err());
    }

    #[test]
    fn errno_classification() {
        assert_eq!(errno(&io::Error::from_raw_os_error(28)), IoErrno::NoSpace);
        assert_eq!(errno(&io::Error::from_raw_os_error(5)), IoErrno::Io);
        assert_eq!(errno(&io::Error::from_raw_os_error(13)), IoErrno::Other(13));
        assert_eq!(errno(&io::Error::other("x")), IoErrno::Io);
    }
}
