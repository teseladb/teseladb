//! The executor of a shard's disk operations on the engine (023 FR-010, FR-012, contract
//! `executors.md` of 012): every [`IoOp`] of a core runs on the engine's files, through the engine's
//! I/O queue in the priority class of its [`IoClass`], and its [`IoDone`] comes back to the core.
//!
//! # Interface for the core's task
//!
//! One [`ShardDisk`] per shard, made on that shard and used only there (it is not `Send`):
//!
//! - [`IoGroups::get_or_create`] once per process, on shard 0 before the shards open their disks:
//!   the scheduling groups of the three classes.
//! - [`ShardDisk::open`] (the files under a data directory), or [`ShardDisk::mem`] (a [`MemDisk`]
//!   or any other model of a disk, [`ShardDisk::model`],
//!   for `teseladb dev` and the tests), with the same contract.
//! - [`ShardDisk::submit`]`(op, class)` for each `(op, class)` that `CoreLogic::poll_io` gives; it
//!   never blocks and never runs the operation inline.
//! - [`ShardDisk::ready`] resolves once a result is waiting; [`ShardDisk::take_done`] hands every
//!   waiting `(IoDone, IoClass)`, to feed to `CoreLogic::on_io_done` in that order.
//! - [`ShardDisk::idle`] resolves once nothing submitted is queued or running (a stop that waits
//!   for the logic's writes); [`ShardDisk::pending`] counts what is.
//! - [`ShardDisk::call`] runs one operation and resolves with its result, for code that waits for
//!   each (the device log's writer). The [`Disk`] trait runs on it synchronously, for recovery and
//!   the tools, and only inside an engine thread (`thread::async_`), as it waits with `get()`.
//! - [`ShardDisk::read_for_send`] reads the bytes of an open file that end an answer to another
//!   node (012 FR-026, 023 FR-017).
//! - [`ShardDisk`] implements `CoreDisk`, the interface of the core's task (`engine::driver`):
//!   `attach` gives it the task's waker, which it calls when results arrive.
//! - [`DiskPlace`] says where a node's disks are.
//!
//! # Order of the results
//!
//! The engine completes operations in any order. Within each class the results come back in the
//! order the operations were submitted: one that finishes early waits for the earlier ones of its
//! class (the `CoreLogic` contract). Two kinds of operation are answered as soon as they end, as the
//! logic matches them by value ([`crate::answered_apart`]): the sync of a log's batch and a log's
//! maintenance, which ran on threads of their own before the engine; no other result waits for
//! them. A log's maintenance runs in the background's group, so no batch of the log waits for it
//! (023 FR-013). When an operation may start is [`rules`]'s.

mod groups;
mod ops;
mod rules;

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Instant;

use teseladb::engine::core::api::recursive_touch_directory;
use teseladb::engine::core::file::{DirectoryEntry, OpenFlags, open_directory, open_file_dma};
use teseladb::engine::core::file_types::FilePermissions;
use teseladb::engine::core::future::{ExceptionPtr, Future, Promise, make_ready_future};
use teseladb::engine::core::temporary_buffer::TemporaryBuffer;
use teseladb::engine::core::with_scheduling_group::with_scheduling_group;
use teseladb_raftlog::{Disk, FileId, IoDone, IoErrno, IoOp, MemDisk};

pub use super::place::{DiskMaker, DiskPlace, StartupDisk};
pub use groups::{BACKGROUND_SHARES, DATA_SHARES, IoGroups, LOG_SHARES};
pub use rules::MAX_SYNCS;

use super::disk_port::{CoreDisk, DiskWaker};
use crate::IoClass;
use crate::io_class::CLASSES;
use ops::{Effect, Finished, OpenFile};
use rules::Shape;

/// Reads and writes of the data's and the background's classes running at once on a shard: what
/// they copy into the engine's buffers stays bounded, and the log's class, which has no such
/// bound, always finds room.
pub const DATA_IN_FLIGHT: usize = 24;

/// Where a shard's operations run.
enum Backend {
    /// The engine's files under a directory.
    Files {
        /// The directory the operations' paths are relative to.
        root: Rc<PathBuf>,
        /// The groups of the classes.
        groups: IoGroups,
        /// The open files.
        files: HashMap<FileId, OpenFile>,
        /// The length of each open file by device and inode: one file open under two
        /// identifiers has one length.
        sizes: HashMap<(u64, u64), u64>,
    },
    /// A disk in memory or another model of one (a disk that fails on purpose in the tests):
    /// each operation runs when it is handed out, in submission order.
    Mem(Box<dyn Disk + Send>),
}

