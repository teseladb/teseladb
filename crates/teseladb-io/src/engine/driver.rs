//! The task of a shard that drives its core's logic (023 FR-009): a turn takes what arrived (the
//! bytes of the core's connections, the results of its disk and of the node's device log, its
//! messages), feeds the logic, ticks it, and hands out what the logic asks for; between turns the
//! task waits on an engine timer at the logic's next deadline or until something arrives. With
//! the same inputs the logic gets the same calls in the same order as with the loop this replaces.

use std::collections::VecDeque;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use teseladb::engine::core::reactor::yield_;
use teseladb::engine::core::timer::{Clock, SteadyClockType, SteadyTimePoint, Timer};
use teseladb_raftlog::IoDone;
use teseladb_types::{Nanos, NodeId};

use super::core_loop::{CoreInput, CoreLogic, REST_AFTER, REST_TICK, STOP_SETTLE_LIMIT};
use super::disk_port::CoreDisk;
use super::peers::Net;
use super::post::{CoreIo, Inbox};
use crate::IoClass;
use crate::admission::Admission;
use crate::device_log::LogPort;
use crate::file_send::FileSend;
use crate::peer::PeerMsg;
use crate::service::ServiceClass;

/// How soon a core tries again to queue WAL operations for the device log when its queue was full.
const LOG_QUEUE_RETRY: Nanos = Nanos::from_millis(1);
/// Longest wait between two looks at a stopping core's disk, so that an executor that died
/// while the core waits for it is noticed.
const STOP_SETTLE_POLL: Duration = Duration::from_millis(10);

fn wall_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Lets the shard run its other tasks. Not generic: every await of an engine future is in
/// non-generic code of this crate (see the workspace's profile).
async fn yield_now() {
    let _ = yield_().await;
}

fn nanos(d: Duration) -> u64 {
    u64::try_from(d.as_nanos()).unwrap_or(u64::MAX)
}

/// The state of a core's task.
pub(crate) struct Driver<L: CoreLogic, X: CoreDisk> {
    pub(crate) logic: L,
    pub(crate) disk: X,
    pub(crate) net: Net,
    io: Rc<CoreIo>,
    inbox: Rc<Inbox<L::Msg>>,
    log: Option<LogPort>,
    origin: Instant,
    step: Nanos,
    next_tick: Nanos,
    /// The last turn with work, or that handed something to the disk or the network.
    busy_at: Nanos,
    /// It ticks every [`REST_TICK`].
    resting: bool,
    stopped: bool,
    /// The disk can take no more operations: a stop does not wait for it.
    pub(crate) disk_gone: bool,
    /// Refuses new requests while the queue wait stays high (012 FR-029).
    admission: Admission,
    /// Background requests waiting for a turn with nothing else to do (012 FR-030), each with the
    /// instant it was queued.
    background: VecDeque<(Instant, L::Msg)>,
    inputs: Vec<(Instant, CoreInput<L::Msg>)>,
    reaped: Vec<(IoDone, IoClass)>,
    logs: Vec<(Instant, IoDone)>,
    outbox: Vec<(NodeId, PeerMsg)>,
    file_outbox: Vec<(NodeId, FileSend)>,
    timer: Pin<Box<Timer>>,
    /// Where the timer is armed, in the core's clock.
    armed: Option<Nanos>,
}

