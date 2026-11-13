//! A port's connections on one io_uring ring (012 FR-024): the ring accepts them with a multishot
//! `accept`, reads each with a multishot `recv` into a provided buffer ring, and writes their
//! replies with `sendmsg`; one thread serves every connection of its ring, with no thread per
//! connection.
//!
//! What the bytes mean is the [`NetHandler`]'s business. The ring gives it each connection, each
//! arrival and each close, and the handler answers through the [`NetRing`]: [`NetRing::send`]
//! queues bytes, [`NetRing::pause`] and [`NetRing::resume`] stop and restart the reading of a
//! connection (backpressure: a connection whose input waits, or whose replies the client does
//! not read, stops being read), and [`NetRing::close`] ends it. The bytes of a provided buffer are
//! copied out and the buffer goes back to the kernel at once, so a slow connection never holds
//! the buffers of the others.
//!
//! Other threads wake the ring through the [`Waker`] of [`NetRing::waker`], whose selector the
//! ring watches with a poll: the handler's [`NetHandler::woken`] then runs, as it does after every
//! round.

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Duration;

use teseladb_sys::uring::{Completion, Op, Part, Ring, opcode};

use crate::poller::{Poller, Waker};

/// Submission slots of a port's ring.
pub const NET_RING_ENTRIES: u32 = 256;
/// Buffers of a ring's provided buffer ring.
pub const RECV_BUFFERS: u16 = 128;
/// Bytes of each provided buffer.
pub const RECV_BUFFER_BYTES: u32 = 16 * 1024;
/// The provided buffer ring's group.
const GROUP: u16 = 0;
/// Most `iovec`s of one `sendmsg` (`UIO_MAXIOV`).
const MAX_IOV: usize = 1024;

/// Whether this kernel has what a port's ring needs (multishot `recv` and `SEND_ZC` came in
/// Linux 6.0, provided buffer rings in 5.19): `Err` names what is missing.
pub fn check_support() -> Result<(), String> {
    let mut ring = Ring::new(4).map_err(|e| format!("io_uring_setup failed: {e}"))?;
    let probe = ring.probe().map_err(|e| format!("IORING_REGISTER_PROBE failed: {e}"))?;
    let needed = [
        (opcode::ACCEPT, "IORING_OP_ACCEPT"),
        (opcode::RECV, "IORING_OP_RECV"),
        (opcode::SENDMSG, "IORING_OP_SENDMSG"),
        (opcode::POLL_ADD, "IORING_OP_POLL_ADD"),
        (opcode::ASYNC_CANCEL, "IORING_OP_ASYNC_CANCEL"),
        (opcode::SEND_ZC, "IORING_OP_SEND_ZC (Linux 6.0, with multishot recv)"),
    ];
    let missing: Vec<&str> =
        needed.iter().filter(|(op, _)| !probe.supports(*op)).map(|(_, n)| *n).collect();
    if !missing.is_empty() {
        return Err(format!("missing {}", missing.join(", ")));
    }
    ring.register_buf_ring(GROUP, 2, 64)
        .map_err(|e| format!("IORING_REGISTER_PBUF_RING failed: {e}"))
}

/// What a port's ring tells the code that serves its connections.
pub trait NetHandler {
    /// A new connection `token` from `peer` to `local`; `false` closes it (and
    /// [`NetHandler::closed`] follows for it too).
    fn accepted(
        &mut self,
        net: &mut NetRing,
        token: u64,
        peer: Option<SocketAddr>,
        local: Option<SocketAddr>,
    ) -> bool;
    /// Bytes that arrived on connection `token`, in order.
    fn received(&mut self, net: &mut NetRing, token: u64, bytes: &[u8]);
    /// A send of connection `token` finished; [`NetRing::queued`] says what still waits.
    fn sent(&mut self, net: &mut NetRing, token: u64);
    /// Connection `token` is gone: the peer closed it, it failed, or [`NetRing::close`] ended it.
    fn closed(&mut self, net: &mut NetRing, token: u64);
    /// After every round of the ring, and when another thread woke it.
    fn woken(&mut self, net: &mut NetRing);
}

/// What an operation of the ring is for.
#[derive(Clone, Copy, Debug)]
enum Purpose {
    Accept(usize),
    Recv(u64),
    Send(u64),
    Wake,
    Cancel,
}

