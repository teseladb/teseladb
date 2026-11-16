//! The destinations of an [`Outlet`](super::Outlet) and their writers on the home shard.
//!
//! - [`FileDest`]: a file appended to through the home shard's disk executor, on the engine's
//!   files (023 FR-010). One writer owns the file and the offset it writes at, so the
//!   read-modify-write of a direct file's last block never races with another write.
//! - [`StreamDest`]: the standard output or error, or another descriptor. A pipe or a terminal is
//!   opened again with `O_NONBLOCK` (a description of its own: the process's descriptor is left as
//!   it is) and written through the engine as a pollable descriptor, which waits for room without
//!   holding the shard; a stream socket (the system journal) is duplicated and made non-blocking
//!   the same way. A regular file takes the bytes into the page cache without waiting for a reader;
//!   anything else (`/dev/null`) is written as it is.
//! - [`SyslogDest`]: the system log, through a datagram socket of the writer's own, non-blocking.
//! - [`WriterDest`]: any [`Write`], written by the home shard as it is (the tests, the tools).

use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use teseladb::engine::core::coroutine::make_coroutine;
use teseladb::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use teseladb::engine::core::internal::pollable_fd::PollableFd;
use teseladb::engine::core::posix::FileDesc;
use teseladb_io::IoClass;
use teseladb_io::engine::disk::{IoGroups, ShardDisk};
use teseladb_raftlog::{FileId, IoDone, IoOp};
use teseladb_types::Bytes;

use super::{Batch, Dest, ShardWriter};
use crate::shard_port::packet;

/// `O_NONBLOCK` of Linux.
const O_NONBLOCK: i32 = 0o4000;
/// The identifier of the file in its writer's disk executor.
const FILE: FileId = FileId(1);
/// The class of the writes: the rest of the core's files.
const CLASS: IoClass = IoClass::Data;

/// A file appended to.
#[derive(Debug)]
pub struct FileDest {
    dir: PathBuf,
    name: PathBuf,
    /// The file as the caller opened it, for appending where no engine runs.
    file: File,
    /// Its length: where the next write goes.
    len: Arc<AtomicU64>,
}

impl FileDest {
    /// The file at `path`, which the caller opened as `file` for appending (created with the
    /// permissions it wants).
    ///
    /// # Errors
    ///
    /// When its length cannot be read, or `path` names no file.
    pub fn new(path: &Path, file: File) -> io::Result<FileDest> {
        let len = file.metadata()?.len();
        let name = path
            .file_name()
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other(format!("{}: not a file", path.display())))?;
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => PathBuf::from("."),
        };
        Ok(FileDest { dir, name, file, len: Arc::new(AtomicU64::new(len)) })
    }
}

impl Dest for FileDest {
    fn writer(&self) -> Rc<dyn ShardWriter> {
        Rc::new(EngineFile {
            dir: self.dir.clone(),
            name: self.name.clone(),
            len: Arc::clone(&self.len),
            disk: RefCell::new(None),
        })
    }

    fn write_now(&self, batch: &Batch) {
        // A message that cannot be written is lost with the device; the node goes on.
        if (&self.file).write_all(batch.bytes()).is_ok()
            && let Ok(m) = self.file.metadata()
        {
            self.len.store(m.len(), Ordering::Release);
        }
    }
}

/// A file on the home shard's disk executor, opened the first time it is written.
struct EngineFile {
    dir: PathBuf,
    name: PathBuf,
    len: Arc<AtomicU64>,
    disk: RefCell<Option<ShardDisk>>,
}

impl ShardWriter for EngineFile {
    fn write(self: Rc<Self>, batch: Batch) -> Future<()> {
        make_coroutine(async move {
            self.append(batch).await;
            Ok(())
        })
    }
}

impl EngineFile {
    /// The executor with the file open; `None` when it cannot be opened now.
    async fn disk(&self) -> Option<ShardDisk> {
        if let Some(d) = self.disk.borrow().clone() {
            return Some(d);
        }
        let groups = IoGroups::get_or_create().await.ok()?;
        let disk = ShardDisk::open(self.dir.clone(), groups).await.ok()?;
        let open = IoOp::Open { file: FILE, path: self.name.clone() };
        if !matches!(disk.call(open, CLASS).await, Ok(IoDone::Ok(_))) {
            return None;
        }
        *self.disk.borrow_mut() = Some(disk.clone());
        Some(disk)
    }