impl<L: CoreLogic, X: CoreDisk> Driver<L, X> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        mut logic: L,
        disk: X,
        net: Net,
        io: Rc<CoreIo>,
        inbox: Rc<Inbox<L::Msg>>,
        log: Option<LogPort>,
        tick: Duration,
        origin: Instant,
    ) -> Driver<L, X> {
        let wake = io.clone();
        let timer = Box::pin(Timer::with_callback(move || wake.wake.wake()));
        // The runtime does not send the bytes of files itself (023 FR-017 waits for the engine's
        // disk executor): the logic answers them whole.
        logic.file_sends(false);
        let step = Nanos(nanos(tick));
        let now = Nanos(nanos(origin.elapsed()));
        Driver {
            logic,
            disk,
            net,
            io,
            inbox,
            log,
            origin,
            step,
            next_tick: Nanos(now.0 + step.0),
            busy_at: now,
            resting: false,
            stopped: false,
            disk_gone: false,
            admission: Admission::new(),
            background: VecDeque::new(),
            inputs: Vec::new(),
            reaped: Vec::new(),
            logs: Vec::new(),
            outbox: Vec::new(),
            file_outbox: Vec::new(),
            timer,
            armed: None,
        }
    }

    fn now(&self) -> Nanos {
        Nanos(nanos(self.origin.elapsed()))
    }

    /// Runs turns until a [`CoreInput::Stop`], then settles the logic's writes.
    pub(crate) async fn run(&mut self) {
        while !self.stopped {
            self.turn();
            if self.stopped {
                break;
            }
            self.sleep().await;
        }
        // The background requests still waiting are answered before the core goes.
        while let Some((_, m)) = self.background.pop_front() {
            let now = self.now();
            self.logic.on_message(m, now);
        }
        self.settle().await;
        self.timer.as_mut().cancel();
    }

    /// The deadline of the next turn without any input.
    fn deadline(&mut self, now: Nanos) -> Nanos {
        let mut deadline = self.next_tick;
        if let Some(d) = self.logic.next_deadline() {
            deadline = deadline.min(d);
        }
        if let Some(d) = self.net.next_deadline() {
            deadline = deadline.min(d);
        }
        // What waits for room in the log's queue is tried again soon.
        if self.log.as_mut().is_some_and(LogPort::pump) {
            deadline = deadline.min(Nanos(now.0 + LOG_QUEUE_RETRY.0));
        }
        deadline
    }

    /// Arms the timer at `at` (core clock) unless it already is.
    fn arm(&mut self, at: Nanos, now: Nanos) {
        if self.armed == Some(at) && self.timer.armed() {
            return;
        }
        let delta = i64::try_from(at.0.saturating_sub(now.0)).unwrap_or(i64::MAX);
        let until = SteadyTimePoint(SteadyClockType::now().0.saturating_add(delta));
        self.timer.as_mut().rearm(until, None);
        self.armed = Some(at);
    }

    /// Waits for the next turn: until something arrives or the deadline passes. A core with
    /// background requests, or with disk results nothing will announce, only lets the shard run
    /// its other tasks.
    async fn sleep(&mut self) {
        let now = self.now();
        let deadline = self.deadline(now);
        if !self.background.is_empty() || self.disk.ready() || deadline <= now {
            yield_now().await;
            return;
        }
        self.arm(deadline, now);
        self.io.wake.wait().await;
    }

    /// One turn.
    pub(crate) fn turn(&mut self) {
        let woke = self.now();
        self.net.collect();
        // What the connections read goes first, as a poller's reads did.
        let mut worked = self.net.read(&mut self.logic, woke);
        self.disk.reap(&mut self.reaped);
        self.io.take_log(&mut self.logs);
        self.inbox.drain_into(&mut self.inputs);
        worked |= !self.reaped.is_empty()
            || !self.logs.is_empty()
            || !self.inputs.is_empty()
            || self.net.has_inputs()
            || !self.background.is_empty();
        if worked {
            let now = self.now();
            self.busy_at = now;
            if self.resting {
                // The rest ends with the clock part of a tick before the work, as every mark
                // comes from a wall clock read and checked then; the whole tick comes at its time.
                self.resting = false;
                self.logic.wake(now, wall_ms());
                self.next_tick = Nanos(now.0 + self.step.0);
            } else {
                self.logic.clock(now, wall_ms());
            }
        }
        let now = self.now();
        for (done, class) in std::mem::take(&mut self.reaped) {
            self.logic.on_io_done(done, class, now);
        }
        let started = Instant::now();
        for (queued, done) in std::mem::take(&mut self.logs) {
            // How long the device log's answer waited for the core (012 FR-035).
            let waited = started.saturating_duration_since(queued);
            self.admission.observe(waited, started);
            teseladb_stages::record(teseladb_stages::Stage::LogNotify, nanos(waited));
            self.logic.on_io_done(done, IoClass::Log, now);
        }
        self.net.inputs(&mut self.logic, now);
        for (queued, input) in std::mem::take(&mut self.inputs) {
            self.on_input(queued, input);
        }
        self.run_background();
        let now = self.now();
        if now >= self.next_tick {
            // How late it runs (012 FR-015): its groups' beats go out with it.
            let late = now.0.saturating_sub(self.next_tick.0);
            teseladb_stages::record(teseladb_stages::Stage::CoreTickLate, late);
            self.logic.tick(now, wall_ms());
            self.resting =
                self.logic.resting() && now.0.saturating_sub(self.busy_at.0) >= REST_AFTER.0;
            let step = if self.resting { Nanos(nanos(REST_TICK)) } else { self.step };
            self.next_tick = Nanos(self.next_tick.0 + step.0).max(Nanos(now.0 + 1));
        }
        self.net.backoffs_done(&self.logic);
        self.pump(now);
        if worked {
            let busy = self.now().0.saturating_sub(woke.0);
            teseladb_stages::record(teseladb_stages::Stage::CoreStep, busy);
        }
    }

    fn on_input(&mut self, queued: Instant, input: CoreInput<L::Msg>) {
        let started = Instant::now();
        let overloaded = self.admission.observe(started.saturating_duration_since(queued), started);
        let now = self.now();
        match input {
            CoreInput::App(m) if overloaded => {
                // 012 FR-029: a new request is refused before the core spends CPU on it.
                match self.logic.refuse_overloaded(m) {
                    Some(m) => self.logic.on_message(m, now),
                    None => self.admission.count_refusal(),
                }
            }
            CoreInput::App(m) => match self.logic.service_class(&m) {
                ServiceClass::Foreground => self.logic.on_message(m, now),
                ServiceClass::Background => self.background.push_back((Instant::now(), m)),
            },
            CoreInput::Stop => self.stopped = true,
        }
    }

    /// Runs one background request if nothing new waits (012 FR-030).
    fn run_background(&mut self) {
        if self.background.is_empty() || !self.inbox.is_empty() {
            return;
        }
        let (queued, m) = self.background.pop_front().expect("a background request");
        // Its wait in the queue (012 FR-015).
        teseladb_stages::record(teseladb_stages::Stage::BackgroundWait, nanos(queued.elapsed()));
        let now = self.now();
        self.logic.on_message(m, now);
    }

    /// Hands the logic's disk operations and frames to their executors. Anything handed out
    /// keeps the core from resting ([`REST_AFTER`]).
    fn pump(&mut self, now: Nanos) {
        let (handed, alive) = self.submit_io(now);
        if handed > 0 {
            self.busy_at = now;
        }
        if !alive {
            return;
        }
        self.logic.take_outbound(&mut self.outbox);
        if !self.outbox.is_empty() {
            self.busy_at = now;
        }
        for (node, msg) in std::mem::take(&mut self.outbox) {
            self.net.send_to(node, &msg, &mut self.logic);
        }
        self.logic.take_file_sends(&mut self.file_outbox);
        if !self.file_outbox.is_empty() {
            self.busy_at = now;
        }
        for (node, send) in std::mem::take(&mut self.file_outbox) {
            self.net.send_file_to(node, &send, &mut self.logic);
        }
    }

    /// Hands the logic's disk operations to their executor: how many, and `false` if the disk
    /// is gone.
    fn submit_io(&mut self, now: Nanos) -> (usize, bool) {
        let mut handed = 0;
        while let Some((op, class)) = self.logic.poll_io(now) {
            handed += 1;
            if let Some(port) = self.log.as_mut().filter(|p| p.takes(&op)) {
                // Answered at once but for a sync, which the writer answers through the shard.
                if let Some(done) = port.submit(op) {
                    self.logic.on_io_done(done, IoClass::Log, now);
                }
                continue;
            }
            if !self.disk.submit(op, class) {
                // Nothing can be made durable any more.
                (self.stopped, self.disk_gone) = (true, true);
                return (handed, false);
            }
        }
        if let Some(port) = self.log.as_mut() {
            port.pump();
        }
        // One submission to the device for the whole turn.
        if !self.disk.flush() {
            (self.stopped, self.disk_gone) = (true, true);
            return (handed, false);
        }
        (handed, true)
    }

    /// After a stop, hands the logic's pending writes to the disk and feeds it their results
    /// until [`CoreLogic::io_settled`], the disk is gone or [`STOP_SETTLE_LIMIT`] passes. Nothing
    /// else reaches the logic: no frame, message or tick, so it takes no new work, and no frame
    /// leaves.
    async fn settle(&mut self) {
        // The connections close now.
        self.net.close_all();
        let end = self.now().0.saturating_add(nanos(STOP_SETTLE_LIMIT));
        loop {
            let now = self.now();
            self.submit_io(now);
            self.disk.reap(&mut self.reaped);
            self.io.take_log(&mut self.logs);
            for (done, class) in std::mem::take(&mut self.reaped) {
                self.logic.on_io_done(done, class, now);
            }
            for (_, done) in std::mem::take(&mut self.logs) {
                self.logic.on_io_done(done, IoClass::Log, now);
            }
            if self.disk_gone || self.logic.io_settled() || !self.disk.alive() || now.0 >= end {
                return;
            }
            let mut at = end.min(now.0.saturating_add(nanos(STOP_SETTLE_POLL)));
            if let Some(d) = self.logic.next_deadline() {
                at = at.min(d.0);
            }
            if self.disk.ready() {
                yield_now().await;
                continue;
            }
            self.arm(Nanos(at), now);
            self.io.wake.wait().await;
        }
    }
}
