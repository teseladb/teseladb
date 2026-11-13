//! Loop of one core: waits on the poller until the next tick or
//! deadline, feeds the core's logic with peer frames, mailbox messages and disk completions, runs
//! the ticks, and executes what the logic asks for: disk operations go to the core's disk thread,
//! or with an io_uring disk ([`crate::NodeDisk::Uring`], 012 FR-002) to the core's ring, whose
//! completions wake the same poller through an `eventfd` and whose submissions of a turn go to the
//! kernel in one call; frames for other nodes go to one outbound connection per node (dialed in the background,
//! redialed with a 10 ms–1 s backoff with jitter after a failure).

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use teseladb_raftlog::{Disk, IoDone, IoOp};
use teseladb_types::{Nanos, NodeId, Rng};

use crate::admission::Admission;
use crate::disk_thread::DiskThread;
use crate::handshake::{self, Inbound, Outbound};
use crate::mailbox::{self, Mailbox, MailboxSender};
use crate::peer::{Hello, PeerMsg, decode_frame, encode_frame_for};
use crate::poller::{Event, Interest, Poller, WAKE_TOKEN};
use crate::secure::{Channel, PeerTls};
use crate::service::ServiceClass;
use crate::timer::TimerWheel;

/// Frames queued for a node that is not connected yet beyond this are dropped (Raft resends).
pub const MAX_PENDING: usize = 8 << 20;
/// First redial delay after a failure.
pub const BACKOFF_MIN_MS: u64 = 10;
/// Maximum redial delay.
pub const BACKOFF_MAX_MS: u64 = 1_000;
/// How long a stop waits for the logic's writes to reach the disk ([`CoreLogic::io_settled`]): a
/// disk that takes longer (full, or hung) is left as a crash would leave it.
pub const STOP_SETTLE_LIMIT: Duration = Duration::from_secs(10);
/// Longest wait between two looks at a stopping core's disk, so that a disk thread that died
/// while the core waits for it is noticed.
const STOP_SETTLE_POLL: Duration = Duration::from_millis(10);

/// How long a core goes without work, with its logic at rest, before it ticks every
/// [`REST_TICK`].
pub const REST_AFTER: Nanos = Nanos::from_secs(1);
/// The tick of a core at rest. Linux stops the tick of an idle CPU the same way
/// (`NO_HZ_IDLE`); the first work ends the rest
/// with a tick, so a mark still comes from a wall clock read in the same tick that checked it for
/// jumps (005 FR-007).
pub const REST_TICK: Duration = Duration::from_millis(100);

/// What a core does; the runtime only moves bytes and time.
pub trait CoreLogic {
    /// Messages other threads send to this core.
    type Msg: Send + 'static;
    /// Periodic tick; `wall_ms` is the node's wall clock.
    fn tick(&mut self, now: Nanos, wall_ms: u64);
    /// The core leaves its rest for work that arrived: the clock part of a tick, so that what
    /// the work stamps comes from a wall clock read now (005 FR-007); the rest of the tick comes
    /// at its time (H-044).
    fn wake(&mut self, now: Nanos, wall_ms: u64);
    /// A turn with work starts, on a core that is not at rest: `wall_ms` is the node's wall
    /// clock read now, for what the turn stamps and compares (005 FR-032). Between two ticks
    /// the logic had the reading of the last one, up to a tick old.
    fn clock(&mut self, now: Nanos, wall_ms: u64);
    /// Next disk operation, if any, with its class (012 FR-023).
    fn poll_io(&mut self, now: Nanos) -> Option<(IoOp, crate::IoClass)>;
    /// Result of a disk operation of `class`, in submission order within its class.
    fn on_io_done(&mut self, done: IoDone, class: crate::IoClass, now: Nanos);
    /// Earliest instant at which `poll_io` may have something new without any input.
    fn next_deadline(&self) -> Option<Nanos>;
    /// Frame received from `from`.
    fn on_peer(&mut self, from: NodeId, msg: PeerMsg, now: Nanos);
    /// Message from another thread.
    fn on_message(&mut self, msg: Self::Msg, now: Nanos);
    /// The core is overloaded (012 FR-029): answers `msg` with `Overloaded` if it is a new
    /// request and returns `None`, or gives it back to run as usual (the default, for messages
    /// that are not requests).
    fn refuse_overloaded(&mut self, msg: Self::Msg) -> Option<Self::Msg> {
        Some(msg)
    }
    /// The service class of `msg` (012 FR-030): a background one runs only when the core has
    /// nothing else to do. Everything is in the foreground by default.
    fn service_class(&self, _msg: &Self::Msg) -> ServiceClass {
        ServiceClass::Foreground
    }
    /// Moves the frames to send to `out`.
    fn take_outbound(&mut self, out: &mut Vec<(NodeId, PeerMsg)>);
    /// Inter-node address of `node`, if known.
    fn peer_addr(&self, node: NodeId) -> Option<String>;
    /// The connection to `node` failed or closed: what was in flight may be lost.
    fn peer_unreachable(&mut self, node: NodeId);
    /// Whether every write the logic has taken is on disk: none waits to be handed out and none
    /// is in flight. A stop waits for it, so that a clean stop loses nothing the logic
    /// acknowledged (005 FR-010: a `w: 1` write is in the leader's log once confirmed).
    fn io_settled(&self) -> bool;
    /// Whether the logic has nothing to do until something arrives: its groups are at rest and
    /// none of its work waits for a tick. A core whose logic rests and that had no work for
    /// [`REST_AFTER`] ticks every [`REST_TICK`] instead of every tick (H-037).
    fn resting(&self) -> bool;
}

