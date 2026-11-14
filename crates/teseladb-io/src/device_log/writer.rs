//! The writer of a device log: one thread per data directory that takes what every core queued,
//! writes it as one record and makes it durable with one sync, then answers each core's syncs.
//! It works in a pipeline (012 FR-035): with io_uring its operations go to the ring and it goes on
//! while they run, so what arrives during a sync is written at once and its group's sync starts as
//! soon as that one ends; each group's cores are answered the moment its sync ends. On another
//! disk each operation runs when handed out. The queues are lock-free rings of one producer (the
//! core) and one consumer (the writer), and the writer sleeps in its poller until a core queues
//! something or the ring completes an operation (012 FR-008).

use std::collections::{BTreeSet, VecDeque};
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use teseladb_raftlog::{Disk, IoDone, IoErrno, IoOp};

use teseladb_devlog::port::LogReq;
use teseladb_devlog::state::LogState;

use super::port::LogPort;
use crate::mailbox::Notify;
use crate::poller::{Poller, Waker};
use crate::spsc::{self, SpscReceiver};
use teseladb_stages::Stage;

/// Requests each core can queue before they wait in its port.
const QUEUE: usize = 1024;
/// The poller's token of the ring's completions.
#[cfg(target_os = "linux")]
const RING_TOKEN: u64 = 1;
/// How long the writer sleeps with nothing to do (it is woken by a request before that).
const IDLE: Duration = Duration::from_millis(100);
/// How long it waits before trying again on a full disk.
const DISK_FULL_RETRY: Duration = Duration::from_secs(1);

/// Where the writer hands a core's answers.
pub type Sink = Box<dyn FnMut(IoDone) -> bool + Send>;

/// What a core asks of the writer.
pub enum Req {
    /// A request for the log, with when it was asked if it is a sync.
    Log(LogReq, Option<Instant>),
    /// The contents of a WAL segment.
    Read { wal: u64, reply: mpsc::Sender<Result<Vec<u8>, IoErrno>> },
    /// The core recovered; its answers go to this sink from now on.
    Attach(Sink),
}

/// What the writer and the ports share: atomics only.
#[derive(Debug)]
#[repr(align(64))]
pub struct Shared {
    waker: Waker,
    stop: AtomicBool,
    disk_full: AtomicBool,
    syncs: AtomicU64,
}

impl Shared {
    pub(crate) fn disk_full(&self) -> bool {
        self.disk_full.load(Ordering::Acquire)
    }
}

impl Notify for Shared {
    fn notify(&self) {
        // The only failure of a wake-up is a saturated counter, which already means "awake".
        let _ = self.waker.wake();
    }
}

/// Parameters of a device log.
#[derive(Clone, Copy, Debug)]
pub struct DeviceLogConfig {
    /// Size of each file of the log.
    pub file_bytes: u64,
}

impl Default for DeviceLogConfig {
    fn default() -> DeviceLogConfig {
        DeviceLogConfig { file_bytes: 64 << 20 }
    }
}

/// A running device log.
#[derive(Debug)]
pub struct DeviceLog<D> {
    shared: Arc<Shared>,
    recovered_files: usize,
    handle: Option<JoinHandle<D>>,
}

/// Executes the writer's operations in the log's class of the core's executor (012 FR-023).
struct LogClass<'a, D>(&'a mut D);

/// The io_uring executor of `disk`, if it has one.
#[cfg(target_os = "linux")]
fn uring<D: Disk + 'static>(disk: &mut D) -> Option<&mut crate::IoUringDisk> {
    (disk as &mut dyn std::any::Any)
        .downcast_mut::<crate::NodeDisk>()
        .and_then(crate::NodeDisk::uring)
}

