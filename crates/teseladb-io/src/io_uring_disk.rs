//! Executor of [`IoOp`] over io_uring (012 FR-002, contract `executors.md`): the core submits and
//! collects its disk operations in its own loop, with no disk thread. Writes and reads go through
//! the core's reserve of registered buffers ([`crate::buffers`]) and the registered file table;
//! synchronizations, renames, removals, reservations and truncations are io_uring operations too.
//!
//! The results are the same as [`crate::FileDisk`]'s, in submission order. io_uring does not order
//! operations, so an operation only starts when it cannot depend on one still running: reads and
//! writes run together while their ranges do not overlap a write's on the same file, and every
//! other operation (a `Sync` among them) waits for all earlier ones to finish and runs alone. A
//! `Sync` therefore covers every write submitted before it on its file.
//!
//! With `O_DIRECT` each file has a second descriptor opened with it: writes and reads of whole
//! 4 KiB blocks use it, and the rest (the engine's few unaligned accesses) the buffered one. Both
//! are the same inode, so `fdatasync` on either covers both.

use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use teseladb_sys::uring::{Op, Probe, Ring, Target, opcode};
use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp};
use teseladb_types::Bytes;

use crate::buffers::{BUFFER_BYTES, BUFFERS_PER_CORE, BufferPool, is_aligned};
use crate::file_disk::{O_DIRECT, errno, io_errno};

/// Submission slots of a core's ring.
pub const RING_ENTRIES: u32 = 64;
/// Slots of the registered file table; files beyond it use their plain descriptor.
pub const FILE_SLOTS: u32 = 1024;
/// Operations the executor cannot do without (FR-007): with fewer, the node uses the disk threads.
pub const REQUIRED_OPS: [(u8, &str); 7] = [
    (opcode::READ_FIXED, "IORING_OP_READ_FIXED"),
    (opcode::WRITE_FIXED, "IORING_OP_WRITE_FIXED"),
    (opcode::FSYNC, "IORING_OP_FSYNC"),
    (opcode::OPENAT, "IORING_OP_OPENAT"),
    (opcode::FALLOCATE, "IORING_OP_FALLOCATE"),
    (opcode::RENAMEAT, "IORING_OP_RENAMEAT"),
    (opcode::UNLINKAT, "IORING_OP_UNLINKAT"),
];

const O_RDONLY: i32 = 0;
const O_RDWR: i32 = 2;
const O_CREAT: i32 = 0o100;
const O_EXCL: i32 = 0o200;
#[cfg(target_arch = "aarch64")]
const O_DIRECTORY: i32 = 0o40000;
#[cfg(not(target_arch = "aarch64"))]
const O_DIRECTORY: i32 = 0o200000;

/// Whether this kernel has what [`IoUringDisk`] needs: `Err` names what is missing (FR-007).
pub fn check_support() -> Result<(), String> {
    support_of(Ring::new(4))
}

/// What a ring the kernel gave (or refused) says of the support.
fn support_of(ring: io::Result<Ring>) -> Result<(), String> {
    let ring = ring.map_err(|e| format!("io_uring_setup failed: {e}"))?;
    let probe = ring.probe().map_err(|e| format!("IORING_REGISTER_PROBE failed: {e}"))?;
    missing_ops(&probe)
}

// Data jobs hold a buffer each and every other job runs alone, so the jobs in flight never pass
// the buffers, which never pass the ring's slots.
const _: () = assert!((BUFFERS_PER_CORE as u32) < RING_ENTRIES);

fn missing_ops(probe: &Probe) -> Result<(), String> {
    let missing: Vec<&str> =
        REQUIRED_OPS.iter().filter(|(op, _)| !probe.supports(*op)).map(|(_, n)| *n).collect();
    if missing.is_empty() { Ok(()) } else { Err(format!("missing {}", missing.join(", "))) }
}

/// A descriptor and its slot in the registered table, if it got one.
#[derive(Debug)]
struct Handle {
    fd: OwnedFd,
    slot: Option<u32>,
}

impl Handle {
    fn target(&self) -> Target {
        self.slot.map_or(Target::Fd(self.fd.as_raw_fd()), Target::Fixed)
    }
}