/// Input of a core's mailbox.
#[derive(Debug)]
pub enum CoreInput<M> {
    /// Completion from the disk thread.
    Disk(IoDone),
    /// Connection accepted for this core.
    Inbound(Inbound),
    /// Outcome of a dial this core asked for.
    Outbound(Outbound),
    /// Message for the logic.
    App(M),
    /// Ends the loop.
    Stop,
}

/// Sender of a core's mailbox.
pub type CoreSender<M> = MailboxSender<CoreInput<M>>;
/// What makes senders of a core's mailbox, to share between threads.
pub type CoreSenders<M> = crate::mailbox::MailboxSenders<CoreInput<M>>;

/// Runtime parameters.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Tick period (10 ms in production).
    pub tick: Duration,
    /// Seed of the backoff jitter.
    pub seed: u64,
    /// TLS for the connections this core dials (013 FR-006); `None` in clear.
    pub tls: Option<PeerTls>,
    /// Where the core's monotonic time starts; `None`: when the runtime is built. The cores of a
    /// node share one, so a time one core publishes means the same on any thread of the node
    /// (005 FR-028).
    pub origin: Option<Instant>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig { tick: Duration::from_millis(10), seed: 1, tls: None, origin: None }
    }
}

struct Conn {
    stream: TcpStream,
    /// In clear or TLS; `wbuf` holds what goes to the socket, already through it.
    channel: Channel,
    peer: NodeId,
    outbound: bool,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
    wpos: usize,
    want_write: bool,
}

enum LinkState {
    /// Not connected; the next frame dials.
    Idle,
    /// Waiting for the backoff timer.
    Backoff,
    /// Dial in progress.
    Dialing,
    /// Connected through this token.
    Up(u64),
}

struct Link {
    state: LinkState,
    pending: Vec<u8>,
    backoff_ms: u64,
    /// The protocol version both nodes spoke on the last connection (0: none yet): what the
    /// frames for this node may use.
    version: u16,
}

/// Poller token of the ring's `eventfd`.
pub const RING_TOKEN: u64 = u64::MAX - 1;

/// Who executes the core's disk operations.
enum Exec<D: Disk + Send + 'static> {
    /// The core's disk thread.
    Thread(DiskThread<D>),
    /// The core itself, through its ring (`D` is an io_uring [`crate::NodeDisk`]).
    #[cfg(target_os = "linux")]
    InLoop(D),
}

/// The ring of `disk`, when it is an io_uring [`crate::NodeDisk`].
#[cfg(target_os = "linux")]
fn ring_of<D: 'static>(disk: &mut D) -> Option<&mut crate::IoUringDisk> {
    (disk as &mut dyn std::any::Any).downcast_mut::<crate::NodeDisk>()?.uring()
}

/// The ring of a disk the loop drives itself, which [`CoreRuntime::new`] only keeps when it has
/// one.
#[cfg(target_os = "linux")]
fn ring<D: 'static>(disk: &mut D) -> &mut crate::IoUringDisk {
    ring_of(disk).expect("a disk driven by the loop has a ring")
}

/// Whether an event's token is a connection's (not the mailbox's nor the ring's).
#[cfg(target_os = "linux")]
fn is_socket(token: u64) -> bool {
    token != WAKE_TOKEN && token != RING_TOKEN
}