    /// Writes `batch` at the end of the file.
    async fn append(&self, batch: Batch) {
        // A batch that cannot be written is lost with the device; the node goes on.
        let Some(disk) = self.disk().await else { return };
        let offset = self.len.load(Ordering::Acquire);
        let n = u64::try_from(batch.len()).unwrap_or(u64::MAX);
        let data = Bytes::from(batch.into_bytes());
        let op = IoOp::WriteAt { file: FILE, offset, data, batch: 0 };
        if matches!(disk.call(op, CLASS).await, Ok(IoDone::Ok(_))) {
            self.len.store(offset + n, Ordering::Release);
        }
    }
}

/// The standard output or error, or another descriptor of a stream.
#[derive(Debug)]
pub struct StreamDest {
    fd: RawFd,
    /// The descriptor, for writing where no engine runs.
    file: File,
}

impl StreamDest {
    /// The standard output.
    ///
    /// # Errors
    ///
    /// When its descriptor cannot be duplicated.
    pub fn stdout() -> io::Result<StreamDest> {
        Ok(StreamDest::from_fd(io::stdout().as_fd().try_clone_to_owned()?))
    }

    /// The standard error.
    ///
    /// # Errors
    ///
    /// When its descriptor cannot be duplicated.
    pub fn stderr() -> io::Result<StreamDest> {
        Ok(StreamDest::from_fd(io::stderr().as_fd().try_clone_to_owned()?))
    }

    /// The stream `fd`, which stays open while the destination lives (a pipe in the tests).
    #[must_use]
    pub fn from_fd(fd: OwnedFd) -> StreamDest {
        let raw = fd.as_raw_fd();
        StreamDest { fd: raw, file: File::from(fd) }
    }
}

/// How a stream is written on the home shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    /// Through the engine, waiting for room without holding the shard: a pipe, a terminal, a
    /// stream socket.
    Pollable,
    /// Straight into the page cache: a regular file.
    Regular,
    /// As it is: anything else (`/dev/null`).
    Inline,
}

/// How the stream whose `/proc/self/fd` link reads `link` is written; `regular` when it is a
/// regular file.
#[must_use]
pub fn stream_kind(link: &str, regular: bool) -> StreamKind {
    let pollable = ["pipe:", "socket:", "/dev/pts/", "/dev/tty"];
    if regular {
        StreamKind::Regular
    } else if pollable.iter().any(|p| link.starts_with(p)) {
        StreamKind::Pollable
    } else {
        StreamKind::Inline
    }
}

impl Dest for StreamDest {
    fn writer(&self) -> Rc<dyn ShardWriter> {
        let proc = format!("/proc/self/fd/{}", self.fd);
        let link = std::fs::read_link(&proc).map(|l| l.to_string_lossy().into_owned());
        let regular = std::fs::metadata(&proc).is_ok_and(|m| m.is_file());
        let kind = link.as_deref().map_or(StreamKind::Inline, |l| stream_kind(l, regular));
        let own = |append: bool| {
            OpenOptions::new().write(true).append(append).custom_flags(O_NONBLOCK).open(&proc)
        };
        let made: io::Result<Rc<dyn ShardWriter>> = match kind {
            StreamKind::Pollable => own(false)
                .map(OwnedFd::from)
                .or_else(|_| nonblocking_dup(&self.file))
                .map(|fd| Rc::new(Pollable::new(fd)) as Rc<dyn ShardWriter>),
            StreamKind::Regular => own(true).map(|f| Rc::new(Plain(f)) as Rc<dyn ShardWriter>),
            StreamKind::Inline => Err(io::Error::other("written as it is")),
        };
        made.unwrap_or_else(|_| match self.file.try_clone() {
            Ok(f) => Rc::new(Plain(f)),
            Err(_) => Rc::new(Lost),
        })
    }