#[derive(Debug)]
struct OpenFile {
    buffered: Handle,
    direct: Option<Handle>,
}

impl OpenFile {
    /// The descriptor for `len` bytes at `offset`: the direct one for whole blocks.
    fn target(&self, offset: u64, len: u64) -> Target {
        match &self.direct {
            Some(d) if is_aligned(offset, len) => d.target(),
            _ => self.buffered.target(),
        }
    }
}

/// One step of an operation that is not a read or a write.
#[derive(Debug)]
enum Step {
    Open { path: CString, flags: i32 },
    Allocate(u64),
    Fsync { on: On },
    Truncate { on: On, len: u64 },
    Rename(CString, CString),
    Unlink(CString, bool),
}

/// The file a step acts on.
#[derive(Clone, Copy, Debug)]
enum On {
    /// A descriptor this operation opened (by order).
    Opened(usize),
    /// An open file of the disk.
    File(FileId),
}

#[derive(Debug)]
enum Work {
    Write { pos: usize, buf: u16 },
    Read { got: Vec<u8>, buf: u16 },
    Steps { steps: VecDeque<Step>, opened: Vec<OwnedFd> },
}

#[derive(Debug)]
struct Job {
    seq: u64,
    op: IoOp,
    work: Work,
    /// The ring operation it waits for.
    waiting: Option<u64>,
    result: Option<IoDone>,
}

/// io_uring executor rooted at a directory.
#[derive(Debug)]
pub struct IoUringDisk {
    root: PathBuf,
    ring: Ring,
    pool: BufferPool,
    direct: bool,
    ftruncate: bool,
    files: HashMap<FileId, OpenFile>,
    free_slots: Vec<u32>,
    queue: VecDeque<(u64, IoOp)>,
    running: VecDeque<Job>,
    by_ring: HashMap<u64, u64>,
    done: VecDeque<(u64, IoDone)>,
    next_seq: u64,
    wake: File,
    /// Whether the buffers are registered: on the first read or write, from the core's thread,
    /// so that their memory is on the NUMA node of the core's CPU (012 FR-006).
    buffers_ready: bool,
}

fn cpath(p: &Path) -> CString {
    // A path from the engine never holds a NUL byte; one that did names no file.
    CString::new(p.as_os_str().as_bytes()).unwrap_or_default()
}

/// The (start, end) range of bytes a read or a write touches, and whether it writes.
fn data_range(op: &IoOp) -> Option<(FileId, u64, u64, bool)> {
    match op {
        IoOp::WriteAt { file, offset, data, .. } => {
            Some((*file, *offset, offset + data.len() as u64, true))
        }
        IoOp::ReadAt { file, offset, len, .. } => {
            Some((*file, *offset, offset + u64::from(*len), false))
        }
        _ => None,
    }
}

fn conflicts(a: (FileId, u64, u64, bool), b: (FileId, u64, u64, bool)) -> bool {
    a.0 == b.0 && (a.3 || b.3) && a.1 < b.2 && b.1 < a.2
}

impl IoUringDisk {
    /// Disk rooted at `root` (created if it does not exist), with its own ring, its reserve of
    /// registered buffers and, with `direct_io`, `O_DIRECT` for whole-block accesses.
    pub fn open(root: impl Into<PathBuf>, direct_io: bool) -> io::Result<IoUringDisk> {
        IoUringDisk::open_with(root, direct_io, FILE_SLOTS)
    }