/// Whether an event's token is a connection's (not the mailbox's).
#[cfg(not(target_os = "linux"))]
fn is_socket(token: u64) -> bool {
    token != WAKE_TOKEN
}

/// Loop of one core.
pub struct CoreRuntime<L: CoreLogic, D: Disk + Send + 'static> {
    logic: L,
    hello: Hello,
    cfg: RuntimeConfig,
    poller: Poller,
    mailbox: Mailbox<CoreInput<L::Msg>>,
    sender: CoreSender<L::Msg>,
    disk: Exec<D>,
    /// Completions taken from the ring this turn.
    #[cfg(target_os = "linux")]
    reaped: Vec<(IoDone, crate::IoClass)>,
    /// The classes of the operations handed to the disk thread, in order, but those it answers
    /// apart, which are all the log's ([`crate::answered_apart`]).
    thread_classes: VecDeque<crate::IoClass>,
    conns: BTreeMap<u64, Conn>,
    links: BTreeMap<NodeId, Link>,
    /// Inter-node addresses announced in the `Hello` of incoming connections: used when the logic
    /// does not know a node's address yet (a node that just joined answers whoever reached it).
    learned: BTreeMap<NodeId, String>,
    timers: TimerWheel<NodeId>,
    rng: Rng,
    start: Instant,
    next_token: u64,
    next_tick: Nanos,
    stopped: bool,
    /// The disk can take no more operations: a stop does not wait for it.
    disk_gone: bool,
    /// The messages drained from the mailbox, with the instant each was queued.
    inputs: Vec<(Instant, CoreInput<L::Msg>)>,
    /// Refuses new requests while the queue wait stays high (012 FR-029).
    admission: Admission,
    /// Background requests waiting for a turn with nothing else to do (012 FR-030).
    background: VecDeque<L::Msg>,
    outbox: Vec<(NodeId, PeerMsg)>,
    events: Vec<Event>,
    fired: Vec<NodeId>,
    /// Whether the last turn took any work (a message, a disk result or a socket event): only
    /// then does the next one spin before sleeping.
    worked: bool,
    /// Turns that spun, for the tests.
    spins: u64,
    /// The last turn with work, or that handed something to the disk or the network.
    busy_at: Nanos,
    /// It ticks every [`REST_TICK`].
    resting: bool,
}