#[derive(Debug)]
struct NetConn {
    stream: std::net::TcpStream,
    /// The multishot `recv` running, if any.
    recv: Option<u64>,
    /// Whether the handler wants it read.
    reading: bool,
    /// The `sendmsg` in flight.
    send: Option<u64>,
    /// Bytes waiting for the next `sendmsg`.
    queue: Vec<Part>,
    /// Bytes queued or in flight.
    queued: usize,
    /// Bytes of the `sendmsg` in flight.
    sending: usize,
    /// Ends once its operations have.
    closing: bool,
    /// What arrived after [`NetRing::pause`] (the reception ends a little later): handed over at
    /// [`NetRing::resume`].
    stash: Vec<u8>,
    /// The first byte of `stash` not handed over yet.
    stash_at: usize,
}

/// A port's connections on one ring.
pub struct NetRing {
    ring: Ring,
    listeners: Vec<Arc<TcpListener>>,
    ops: HashMap<u64, Purpose>,
    conns: HashMap<u64, NetConn>,
    next_token: u64,
    poller: Poller,
    /// Whether the ring accepts new connections.
    accepting: bool,
    /// Connections that ended, for [`NetHandler::closed`].
    ended: Vec<u64>,
    /// Bytes copied out of a provided buffer.
    scratch: Vec<u8>,
    /// Connections resumed with bytes in their stash.
    unstash: Vec<u64>,
}

impl std::fmt::Debug for NetRing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetRing").field("connections", &self.conns.len()).finish()
    }
}

impl NetRing {
    /// A ring that accepts on `listeners` (which may be shared with other rings: each connection
    /// goes to one of them).
    pub fn new(listeners: Vec<Arc<TcpListener>>) -> io::Result<NetRing> {
        let mut ring = Ring::new(NET_RING_ENTRIES)?;
        ring.register_buf_ring(GROUP, RECV_BUFFERS, RECV_BUFFER_BYTES)?;
        let mut net = NetRing {
            ring,
            listeners,
            ops: HashMap::new(),
            conns: HashMap::new(),
            next_token: 1,
            poller: Poller::new()?,
            accepting: true,
            ended: Vec::new(),
            scratch: Vec::with_capacity(RECV_BUFFER_BYTES as usize),
            unstash: Vec::new(),
        };
        for i in 0..net.listeners.len() {
            let fd = net.listeners[i].as_raw_fd();
            net.start(Op::AcceptMulti { fd }, Purpose::Accept(i))?;
        }
        let fd = net.poller.as_raw_fd();
        net.start(Op::PollIn { fd }, Purpose::Wake)?;
        Ok(net)
    }

    /// Wakes the ring from another thread.
    pub fn waker(&self) -> Waker {
        self.poller.waker()
    }

    /// Open connections.
    pub fn connections(&self) -> usize {
        self.conns.len()
    }

    /// Pushes `op`, submitting what waits when the queue is full.
    fn start(&mut self, mut op: Op, purpose: Purpose) -> io::Result<u64> {
        loop {
            match self.ring.push(op) {
                Ok(id) => {
                    self.ops.insert(id, purpose);
                    return Ok(id);
                }
                Err((back, _)) if self.ring.space() == 0 => {
                    self.ring.submit(0)?;
                    op = back;
                }
                Err((_, why)) => return Err(io::Error::other(why)),
            }
        }
    }

    /// Queues `bytes` on connection `token`; `false` if it is gone or closing.
    pub fn send(&mut self, token: u64, bytes: Part) -> bool {
        let Some(c) = self.conns.get_mut(&token).filter(|c| !c.closing) else { return false };
        c.queued += bytes.bytes().len();
        c.queue.push(bytes);
        if c.send.is_none() {
            self.flush(token);
        }
        true
    }

    /// Bytes of connection `token` queued or being sent.
    pub fn queued(&self, token: u64) -> usize {
        self.conns.get(&token).map_or(0, |c| c.queued)
    }

    /// Stops reading connection `token`: no more [`NetHandler::received`] for it until
    /// [`NetRing::resume`], and what arrives waits in the socket.
    pub fn pause(&mut self, token: u64) {
        let Some(c) = self.conns.get_mut(&token) else { return };
        c.reading = false;
        if let Some(id) = c.recv {
            // The reception ends with `ECANCELED`; a failed push leaves it running, and the next
            // arrival is still handed over.
            let _ = self.start(Op::Cancel { id }, Purpose::Cancel);
        }
    }

    /// Reads connection `token` again; what arrived while it was paused comes first.
    pub fn resume(&mut self, token: u64) {
        let Some(c) = self.conns.get_mut(&token) else { return };
        c.reading = true;
        if c.stash_at < c.stash.len() {
            self.unstash.push(token);
        }
        self.arm_recv(token);
    }