    fn write_now(&self, batch: &Batch) {
        let _ = (&self.file).write_all(batch.bytes());
    }
}

/// A duplicate of `file` made non-blocking: a stream socket, which cannot be opened again.
fn nonblocking_dup(file: &File) -> io::Result<OwnedFd> {
    let sock = UnixStream::from(file.as_fd().try_clone_to_owned()?);
    sock.set_nonblocking(true)?;
    Ok(OwnedFd::from(sock))
}

/// A non-blocking descriptor written through the engine.
struct Pollable {
    fd: RefCell<PollableFd>,
}

impl Pollable {
    fn new(fd: OwnedFd) -> Pollable {
        Pollable {
            fd: RefCell::new(PollableFd::from_file_desc(FileDesc::from_fd(fd.into_raw_fd()))),
        }
    }

    /// Writes `bytes` whole; a failure loses them.
    fn send(self: &Rc<Self>, bytes: Vec<u8>) -> Future<()> {
        let mut p = Box::new(packet(bytes));
        let sent = self.fd.borrow_mut().write_all(&mut p);
        let keep = Rc::clone(self);
        sent.then_wrapped(move |r: Future<()>| {
            drop((p, keep));
            let _ = r.get();
            Ok::<(), ExceptionPtr>(())
        })
    }
}

impl ShardWriter for Pollable {
    fn write(self: Rc<Self>, batch: Batch) -> Future<()> {
        self.send(batch.into_bytes())
    }
}

/// A descriptor written as it is: a regular file, `/dev/null`.
struct Plain(File);

impl ShardWriter for Plain {
    fn write(self: Rc<Self>, batch: Batch) -> Future<()> {
        let _ = (&self.0).write_all(batch.bytes());
        make_ready_future(())
    }
}

/// A stream that could not be reached: its messages are lost.
struct Lost;

impl ShardWriter for Lost {
    fn write(self: Rc<Self>, _batch: Batch) -> Future<()> {
        make_ready_future(())
    }
}

/// The system log: a datagram socket connected to its socket at `path`.
#[derive(Debug)]
pub struct SyslogDest {
    path: PathBuf,
    sock: UnixDatagram,
    prefix: Vec<u8>,
}

impl SyslogDest {
    /// The system log at `path`, through `sock` connected to it, each message after `prefix`
    /// (priority and tag).
    #[must_use]
    pub fn new(path: PathBuf, sock: UnixDatagram, prefix: Vec<u8>) -> SyslogDest {
        SyslogDest { path, sock, prefix }
    }
}

/// `msg` after `prefix`: one datagram of the system log.
fn datagram(prefix: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut d = Vec::with_capacity(prefix.len() + msg.len());
    d.extend_from_slice(prefix);
    d.extend_from_slice(msg);
    d
}

impl Dest for SyslogDest {
    fn writer(&self) -> Rc<dyn ShardWriter> {
        let own = UnixDatagram::unbound().and_then(|s| {
            s.connect(&self.path)?;
            s.set_nonblocking(true)?;
            Ok(s)
        });
        match own {
            Ok(s) => Rc::new(Datagrams {
                out: Rc::new(Pollable::new(OwnedFd::from(s))),
                prefix: self.prefix.clone(),
            }),
            Err(_) => Rc::new(Lost),
        }
    }

    fn write_now(&self, batch: &Batch) {
        for msg in batch.messages() {
            let _ = self.sock.send(&datagram(&self.prefix, msg));
        }
    }
}

/// The system log's socket on the home shard: one datagram per message.
struct Datagrams {
    out: Rc<Pollable>,
    prefix: Vec<u8>,
}

impl ShardWriter for Datagrams {
    fn write(self: Rc<Self>, batch: Batch) -> Future<()> {
        make_coroutine(async move {
            for msg in batch.messages() {
                let _ = self.out.send(datagram(&self.prefix, msg)).await;
            }
            Ok(())
        })
    }
}

/// Any writer, written as it is by the home shard.
pub struct WriterDest {
    out: Arc<Mutex<Box<dyn Write + Send>>>,
}