fn wall_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl<L: CoreLogic, D: Disk + Send + 'static> CoreRuntime<L, D> {
    /// Runtime of the core named in `hello.core`, with its disk thread already started. Returns
    /// the runtime and a sender for its mailbox (for the acceptor and the node).
    pub fn new(
        logic: L,
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))] mut disk: D,
        hello: Hello,
        cfg: RuntimeConfig,
    ) -> io::Result<(CoreRuntime<L, D>, CoreSender<L::Msg>)> {
        let poller = Poller::new()?;
        let (sender, mailbox) = mailbox::channel(Arc::new(poller.waker()));
        #[cfg(target_os = "linux")]
        let ring = ring_of(&mut disk).map(|r| r.event_fd());
        #[cfg(not(target_os = "linux"))]
        let ring: Option<i32> = None;
        let disk = match ring {
            #[cfg(target_os = "linux")]
            Some(fd) => {
                poller.register(&fd, RING_TOKEN, Interest::READ)?;
                Exec::InLoop(disk)
            }
            _ => {
                let name = format!("disk-{}", hello.core);
                Exec::Thread(DiskThread::spawn(name, disk, sender.clone(), CoreInput::Disk)?)
            }
        };
        let start = cfg.origin.unwrap_or_else(Instant::now);
        let rt = CoreRuntime {
            logic,
            hello,
            rng: Rng::new(cfg.seed),
            next_tick: Nanos(cfg.tick.as_nanos() as u64),
            cfg,
            poller,
            mailbox,
            sender: sender.clone(),
            disk,
            #[cfg(target_os = "linux")]
            reaped: Vec::new(),
            thread_classes: VecDeque::new(),
            conns: BTreeMap::new(),
            links: BTreeMap::new(),
            learned: BTreeMap::new(),
            timers: TimerWheel::new(0),
            start,
            next_token: 1,
            stopped: false,
            disk_gone: false,
            inputs: Vec::new(),
            admission: Admission::new(),
            background: VecDeque::new(),
            outbox: Vec::new(),
            events: Vec::new(),
            fired: Vec::new(),
            worked: false,
            spins: 0,
            busy_at: Nanos::ZERO,
            resting: false,
        };
        Ok((rt, sender))
    }

    /// The core's logic.
    pub fn logic(&mut self) -> &mut L {
        &mut self.logic
    }

    fn now(&self) -> Nanos {
        Nanos(self.start.elapsed().as_nanos() as u64)
    }

    /// Runs until a [`CoreInput::Stop`]. Returns the logic and the disk (`None` if the disk
    /// thread panicked).
    pub fn run(mut self) -> io::Result<(L, Option<D>)> {
        while !self.stopped {
            self.turn()?;
        }
        // The background requests still waiting are answered before the core goes.
        while let Some(m) = self.background.pop_front() {
            let now = self.now();
            self.logic.on_message(m, now);
        }
        self.settle()?;
        let CoreRuntime { logic, disk, .. } = self;
        let disk = match disk {
            Exec::Thread(t) => t.shutdown(),
            #[cfg(target_os = "linux")]
            Exec::InLoop(mut d) => {
                ring(&mut d).drain();
                Some(d)
            }
        };
        Ok((logic, disk))
    }

    /// After a stop, hands the logic's pending writes to the disk and feeds it their completions
    /// until [`CoreLogic::io_settled`], the disk is gone or [`STOP_SETTLE_LIMIT`] passes. Nothing
    /// else reaches the logic: no frame, message or tick, so it takes no new work, and no frame
    /// leaves.
    fn settle(&mut self) -> io::Result<()> {
        // The connections close now: a level-triggered socket nobody reads would wake every wait.
        self.conns.clear();
        let end = self.now().0.saturating_add(STOP_SETTLE_LIMIT.as_nanos() as u64);
        while !self.disk_gone && !self.logic.io_settled() && self.disk_alive() {
            let now = self.now();
            if now.0 >= end {
                break;
            }
            let mut deadline = end.min(now.0.saturating_add(STOP_SETTLE_POLL.as_nanos() as u64));
            if let Some(d) = self.logic.next_deadline() {
                deadline = deadline.min(d.0);
            }
            let timeout = Duration::from_nanos(deadline.saturating_sub(now.0));
            #[cfg(target_os = "linux")]
            let timeout = if self.ring_ready() { Duration::ZERO } else { timeout };
            self.poller.poll(Some(timeout))?;
            self.reap_ring()?;
            self.mailbox.drain_timed_into(&mut self.inputs);
            for (_, input) in std::mem::take(&mut self.inputs) {
                if let CoreInput::Disk(done) = input {
                    let class = self.thread_class(&done);
                    self.logic.on_io_done(done, class, self.now());
                }
            }
            self.submit_io(self.now());
        }
        Ok(())
    }

    /// Whether the thread that runs the disk operations still runs (the ring always does).
    fn disk_alive(&self) -> bool {
        match &self.disk {
            Exec::Thread(t) => t.is_running(),
            #[cfg(target_os = "linux")]
            Exec::InLoop(_) => true,
        }
    }

    /// Runs one background request if nothing new waits in the mailbox (012 FR-030).
    fn run_background(&mut self) {
        if self.background.is_empty() || !self.mailbox.is_empty() {
            return;
        }
        let m = self.background.pop_front().expect("a background request");
        let now = self.now();
        self.logic.on_message(m, now);
    }

    /// How many turns spun before sleeping.
    pub fn spins(&self) -> u64 {
        self.spins
    }

    /// One iteration of the loop.
    pub fn turn(&mut self) -> io::Result<()> {
        let now = self.now();
        let mut deadline = self.next_tick;
        if let Some(d) = self.logic.next_deadline() {
            deadline = deadline.min(d);
        }
        if let Some(ms) = self.timers.next_deadline() {
            deadline = deadline.min(Nanos::from_millis(ms));
        }
        let timeout = Duration::from_nanos(deadline.0.saturating_sub(now.0));
        // A background request waits for nothing but the core's other work.
        let timeout = if self.background.is_empty() { timeout } else { Duration::ZERO };
        // Results of the ring that no completion will signal are taken at once.
        #[cfg(target_os = "linux")]
        let timeout = if self.ring_ready() { Duration::ZERO } else { timeout };
        self.events.clear();
        // A message or a socket event that arrives within a short spin is taken without
        // sleeping: a wake-up costs several microseconds on every request that crosses threads
        // (H-013). The spin looks at the mailbox and, without waiting, at the poller; the events
        // it takes are not asked for again. Only a core that just worked spins: one that woke
        // for its tick alone sleeps again at once (H-037: every core spun after each tick,
        // 3 % of a core with the node idle).
        let mut failed = None;
        let spin = self.worked && !timeout.is_zero();
        self.spins += u64::from(spin);
        let spun = spin
            && crate::spin::spin_until(crate::spin::SPIN_BEFORE_SLEEP, || {
                if !self.mailbox.is_empty() {
                    return true;
                }
                match self.poller.poll(Some(Duration::ZERO)) {
                    Ok(ev) => {
                        self.events.extend_from_slice(ev);
                        !self.events.is_empty()
                    }
                    Err(e) => {
                        failed = Some(e);
                        true
                    }
                }
            });
        if let Some(e) = failed {
            return Err(e);
        }
        if self.events.is_empty() {
            let wait = if spun { Duration::ZERO } else { timeout };
            self.events.extend_from_slice(self.poller.poll(Some(wait))?);
        }
        let woke = self.now();
        let events = std::mem::take(&mut self.events);
        let mut worked = false;
        for e in &events {
            if is_socket(e.token) {
                worked = true;
                self.on_socket(e);
            }
        }
        worked |= self.reap_ring()?;
        self.events = events;
        self.mailbox.drain_timed_into(&mut self.inputs);
        self.worked = worked || !self.inputs.is_empty() || !self.background.is_empty();
        let step = Nanos(self.cfg.tick.as_nanos() as u64);
        if self.worked {
            let now = self.now();
            self.busy_at = now;
            if self.resting {
                // The rest ends with the clock part of a tick before the work, as every mark
                // comes from a wall clock read and checked then; the whole tick comes at its time.
                self.resting = false;
                self.logic.wake(now, wall_ms());
                self.next_tick = Nanos(now.0 + step.0);
            } else {
                self.logic.clock(now, wall_ms());
            }
        }
        for (queued, input) in std::mem::take(&mut self.inputs) {
            self.on_input(queued, input)?;
        }
        self.run_background();
        let now = self.now();
        if now >= self.next_tick {
            self.logic.tick(now, wall_ms());
            self.resting =
                self.logic.resting() && now.0.saturating_sub(self.busy_at.0) >= REST_AFTER.0;
            let step = if self.resting { Nanos(REST_TICK.as_nanos() as u64) } else { step };
            self.next_tick = Nanos(self.next_tick.0 + step.0).max(Nanos(now.0 + 1));
        }
        self.timers.advance(now.as_millis(), &mut self.fired);
        for node in std::mem::take(&mut self.fired) {
            self.on_backoff_done(node);
        }
        self.pump(now);
        if self.worked {
            let busy = self.now().0.saturating_sub(woke.0);
            teseladb_stages::record(teseladb_stages::Stage::CoreStep, busy);
        }
        Ok(())
    }

    /// Whether the ring has results that no completion will signal.
    #[cfg(target_os = "linux")]
    fn ring_ready(&mut self) -> bool {
        match &mut self.disk {
            Exec::InLoop(d) => ring(d).ready(),
            Exec::Thread(_) => false,
        }
    }

    /// Feeds the logic with the ring's completions. A ring that fails is a disk that cannot make
    /// anything durable any more: the loop ends with the error. Returns whether it took any.
    fn reap_ring(&mut self) -> io::Result<bool> {
        #[cfg(target_os = "linux")]
        if let Exec::InLoop(d) = &mut self.disk {
            ring(d).reap(&mut self.reaped)?;
            let now = self.now();
            let took = !self.reaped.is_empty();
            for (done, class) in std::mem::take(&mut self.reaped) {
                self.logic.on_io_done(done, class, now);
            }
            return Ok(took);
        }
        Ok(false)
    }

    /// Hands the logic's disk operations and frames to their executors.
    /// Anything handed out keeps the core from resting ([`REST_AFTER`]).
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
            self.send_to(node, &msg);
        }
    }

    /// Hands the logic's disk operations to their executor: how many, and `false` if the disk
    /// is gone.
    fn submit_io(&mut self, now: Nanos) -> (usize, bool) {
        let mut handed = 0;
        while let Some((op, class)) = self.logic.poll_io(now) {
            handed += 1;
            let gone = match &mut self.disk {
                Exec::Thread(t) => {
                    if !crate::answered_apart(&op) {
                        self.thread_classes.push_back(class);
                    }
                    t.submit(op).is_err()
                }
                #[cfg(target_os = "linux")]
                Exec::InLoop(d) => {
                    ring(d).submit_in(op, class);
                    false
                }
            };
            if gone {
                // The disk thread is gone: nothing can be made durable any more.
                (self.stopped, self.disk_gone) = (true, true);
                return (handed, false);
            }
        }
        // One `io_uring_enter` for the whole turn.
        #[cfg(target_os = "linux")]
        if let Exec::InLoop(d) = &mut self.disk
            && ring(d).flush().is_err()
        {
            (self.stopped, self.disk_gone) = (true, true);
            return (handed, false);
        }
        (handed, true)
    }

    /// The class of a result of the disk thread: the log's for one answered apart, else the class
    /// of the oldest operation handed to it, since it answers the rest in order.
    fn thread_class(&mut self, done: &IoDone) -> crate::IoClass {
        let apart = match done {
            IoDone::Ok(op) | IoDone::Err { op, .. } => crate::answered_apart(op),
            IoDone::Read { .. } => false,
        };
        if apart {
            return crate::IoClass::Log;
        }
        self.thread_classes
            .pop_front()
            .expect("a result for each operation handed to the disk thread")
    }

    fn on_input(&mut self, queued: Instant, input: CoreInput<L::Msg>) -> io::Result<()> {
        let started = Instant::now();
        let overloaded = self.admission.observe(started.saturating_duration_since(queued), started);
        let now = self.now();
        match input {
            CoreInput::Disk(done) => {
                let class = self.thread_class(&done);
                self.logic.on_io_done(done, class, now);
            }
            CoreInput::App(m) if overloaded => {
                // 012 FR-029: a new request is refused before the core spends CPU on it.
                match self.logic.refuse_overloaded(m) {
                    Some(m) => self.logic.on_message(m, now),
                    None => self.admission.count_refusal(),
                }
            }
            CoreInput::App(m) => match self.logic.service_class(&m) {
                ServiceClass::Foreground => self.logic.on_message(m, now),
                ServiceClass::Background => self.background.push_back(m),
            },
            CoreInput::Stop => self.stopped = true,
            CoreInput::Inbound(Inbound { stream, channel, got }) => {
                self.learned.insert(got.hello.node_id, got.hello.peer_addr.clone());
                let token = self.add_conn(stream, channel, got.hello.node_id, false, got.rest)?;
                self.deliver(token);
            }
            CoreInput::Outbound(Outbound { node, result }) => match result {
                Ok((stream, channel, got)) => {
                    let version = got.version;
                    let token = self.add_conn(stream, channel, node, true, got.rest)?;
                    let link = self.links.get_mut(&node).expect("link of a dial");
                    link.state = LinkState::Up(token);
                    link.backoff_ms = 0;
                    link.version = version;
                    let pending = std::mem::take(&mut link.pending);
                    // Frames that came with the handshake go first; garbage there closes it.
                    if self.deliver(token) {
                        self.queue_on(token, &pending);
                    }
                }
                Err(_) => self.link_failed(node),
            },
        }
        Ok(())
    }

    /// Takes over an established stream. A poller that cannot watch a fresh socket is broken, so
    /// that error ends the loop.
    fn add_conn(
        &mut self,
        stream: TcpStream,
        channel: Channel,
        peer: NodeId,
        outbound: bool,
        rest: Vec<u8>,
    ) -> io::Result<u64> {
        let token = self.next_token;
        stream.set_nonblocking(true)?;
        self.poller.register(&stream, token, Interest::READ)?;
        self.next_token += 1;
        let conn = Conn {
            stream,
            channel,
            peer,
            outbound,
            rbuf: rest,
            wbuf: Vec::new(),
            wpos: 0,
            want_write: false,
        };
        self.conns.insert(token, conn);
        Ok(token)
    }

    fn on_socket(&mut self, e: &Event) {
        // Every watched token is an open connection; writing may close it before reading.
        if e.writable {
            self.flush(e.token);
        }
        // Non-short-circuit: a hang-up is noticed by reading the end of the stream.
        if e.readable | e.hangup {
            self.read(e.token);
        }
    }

    fn read(&mut self, token: u64) {
        // Writing may have closed it in the same event.
        let Some(conn) = self.conns.get_mut(&token) else { return };
        let mut raw = Vec::new();
        let open = read_available(&mut conn.stream, &mut raw);
        let mut back = Vec::new();
        let sound = conn.channel.incoming(&raw, &mut conn.rbuf, &mut back);
        // Frames read before the end of the stream are still delivered.
        let valid = self.read_frames(token);
        if !(open & sound & valid) {
            return self.close(token);
        }
        if !back.is_empty() {
            // TLS's own answers (a key update, an alert) go straight out.
            let conn = self.conns.get_mut(&token).expect("open connection");
            conn.wbuf.extend_from_slice(&back);
            self.flush(token);
        }
    }

    /// Delivers what is buffered on `token` and closes it if a frame is invalid. Returns whether
    /// it is still open.
    fn deliver(&mut self, token: u64) -> bool {
        let valid = self.read_frames(token);
        if !valid {
            self.close(token);
        }
        valid
    }

    /// Delivers the complete frames buffered on `token`. Returns `false` at an invalid one
    /// (nothing after it is delivered; the contract closes the connection).
    fn read_frames(&mut self, token: u64) -> bool {
        let now = self.now();
        let conn = self.conns.get_mut(&token).expect("open connection");
        let mut pos = 0;
        let valid = loop {
            match decode_frame(&conn.rbuf[pos..]) {
                Ok(Some((msg, used))) => {
                    pos += used;
                    self.logic.on_peer(conn.peer, msg, now);
                }
                Ok(None) => break true,
                Err(_) => break false,
            }
        };
        conn.rbuf.drain(..pos);
        valid
    }

    fn close(&mut self, token: u64) {
        let conn = self.conns.remove(&token).expect("open connection");
        let _ = self.poller.deregister(&conn.stream);
        if conn.outbound {
            self.link_failed(conn.peer);
        }
    }

    /// The outbound link to `node` is down: drop what was queued, tell the logic and redial later.
    fn link_failed(&mut self, node: NodeId) {
        let link = self.links.get_mut(&node).expect("link of a failed dial or connection");
        let backoff = (link.backoff_ms * 2).clamp(BACKOFF_MIN_MS, BACKOFF_MAX_MS);
        link.backoff_ms = backoff;
        link.state = LinkState::Backoff;
        link.pending.clear();
        // An announced address that fails is forgotten: the next dial uses the logic's, which may be
        // newer (the node moved again without dialing us).
        self.learned.remove(&node);
        // Jitter of ±25 % so that the nodes of a restarted cluster do not dial in lockstep.
        let jitter = self.rng.range(0, backoff / 2 + 1);
        let at = self.now().as_millis() + backoff - backoff / 4 + jitter;
        self.timers.insert(at, node);
        self.logic.peer_unreachable(node);
    }

    fn on_backoff_done(&mut self, node: NodeId) {
        let link = self.links.get_mut(&node).expect("link of a backoff timer");
        link.state = LinkState::Idle;
        if !link.pending.is_empty() {
            self.dial(node);
        }
    }

    fn dial(&mut self, node: NodeId) {
        // The address the node itself announced in its last `Hello` is newer than the logic's
        // (a node that came back on another port); the logic's serves nodes that never dialed us.
        let Some(addr) = self.learned.get(&node).cloned().or_else(|| self.logic.peer_addr(node))
        else {
            // Unknown address: nothing to dial; frames are dropped (Raft resends).
            self.links.get_mut(&node).expect("link").pending.clear();
            return;
        };
        self.links.get_mut(&node).expect("link").state = LinkState::Dialing;
        handshake::connect_with(
            node,
            addr,
            self.hello.clone(),
            self.cfg.tls.as_ref().map(PeerTls::client),
            self.sender.clone(),
            CoreInput::Outbound,
        );
    }

    fn send_to(&mut self, node: NodeId, msg: &PeerMsg) {
        let mut frame = Vec::new();
        // A forwarded request or answer larger than a frame goes in pieces to a node that reads
        // them (protocol 6). A frame that cannot be encoded even so is the logic's bug; it is
        // dropped like a lost message rather than taking the core down.
        let version = self.links.get(&node).map_or(0, |l| l.version);
        if encode_frame_for(msg, false, version, &mut frame).is_err() {
            return;
        }
        let link = self.links.entry(node).or_insert(Link {
            state: LinkState::Idle,
            pending: Vec::new(),
            backoff_ms: 0,
            version: 0,
        });
        match link.state {
            LinkState::Up(token) => self.queue_on(token, &frame),
            LinkState::Idle | LinkState::Backoff | LinkState::Dialing => {
                if link.pending.len() + frame.len() <= MAX_PENDING {
                    link.pending.extend_from_slice(&frame);
                }
                if matches!(link.state, LinkState::Idle) {
                    self.dial(node);
                }
            }
        }
    }

    fn queue_on(&mut self, token: u64, bytes: &[u8]) {
        let conn = self.conns.get_mut(&token).expect("connection of an up link");
        if !conn.channel.outgoing(bytes, &mut conn.wbuf) {
            return self.close(token);
        }
        self.flush(token);
    }

    fn flush(&mut self, token: u64) {
        let conn = self.conns.get_mut(&token).expect("open connection");
        if !write_pending(&mut conn.stream, &conn.wbuf, &mut conn.wpos) {
            return self.close(token);
        }
        let done = conn.wpos == conn.wbuf.len();
        if done {
            conn.wbuf.clear();
            conn.wpos = 0;
        }
        if done == conn.want_write {
            // Interest changes: write while bytes are left, read only once they are out.
            conn.want_write = !done;
            let interest = if done { Interest::READ } else { Interest::BOTH };
            match self.poller.reregister(&conn.stream, token, interest) {
                Ok(()) => {}
                Err(_) => self.close(token),
            }
        }
    }
}

