//! The clients' port on AF_XDP (012 FR-033): one [`XdpPort`] per receive queue of the interface,
//! each with its AF_XDP socket and UMEM ([`teseladb_sys::xdp::Xsk`]) and its own TCP stack in
//! user space ([`crate::user_tcp`]). On the engine (023 FR-021) a task of the queue's shard
//! waits on the socket with the shard's reactor, takes the frames that arrived, runs the stack's
//! timers and sends what the stack has, with no system call per connection and none per frame.
//!
//! The handler of [`crate::conn_port`] sees the same connections as on the io_uring rings. A
//! paused connection keeps what arrives in its stack, whose window closes once the buffer of
//! [`crate::user_tcp::RECEIVE_BUFFER`] bytes fills: the client then stops sending.
//!
//! A NIC keeps each connection on one queue (its RSS hashes the four addresses and ports), but an
//! interface whose queue follows the sender's choice (a `veth`, whose sender picks another queue
//! after a retransmission timeout) can move it. The ports of a listener share a table of the
//! flows each one has ([`Flows`], without a lock): a frame of no connection of its port goes to
//! the port that has its flow, as a message to that port's shard, and only a frame no port has is
//! the port's own (a `SYN` opens a connection, anything else is reset).
//!
//! [`XdpListener::open`] loads and attaches the XDP program and makes one port per queue; it
//! fails with what is missing (the kernel, the interface, the driver or the permissions), and the
//! caller then keeps the kernel's TCP for the port.

use std::collections::HashSet;
use std::io;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use std::os::fd::{AsFd, OwnedFd};

use teseladb_sys::xdp::{AttachMode, XdpProgram, Xsk, XskStats};

use crate::conn_port::{ConnPort, NetHandler};
use crate::user_tcp::{Arrival, Event, TcpStack, flow_of};

/// Slots of the table of flows: a power of two, eight bytes each.
const FLOW_SLOTS: usize = 1 << 17;
/// Slots a lookup tries before it gives up.
const FLOW_PROBES: usize = 64;
const EMPTY: u64 = 0;
const GONE: u64 = u64::MAX;
/// The low byte of an entry names its port (plus one); the rest is the flow's.
const PORT_BITS: u64 = 0xff;

/// Which port of a listener has each flow, shared by the ports without a lock: open addressing
/// over atomics, written only by the port that has the flow, read by the others as a hint.
#[derive(Debug)]
pub struct Flows {
    slots: Box<[AtomicU64]>,
}

impl Default for Flows {
    fn default() -> Flows {
        Flows { slots: (0..FLOW_SLOTS).map(|_| AtomicU64::new(EMPTY)).collect() }
    }
}

impl Flows {
    fn tag(flow: u64) -> u64 {
        // Neither empty nor, with any port, gone.
        const TOP: u64 = GONE & !PORT_BITS;
        match flow & !PORT_BITS {
            0 => PORT_BITS + 1,
            TOP => TOP - (PORT_BITS + 1),
            t => t,
        }
    }

    fn probe(flow: u64) -> impl Iterator<Item = usize> {
        let start = (flow >> 8) as usize;
        (0..FLOW_PROBES).map(move |i| (start + i) & (FLOW_SLOTS - 1))
    }

    /// Port `port` (below 255) has `flow` now; `false` if the table has no room near it.
    pub fn insert(&self, flow: u64, port: u32) -> bool {
        let entry = Flows::tag(flow) | (u64::from(port) + 1).min(PORT_BITS);
        for i in Flows::probe(flow) {
            let slot = &self.slots[i];
            let seen = slot.load(Ordering::Acquire);
            if seen & !PORT_BITS == Flows::tag(flow) {
                slot.store(entry, Ordering::Release);
                return true;
            }
            if (seen == EMPTY || seen == GONE)
                && slot.compare_exchange(seen, entry, Ordering::AcqRel, Ordering::Acquire).is_ok()
            {
                return true;
            }
        }
        false
    }

    /// `flow` is gone from port `port`.
    pub fn remove(&self, flow: u64, port: u32) {
        let entry = Flows::tag(flow) | (u64::from(port) + 1).min(PORT_BITS);
        for i in Flows::probe(flow) {
            let slot = &self.slots[i];
            match slot.load(Ordering::Acquire) {
                EMPTY => return,
                e if e == entry => {
                    let _ = slot.compare_exchange(e, GONE, Ordering::AcqRel, Ordering::Acquire);
                    return;
                }
                _ => {}
            }
        }
    }

    /// The port that has `flow`, if one does.
    pub fn owner(&self, flow: u64) -> Option<u32> {
        for i in Flows::probe(flow) {
            match self.slots[i].load(Ordering::Acquire) {
                EMPTY => return None,
                e if e & !PORT_BITS == Flows::tag(flow) => return Some((e & PORT_BITS) as u32 - 1),
                _ => {}
            }
        }
        None
    }
}