    /// [`IoUringDisk::open`] with `file_slots` slots in the registered file table: the files
    /// opened beyond them use their plain descriptor.
    pub fn open_with(
        root: impl Into<PathBuf>,
        direct_io: bool,
        file_slots: u32,
    ) -> io::Result<IoUringDisk> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let mut ring = Ring::new(RING_ENTRIES)?;
        let probe = ring.probe()?;
        missing_ops(&probe).map_err(io::Error::other)?;
        ring.register_files(file_slots)?;
        let wake = File::from(teseladb_sys::uring::eventfd()?);
        ring.register_eventfd(wake.as_raw_fd())?;
        Ok(IoUringDisk {
            root,
            ring,
            pool: BufferPool::new(BUFFERS_PER_CORE),
            direct: direct_io,
            ftruncate: probe.supports(opcode::FTRUNCATE),
            files: HashMap::new(),
            free_slots: (0..file_slots).rev().collect(),
            queue: VecDeque::new(),
            running: VecDeque::new(),
            by_ring: HashMap::new(),
            done: VecDeque::new(),
            next_seq: 0,
            wake,
            buffers_ready: false,
        })
    }

    /// Root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether whole-block accesses use `O_DIRECT`.
    pub fn direct_io(&self) -> bool {
        self.direct
    }

    /// The `eventfd` the ring signals on every completion: the core's poller watches it.
    pub fn event_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }

    /// Operations submitted whose result has not been collected.
    pub fn pending(&self) -> usize {
        self.queue.len() + self.running.len() + self.done.len()
    }

    /// Uses `IORING_OP_FTRUNCATE` (`on`) or truncates from the core's thread, as on kernels
    /// before 6.9 that do not have it.
    pub fn set_ftruncate(&mut self, on: bool) {
        self.ftruncate = on;
    }

    /// Whether results are waiting for [`IoUringDisk::reap`] without a completion to signal them
    /// (an operation that failed before reaching the ring).
    pub fn ready(&self) -> bool {
        !self.done.is_empty()
    }

    /// Waits for every submitted operation and drops the results (when the core stops).
    pub fn drain(&mut self) {
        while !self.queue.is_empty() || !self.running.is_empty() {
            self.start_ready();
            let wait = u32::from(self.running.iter().any(|j| j.waiting.is_some()));
            if self.ring.submit(wait).is_err() {
                break;
            }
            self.collect();
        }
        self.done.clear();
    }

    /// Queues `op` behind the ones already submitted; it starts on the next [`IoUringDisk::flush`].
    pub fn submit(&mut self, op: IoOp) {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.queue.push_back((seq, op));
    }

    /// Starts what can start and hands it to the kernel in one `io_uring_enter`.
    pub fn flush(&mut self) -> io::Result<()> {
        self.start_ready();
        self.ring.submit(0)?;
        Ok(())
    }

    /// Collects the completions that arrived, moves the operations on, and appends to `out` the
    /// results of the finished ones, in submission order.
    pub fn reap(&mut self, out: &mut Vec<IoDone>) -> io::Result<()> {
        let mut counter = [0u8; 8];
        // Resets the eventfd; nothing to read means no completion was signalled since.
        let _ = (&self.wake).read(&mut counter);
        self.collect();
        self.flush()?;
        out.extend(self.done.drain(..).map(|(_, d)| d));
        Ok(())
    }

    fn collect(&mut self) {
        while let Some(c) = self.ring.complete() {
            // Every operation the ring completes was pushed by a job still running.
            let seq = self.by_ring.remove(&c.id).expect("a job's ring operation");
            let i = self.running.iter().position(|j| j.seq == seq).expect("its job is running");
            self.running[i].waiting = None;
            if let (Some(fd), Work::Steps { opened, .. }) = (c.fd, &mut self.running[i].work) {
                opened.push(fd);
            }
            self.advance(i, Some(c.result));
        }
        while self.running.front().is_some_and(|j| j.result.is_some()) {
            let j = self.running.pop_front().expect("front job");
            self.done.push_back((j.seq, j.result.expect("finished job")));
        }
    }

    /// Whether the operation at the head of the queue may start now.
    fn can_start(&self, op: &IoOp) -> bool {
        let Some(new) = data_range(op) else { return self.running.is_empty() };
        if self.pool.available() == 0 {
            return false;
        }
        self.running.iter().all(|j| data_range(&j.op).is_some_and(|r| !conflicts(r, new)))
    }

    fn start_ready(&mut self) {
        while let Some((_, op)) = self.queue.front() {
            if !self.can_start(op) {
                return;
            }
            let (seq, op) = self.queue.pop_front().expect("front op");
            if data_range(&op).is_some() && !self.buffers_ready {
                match self.ring.register_buffers(BufferPool::allocate(BUFFERS_PER_CORE)) {
                    Ok(()) => self.buffers_ready = true,
                    Err(e) => {
                        // Finished at once, behind the ones still running.
                        let result = Some(fail(&op, errno(&e)));
                        let work = Work::Steps { steps: VecDeque::new(), opened: Vec::new() };
                        self.running.push_back(Job { seq, op, work, waiting: None, result });
                        continue;
                    }
                }
            }
            let work = match &op {
                IoOp::WriteAt { .. } => {
                    Work::Write { pos: 0, buf: self.pool.take().expect("a free buffer") }
                }
                IoOp::ReadAt { .. } => {
                    Work::Read { got: Vec::new(), buf: self.pool.take().expect("a free buffer") }
                }
                _ => match self.plan(&op) {
                    Ok(steps) => Work::Steps { steps, opened: Vec::new() },
                    Err(e) => {
                        // Finished at once, behind the ones still running.
                        let result = Some(fail(&op, e));
                        let work = Work::Steps { steps: VecDeque::new(), opened: Vec::new() };
                        self.running.push_back(Job { seq, op, work, waiting: None, result });
                        continue;
                    }
                },
            };
            self.running.push_back(Job { seq, op, work, waiting: None, result: None });
            self.advance(self.running.len() - 1, None);
        }
        self.collect();
    }

    /// The steps of an operation that is not a read or a write. Those done without the ring
    /// (closing, and creating the parent directories) happen here.
    fn plan(&mut self, op: &IoOp) -> Result<VecDeque<Step>, IoErrno> {
        let mut steps = VecDeque::new();
        let direct_flags = O_RDWR | O_DIRECT;
        match op {
            IoOp::Create { path, prealloc, .. } => {
                let full = self.root.join(path);
                fs::create_dir_all(full.parent().unwrap_or(&self.root)).map_err(io_errno)?;
                let c = cpath(&full);
                steps.push_back(Step::Open { path: c.clone(), flags: O_RDWR | O_CREAT | O_EXCL });
                if self.direct {
                    steps.push_back(Step::Open { path: c, flags: direct_flags });
                }
                if *prealloc > 0 {
                    steps.push_back(Step::Allocate(*prealloc));
                }
            }
            IoOp::Open { path, .. } => {
                let c = cpath(&self.root.join(path));
                steps.push_back(Step::Open { path: c.clone(), flags: O_RDWR });
                if self.direct {
                    steps.push_back(Step::Open { path: c, flags: direct_flags });
                }
            }
            IoOp::Sync { file, .. } => {
                self.files.get(file).ok_or(IoErrno::BadFile)?;
                steps.push_back(Step::Fsync { on: On::File(*file) });
            }
            IoOp::SyncDir { path } => {
                let c = cpath(&self.root.join(path));
                steps.push_back(Step::Open { path: c, flags: O_RDONLY | O_DIRECTORY });
                steps.push_back(Step::Fsync { on: On::Opened(0) });
            }
            IoOp::Truncate { file, len } => {
                let f = self.files.get(file).ok_or(IoErrno::BadFile)?;
                if !self.ftruncate {
                    // A kernel without IORING_OP_FTRUNCATE (before 6.9): the core truncates.
                    let fd = f.buffered.fd.try_clone().map_err(io_errno)?;
                    File::from(fd).set_len(*len).map_err(io_errno)?;
                    return Ok(steps);
                }
                steps.push_back(Step::Truncate { on: On::File(*file), len: *len });
            }
            IoOp::Rename { from, to } => {
                let (f, t) = (cpath(&self.root.join(from)), cpath(&self.root.join(to)));
                steps.push_back(Step::Rename(f, t));
            }
            IoOp::Remove { path } => {
                steps.push_back(Step::Unlink(cpath(&self.root.join(path)), false))
            }
            IoOp::RemoveDir { path } => {
                steps.push_back(Step::Unlink(cpath(&self.root.join(path)), true))
            }
            IoOp::Close { file } => {
                let f = self.files.remove(file).ok_or(IoErrno::BadFile)?;
                self.release(f);
            }
            IoOp::WriteAt { .. } | IoOp::ReadAt { .. } => {
                unreachable!("reads and writes have no steps")
            }
        }
        Ok(steps)
    }

    /// Gives up the slots of a closed file; its descriptors close when dropped.
    fn release(&mut self, f: OpenFile) {
        for h in std::iter::once(f.buffered).chain(f.direct) {
            if let Some(s) = h.slot {
                // Clearing it only lets the kernel drop its reference now: the next file that
                // takes the slot replaces whatever is there, so a failure here changes nothing.
                let _ = self.ring.set_file(s, -1);
                self.free_slots.push(s);
            }
        }
    }

    fn handle(&mut self, fd: OwnedFd) -> Handle {
        let slot = self.free_slots.pop();
        let slot = slot.and_then(|s| match self.ring.set_file(s, fd.as_raw_fd()) {
            Ok(()) => Some(s),
            Err(_) => {
                self.free_slots.push(s);
                None
            }
        });
        Handle { fd, slot }
    }

    fn target_of(&self, job: &Job, on: On) -> Option<Target> {
        match on {
            On::Opened(i) => match &job.work {
                Work::Steps { opened, .. } => opened.get(i).map(|fd| Target::Fd(fd.as_raw_fd())),
                _ => None,
            },
            On::File(f) => self.files.get(&f).map(|o| o.buffered.target()),
        }
    }

    /// Moves job `i` on after the ring operation it waited for ended with `last` (`None` when it
    /// is just starting).
    fn advance(&mut self, i: usize, last: Option<io::Result<u32>>) {
        let next = match self.running[i].work {
            Work::Steps { .. } => self.advance_steps(i, last),
            Work::Write { .. } | Work::Read { .. } => {
                data_step(&mut self.ring, &self.files, &mut self.running[i], last)
            }
        };
        match next {
            Ok(Some(op)) => match self.ring.push(op) {
                Ok(id) => {
                    self.by_ring.insert(id, self.running[i].seq);
                    self.running[i].waiting = Some(id);
                }
                Err(_) => self.finish(i, Err(IoErrno::Io)),
            },
            Ok(None) => self.finish(i, Ok(())),
            Err(e) => self.finish(i, Err(e)),
        }
    }

    fn advance_steps(
        &mut self,
        i: usize,
        last: Option<io::Result<u32>>,
    ) -> Result<Option<Op>, IoErrno> {
        if let Some(Err(e)) = last {
            return Err(errno(&e));
        }
        let step = match &mut self.running[i].work {
            Work::Steps { steps, .. } => steps.pop_front(),
            _ => unreachable!(),
        };
        let Some(step) = step else { return Ok(None) };
        let job = &self.running[i];
        Ok(Some(match step {
            Step::Open { path, flags } => Op::OpenAt { path, flags, mode: 0o666 },
            Step::Allocate(len) => {
                let target = self.target_of(job, On::Opened(0)).ok_or(IoErrno::BadFile)?;
                Op::Fallocate { target, len }
            }
            Step::Fsync { on } => {
                let target = self.target_of(job, on).ok_or(IoErrno::BadFile)?;
                Op::Fsync { target, data_only: matches!(on, On::File(_)) }
            }
            Step::Truncate { on, len } => {
                Op::Ftruncate { target: self.target_of(job, on).ok_or(IoErrno::BadFile)?, len }
            }
            Step::Rename(from, to) => Op::RenameAt { from, to },
            Step::Unlink(path, dir) => Op::UnlinkAt { path, dir },
        }))
    }

    fn finish(&mut self, i: usize, r: Result<(), IoErrno>) {
        let job = &mut self.running[i];
        if let Work::Write { buf, .. } | Work::Read { buf, .. } = job.work {
            self.pool.give(buf);
        }
        let opened = match &mut job.work {
            Work::Steps { opened, .. } => std::mem::take(opened),
            _ => Vec::new(),
        };
        let done = match (r, &job.op) {
            (Err(e), op) => {
                // A file created by this operation does not stay when it fails afterwards.
                if let (IoOp::Create { path, .. }, false) = (op, opened.is_empty()) {
                    let _ = fs::remove_file(self.root.join(path));
                }
                fail(op, e)
            }
            (Ok(()), IoOp::ReadAt { token, .. }) => {
                let got = match &mut job.work {
                    Work::Read { got, .. } => std::mem::take(got),
                    _ => Vec::new(),
                };
                IoDone::Read { token: *token, data: Ok(Bytes::from_vec(got)) }
            }
            (Ok(()), IoOp::Create { file, .. } | IoOp::Open { file, .. }) => {
                let file = *file;
                let op = job.op.without_data();
                let mut fds = opened.into_iter();
                let buffered = fds.next().map(|fd| self.handle(fd));
                let direct = fds.next().map(|fd| self.handle(fd));
                let buffered = buffered.expect("a finished open has its descriptor");
                if let Some(old) = self.files.insert(file, OpenFile { buffered, direct }) {
                    self.release(old);
                }
                IoDone::Ok(op)
            }
            (Ok(()), op) => IoDone::Ok(op.without_data()),
        };
        self.running[i].result = Some(done);
    }
}