/// An operation waiting to start.
struct Queued {
    seq: u64,
    op: IoOp,
    class: IoClass,
    queued: Instant,
}

/// A result's place in its class's order.
struct Slot {
    seq: u64,
    result: Option<IoDone>,
}

/// Who gets a result.
enum Reply {
    /// The core's queue of results.
    Core,
    /// A caller of [`ShardDisk::call`].
    Caller(Promise<IoDone>),
}

/// The state of a shard's executor.
struct Inner {
    backend: Backend,
    queues: [VecDeque<Queued>; CLASSES],
    /// What runs: sequence number, class and shape.
    running: Vec<(u64, IoClass, Shape)>,
    /// Each class's results in submission order, but those answered apart.
    order: [VecDeque<Slot>; CLASSES],
    /// Who gets each result, by sequence number.
    replies: HashMap<u64, Reply>,
    /// Results for the core, in the order it takes them.
    done: VecDeque<(IoDone, IoClass)>,
    /// Waiters of [`ShardDisk::ready`] and [`ShardDisk::idle`].
    ready_waiters: Vec<Promise<()>>,
    idle_waiters: Vec<Promise<()>>,
    /// Promises to keep once the state is no longer borrowed.
    to_fire: Vec<(Promise<IoDone>, IoDone)>,
    next_seq: u64,
    /// The core's task, woken when results for it arrive ([`CoreDisk::attach`]).
    waker: Option<DiskWaker>,
    /// Whether results arrived since the task was last woken.
    to_wake: bool,
    /// The class of the operations of the synchronous [`Disk`].
    sync_class: IoClass,
    /// Whether a turn of [`drive`] is on the stack: a result that comes inline waits for it.
    driving: bool,
}

/// The executor of one shard's disk operations. Cloning gives another handle to the same one.
#[derive(Clone)]
pub struct ShardDisk {
    inner: Rc<RefCell<Inner>>,
}

impl std::fmt::Debug for ShardDisk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShardDisk").field("pending", &self.pending()).finish()
    }
}

impl ShardDisk {
    fn with_backend(backend: Backend) -> ShardDisk {
        let inner = Inner {
            backend,
            queues: Default::default(),
            running: Vec::new(),
            order: Default::default(),
            replies: HashMap::new(),
            done: VecDeque::new(),
            ready_waiters: Vec::new(),
            idle_waiters: Vec::new(),
            to_fire: Vec::new(),
            next_seq: 0,
            waker: None,
            to_wake: false,
            sync_class: IoClass::Data,
            driving: false,
        };
        ShardDisk { inner: Rc::new(RefCell::new(inner)) }
    }

    /// The executor of the files under `root` (made if missing), with the classes in `groups`.
    pub fn open(root: impl Into<PathBuf>, groups: IoGroups) -> Future<ShardDisk> {
        let root = root.into();
        let dir = root.to_string_lossy().into_owned();
        recursive_touch_directory(&dir, FilePermissions::DEFAULT_DIR_PERMISSIONS).then(move |()| {
            let files = HashMap::new();
            let sizes = HashMap::new();
            let backend = Backend::Files { root: Rc::new(root), groups, files, sizes };
            Ok::<_, ExceptionPtr>(ShardDisk::with_backend(backend))
        })
    }

    /// The executor of a disk in memory.
    #[must_use]
    pub fn mem(disk: MemDisk) -> ShardDisk {
        ShardDisk::model(Box::new(disk))
    }

    /// The executor of a model of a disk, with the contract of [`ShardDisk::mem`].
    #[must_use]
    pub fn model(disk: Box<dyn Disk + Send>) -> ShardDisk {
        ShardDisk::with_backend(Backend::Mem(disk))
    }

    /// The model of a disk it ran on, once nothing else holds the executor and nothing runs;
    /// `None` for the files.
    #[must_use]
    pub fn into_model(self) -> Option<Box<dyn Disk + Send>> {
        let inner = Rc::try_unwrap(self.inner).ok()?.into_inner();
        match inner.backend {
            Backend::Mem(m) => Some(m),
            Backend::Files { .. } => None,
        }
    }