/// Longest wait while frames wait for room in the transmit ring.
const TX_RETRY: Duration = Duration::from_micros(200);

/// The XDP program of a port, attached to its interface, and one port per queue.
#[derive(Debug)]
pub struct XdpListener {
    /// Taken off the interface when the listener goes.
    pub program: XdpProgram,
    /// The ports, by queue.
    pub ports: Vec<XdpPort>,
}

impl XdpListener {
    /// Sends the IPv4 TCP segments to `port` (of `address` only, if given) that arrive on
    /// interface `interface` to ports of their own, at most `max_connections` each.
    ///
    /// # Errors
    ///
    /// Why the interface cannot take AF_XDP: no such interface, a kernel without AF_XDP or
    /// `bpf_link`, a program the verifier refuses, or missing `CAP_NET_ADMIN` and `CAP_BPF`.
    pub fn open(
        interface: &str,
        port: u16,
        address: Option<Ipv4Addr>,
        max_connections: usize,
        secret: u64,
    ) -> io::Result<XdpListener> {
        let what = |step: &'static str| {
            move |e: io::Error| io::Error::new(e.kind(), format!("{step}: {e}"))
        };
        teseladb_sys::xdp::check_support().map_err(io::Error::other)?;
        let ifindex = teseladb_sys::xdp::if_index(interface).map_err(what("the interface"))?;
        let queues = teseladb_sys::xdp::rx_queues(interface).map_err(what("its queues"))?;
        let mtu = teseladb_sys::xdp::mtu(interface).map_err(what("its MTU"))?;
        let mut program =
            XdpProgram::load(port, address, queues).map_err(what("loading the XDP program"))?;
        let flows = Arc::new(Flows::default());
        let mut ports = Vec::with_capacity(queues as usize);
        for q in 0..queues {
            let xsk = Xsk::open(ifindex, q).map_err(what("the AF_XDP socket"))?;
            program.insert(q, &xsk).map_err(what("the socket map"))?;
            // Each queue's stack numbers its connections apart, with a secret of its own.
            let stack = TcpStack::new(port, address, mtu, max_connections, secret ^ u64::from(q));
            ports.push(XdpPort::new(xsk, stack, q, flows.clone()));
        }
        program.attach(ifindex).map_err(what("attaching the XDP program"))?;
        Ok(XdpListener { program, ports })
    }

    /// The mode the program runs in.
    pub fn mode(&self) -> Option<AttachMode> {
        self.program.mode()
    }
}

/// One queue of the port: its socket and its TCP stack. Whoever runs it waits on its socket
/// ([`XdpPort::socket`]) and calls [`XdpPort::round`] when frames came or its wait ended; on the
/// engine, a task of the queue's shard (023 FR-021).
#[derive(Debug)]
pub struct XdpPort {
    xsk: Xsk,
    stack: TcpStack,
    queue: u32,
    paused: HashSet<u64>,
    /// Connections resumed with bytes in their stack, handed over at the next round.
    resumed: Vec<u64>,
    started: Instant,
    /// A frame the transmit ring had no room for: the next round waits less.
    backlog: bool,
    flows: Arc<Flows>,
    strangers: Vec<Vec<u8>>,
}

impl XdpPort {
    fn new(xsk: Xsk, stack: TcpStack, queue: u32, flows: Arc<Flows>) -> XdpPort {
        XdpPort {
            flows,
            strangers: Vec::new(),
            xsk,
            stack,
            queue,
            paused: HashSet::new(),
            resumed: Vec::new(),
            started: Instant::now(),
            backlog: false,
        }
    }

    /// The queue it serves.
    pub fn queue(&self) -> u32 {
        self.queue
    }

    /// Whether its driver moves frames without copying them.
    pub fn zerocopy(&self) -> bool {
        self.xsk.zerocopy()
    }

    /// Whether its UMEM is on 2 MiB pages (FR-032).
    pub fn on_huge_pages(&self) -> bool {
        self.xsk.on_huge_pages()
    }

    /// The kernel's counters of its socket.
    ///
    /// # Errors
    ///
    /// The error of `getsockopt`.
    pub fn stats(&self) -> io::Result<XskStats> {
        self.xsk.stats()
    }

    /// A copy of its socket, for a reactor to wait on until frames arrive.
    ///
    /// # Errors
    ///
    /// The error of `dup`.
    pub fn socket(&self) -> io::Result<OwnedFd> {
        self.xsk.as_fd().try_clone_to_owned()
    }

