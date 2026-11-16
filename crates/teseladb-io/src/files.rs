//! Blocking file access for the node's code that runs before its engine or on threads of its own,
//! off the shards: the node's identity and data format, read before the engine starts because they
//! size it ([`BlockingDisk`]); the client workers that read a block outside the cores (005
//! FR-028); the files of the search segments (011 FR-012); the spilled runs of aggregations (010
//! FR-010). The cores' files and the device log go through the engine (`engine::disk`); the
//! callers on threads of their own move onto the shards with their phases of 023.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp};
use teseladb_types::Bytes;

/// Reads until `buf` is full or the end of the file is reached; retries interrupted reads.
///
/// # Errors
///
/// The first read error that is not an interruption.
pub fn read_up_to(f: &impl FileExt, buf: &mut [u8], offset: u64) -> io::Result<usize> {
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

/// Writes `bytes` as the whole content of a new file at `path` and makes it durable.
///
/// # Errors
///
/// The error of creating, writing or syncing the file.
pub fn write_whole(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = File::create(path)?;
    f.write_all(bytes)?;
    f.sync_data()
}

/// Syncs directory `dir`, so that the creations, renames and removals in it are durable.
///
/// # Errors
///
/// The error of opening or syncing the directory.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// The [`IoErrno`] of a system error.
fn errno(e: &io::Error) -> IoErrno {
    match e.raw_os_error() {
        Some(28) => IoErrno::NoSpace,
        Some(2) => IoErrno::NotFound,
        Some(17) => IoErrno::AlreadyExists,
        Some(39) => IoErrno::NotEmpty,
        Some(5) | None => IoErrno::Io,
        Some(code) => IoErrno::Other(code),
    }
}

/// Executes [`IoOp`]s with blocking system calls on the files under a directory, one after
/// another, for a thread that is not a shard.
#[derive(Debug)]
pub struct BlockingDisk {
    root: PathBuf,
    files: HashMap<FileId, File>,
}

impl BlockingDisk {
    /// The disk of the files under `root` (made if it does not exist).
    ///
    /// # Errors
    ///
    /// When the directory cannot be made.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<BlockingDisk> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(BlockingDisk { root, files: HashMap::new() })
    }

    fn file(&self, id: FileId) -> Result<&File, IoErrno> {
        self.files.get(&id).ok_or(IoErrno::BadFile)
    }

    fn make_parent(&self, full: &Path) -> Result<(), IoErrno> {
        fs::create_dir_all(full.parent().unwrap_or(&self.root)).map_err(|e| errno(&e))
    }

    fn run(&mut self, op: &IoOp) -> Result<Option<Bytes>, IoErrno> {
        let rw = || {
            let mut o = OpenOptions::new();
            o.read(true).write(true);
            o
        };
        let e = |e: io::Error| errno(&e);
        match op {
            IoOp::Create { file, path, prealloc } => {
                let full = self.root.join(path);
                self.make_parent(&full)?;
                let f = rw().create_new(true).open(&full).map_err(e)?;
                if let Err(err) = f.set_len(*prealloc) {
                    let _ = fs::remove_file(&full);
                    return Err(errno(&err));
                }
                self.files.insert(*file, f);
            }
            IoOp::Open { file, path } => {
                let f = rw().open(self.root.join(path)).map_err(e)?;
                self.files.insert(*file, f);
            }
            IoOp::WriteAt { file, offset, data, .. } => {
                self.file(*file)?.write_all_at(data, *offset).map_err(e)?;
            }
            IoOp::ReadAt { file, offset, len, .. } => {
                let mut buf = vec![0u8; *len as usize];
                let got = read_up_to(self.file(*file)?, &mut buf, *offset).map_err(e)?;
                buf.truncate(got);
                return Ok(Some(Bytes::from_vec(buf)));
            }
            IoOp::Sync { file, .. } => self.file(*file)?.sync_data().map_err(e)?,
            IoOp::SyncDir { path } => sync_dir(&self.root.join(path)).map_err(e)?,
            IoOp::Truncate { file, len } => self.file(*file)?.set_len(*len).map_err(e)?,
            IoOp::Rename { from, to } => {
                fs::rename(self.root.join(from), self.root.join(to)).map_err(e)?;
            }
            IoOp::Link { from, to } => {
                let to = self.root.join(to);
                self.make_parent(&to)?;
                fs::hard_link(self.root.join(from), to).map_err(e)?;
            }
            IoOp::Remove { path } => fs::remove_file(self.root.join(path)).map_err(e)?,
            IoOp::RemoveDir { path } => fs::remove_dir(self.root.join(path)).map_err(e)?,
            IoOp::Close { file } => {
                self.files.remove(file).ok_or(IoErrno::BadFile)?;
            }
        }
        Ok(None)
    }
}