/// Reads into `buf` everything `r` has now. Returns whether it is still open: `false` at the end
/// of the stream or on an error other than `WouldBlock` (`Interrupted` is retried).
fn read_available(r: &mut impl Read, buf: &mut Vec<u8>) -> bool {
    let mut chunk = [0u8; 64 * 1024];
    loop {
        match r.read(&mut chunk) {
            Ok(0) => return false,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) => match e.kind() {
                ErrorKind::WouldBlock => return true,
                ErrorKind::Interrupted => {}
                _ => return false,
            },
        }
    }
}

/// Writes `buf[*pos..]` to `w` until it is all out or `w` would block, advancing `pos`. Returns
/// whether the stream is still usable (`Interrupted` is retried).
fn write_pending(w: &mut impl Write, buf: &[u8], pos: &mut usize) -> bool {
    while *pos < buf.len() {
        match w.write(&buf[*pos..]) {
            Ok(n) => *pos += n,
            Err(e) => match e.kind() {
                ErrorKind::WouldBlock => return true,
                ErrorKind::Interrupted => {}
                _ => return false,
            },
        }
    }
    true
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    /// Answers each call with the next scripted result: `Ok(n)` moves `n` bytes.
    struct Script(Vec<Result<usize, ErrorKind>>);

    impl Script {
        fn next(&mut self, max: usize) -> io::Result<usize> {
            match self.0.remove(0) {
                Ok(n) => Ok(n.min(max)),
                Err(kind) => Err(io::Error::from(kind)),
            }
        }
    }

    impl Read for Script {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.next(out.len())?;
            out[..n].fill(7);
            Ok(n)
        }
    }

    impl Write for Script {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.next(data.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn reads_until_it_would_block_or_the_stream_ends() {
        use ErrorKind::{ConnectionReset, Interrupted, WouldBlock};
        let mut buf = Vec::new();
        assert!(read_available(
            &mut Script(vec![Ok(3), Err(Interrupted), Ok(2), Err(WouldBlock)]),
            &mut buf
        ));
        assert_eq!(buf, [7; 5]);
        assert!(!read_available(&mut Script(vec![Ok(1), Ok(0)]), &mut buf));
        assert!(!read_available(&mut Script(vec![Err(ConnectionReset)]), &mut buf));
        assert_eq!(buf.len(), 6);
    }

    #[test]
    fn writes_until_it_would_block_or_the_stream_breaks() {
        use ErrorKind::{BrokenPipe, Interrupted, WouldBlock};
        let data = [1u8; 10];
        let mut pos = 0;
        assert!(write_pending(
            &mut Script(vec![Ok(4), Err(Interrupted), Err(WouldBlock)]),
            &data,
            &mut pos
        ));
        assert_eq!(pos, 4);
        assert!(write_pending(&mut Script(vec![Ok(6)]), &data, &mut pos));
        assert_eq!(pos, 10);
        let mut pos = 0;
        assert!(!write_pending(&mut Script(vec![Ok(2), Err(BrokenPipe)]), &data, &mut pos));
        assert_eq!(pos, 2);
    }
}