    /// Runs the operations of the synchronous [`Disk`] in `class` (the data's unless set): the
    /// device log's writer runs its rarer steps in the log's.
    pub fn set_sync_class(&self, class: IoClass) {
        self.inner.borrow_mut().sync_class = class;
    }

    /// The directory of the files, `None` for a disk in memory.
    #[must_use]
    pub fn root(&self) -> Option<PathBuf> {
        match &self.inner.borrow().backend {
            Backend::Files { root, .. } => Some(root.as_ref().clone()),
            Backend::Mem(_) => None,
        }
    }

    /// Queues `op` behind the ones of `class` already submitted; its result comes from
    /// [`ShardDisk::take_done`].
    pub fn submit(&self, op: IoOp, class: IoClass) {
        self.push(op, class, Reply::Core);
    }

    /// Runs `op` in `class` and resolves with its result, which [`ShardDisk::take_done`] does not
    /// give.
    pub fn call(&self, op: IoOp, class: IoClass) -> Future<IoDone> {
        let mut p = Promise::new();
        let f = p.get_future();
        self.push(op, class, Reply::Caller(p));
        f
    }

    fn push(&self, op: IoOp, class: IoClass, reply: Reply) {
        {
            let mut i = self.inner.borrow_mut();
            let seq = i.next_seq;
            i.next_seq += 1;
            if !crate::answered_apart(&op) {
                i.order[class.index()].push_back(Slot { seq, result: None });
            }
            i.replies.insert(seq, reply);
            i.queues[class.index()].push_back(Queued { seq, op, class, queued: Instant::now() });
            if i.driving {
                return;
            }
        }
        drive(&self.inner);
    }

    /// Appends every result waiting to `out`, with its class.
    pub fn take_done(&self, out: &mut Vec<(IoDone, IoClass)>) {
        out.extend(self.inner.borrow_mut().done.drain(..));
    }

    /// Resolves once a result is waiting for [`ShardDisk::take_done`].
    pub fn ready(&self) -> Future<()> {
        let mut i = self.inner.borrow_mut();
        if !i.done.is_empty() {
            return make_ready_future(());
        }
        let mut p = Promise::new();
        let f = p.get_future();
        i.ready_waiters.push(p);
        f
    }

    /// Operations submitted whose result has not been handed out.
    #[must_use]
    pub fn pending(&self) -> usize {
        let i = self.inner.borrow();
        i.replies.len() + i.done.len()
    }

    /// Resolves once no operation is queued or running.
    pub fn idle(&self) -> Future<()> {
        let mut i = self.inner.borrow_mut();
        if i.replies.is_empty() {
            return make_ready_future(());
        }
        let mut p = Promise::new();
        let f = p.get_future();
        i.idle_waiters.push(p);
        f
    }

    /// Reads `len` bytes at `offset` of the open file `file`, for an answer that ends with them;
    /// `None` when the file is not open or holds fewer bytes there, so the answer falls back to
    /// its other body. The read runs in the data's class, apart from the core's operations.
    pub fn read_for_send(
        &self,
        file: FileId,
        offset: u64,
        len: u32,
    ) -> Future<Option<TemporaryBuffer<u8>>> {
        let found = match &self.inner.borrow().backend {
            Backend::Files { files, groups, .. } => {
                files.get(&file).map(|f| (f.file.clone(), groups.of(IoClass::Data)))
            }
            Backend::Mem(_) => None,
        };
        let Some((mut handle, group)) = found else { return make_ready_future(None) };
        let want = len as usize;
        with_scheduling_group(group, move || teseladb_sys::dma::read(&mut handle, offset, want))
            .then_wrapped(move |f: Future<TemporaryBuffer<u8>>| {
                Ok::<_, ExceptionPtr>(
                    f.get().ok().filter(|b| b.size() >= want).map(|b| b.prefix(want)),
                )
            })
    }
}