impl Disk for BlockingDisk {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        let mut v = Vec::new();
        for entry in fs::read_dir(self.root.join(dir)).map_err(|e| errno(&e))? {
            v.push(dir.join(entry.map_err(|e| errno(&e))?.file_name()));
        }
        v.sort();
        Ok(v)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        fs::read(self.root.join(path)).map_err(|e| errno(&e))
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        let result = self.run(&op);
        match (op, result) {
            (IoOp::ReadAt { token, .. }, r) => {
                IoDone::Read { token, data: r.map(Option::unwrap_or_default) }
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
        let s = Script(std::cell::RefCell::new(vec![Ok(1), Ok(0)]));
        assert_eq!(read_up_to(&s, &mut buf, 0).unwrap(), 1, "the end of the file stops it");
        let s = Script(std::cell::RefCell::new(vec![Err(io::Error::from_raw_os_error(5))]));
        assert!(read_up_to(&s, &mut buf, 0).is_err());
        assert!(s.write_at(b"x", 0).is_err());
    }

    #[test]
    fn the_blocking_disk_runs_every_operation_and_names_its_errors() {
        let root = std::env::temp_dir().join(format!("teseladb-blocking-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let mut d = BlockingDisk::open(&root).unwrap();
        let ok = |d: &mut BlockingDisk, op: IoOp| {
            let r = d.execute(op.clone());
            assert_eq!(r, IoDone::Ok(op.without_data()), "{op:?}");
        };
        let (a, b) = (FileId(1), FileId(2));
        ok(&mut d, IoOp::Create { file: a, path: "m/x.tmp".into(), prealloc: 8 });
        ok(&mut d, IoOp::WriteAt { file: a, offset: 2, data: Bytes::from("ab"), batch: 0 });
        ok(&mut d, IoOp::Sync { file: a, batch: 0 });
        let read = d.execute(IoOp::ReadAt { file: a, offset: 1, len: 20, token: 4 });
        assert_eq!(
            read,
            IoDone::Read { token: 4, data: Ok(Bytes::from_vec(b"\0ab\0\0\0\0".to_vec())) }
        );
        ok(&mut d, IoOp::Truncate { file: a, len: 4 });
        ok(&mut d, IoOp::Close { file: a });
        ok(&mut d, IoOp::Rename { from: "m/x.tmp".into(), to: "m/x".into() });
        ok(&mut d, IoOp::Link { from: "m/x".into(), to: "n/y".into() });
        ok(&mut d, IoOp::SyncDir { path: "m".into() });
        ok(&mut d, IoOp::Open { file: b, path: "n/y".into() });
        ok(&mut d, IoOp::Close { file: b });
        assert_eq!(d.read_all(Path::new("m/x")).unwrap(), b"\0\0ab");
        assert_eq!(d.list(Path::new("m")).unwrap(), [PathBuf::from("m/x")]);
        ok(&mut d, IoOp::Remove { path: "n/y".into() });
        ok(&mut d, IoOp::RemoveDir { path: "n".into() });
        let err = |d: &mut BlockingDisk, op: IoOp| match d.execute(op) {
            IoDone::Err { errno, .. } => errno,
            other => panic!("{other:?}"),
        };
        assert_eq!(err(&mut d, IoOp::RemoveDir { path: "m".into() }), IoErrno::NotEmpty);
        assert_eq!(err(&mut d, IoOp::Open { file: a, path: "none".into() }), IoErrno::NotFound);
        let again = IoOp::Create { file: a, path: "m/x".into(), prealloc: 0 };
        assert_eq!(err(&mut d, again), IoErrno::AlreadyExists);
        assert_eq!(err(&mut d, IoOp::Sync { file: FileId(9), batch: 0 }), IoErrno::BadFile);
        assert_eq!(err(&mut d, IoOp::Close { file: FileId(9) }), IoErrno::BadFile);
        let unread = d.execute(IoOp::ReadAt { file: FileId(9), offset: 0, len: 1, token: 5 });
        assert_eq!(unread, IoDone::Read { token: 5, data: Err(IoErrno::BadFile) });
        assert_eq!(d.list(Path::new("none")), Err(IoErrno::NotFound));
        assert_eq!(errno(&io::Error::from_raw_os_error(28)), IoErrno::NoSpace);
        assert_eq!(errno(&io::Error::from_raw_os_error(13)), IoErrno::Other(13));
        assert_eq!(errno(&io::Error::other("x")), IoErrno::Io);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn whole_files_are_written_and_their_directory_synced() {
        let root = std::env::temp_dir().join(format!("teseladb-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for len in [0usize, 100, 4096, 10_000] {
            let p = root.join(format!("f-{len}"));
            let bytes: Vec<u8> = (0..len).map(|i| (i % 253) as u8).collect();
            write_whole(&p, &bytes).unwrap();
            assert_eq!(std::fs::read(&p).unwrap(), bytes, "len={len}");
        }
        sync_dir(&root).unwrap();
        assert!(write_whole(&root.join("missing/x"), b"x").is_err());
        assert!(sync_dir(&root.join("missing")).is_err());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