    /// Ends connection `token` once what is queued has gone out; [`NetHandler::closed`] follows.
    pub fn close(&mut self, token: u64) {
        let Some(c) = self.conns.get_mut(&token) else { return };
        c.closing = true;
        c.reading = false;
        if let Some(id) = c.recv {
            let _ = self.start(Op::Cancel { id }, Purpose::Cancel);
        }
        self.finish_if_idle(token);
    }

    /// Stops accepting connections; those open go on.
    pub fn stop_accepting(&mut self) {
        if !std::mem::replace(&mut self.accepting, false) {
            return;
        }
        let accepts: Vec<u64> = self
            .ops
            .iter()
            .filter(|(_, p)| matches!(p, Purpose::Accept(_)))
            .map(|(id, _)| *id)
            .collect();
        for id in accepts {
            let _ = self.start(Op::Cancel { id }, Purpose::Cancel);
        }
    }

    fn arm_recv(&mut self, token: u64) {
        let Some(c) = self.conns.get(&token) else { return };
        if c.recv.is_some() || !c.reading || c.closing {
            return;
        }
        let fd = c.stream.as_raw_fd();
        match self.start(Op::RecvMulti { fd, group: GROUP }, Purpose::Recv(token)) {
            Ok(id) => {
                if let Some(c) = self.conns.get_mut(&token) {
                    c.recv = Some(id);
                }
            }
            Err(_) => self.close(token),
        }
    }

    /// Sends what waits on connection `token` in one `sendmsg` (up to [`MAX_IOV`] parts).
    fn flush(&mut self, token: u64) {
        let Some(c) = self.conns.get_mut(&token) else { return };
        if c.send.is_some() || c.queue.is_empty() {
            return;
        }
        let rest = c.queue.split_off(c.queue.len().min(MAX_IOV));
        let parts = std::mem::replace(&mut c.queue, rest);
        c.sending = parts.iter().map(|p| p.bytes().len()).sum();
        let fd = c.stream.as_raw_fd();
        match self.start(Op::SendMsg { fd, parts }, Purpose::Send(token)) {
            Ok(id) => {
                if let Some(c) = self.conns.get_mut(&token) {
                    c.send = Some(id);
                }
            }
            Err(_) => {
                if let Some(c) = self.conns.get_mut(&token) {
                    c.queued = 0;
                }
                self.close(token);
            }
        }
    }

    /// Removes connection `token` if it is closing and no operation of it runs.
    fn finish_if_idle(&mut self, token: u64) {
        let idle = self
            .conns
            .get(&token)
            .is_some_and(|c| c.closing && c.recv.is_none() && c.send.is_none());
        if idle {
            // The socket closes here, after the kernel's last use of it.
            self.conns.remove(&token);
            self.ended.push(token);
        }
    }

    /// One round: submits, waits up to `timeout` for a completion, and hands every completion
    /// to `handler`, then calls [`NetHandler::woken`].
    pub fn run_once(&mut self, handler: &mut impl NetHandler, timeout: Duration) -> io::Result<()> {
        self.ring.submit_and_wait_for(timeout)?;
        while let Some(c) = self.ring.complete() {
            self.on_completion(c, handler)?;
        }
        self.deliver_stashes(handler);
        self.deliver_ends(handler);
        handler.woken(self);
        self.deliver_stashes(handler);
        self.deliver_ends(handler);
        self.ring.submit(0)?;
        Ok(())
    }

    /// Hands over what arrived on the connections resumed since, a buffer's worth at a time
    /// while they are still read: a handler that pauses again stops the rest, as it stops the
    /// reception.
    fn deliver_stashes(&mut self, handler: &mut impl NetHandler) {
        while let Some(token) = self.unstash.pop() {
            while let Some(c) = self.conns.get_mut(&token).filter(|c| c.reading) {
                if c.stash_at == c.stash.len() {
                    c.stash.clear();
                    c.stash_at = 0;
                    break;
                }
                let end = c.stash.len().min(c.stash_at + RECV_BUFFER_BYTES as usize);
                let piece = c.stash[c.stash_at..end].to_vec();
                c.stash_at = end;
                handler.received(self, token, &piece);
            }
        }
    }

    fn deliver_ends(&mut self, handler: &mut impl NetHandler) {
        while let Some(token) = self.ended.pop() {
            handler.closed(self, token);
        }
    }