impl<D: Disk + 'static> Disk for LogClass<'_, D> {
    fn list(&mut self, dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>, IoErrno> {
        self.0.list(dir)
    }

    fn read_all(&mut self, path: &std::path::Path) -> Result<Vec<u8>, IoErrno> {
        self.0.read_all(path)
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        #[cfg(target_os = "linux")]
        if let Some(u) = uring(self.0) {
            return u.execute_in(op, crate::IoClass::Log);
        }
        self.0.execute(op)
    }
}

impl<D: Disk + Send + 'static> DeviceLog<D> {
    /// Recovers the log of `disk` and starts its writer, with a port for each of `cores` cores.
    ///
    /// # Errors
    ///
    /// The error of reading the log, or of starting the thread.
    pub fn start(
        mut disk: D,
        cores: u16,
        cfg: DeviceLogConfig,
    ) -> io::Result<(DeviceLog<D>, Vec<LogPort>)> {
        let state = LogState::recover(&mut LogClass(&mut disk), cfg.file_bytes)
            .map_err(|e| io::Error::other(format!("raft log: {e:?}")))?;
        let poller = Poller::new()?;
        let shared = Arc::new(Shared {
            waker: poller.waker(),
            stop: AtomicBool::new(false),
            disk_full: AtomicBool::new(false),
            syncs: AtomicU64::new(0),
        });
        let mut ports = Vec::new();
        let mut queues = Vec::new();
        for core in 0..cores {
            let (tx, rx) = spsc::channel(QUEUE, shared.clone());
            let wals: BTreeSet<u64> = state.segments_of(core).into_iter().collect();
            let (port, sink) = LogPort::new(core, tx, wals, shared.clone());
            ports.push(port);
            queues.push((rx, sink));
        }
        let recovered_files = state.files();
        let s = shared.clone();
        let handle = thread::Builder::new().name("raft-log".into()).spawn(move || {
            // Every confirmed write waits for this thread's sync.
            let _ = teseladb_sys::favor_current_thread();
            run(disk, state, queues, &s, poller)
        })?;
        Ok((DeviceLog { shared, recovered_files, handle: Some(handle) }, ports))
    }

    /// Files of the log found when it started.
    #[must_use]
    pub fn recovered_files(&self) -> usize {
        self.recovered_files
    }

    /// Syncs of the log since it started.
    #[must_use]
    pub fn syncs(&self) -> u64 {
        self.shared.syncs.load(Ordering::Relaxed)
    }

    /// Stops the writer once every port is gone or idle, and returns its disk (`None` if the
    /// thread panicked).
    pub fn stop(mut self) -> Option<D> {
        self.halt()
    }

    fn halt(&mut self) -> Option<D> {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.notify();
        self.handle.take().and_then(|h| h.join().ok())
    }
}