impl std::fmt::Debug for WriterDest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WriterDest")
    }
}

impl WriterDest {
    /// The destination `out`.
    #[must_use]
    pub fn new(out: Box<dyn Write + Send>) -> WriterDest {
        WriterDest { out: Arc::new(Mutex::new(out)) }
    }
}

/// Writes `batch` to `out`; a failure loses it.
fn write_locked(out: &Mutex<Box<dyn Write + Send>>, batch: &Batch) {
    let mut out = out.lock().unwrap_or_else(PoisonError::into_inner);
    let _ = out.write_all(batch.bytes()).and_then(|()| out.flush());
}

impl Dest for WriterDest {
    fn writer(&self) -> Rc<dyn ShardWriter> {
        Rc::new(Locked(Arc::clone(&self.out)))
    }

    fn write_now(&self, batch: &Batch) {
        write_locked(&self.out, batch);
    }
}

/// [`WriterDest`] on the home shard: only its writer task takes the lock there.
struct Locked(Arc<Mutex<Box<dyn Write + Send>>>);

impl ShardWriter for Locked {
    fn write(self: Rc<Self>, batch: Batch) -> Future<()> {
        write_locked(&self.0, &batch);
        make_ready_future(())
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn each_stream_is_written_the_way_its_kind_allows() {
        assert_eq!(stream_kind("pipe:[1234]", false), StreamKind::Pollable);
        assert_eq!(stream_kind("socket:[99]", false), StreamKind::Pollable);
        assert_eq!(stream_kind("/dev/pts/3", false), StreamKind::Pollable);
        assert_eq!(stream_kind("/dev/tty1", false), StreamKind::Pollable);
        assert_eq!(stream_kind("/var/log/teseladb.log", true), StreamKind::Regular);
        assert_eq!(stream_kind("/dev/null", false), StreamKind::Inline);
    }

    #[test]
    fn without_an_engine_each_destination_writes_at_once() {
        let dir = std::env::temp_dir().join(format!("teseladb-outlet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A file: appended after what it held, its length followed.
        let path = dir.join("out.log");
        std::fs::write(&path, b"before\n").unwrap();
        let file = OpenOptions::new().append(true).open(&path).unwrap();
        let dest = FileDest::new(&path, file).unwrap();
        assert_eq!(dest.len.load(Ordering::Acquire), 7);
        dest.write_now(&Batch::one(b"after\n"));
        assert_eq!(std::fs::read(&path).unwrap(), b"before\nafter\n");
        assert_eq!(dest.len.load(Ordering::Acquire), 13);
        assert!(FileDest::new(Path::new("/"), File::open(&path).unwrap()).is_err());
        // A pipe.
        let (mut r, w) = io::pipe().unwrap();
        let stream = StreamDest::from_fd(OwnedFd::from(w));
        stream.write_now(&Batch::one(b"line\n"));
        drop(stream);
        let mut got = String::new();
        io::Read::read_to_string(&mut r, &mut got).unwrap();
        assert_eq!(got, "line\n");
        // The system log: one datagram per message, after the prefix.
        let at = dir.join("log.sock");
        let server = UnixDatagram::bind(&at).unwrap();
        let sock = UnixDatagram::unbound().unwrap();
        sock.connect(&at).unwrap();
        let syslog = SyslogDest::new(at, sock, b"<14>t: ".to_vec());
        let mut two = Batch::one(b"a");
        two.push(b"b");
        syslog.write_now(&two);
        let mut buf = [0u8; 64];
        let n = server.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"<14>t: a");
        let n = server.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"<14>t: b");
        // Any writer.
        let kept = Arc::new(Mutex::new(Vec::new()));
        struct Keep(Arc<Mutex<Vec<u8>>>);
        impl Write for Keep {
            fn write(&mut self, b: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let w = WriterDest::new(Box::new(Keep(Arc::clone(&kept))));
        w.write_now(&Batch::one(b"w\n"));
        assert_eq!(kept.lock().unwrap().as_slice(), b"w\n");
        assert!(format!("{w:?}").contains("WriterDest"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