/// The next ring operation of a read or a write after the last one ended with `last` (`None`
/// when it is just starting); `None` when the job is done.
fn data_step(
    ring: &mut Ring,
    files: &HashMap<FileId, OpenFile>,
    job: &mut Job,
    last: Option<io::Result<u32>>,
) -> Result<Option<Op>, IoErrno> {
    match (&job.op, &mut job.work) {
        (IoOp::WriteAt { file, offset, data, .. }, Work::Write { pos, buf }) => {
            match last {
                Some(Err(e)) => return Err(errno(&e)),
                // A write that moves nothing would loop for ever.
                Some(Ok(0)) => return Err(IoErrno::Io),
                Some(Ok(n)) => *pos += n as usize,
                None => {}
            }
            if *pos >= data.len() {
                return Ok(None);
            }
            let f = files.get(file).ok_or(IoErrno::BadFile)?;
            let n = (data.len() - *pos).min(BUFFER_BYTES);
            let at = offset + *pos as u64;
            let target = f.target(at, n as u64);
            let b = ring.buffer(*buf).expect("the job's buffer is idle between its operations");
            b[..n].copy_from_slice(&data[*pos..*pos + n]);
            Ok(Some(Op::WriteFixed { target, buf: *buf, at: 0, len: n as u32, offset: at }))
        }
        (IoOp::ReadAt { file, offset, len, .. }, Work::Read { got, buf }) => {
            let len = *len as usize;
            match last {
                Some(Err(e)) => return Err(errno(&e)),
                Some(Ok(0)) => return Ok(None),
                Some(Ok(n)) => {
                    let b = ring.buffer(*buf).expect("the read has completed");
                    got.extend_from_slice(&b[..n as usize]);
                }
                None => {}
            }
            if got.len() >= len {
                return Ok(None);
            }
            let at = offset + got.len() as u64;
            let n = (len - got.len()).min(BUFFER_BYTES);
            let target = files.get(file).ok_or(IoErrno::BadFile)?.target(at, n as u64);
            Ok(Some(Op::ReadFixed { target, buf: *buf, at: 0, len: n as u32, offset: at }))
        }
        _ => unreachable!("a data job is a read or a write"),
    }
}