impl<D> Drop for DeviceLog<D> {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.notify();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Nanoseconds since `at`, for a stage.
fn since(at: Instant) -> u64 {
    u64::try_from(at.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// The group whose sync runs, for its stages.
struct Group {
    started: Instant,
    /// Results of operations handed out before the group's, which come back first.
    before: usize,
    /// Whether it started with the write of a record.
    write: bool,
    wrote: Option<Instant>,
}

/// What the writer handed to its disk.
#[derive(Default)]
struct Flight {
    in_flight: usize,
    /// Results of a disk that runs each operation when handed out.
    ready: VecDeque<IoDone>,
    /// When the oldest sync waiting for the next group was taken.
    first_taken: Option<Instant>,
    group: Option<Group>,
}

impl Flight {
    fn submit<D: Disk + 'static>(&mut self, disk: &mut D, op: IoOp) {
        self.in_flight += 1;
        #[cfg(target_os = "linux")]
        if let Some(u) = uring(disk) {
            u.submit_in(op, crate::IoClass::Log);
            return;
        }
        self.ready.push_back(disk.execute(op));
    }

    /// The results back, in the order handed out. A failed look at the ring is tried again on
    /// the next turn.
    fn collect<D: Disk + 'static>(&mut self, disk: &mut D, out: &mut Vec<IoDone>) {
        #[cfg(target_os = "linux")]
        if let Some(u) = uring(disk) {
            let mut got = Vec::new();
            if u.reap(&mut got).is_ok() {
                out.extend(got.into_iter().map(|(d, _)| d));
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = disk;
        out.extend(self.ready.drain(..));
    }

    /// A sync of a core taken from its queue.
    fn taken(&mut self, asked: Instant) {
        let now = Instant::now();
        let waited = u64::try_from(now.saturating_duration_since(asked).as_nanos());
        teseladb_stages::record(Stage::LogQueueWait, waited.unwrap_or(u64::MAX));
        self.first_taken.get_or_insert(now);
    }

    /// The sync of a group starts with `ops`.
    fn group_started(&mut self, ops: &[IoOp]) {
        let now = Instant::now();
        if let Some(t) = self.first_taken.take() {
            teseladb_stages::record(Stage::LogTurnWait, since(t));
        }
        let write = matches!(ops.first(), Some(IoOp::WriteAt { .. }));
        self.group = Some(Group { started: now, before: self.in_flight, write, wrote: None });
    }

    /// A result back; `ended` if it ended the group's sync.
    fn on_result(&mut self, done: &IoDone, ended: bool) {
        self.in_flight -= 1;
        let Some(g) = self.group.as_mut() else { return };
        if g.before > 0 {
            g.before -= 1;
            return;
        }
        if g.write && g.wrote.is_none() {
            teseladb_stages::record(Stage::LogWrite, since(g.started));
            g.wrote = Some(Instant::now());
        } else if ended {
            teseladb_stages::record(Stage::LogSync, since(g.wrote.unwrap_or(g.started)));
        }
        if ended || matches!(done, IoDone::Err { .. }) {
            self.group = None;
        }
    }
}

/// Hands what was submitted to the ring to the kernel. A failed submission stays in the ring's
/// queue and goes with the next one.
fn flush_ring<D: Disk + 'static>(disk: &mut D) {
    #[cfg(target_os = "linux")]
    if let Some(u) = uring(disk) {
        let _ = u.flush();
    }
    #[cfg(not(target_os = "linux"))]
    let _ = disk;
}

/// Hands each core its answers.
fn answer(queues: &mut [(SpscReceiver<Req>, Sink)], answers: teseladb_devlog::state::Answers) {
    for (core, done) in answers {
        let _ = (queues[usize::from(core)].1)(done);
    }
}

/// The writer's loop.
fn run<D: Disk + 'static>(
    mut disk: D,
    mut state: LogState,
    mut queues: Vec<(SpscReceiver<Req>, Sink)>,
    shared: &Shared,
    mut poller: Poller,
) -> D {
    let mut reqs = Vec::new();
    let mut recovering = queues.len();
    let mut woke_by_timer = false;
    if queues.is_empty() {
        state.allow_tidying();
    }
    #[cfg(target_os = "linux")]
    if let Some(fd) = uring(&mut disk).map(|u| u.event_fd()) {
        // Without it the writer sees the ring's results on its next wake-up or timeout.
        let _ = poller.register(&fd, RING_TOKEN, crate::poller::Interest::READ);
    }
    let mut flight = Flight::default();
    let mut ops = Vec::new();
    let mut results = Vec::new();
    loop {
        let mut took = false;
        for (core, (rx, sink)) in queues.iter_mut().enumerate() {
            rx.drain_into(&mut reqs);
            let core = u16::try_from(core).expect("a node has fewer than 65,536 cores");
            for req in reqs.drain(..) {
                took = true;
                let disk = &mut LogClass(&mut disk);
                match req {
                    Req::Log(r, asked) => {
                        if let Some(at) = asked {
                            flight.taken(at);
                        }
                        r.apply(&mut state, core);
                    }
                    Req::Read { wal, reply } => {
                        let _ = reply.send(state.read(disk, core, wal));
                    }
                    Req::Attach(s) => {
                        *sink = s;
                        recovering -= 1;
                        if recovering == 0 {
                            state.allow_tidying();
                        }
                    }
                }
            }
        }
        let mut worked = took;
        flight.collect(&mut disk, &mut results);
        for done in results.drain(..) {
            worked = true;
            let before = state.syncs();
            let answers = state.on_done(&done);
            let ended = state.syncs() != before;
            if ended {
                crate::counters::record_log_sync();
            }
            flight.on_result(&done, ended);
            answer(&mut queues, answers);
        }
        // A full disk is tried again after its wait, or when something new comes.
        if took || !state.disk_full() || woke_by_timer {
            let starting = !state.syncing();
            let answers = state.next(&mut LogClass(&mut disk), &mut ops);
            if starting && state.syncing() {
                flight.group_started(&ops);
            }
            if !state.has_waiting() && !state.syncing() {
                flight.first_taken = None;
            }
            shared.disk_full.store(state.disk_full(), Ordering::Release);
            worked |= !answers.is_empty() || !ops.is_empty();
            answer(&mut queues, answers);
            for op in ops.drain(..) {
                flight.submit(&mut disk, op);
            }
            flush_ring(&mut disk);
        }
        shared.syncs.store(state.syncs(), Ordering::Relaxed);
        if worked {
            continue;
        }
        if shared.stop.load(Ordering::Acquire)
            && flight.in_flight == 0
            && queues.iter().all(|(rx, _)| rx.is_empty())
        {
            break;
        }
        let wait = if state.disk_full() { DISK_FULL_RETRY } else { IDLE };
        let before = Instant::now();
        // A failed wait is one that returned early: the loop looks again.
        let _ = poller.poll(Some(wait));
        woke_by_timer = before.elapsed() >= wait;
    }
    // What was written without a sync (a removal, the copy of old files) is made durable.
    state.sync(0, IoOp::SyncDir { path: super::format::DIR.into() });
    let before = state.syncs();
    let _ = state.flush(&mut LogClass(&mut disk));
    for _ in before..state.syncs() {
        crate::counters::record_log_sync();
    }
    disk
}