/// Runs what can run, until nothing more can, keeping each promise outside the borrow of the
/// state (a continuation may run inline and submit again).
fn drive(inner: &Rc<RefCell<Inner>>) {
    loop {
        let (launch, fire, ready, idle, wake) = {
            let mut i = inner.borrow_mut();
            i.driving = true;
            let launch = i.startable();
            let wake = if std::mem::take(&mut i.to_wake) { i.waker.clone() } else { None };
            let fire = std::mem::take(&mut i.to_fire);
            let ready =
                if i.done.is_empty() { Vec::new() } else { std::mem::take(&mut i.ready_waiters) };
            let idle =
                if i.replies.is_empty() { std::mem::take(&mut i.idle_waiters) } else { Vec::new() };
            let quiet = launch.is_empty() && fire.is_empty() && ready.is_empty() && idle.is_empty();
            if quiet && wake.is_none() {
                i.driving = false;
                return;
            }
            (launch, fire, ready, idle, wake)
        };
        if let Some(w) = wake {
            w.wake();
        }
        for (mut p, d) in fire {
            p.set_value(d);
        }
        for mut p in ready.into_iter().chain(idle) {
            p.set_value(());
        }
        for l in launch {
            launch_one(inner, l);
        }
    }
}

/// An operation the state handed out to start on the engine.
struct Launch {
    seq: u64,
    op: IoOp,
    class: IoClass,
    root: Rc<PathBuf>,
    file: Option<OpenFile>,
    group: teseladb::engine::core::scheduling::SchedulingGroup,
}

fn launch_one(inner: &Rc<RefCell<Inner>>, l: Launch) {
    let Launch { seq, op, class, root, file, group } = l;
    let rc = inner.clone();
    // The operation's future never fails; the engine failing to queue its task (out of memory)
    // is the operation's failure.
    let failed = ops::fail(&op, IoErrno::Io);
    let _ = with_scheduling_group(group, move || ops::start(root, op, file)).then_wrapped(
        move |f: Future<Finished>| {
            let fin = f.get().unwrap_or(Finished { done: failed, effect: Effect::None });
            let inline = {
                let mut i = rc.borrow_mut();
                i.finish(seq, class, fin);
                i.driving
            };
            if !inline {
                drive(&rc);
            }
        },
    );
}

/// Whether `op` is a write the executor reads, patches and writes back as whole blocks: one of
/// other bytes than whole blocks to a file open with `O_DIRECT`.
fn patched(files: &HashMap<FileId, OpenFile>, op: &IoOp) -> bool {
    matches!(op, IoOp::WriteAt { file, offset, data, .. }
        if !data.is_empty()
            && !ops::is_aligned(*offset, data.len() as u64)
            && files.get(file).is_some_and(|f| f.direct))
}

/// The open file `op` works on, if it works on one.
fn file_of(op: &IoOp) -> Option<FileId> {
    match op {
        IoOp::WriteAt { file, .. }
        | IoOp::ReadAt { file, .. }
        | IoOp::Sync { file, .. }
        | IoOp::Truncate { file, .. }
        | IoOp::Close { file } => Some(*file),
        _ => None,
    }
}

impl Inner {
    /// Whether two identifiers name one file, by identifier or by inode.
    fn same_file(files: &HashMap<FileId, OpenFile>, a: FileId, b: FileId) -> bool {
        a == b || matches!((files.get(&a), files.get(&b)), (Some(x), Some(y)) if x.inode == y.inode)
    }

    /// Takes out of the queues what may start now, the log's class first.
    fn startable(&mut self) -> Vec<Launch> {
        let mut out = Vec::new();
        if let Backend::Mem(_) = self.backend {
            self.run_in_memory();
            return out;
        }
        // An operation that ends at once (on a file not open) may let one of an earlier class
        // start: the classes are looked at again until none moves.
        loop {
            let mut moved = false;
            for class in [IoClass::Log, IoClass::Data, IoClass::Background] {
                while let Some(l) = self.start_front(class) {
                    moved = true;
                    out.extend(l);
                }
            }
            if !moved {
                return out;
            }
        }
    }

    /// Runs every queued operation on the disk in memory, in submission order.
    fn run_in_memory(&mut self) {
        loop {
            let next =
                (0..CLASSES).filter_map(|c| self.queues[c].front().map(|q| (q.seq, c))).min();
            let Some((_, c)) = next else { return };
            let q = self.queues[c].pop_front().expect("the front just seen");
            let Backend::Mem(disk) = &mut self.backend else { unreachable!("a disk in memory") };
            let done = disk.execute(q.op);
            self.finish(q.seq, q.class, Finished { done, effect: Effect::None });
        }
    }

