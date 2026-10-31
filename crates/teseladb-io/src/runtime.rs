//! Loop of one core: waits on the poller until the next tick or
//! deadline, feeds the core's logic with peer frames, mailbox messages and disk completions, runs
//! the ticks, and executes what the logic asks for: disk operations go to the core's disk thread
//! and frames for other nodes go to one outbound connection per node (dialed in the background,
//! redialed with a 10 ms–1 s backoff with jitter after a failure).

use std::collections::BTreeMap;
use std::io::{self, ErrorKind, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use teseladb_raftlog::{Disk, IoDone, IoOp};
use teseladb_types::{Nanos, NodeId, Rng};

use crate::disk_thread::DiskThread;
use crate::handshake::{self, Inbound, Outbound};
use crate::mailbox::{self, Mailbox, MailboxSender};
use crate::peer::{Hello, PeerMsg, decode_frame, encode_frame};
use crate::poller::{Event, Interest, Poller, WAKE_TOKEN};
use crate::secure::{Channel, PeerTls};
use crate::timer::TimerWheel;

/// Frames queued for a node that is not connected yet beyond this are dropped (Raft resends).
pub const MAX_PENDING: usize = 8 << 20;
/// First redial delay after a failure.
pub const BACKOFF_MIN_MS: u64 = 10;
/// Maximum redial delay.
pub const BACKOFF_MAX_MS: u64 = 1_000;

/// What a core does; the runtime only moves bytes and time.
pub trait CoreLogic {
    /// Messages other threads send to this core.
    type Msg: Send + 'static;
    /// Periodic tick; `wall_ms` is the node's wall clock.
    fn tick(&mut self, now: Nanos, wall_ms: u64);
    /// Next disk operation, if any.
    fn poll_io(&mut self, now: Nanos) -> Option<IoOp>;
    /// Result of a disk operation, in submission order.
    fn on_io_done(&mut self, done: IoDone, now: Nanos);
    /// Earliest instant at which `poll_io` may have something new without any input.
    fn next_deadline(&self) -> Option<Nanos>;
    /// Frame received from `from`.
    fn on_peer(&mut self, from: NodeId, msg: PeerMsg, now: Nanos);
    /// Message from another thread.
    fn on_message(&mut self, msg: Self::Msg, now: Nanos);
    /// Moves the frames to send to `out`.
    fn take_outbound(&mut self, out: &mut Vec<(NodeId, PeerMsg)>);
    /// Inter-node address of `node`, if known.
    fn peer_addr(&self, node: NodeId) -> Option<String>;
    /// The connection to `node` failed or closed: what was in flight may be lost.
    fn peer_unreachable(&mut self, node: NodeId);
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
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        RuntimeConfig { tick: Duration::from_millis(10), seed: 1, tls: None }
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
}

/// Loop of one core.
pub struct CoreRuntime<L: CoreLogic, D: Disk + Send + 'static> {
    logic: L,
    hello: Hello,
    cfg: RuntimeConfig,
    poller: Poller,
    mailbox: Mailbox<CoreInput<L::Msg>>,
    sender: CoreSender<L::Msg>,
    disk: DiskThread<D>,
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
    inputs: Vec<CoreInput<L::Msg>>,
    outbox: Vec<(NodeId, PeerMsg)>,
    events: Vec<Event>,
    fired: Vec<NodeId>,
}

fn wall_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

impl<L: CoreLogic, D: Disk + Send + 'static> CoreRuntime<L, D> {
    /// Runtime of the core named in `hello.core`, with its disk thread already started. Returns
    /// the runtime and a sender for its mailbox (for the acceptor and the node).
    pub fn new(
        logic: L,
        disk: D,
        hello: Hello,
        cfg: RuntimeConfig,
    ) -> io::Result<(CoreRuntime<L, D>, CoreSender<L::Msg>)> {
        let poller = Poller::new()?;
        let (sender, mailbox) = mailbox::channel(Arc::new(poller.waker()));
        let name = format!("disk-{}", hello.core);
        let disk = DiskThread::spawn(name, disk, sender.clone(), CoreInput::Disk)?;
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
            conns: BTreeMap::new(),
            links: BTreeMap::new(),
            learned: BTreeMap::new(),
            timers: TimerWheel::new(0),
            start: Instant::now(),
            next_token: 1,
            stopped: false,
            inputs: Vec::new(),
            outbox: Vec::new(),
            events: Vec::new(),
            fired: Vec::new(),
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
        let CoreRuntime { logic, disk, .. } = self;
        Ok((logic, disk.shutdown()))
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
        self.events.clear();
        self.events.extend_from_slice(self.poller.poll(Some(timeout))?);
        let events = std::mem::take(&mut self.events);
        for e in &events {
            if e.token != WAKE_TOKEN {
                self.on_socket(e);
            }
        }
        self.events = events;
        self.mailbox.drain_into(&mut self.inputs);
        for input in std::mem::take(&mut self.inputs) {
            self.on_input(input)?;
        }
        let now = self.now();
        if now >= self.next_tick {
            self.logic.tick(now, wall_ms());
            let step = Nanos(self.cfg.tick.as_nanos() as u64);
            self.next_tick = Nanos(self.next_tick.0 + step.0).max(Nanos(now.0 + 1));
        }
        self.timers.advance(now.as_millis(), &mut self.fired);
        for node in std::mem::take(&mut self.fired) {
            self.on_backoff_done(node);
        }
        self.pump(now);
        Ok(())
    }

    /// Hands the logic's disk operations and frames to their executors.
    fn pump(&mut self, now: Nanos) {
        while let Some(op) = self.logic.poll_io(now) {
            if self.disk.submit(op).is_err() {
                // The disk thread is gone: nothing can be made durable any more.
                self.stopped = true;
                return;
            }
        }
        self.logic.take_outbound(&mut self.outbox);
        for (node, msg) in std::mem::take(&mut self.outbox) {
            self.send_to(node, &msg);
        }
    }

    fn on_input(&mut self, input: CoreInput<L::Msg>) -> io::Result<()> {
        let now = self.now();
        match input {
            CoreInput::Disk(done) => self.logic.on_io_done(done, now),
            CoreInput::App(m) => self.logic.on_message(m, now),
            CoreInput::Stop => self.stopped = true,
            CoreInput::Inbound(Inbound { stream, channel, got }) => {
                self.learned.insert(got.hello.node_id, got.hello.peer_addr.clone());
                let token = self.add_conn(stream, channel, got.hello.node_id, false, got.rest)?;
                self.deliver(token);
            }
            CoreInput::Outbound(Outbound { node, result }) => match result {
                Ok((stream, channel, got)) => {
                    let token = self.add_conn(stream, channel, node, true, got.rest)?;
                    let link = self.links.get_mut(&node).expect("link of a dial");
                    link.state = LinkState::Up(token);
                    link.backoff_ms = 0;
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
        // A frame that cannot be encoded (too large) is the logic's bug; it is dropped like a
        // lost message rather than taking the core down.
        if encode_frame(msg, false, &mut frame).is_err() {
            return;
        }
        let link = self.links.entry(node).or_insert(Link {
            state: LinkState::Idle,
            pending: Vec::new(),
            backoff_ms: 0,
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
