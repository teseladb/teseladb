//! The writer of a device log (012 FR-035, 023 FR-013): a task of shard 0 that takes what every
//! core sent, writes it as one record and makes it durable with one sync, then answers each core's
//! syncs. It works in a pipeline: its operations go to shard 0's disk executor in the log's class
//! and it goes on while they run, so what arrives during a sync is written at once and its group's
//! sync starts as soon as that one ends; each group's cores are answered the moment its sync ends.
//! The cores' requests come in one message per turn of each core ([`super::LogPort::pump`]); the
//! answers go back to each core's shard. The writer runs in an engine thread of shard 0, as the
//! log's rarer steps (a new file, copying the oldest one forward, a core's read of its segment)
//! run on the executor synchronously.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use teseladb::engine::core::future::{ExceptionPtr, Promise, make_ready_future};
use teseladb::engine::core::thread::{ThreadAttributes, async_with_attributes as in_thread_with};
use teseladb::engine::core::timer::{Clock, SteadyClockType, SteadyTimePoint};
use teseladb::engine::core::with_timeout::with_timeout;
use teseladb_devlog::port::LogReq;
use teseladb_devlog::state::LogState;
use teseladb_raftlog::{Disk, IoDone, IoErrno, IoOp};
use teseladb_stages::Stage;

use super::bound::{DeviceLogConfig, LogBound, LogDisk, Sink};
use super::port::LogPort;
use crate::IoClass;
use crate::engine::NodeEngine;
use crate::engine::disk::{IoGroups, ShardDisk};
use crate::engine::post::ShardPost;

/// How long the writer sleeps with nothing to do (a request or a result wakes it before that).
const IDLE: Duration = Duration::from_millis(100);
/// How long it waits before trying again on a full disk.
const DISK_FULL_RETRY: Duration = Duration::from_secs(1);
/// The stack of the writer's engine thread. The engine's default (128 KiB, as the reference
/// engine's) is too small for the writer's frames in a debug build, which has no guard page
/// below it: the node aborted at the writer's first wait (a future's state found already set
/// in `set_urgent_state`), and does not with this stack. It has room in every build.
const WRITER_STACK: usize = 1 << 20;

/// What a core asks of the writer.
pub enum Req {
    /// A request for the log, with when it was asked if it is a sync.
    Log(LogReq, Option<Instant>),
    /// The contents of a WAL segment.
    Read { wal: u64, reply: mpsc::SyncSender<Result<Vec<u8>, IoErrno>> },
    /// The core recovered; its answers go to this sink from now on.
    Attach(Sink),
}

/// What the writer and the ports share: atomics only.
#[derive(Debug, Default)]
#[repr(align(64))]
pub struct Shared {
    stop: AtomicBool,
    disk_full: AtomicBool,
    syncs: AtomicU64,
}

impl Shared {
    pub(crate) fn disk_full(&self) -> bool {
        self.disk_full.load(Ordering::Acquire)
    }
}

/// What a stopped writer gives back: the model of a disk it ran on.
type Stopped = Option<Box<dyn Disk + Send>>;

/// What wakes the writer: a request, a result of its disk, or its stop.
#[derive(Default)]
struct Signal {
    set: Cell<bool>,
    waiter: RefCell<Option<Promise<()>>>,
}

impl Signal {
    fn notify(&self) {
        self.set.set(true);
        if let Some(mut p) = self.waiter.borrow_mut().take() {
            p.set_value(());
        }
    }

    /// Waits in the writer's engine thread until notified or `limit` passes; whether `limit`
    /// passed.
    fn wait(&self, limit: Duration) -> bool {
        if self.set.replace(false) {
            return false;
        }
        let mut p = Promise::new();
        let f = p.get_future();
        *self.waiter.borrow_mut() = Some(p);
        let nanos = i64::try_from(limit.as_nanos()).unwrap_or(i64::MAX);
        let until = SteadyTimePoint(SteadyClockType::now().0.saturating_add(nanos));
        let timed_out = with_timeout::<SteadyClockType, ()>(until, f).get().is_err();
        self.waiter.borrow_mut().take();
        self.set.set(false);
        timed_out
    }
}

/// A writer's mailbox on shard 0.
#[derive(Default)]
struct Inbox {
    reqs: RefCell<Vec<(u16, Req)>>,
    signal: Signal,
}

thread_local! {
    /// The mailboxes of the writers running on this shard, by identifier.
    static WRITERS: RefCell<HashMap<u64, Rc<Inbox>>> = RefCell::new(HashMap::new());
}