    /// Starts the front of `class`'s queue if it may start: `None` if it may not, `Some(None)` if
    /// it finished at once (a file not open), `Some(Some(l))` to launch.
    fn start_front(&mut self, class: IoClass) -> Option<Option<Launch>> {
        let Backend::Files { files, .. } = &self.backend else { return None };
        let q = self.queues[class.index()].front()?;
        let named = file_of(&q.op);
        let shape_of = |op: &IoOp| Shape::of(op, patched(files, op));
        let shape = shape_of(&q.op);
        let data_class = class != IoClass::Log;
        if data_class
            && matches!(shape, Shape::Access { .. })
            && self
                .running
                .iter()
                .filter(|(_, c, s)| *c != IoClass::Log && matches!(s, Shape::Access { .. }))
                .count()
                >= DATA_IN_FLIGHT
        {
            return None;
        }
        // The data's class and the background's share files: an operation of one also waits for
        // what it conflicts with in the other, running or submitted before it.
        let shared = |c: IoClass| c == class || (data_class && c != IoClass::Log);
        let seq = q.seq;
        let other = match class {
            IoClass::Log => None,
            IoClass::Data => Some(IoClass::Background),
            IoClass::Background => Some(IoClass::Data),
        };
        let mut earlier: Vec<Shape> = Vec::new();
        if let Some(o) = other {
            for e in self.queues[o.index()].iter().take_while(|e| e.seq < seq) {
                earlier.push(shape_of(&e.op));
            }
        }
        let running = self.running.iter().filter(|(_, c, _)| shared(*c)).map(|(_, _, s)| *s);
        let same = |a: FileId, b: FileId| Inner::same_file(files, a, b);
        if !rules::may_start(running.chain(earlier), shape, &same) {
            return None;
        }
        if named.is_some_and(|f| !files.contains_key(&f)) {
            // A file that is not open: its result is ready, in its place in the order.
            let q = self.queues[class.index()].pop_front().expect("the front just seen");
            let done = ops::fail(&q.op, IoErrno::BadFile);
            self.finish(q.seq, class, Finished { done, effect: Effect::None });
            return Some(None);
        }
        let q = self.queues[class.index()].pop_front().expect("the front just seen");
        let waited = u64::try_from(q.queued.elapsed().as_nanos()).unwrap_or(u64::MAX);
        teseladb_stages::record(teseladb_stages::Stage::DiskQueueWait, waited);
        let Backend::Files { root, groups, files, sizes } = &mut self.backend else {
            unreachable!("the files' backend")
        };
        let file = match &q.op {
            IoOp::Close { file } => files.remove(file),
            op => file_of(op).and_then(|f| files.get(&f).cloned()).map(|mut f| {
                f.size = sizes.get(&f.inode).copied().unwrap_or(f.size);
                f
            }),
        };
        let group =
            if shape == Shape::Free { groups.of(IoClass::Background) } else { groups.of(class) };
        self.running.push((q.seq, class, shape));
        Some(Some(Launch { seq: q.seq, op: q.op, class, root: root.clone(), file, group }))
    }

    /// Records the end of operation `seq` of `class` and hands out what it lets go.
    fn finish(&mut self, seq: u64, class: IoClass, fin: Finished) {
        if let Some(at) = self.running.iter().position(|(s, _, _)| *s == seq) {
            self.running.swap_remove(at);
        }
        if let Backend::Files { files, sizes, .. } = &mut self.backend {
            match fin.effect {
                Effect::None => {}
                Effect::Opened(id, f) => {
                    sizes.insert(f.inode, f.size);
                    files.insert(id, f);
                }
                Effect::Grew(id, end) => {
                    if let Some(f) = files.get(&id) {
                        let size = sizes.entry(f.inode).or_insert(end);
                        *size = (*size).max(end);
                    }
                }
                Effect::Resized(id, len) => {
                    if let Some(f) = files.get(&id) {
                        sizes.insert(f.inode, len);
                    }
                }
            }
        }
        match self.order[class.index()].iter_mut().find(|s| s.seq == seq) {
            Some(slot) => slot.result = Some(fin.done),
            // Answered apart: it goes now.
            None => self.hand_out(seq, class, fin.done),
        }
        while self.order[class.index()].front().is_some_and(|s| s.result.is_some()) {
            let slot = self.order[class.index()].pop_front().expect("the front just seen");
            let done = slot.result.expect("a finished slot");
            self.hand_out(slot.seq, class, done);
        }
    }