fn fail(op: &IoOp, errno: IoErrno) -> IoDone {
    match op {
        IoOp::ReadAt { token, .. } => IoDone::Read { token: *token, data: Err(errno) },
        op => IoDone::Err { op: op.without_data(), errno },
    }
}

impl Disk for IoUringDisk {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        let mut v = Vec::new();
        for entry in fs::read_dir(self.root.join(dir)).map_err(io_errno)? {
            let entry = entry.map_err(io_errno)?;
            v.push(dir.join(entry.file_name()));
        }
        v.sort();
        Ok(v)
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        fs::read(self.root.join(path)).map_err(io_errno)
    }

    /// Submits `op` and waits for it; earlier submitted results stay for [`IoUringDisk::reap`].
    fn execute(&mut self, op: IoOp) -> IoDone {
        let seq = self.next_seq;
        let fallback = op.without_data();
        self.submit(op);
        loop {
            if let Some(i) = self.done.iter().position(|(s, _)| *s == seq) {
                return self.done.remove(i).expect("found result").1;
            }
            self.start_ready();
            let waited = if self.running.iter().any(|j| j.waiting.is_some()) {
                self.ring.submit(1)
            } else {
                self.ring.submit(0)
            };
            if let Err(e) = waited {
                return fail(&fallback, errno(&e));
            }
            self.collect();
        }
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn ranges_conflict_only_with_a_write_on_the_same_file() {
        let w = |f, a, b| (FileId(f), a, b, true);
        let r = |f, a, b| (FileId(f), a, b, false);
        assert!(conflicts(w(1, 0, 10), r(1, 5, 15)));
        assert!(conflicts(r(1, 5, 15), w(1, 0, 10)));
        assert!(!conflicts(r(1, 0, 10), r(1, 0, 10)));
        assert!(!conflicts(w(1, 0, 10), w(2, 0, 10)));
        assert!(!conflicts(w(1, 0, 10), w(1, 10, 20)));
    }

    fn temp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("teseladb-iud-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn ok(d: &mut IoUringDisk, op: IoOp) {
        let r = d.execute(op);
        assert!(matches!(r, IoDone::Ok(_)), "{r:?}");
    }

    #[test]
    fn a_refused_ring_says_why() {
        let e = support_of(Err(io::Error::from_raw_os_error(1))).unwrap_err();
        assert!(e.starts_with("io_uring_setup failed: "), "{e}");
    }

    #[test]
    fn files_past_the_registered_table_use_their_descriptor() {
        let root = temp("slots");
        let mut d = IoUringDisk::open_with(&root, false, 1).unwrap();
        assert_eq!((d.root(), d.direct_io()), (root.as_path(), false));
        for f in 1..=2 {
            ok(&mut d, IoOp::Create { file: FileId(f), path: format!("f{f}").into(), prealloc: 0 });
            let data = Bytes::from_vec(vec![f as u8; 10]);
            ok(&mut d, IoOp::WriteAt { file: FileId(f), offset: 0, data, batch: 0 });
        }
        assert!(d.files[&FileId(2)].buffered.slot.is_none(), "the table has one slot");
        let r = d.execute(IoOp::ReadAt { file: FileId(2), offset: 0, len: 10, token: 1 });
        assert_eq!(r, IoDone::Read { token: 1, data: Ok(Bytes::from_vec(vec![2; 10])) });
        ok(&mut d, IoOp::Close { file: FileId(1) });
        ok(&mut d, IoOp::Close { file: FileId(2) });
        // The slot the first file left is taken again.
        ok(&mut d, IoOp::Open { file: FileId(3), path: "f2".into() });
        assert_eq!(d.files[&FileId(3)].buffered.slot, Some(0));
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn without_ftruncate_the_core_truncates() {
        let root = temp("trunc");
        let mut d = IoUringDisk::open(&root, false).unwrap();
        d.set_ftruncate(false);
        ok(&mut d, IoOp::Create { file: FileId(1), path: "f".into(), prealloc: 8192 });
        ok(&mut d, IoOp::Truncate { file: FileId(1), len: 100 });
        assert_eq!(fs::metadata(root.join("f")).unwrap().len(), 100);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn stopping_waits_for_what_was_submitted() {
        let root = temp("drain");
        let mut d = IoUringDisk::open(&root, false).unwrap();
        d.submit(IoOp::Create { file: FileId(1), path: "f".into(), prealloc: 0 });
        for i in 0..40u64 {
            let data = Bytes::from_vec(vec![i as u8; 4096]);
            d.submit(IoOp::WriteAt { file: FileId(1), offset: i * 4096, data, batch: i });
        }
        d.submit(IoOp::Sync { file: FileId(9), batch: 0 });
        assert_eq!(d.pending(), 42);
        d.drain();
        // Writes already handed to the kernel are waited for too.
        let data = Bytes::from_vec(vec![7; 4096]);
        d.submit(IoOp::WriteAt { file: FileId(1), offset: 40 * 4096, data, batch: 40 });
        d.flush().unwrap();
        d.drain();
        assert_eq!(d.pending(), 0);
        assert!(!d.ready());
        assert_eq!(fs::metadata(root.join("f")).unwrap().len(), 41 * 4096);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn this_kernel_supports_the_executor() {
        check_support().unwrap();
        let mut p = Ring::new(2).unwrap().probe().unwrap();
        assert!(missing_ops(&p).is_ok());
        p = Probe::with(&[opcode::READ_FIXED]);
        assert!(missing_ops(&p).unwrap_err().contains("IORING_OP_WRITE_FIXED"));
    }
}
