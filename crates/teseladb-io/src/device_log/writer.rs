//! The writer of a device log: one thread per data directory that takes what every core queued,
//! writes it as one record and makes it durable with one sync, then answers each core's syncs.
//! Requests that arrive while a sync runs go in the next one. The queues are lock-free rings of
//! one producer (the core) and one consumer (the writer), and the writer sleeps parked until a core
//! queues something (012 FR-008).

use std::collections::BTreeSet;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle, Thread};
use std::time::Duration;

use teseladb_raftlog::{Disk, IoDone, IoErrno, IoOp};
use teseladb_types::Bytes;

use super::port::LogPort;
use super::state::LogState;
use crate::mailbox::Notify;
use crate::spsc::{self, SpscReceiver};

/// Requests each core can queue before they wait in its port.
const QUEUE: usize = 1024;
/// Bytes waiting that the writer writes without waiting for a sync.
const WRITE_AHEAD: usize = 1 << 20;
/// How long the writer sleeps with nothing to do (it is woken by a request before that).
const IDLE: Duration = Duration::from_millis(100);
/// How long it waits before trying again on a full disk.
const DISK_FULL_RETRY: Duration = Duration::from_secs(1);

/// Where the writer hands a core's answers.
pub type Sink = Box<dyn FnMut(IoDone) -> bool + Send>;

/// What a core asks of the writer.
pub enum Req {
    Create {
        wal: u64,
        len: u64,
    },
    Write {
        wal: u64,
        offset: u64,
        data: Bytes,
    },
    Truncate {
        wal: u64,
        len: u64,
    },
    Remove {
        wal: u64,
    },
    /// Answered once everything before it is durable.
    Sync(IoOp),
    Read {
        wal: u64,
        reply: mpsc::Sender<Result<Vec<u8>, IoErrno>>,
    },
    /// The core recovered; its answers go to this sink from now on.
    Attach(Sink),
}

/// What the writer and the ports share: atomics only.
#[derive(Debug)]
#[repr(align(64))]
pub struct Shared {
    thread: OnceLock<Thread>,
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
        if let Some(t) = self.thread.get() {
            t.unpark();
        }
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

impl<D: Disk + 'static> Disk for LogClass<'_, D> {
    fn list(&mut self, dir: &std::path::Path) -> Result<Vec<std::path::PathBuf>, IoErrno> {
        self.0.list(dir)
    }

    fn read_all(&mut self, path: &std::path::Path) -> Result<Vec<u8>, IoErrno> {
        self.0.read_all(path)
    }

    fn execute(&mut self, op: IoOp) -> IoDone {
        #[cfg(target_os = "linux")]
        if let Some(u) = (self.0 as &mut dyn std::any::Any)
            .downcast_mut::<crate::NodeDisk>()
            .and_then(crate::NodeDisk::uring)
        {
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
        let shared = Arc::new(Shared {
            thread: OnceLock::new(),
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
            run(disk, state, queues, &s)
        })?;
        let _ = shared.thread.set(handle.thread().clone());
        shared.notify();
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
        if let Some(h) = self.handle.take() {
            h.thread().unpark();
            let _ = h.join();
        }
    }
}

/// The writer's loop.
fn run<D: Disk + 'static>(
    mut disk: D,
    mut state: LogState,
    mut queues: Vec<(SpscReceiver<Req>, Sink)>,
    shared: &Shared,
) -> D {
    let mut reqs = Vec::new();
    let mut recovering = queues.len();
    let mut woke_by_timer = false;
    if queues.is_empty() {
        state.allow_tidying();
    }
    loop {
        let mut took = false;
        for (core, (rx, sink)) in queues.iter_mut().enumerate() {
            rx.drain_into(&mut reqs);
            let core = u16::try_from(core).expect("a node has fewer than 65,536 cores");
            for req in reqs.drain(..) {
                took = true;
                let disk = &mut LogClass(&mut disk);
                match req {
                    Req::Create { wal, len } => state.create(core, wal, len),
                    Req::Write { wal, offset, data } => state.write(core, wal, offset, &data),
                    Req::Truncate { wal, len } => state.truncate(core, wal, len),
                    Req::Remove { wal } => state.remove(core, wal),
                    Req::Sync(op) => state.sync(core, op),
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
        // A full disk is tried again after its wait, or when something new comes.
        let due = state.has_waiting() || state.buffered() >= WRITE_AHEAD;
        let flushed = if due && (took || !state.disk_full() || woke_by_timer) {
            let answers = state.flush(&mut LogClass(&mut disk));
            shared.disk_full.store(state.disk_full(), Ordering::Release);
            shared.syncs.store(state.syncs(), Ordering::Relaxed);
            for (core, done) in answers {
                let _ = (queues[usize::from(core)].1)(done);
            }
            !state.disk_full()
        } else {
            false
        };
        if took || flushed {
            continue;
        }
        if shared.stop.load(Ordering::Acquire) && queues.iter().all(|(rx, _)| rx.is_empty()) {
            break;
        }
        let wait = if state.disk_full() { DISK_FULL_RETRY } else { IDLE };
        let before = std::time::Instant::now();
        thread::park_timeout(wait);
        woke_by_timer = before.elapsed() >= wait;
    }
    // What was written without a sync (a removal, the copy of old files) is made durable.
    state.sync(0, IoOp::SyncDir { path: super::format::DIR.into() });
    let _ = state.flush(&mut LogClass(&mut disk));
    disk
}