    fn hand_out(&mut self, seq: u64, class: IoClass, done: IoDone) {
        match self.replies.remove(&seq) {
            Some(Reply::Caller(p)) => self.to_fire.push((p, done)),
            Some(Reply::Core) | None => {
                self.done.push_back((done, class));
                self.to_wake = true;
            }
        }
    }
}

impl ShardDisk {
    /// The files of directory `dir` (full paths under the root, sorted), on the engine; waits in
    /// the calling engine thread.
    fn list_files(root: &Path, dir: &Path) -> Result<Vec<PathBuf>, ExceptionPtr> {
        let full = root.join(dir).to_string_lossy().into_owned();
        let base = dir.to_path_buf();
        let mut d = open_directory(&full).get()?;
        let names: Rc<RefCell<Vec<PathBuf>>> = Rc::default();
        let got = names.clone();
        let mut sub = d.list_directory(move |e: DirectoryEntry| {
            got.borrow_mut().push(base.join(e.name.as_str()));
            make_ready_future(())
        });
        // The listing and the directory live until the listing is done.
        let listed = sub.done().get();
        drop(sub);
        d.close().get()?;
        listed?;
        let mut v = std::mem::take(&mut *names.borrow_mut());
        v.sort();
        Ok(v)
    }

    /// The whole contents of file `path` under `root`, on the engine; waits in the calling engine
    /// thread.
    fn read_whole(root: &Path, path: &Path) -> Result<Vec<u8>, ExceptionPtr> {
        let full = root.join(path).to_string_lossy().into_owned();
        let mut f = open_file_dma(&full, OpenFlags::RO).get()?;
        let size = f.size().get()?;
        let read = if size == 0 {
            Ok(Vec::new())
        } else {
            teseladb_sys::dma::read(&mut f, 0, size as usize)
                .get()
                .map(|b| b.as_slice()[..b.size().min(size as usize)].to_vec())
        };
        f.close().get()?;
        read
    }
}

/// Synchronous access for recovery and the tools. Each call waits with `get()`, so it runs only
/// inside an engine thread (`thread::async_`) of the shard that owns the executor.
impl Disk for ShardDisk {
    fn list(&mut self, dir: &Path) -> Result<Vec<PathBuf>, IoErrno> {
        let root = match &mut self.inner.borrow_mut().backend {
            Backend::Mem(d) => return d.list(dir),
            Backend::Files { root, .. } => root.clone(),
        };
        ShardDisk::list_files(&root, dir).map_err(|e| ops::errno_of(&e))
    }

    fn read_all(&mut self, path: &Path) -> Result<Vec<u8>, IoErrno> {
        let root = match &mut self.inner.borrow_mut().backend {
            Backend::Mem(d) => return d.read_all(path),
            Backend::Files { root, .. } => root.clone(),
        };
        ShardDisk::read_whole(&root, path).map_err(|e| ops::errno_of(&e))
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        let fallback = ops::fail(&op, IoErrno::Io);
        let class = self.inner.borrow().sync_class;
        self.call(op, class).get().unwrap_or(fallback)
    }
}

/// The executor of a core's task (023 FR-010): each operation starts as soon as the rules let it,
/// so a turn has nothing left to send; the engine hands what started to the kernel once per poll.
impl CoreDisk for ShardDisk {
    fn attach(&mut self, waker: DiskWaker) {
        let mut i = self.inner.borrow_mut();
        i.to_wake |= !i.done.is_empty();
        i.waker = Some(waker);
    }

    fn submit(&mut self, op: IoOp, class: IoClass) -> bool {
        ShardDisk::submit(self, op, class);
        true
    }

    fn flush(&mut self) -> bool {
        true
    }

    fn reap(&mut self, out: &mut Vec<(IoDone, IoClass)>) {
        self.take_done(out);
    }

    fn ready(&self) -> bool {
        !self.inner.borrow().done.is_empty()
    }

    fn alive(&self) -> bool {
        true
    }

    fn sends_files(&self) -> bool {
        true
    }