    fn on_completion(&mut self, c: Completion, handler: &mut impl NetHandler) -> io::Result<()> {
        let purpose = if c.more { self.ops.get(&c.id).copied() } else { self.ops.remove(&c.id) };
        match purpose {
            Some(Purpose::Accept(i)) => self.on_accept(i, c, handler),
            Some(Purpose::Recv(token)) => self.on_recv(token, c, handler),
            Some(Purpose::Send(token)) => self.on_send(token, &c, handler),
            Some(Purpose::Wake) => {
                // Drain the selector (its waker's counter), then watch it again.
                self.poller.poll(Some(Duration::ZERO))?;
                let fd = self.poller.as_raw_fd();
                self.start(Op::PollIn { fd }, Purpose::Wake)?;
            }
            Some(Purpose::Cancel) | None => {}
        }
        Ok(())
    }

    fn on_accept(&mut self, i: usize, c: Completion, handler: &mut impl NetHandler) {
        if let Some(fd) = c.fd {
            let stream = std::net::TcpStream::from(fd);
            let token = self.next_token;
            self.next_token += 1;
            let conn = NetConn {
                stream,
                recv: None,
                reading: true,
                send: None,
                queue: Vec::new(),
                queued: 0,
                sending: 0,
                closing: false,
                stash: Vec::new(),
                stash_at: 0,
            };
            let ready = conn.stream.set_nodelay(true).is_ok();
            let (peer, local) = (conn.stream.peer_addr().ok(), conn.stream.local_addr().ok());
            self.conns.insert(token, conn);
            let keep = ready && handler.accepted(self, token, peer, local);
            if keep {
                self.arm_recv(token);
            } else {
                self.close(token);
            }
        }
        // An accept that ended (an error, or no room for the connection) starts again while the
        // ring accepts.
        if !c.more && self.accepting {
            let fd = self.listeners[i].as_raw_fd();
            if self.start(Op::AcceptMulti { fd }, Purpose::Accept(i)).is_err() {
                self.accepting = false;
            }
        }
    }

    fn on_recv(&mut self, token: u64, c: Completion, handler: &mut impl NetHandler) {
        if let Some(bid) = c.buffer {
            self.scratch.clear();
            if let Some(bytes) = self.ring.provided(GROUP, bid) {
                self.scratch.extend_from_slice(bytes);
            }
            self.ring.recycle(GROUP, bid);
            match self.conns.get_mut(&token) {
                Some(c) if !c.reading => c.stash.extend_from_slice(&self.scratch),
                Some(_) if !self.scratch.is_empty() => {
                    let bytes = std::mem::take(&mut self.scratch);
                    handler.received(self, token, &bytes);
                    self.scratch = bytes;
                }
                _ => {}
            }
        }
        if c.more {
            return;
        }
        let Some(conn) = self.conns.get_mut(&token) else { return };
        conn.recv = None;
        match c.result {
            // The peer closed its side.
            Ok(0) => {
                conn.closing = true;
                conn.reading = false;
                self.finish_if_idle(token);
            }
            // Every buffer was taken at once (a burst): read on.
            Err(e) if e.raw_os_error() == Some(ENOBUFS) => self.arm_recv(token),
            // Cancelled by `pause` or `close`, or ended with data: read on if wanted.
            Ok(_) => self.arm_recv(token),
            Err(e) if e.raw_os_error() == Some(ECANCELED) => {
                self.arm_recv(token);
                self.finish_if_idle(token);
            }
            Err(_) => self.close(token),
        }
    }

    fn on_send(&mut self, token: u64, c: &Completion, handler: &mut impl NetHandler) {
        let Some(conn) = self.conns.get_mut(&token) else { return };
        conn.send = None;
        let sending = std::mem::take(&mut conn.sending);
        match c.result {
            Ok(n) if n as usize == sending => conn.queued -= sending,
            // With `MSG_WAITALL` a `sendmsg` returns short only with a failure: the rest of the
            // stream cannot follow a cut message, so the connection ends.
            _ => {
                conn.queue.clear();
                conn.queued = 0;
                conn.closing = true;
                conn.reading = false;
                if let Some(id) = conn.recv {
                    let _ = self.start(Op::Cancel { id }, Purpose::Cancel);
                }
            }
        }
        self.flush(token);
        self.finish_if_idle(token);
        if self.conns.contains_key(&token) {
            handler.sent(self, token);
        }
    }
}

const ENOBUFS: i32 = 105;
const ECANCELED: i32 = 125;