/// The identifiers of the writers.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Hands core `core`'s requests to writer `log` (on its shard).
pub(crate) fn deliver(log: u64, core: u16, batch: Vec<Req>) {
    let Some(inbox) = WRITERS.with(|w| w.borrow().get(&log).cloned()) else { return };
    inbox.reqs.borrow_mut().extend(batch.into_iter().map(|r| (core, r)));
    inbox.signal.notify();
}

/// A running device log.
#[derive(Debug)]
pub struct DeviceLog {
    shared: Arc<Shared>,
    recovered_files: usize,
    post: ShardPost,
    log: u64,
    /// The writer's end, with the disk in memory it ran on.
    stopped: mpsc::Receiver<Stopped>,
}

/// What the writer found on start, for the ports.
struct Started {
    files: usize,
    wals: Vec<BTreeSet<u64>>,
    answers: Vec<mpsc::Receiver<IoDone>>,
}

impl DeviceLog {
    /// Recovers the log of `disk` on shard 0 of `engine` and starts its writer there, with a port
    /// for each of `cores` cores; its records carry the HLC bound `bound` offers, and it publishes
    /// in it the bound found on start and each one made durable (005 FR-003, D-172).
    ///
    /// # Errors
    ///
    /// The error of reading the log, or the engine stopped.
    pub fn start(
        engine: &NodeEngine,
        disk: LogDisk,
        cores: u16,
        cfg: DeviceLogConfig,
        bound: Arc<LogBound>,
    ) -> io::Result<(DeviceLog, Vec<LogPort>)> {
        let log = NEXT.fetch_add(1, Ordering::Relaxed);
        let shared = Arc::new(Shared::default());
        let (started_tx, started_rx) = mpsc::sync_channel::<io::Result<Started>>(1);
        let (stopped_tx, stopped_rx) = mpsc::sync_channel::<Stopped>(1);
        let s = shared.clone();
        let post = engine.post_to(0);
        let sent = post.post(move || {
            let attr = ThreadAttributes { sched_group: None, stack_size: WRITER_STACK };
            drop(in_thread_with(attr, move || {
                let opened = match open_disk(disk) {
                    Ok(d) => d,
                    Err(e) => return drop(started_tx.send(Err(e))),
                };
                run(opened, log, cores, cfg, (&s, &bound), started_tx, stopped_tx);
            }));
        });
        let gone = || io::Error::other("the engine stopped");
        if !sent {
            return Err(gone());
        }
        let started = started_rx.recv().map_err(|_| gone())??;
        let ports = started
            .wals
            .into_iter()
            .zip(started.answers)
            .enumerate()
            .map(|(core, (wals, answers))| {
                let core = u16::try_from(core).expect("a node has fewer than 65,536 cores");
                LogPort::new(core, (post.clone(), log), wals, answers, shared.clone())
            })
            .collect();
        let dl =
            DeviceLog { shared, recovered_files: started.files, post, log, stopped: stopped_rx };
        Ok((dl, ports))
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

    /// Stops the writer once every port is gone or idle, and waits for it: what was written
    /// without a sync is made durable. Gives back the model of a disk it ran on.
    pub fn stop(self) -> Option<Box<dyn Disk + Send>> {
        self.halt();
        self.stopped.recv().ok().flatten()
    }

    fn halt(&self) {
        self.shared.stop.store(true, Ordering::Release);
        let log = self.log;
        let _ = self.post.post(move || {
            if let Some(inbox) = WRITERS.with(|w| w.borrow().get(&log).cloned()) {
                inbox.signal.notify();
            }
        });
    }
}

impl Drop for DeviceLog {
    fn drop(&mut self) {
        self.halt();
    }
}

/// The executor of the log's disk, on shard 0.
fn open_disk(disk: LogDisk) -> io::Result<ShardDisk> {
    match disk {
        LogDisk::Memory(m) => Ok(ShardDisk::model(m)),
        LogDisk::Files(root) => {
            let groups = IoGroups::get_or_create().get().map_err(engine_error)?;
            ShardDisk::open(root, groups).get().map_err(engine_error)
        }
    }
}

fn engine_error(e: ExceptionPtr) -> io::Error {
    io::Error::other(e.to_string())
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
    /// When the oldest sync waiting for the next group was taken.
    first_taken: Option<Instant>,
    group: Option<Group>,
}

impl Flight {
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

/// Hands each core its answers.
fn answer(sinks: &mut [Sink], answers: teseladb_devlog::state::Answers) {
    for (core, done) in answers {
        let _ = (sinks[usize::from(core)])(done);
    }
}

/// The writer: recovers the log on `disk`, tells the starter what it found, then serves the cores
/// until stopped. Runs in an engine thread of shard 0.
#[allow(clippy::too_many_lines)]
fn run(
    mut disk: ShardDisk,
    log: u64,
    cores: u16,
    cfg: DeviceLogConfig,
    (shared, bound): (&Shared, &LogBound),
    started: mpsc::SyncSender<io::Result<Started>>,
    stopped: mpsc::SyncSender<Stopped>,
) {
    // The log's own steps, run synchronously, go in its class like the rest.
    disk.set_sync_class(IoClass::Log);
    let mut state = match LogState::recover(&mut disk, cfg.file_bytes) {
        Ok(s) => s,
        Err(e) => return drop(started.send(Err(io::Error::other(format!("raft log: {e:?}"))))),
    };
    bound.set_durable(state.durable_bound());
    let inbox = Rc::new(Inbox::default());
    WRITERS.with(|w| w.borrow_mut().insert(log, inbox.clone()));
    let mut sinks: Vec<Sink> = Vec::new();
    let mut answers = Vec::new();
    let mut wals = Vec::new();
    for core in 0..cores {
        let (tx, rx) = mpsc::channel();
        sinks.push(Box::new(move |d| tx.send(d).is_ok()));
        answers.push(rx);
        wals.push(state.segments_of(core).into_iter().collect());
    }
    let _ = started.send(Ok(Started { files: state.files(), wals, answers }));
    let mut recovering = usize::from(cores);
    if recovering == 0 {
        state.allow_tidying();
    }
    let mut flight = Flight::default();
    let mut ops = Vec::new();
    let mut results = Vec::new();
    let mut reqs = Vec::new();
    let mut woke_by_timer = false;
    // Whether a wake-up on the disk's next result is armed.
    let armed = Rc::new(Cell::new(false));
    loop {
        // The records written from here on carry the bound offered (D-172).
        state.offer_bound(bound.offered());
        reqs.append(&mut inbox.reqs.borrow_mut());
        let took = !reqs.is_empty();
        for (core, req) in reqs.drain(..) {
            match req {
                Req::Log(r, asked) => {
                    if let Some(at) = asked {
                        flight.taken(at);
                    }
                    r.apply(&mut state, core);
                }
                Req::Read { wal, reply } => {
                    let _ = reply.send(state.read(&mut disk, core, wal));
                }
                Req::Attach(s) => {
                    sinks[usize::from(core)] = s;
                    recovering -= 1;
                    if recovering == 0 {
                        state.allow_tidying();
                    }
                }
            }
        }
        let mut worked = took;
        disk.take_done(&mut results);
        for (done, _) in results.drain(..) {
            worked = true;
            let before = state.syncs();
            let ready = state.on_done(&done);
            let ended = state.syncs() != before;
            if ended {
                crate::counters::record_log_sync();
            }
            flight.on_result(&done, ended);
            answer(&mut sinks, ready);
        }
        // A full disk is tried again after its wait, or when something new comes.
        if took || !state.disk_full() || woke_by_timer {
            let starting = !state.syncing();
            let ready = state.next(&mut disk, &mut ops);
            if starting && state.syncing() {
                flight.group_started(&ops);
            }
            if !state.has_waiting() && !state.syncing() {
                flight.first_taken = None;
            }
            shared.disk_full.store(state.disk_full(), Ordering::Release);
            worked |= !ready.is_empty() || !ops.is_empty();
            answer(&mut sinks, ready);
            for op in ops.drain(..) {
                flight.in_flight += 1;
                disk.submit(op, IoClass::Log);
            }
        }
        shared.syncs.store(state.syncs(), Ordering::Relaxed);
        bound.set_durable(state.durable_bound());
        if worked {
            continue;
        }
        if shared.stop.load(Ordering::Acquire)
            && flight.in_flight == 0
            && inbox.reqs.borrow().is_empty()
        {
            break;
        }
        if flight.in_flight > 0 && !armed.replace(true) {
            // The disk's next result wakes the writer.
            let (sig, armed) = (inbox.clone(), armed.clone());
            drop(disk.ready().then(move |()| {
                armed.set(false);
                sig.signal.notify();
                make_ready_future(())
            }));
        }
        let wait = if state.disk_full() { DISK_FULL_RETRY } else { IDLE };
        woke_by_timer = inbox.signal.wait(wait);
    }
    // What was written without a sync (a removal, the copy of old files) is made durable.
    state.sync(0, IoOp::SyncDir { path: super::format::DIR.into() });
    let before = state.syncs();
    let _ = state.flush(&mut disk);
    for _ in before..state.syncs() {
        crate::counters::record_log_sync();
    }
    shared.syncs.store(state.syncs(), Ordering::Relaxed);
    bound.set_durable(state.durable_bound());
    WRITERS.with(|w| w.borrow_mut().remove(&log));
    let _ = stopped.send(disk.into_model());
}