    fn read_for_send(
        &self,
        file: FileId,
        offset: u64,
        len: u32,
    ) -> Option<Future<Option<TemporaryBuffer<u8>>>> {
        Some(ShardDisk::read_for_send(self, file, offset, len))
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// The state of an executor with operations `ops` handed out, each in its class, none
    /// finished.
    fn handed_out(ops: &[(IoOp, IoClass)]) -> Inner {
        let disk = ShardDisk::mem(MemDisk::new());
        let mut i = Rc::try_unwrap(disk.inner).ok().expect("one handle").into_inner();
        for (seq, (op, class)) in ops.iter().enumerate() {
            let seq = seq as u64;
            if !crate::answered_apart(op) {
                i.order[class.index()].push_back(Slot { seq, result: None });
            }
            i.replies.insert(seq, Reply::Core);
            i.running.push((seq, *class, Shape::of(op, false)));
        }
        i
    }

    fn sync(file: u32, batch: u64) -> IoOp {
        IoOp::Sync { file: FileId(file), batch }
    }

    fn end(i: &mut Inner, seq: u64, class: IoClass, op: &IoOp) {
        i.finish(seq, class, Finished { done: IoDone::Ok(op.clone()), effect: Effect::None });
    }

    fn taken(i: &mut Inner) -> Vec<(IoDone, IoClass)> {
        i.done.drain(..).collect()
    }

    #[test]
    fn a_result_that_ends_early_waits_for_the_earlier_ones_of_its_class() {
        let (a, b, c, l) = (sync(1, 0), sync(2, 0), sync(3, 0), sync(4, 0));
        let mut i = handed_out(&[
            (a.clone(), IoClass::Data),
            (b.clone(), IoClass::Data),
            (c.clone(), IoClass::Data),
            (l.clone(), IoClass::Log),
        ]);
        end(&mut i, 2, IoClass::Data, &c);
        assert!(taken(&mut i).is_empty(), "the third waits for the first two");
        // Another class does not wait for this one.
        end(&mut i, 3, IoClass::Log, &l);
        assert_eq!(taken(&mut i), [(IoDone::Ok(l), IoClass::Log)]);
        end(&mut i, 0, IoClass::Data, &a);
        assert_eq!(taken(&mut i), [(IoDone::Ok(a), IoClass::Data)]);
        end(&mut i, 1, IoClass::Data, &b);
        assert_eq!(taken(&mut i), [(IoDone::Ok(b), IoClass::Data), (IoDone::Ok(c), IoClass::Data)]);
        assert!(i.running.is_empty() && i.replies.is_empty());
        assert!(i.order.iter().all(VecDeque::is_empty));
    }

    #[test]
    fn a_batch_sync_and_a_log_chore_go_as_they_end_and_hold_nothing() {
        let first = sync(1, 0);
        let batch = sync(2, 7);
        let chore = IoOp::SyncDir { path: "core-00/wal".into() };
        let after = sync(3, 0);
        let mut i = handed_out(&[
            (first.clone(), IoClass::Log),
            (batch.clone(), IoClass::Log),
            (chore.clone(), IoClass::Log),
            (after.clone(), IoClass::Log),
        ]);
        end(&mut i, 2, IoClass::Log, &chore);
        assert_eq!(taken(&mut i), [(IoDone::Ok(chore), IoClass::Log)], "before the first");
        end(&mut i, 0, IoClass::Log, &first);
        end(&mut i, 3, IoClass::Log, &after);
        // The batch's sync still runs: what follows it does not wait.
        assert_eq!(
            taken(&mut i),
            [(IoDone::Ok(first), IoClass::Log), (IoDone::Ok(after), IoClass::Log)]
        );
        end(&mut i, 1, IoClass::Log, &batch);
        assert_eq!(taken(&mut i), [(IoDone::Ok(batch), IoClass::Log)]);
    }

    #[test]
    fn the_effects_of_an_operation_change_the_table_of_files_only_for_files() {
        // A disk in memory has no table: an effect leaves it as it is.
        let op = sync(1, 0);
        let mut i = handed_out(&[(op.clone(), IoClass::Data)]);
        i.finish(
            0,
            IoClass::Data,
            Finished { done: IoDone::Ok(op.clone()), effect: Effect::Grew(FileId(1), 9) },
        );
        assert_eq!(taken(&mut i), [(IoDone::Ok(op), IoClass::Data)]);
    }
}