    fn now(&self) -> u64 {
        u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX)
    }

    /// How long it may wait for frames before its next round, at most `timeout`: less while
    /// its stack has a timer due or frames wait for room in the transmit ring.
    pub fn wait_at_most(&self, timeout: Duration) -> Duration {
        let now = self.now();
        let mut wait = timeout;
        if let Some(d) = self.stack.next_deadline() {
            wait = wait.min(Duration::from_micros(d.saturating_sub(now)));
        }
        if self.backlog || self.xsk.sending() > 0 {
            wait = wait.min(TX_RETRY);
        }
        wait
    }

    /// One round: takes the frames of the socket and `forwarded` (those other ports got for
    /// this port's connections), hands a frame of another port's connection to `forward` with
    /// that port's queue, runs the stack's timers, hands what happened to `handler`, then sends
    /// what the stack has.
    ///
    /// # Errors
    ///
    /// The error of the socket's send; the port is then unusable.
    pub fn round(
        &mut self,
        handler: &mut dyn NetHandler,
        forwarded: Vec<Vec<u8>>,
        forward: &mut dyn FnMut(u32, Vec<u8>),
    ) -> io::Result<()> {
        let now = self.now();
        self.take_frames(now, forwarded, forward);
        self.stack.tick(now);
        self.deliver(handler);
        handler.woken(self);
        self.deliver(handler);
        self.transmit(now)
    }

    /// Hands the stack's events to `handler` until none is left.
    fn deliver(&mut self, handler: &mut dyn NetHandler) {
        loop {
            for token in std::mem::take(&mut self.resumed) {
                if !self.paused.contains(&token) {
                    let bytes = self.stack.read(token);
                    if !bytes.is_empty() {
                        handler.received(self, token, &bytes);
                    }
                }
            }
            let Some(e) = self.stack.event() else { break };
            match e {
                Event::Accepted { token, peer, local } => {
                    if !handler.accepted(self, token, Some(peer), Some(local)) {
                        self.stack.close(token);
                    }
                }
                Event::Readable(token) => {
                    if !self.paused.contains(&token) {
                        let bytes = self.stack.read(token);
                        if !bytes.is_empty() {
                            handler.received(self, token, &bytes);
                        }
                    }
                }
                Event::Sent(token) => handler.sent(self, token),
                Event::Closed(token) => {
                    self.paused.remove(&token);
                    handler.closed(self, token);
                }
            }
        }
    }

    /// Gives the stack the frames of the socket and those other ports forwarded; a frame of a
    /// connection of another port goes to it.
    fn take_frames(
        &mut self,
        now: u64,
        forwarded: Vec<Vec<u8>>,
        forward: &mut dyn FnMut(u32, Vec<u8>),
    ) {
        let (stack, strangers) = (&mut self.stack, &mut self.strangers);
        self.xsk.receive(&mut |frame| {
            if stack.receive(frame, now) == Arrival::Stranger {
                strangers.push(frame.to_vec());
            }
        });
        for frame in std::mem::take(&mut self.strangers) {
            match flow_of(&frame).and_then(|f| self.flows.owner(f)).filter(|o| *o != self.queue) {
                Some(owner) => forward(owner, frame),
                None => self.stack.adopt(&frame, now),
            }
        }
        for frame in forwarded {
            if self.stack.receive(&frame, now) == Arrival::Stranger {
                self.stack.adopt(&frame, now);
            }
        }
        self.publish_flows();
    }

    /// Writes the flows the stack took and forgot into the listener's table.
    fn publish_flows(&mut self) {
        let (born, died) = self.stack.take_flows();
        for f in died {
            self.flows.remove(f, self.queue);
        }
        for f in born {
            self.flows.insert(f, self.queue);
        }
    }

    /// Sends what the stack has and takes back the frames the kernel sent.
    fn transmit(&mut self, now: u64) -> io::Result<()> {
        let xsk = &mut self.xsk;
        let mut full = false;
        self.stack.transmit(now, &mut |f| {
            let ok = xsk.send(f);
            full |= !ok;
            ok
        });
        self.backlog = full;
        self.publish_flows();
        self.xsk.flush()
    }
}

impl ConnPort for XdpPort {
    fn send(&mut self, token: u64, bytes: &[u8]) -> bool {
        self.stack.send(token, bytes)
    }

    fn queued(&self, token: u64) -> usize {
        self.stack.queued(token)
    }

    fn pause(&mut self, token: u64) {
        self.paused.insert(token);
    }

    fn resume(&mut self, token: u64) {
        if self.paused.remove(&token) {
            self.resumed.push(token);
        }
    }

    fn close(&mut self, token: u64) {
        self.paused.remove(&token);
        self.stack.close(token);
    }

    fn stop_accepting(&mut self) {
        self.stack.stop_accepting();
    }

    fn connections(&self) -> usize {
        self.stack.open()
    }
}
