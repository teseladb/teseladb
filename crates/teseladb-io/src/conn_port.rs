//! What the code that serves a port's connections sees of the port (012 FR-024, FR-033), the same
//! on the io_uring rings of [`crate::net_ring`] and on AF_XDP with the TCP stack of
//! [`crate::user_tcp`] ([`crate::xdp_port`]): the port gives the handler each connection, each
//! arrival and each close, and the handler answers through the port: [`ConnPort::send`] queues
//! bytes, [`ConnPort::pause`] and [`ConnPort::resume`] stop and restart the reading of a
//! connection (backpressure), and [`ConnPort::close`] ends it.

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use teseladb_sys::uring::Part;

use crate::poller::Waker;

/// What a handler asks of a port's connections.
pub trait ConnPort {
    /// Queues `bytes` on connection `token`; `false` if it is gone or closing.
    fn send(&mut self, token: u64, bytes: Part) -> bool;
    /// Bytes of connection `token` queued or being sent.
    fn queued(&self, token: u64) -> usize;
    /// Stops reading connection `token` until [`ConnPort::resume`].
    fn pause(&mut self, token: u64);
    /// Reads connection `token` again; what arrived while it was paused comes first.
    fn resume(&mut self, token: u64);
    /// Ends connection `token` once what is queued has gone out; [`NetHandler::closed`] follows.
    fn close(&mut self, token: u64);
    /// Stops accepting connections; those open go on.
    fn stop_accepting(&mut self);
    /// Open connections.
    fn connections(&self) -> usize;
}

/// A port that runs: it waits for its connections and hands what happens to a handler.
pub trait PortDriver: ConnPort {
    /// One round: waits up to `timeout`, hands what happened to `handler`, then calls
    /// [`NetHandler::woken`].
    ///
    /// # Errors
    ///
    /// The error of the port's wait; the port is then unusable.
    fn run_once(&mut self, handler: &mut dyn NetHandler, timeout: Duration) -> io::Result<()>;
    /// What wakes the port from another thread.
    fn waker(&self) -> Waker;
}

/// What a port tells the code that serves its connections.
pub trait NetHandler {
    /// A new connection `token` from `peer` to `local`; `false` closes it (and
    /// [`NetHandler::closed`] follows for it too).
    fn accepted(
        &mut self,
        net: &mut dyn ConnPort,
        token: u64,
        peer: Option<SocketAddr>,
        local: Option<SocketAddr>,
    ) -> bool;
    /// Bytes that arrived on connection `token`, in order.
    fn received(&mut self, net: &mut dyn ConnPort, token: u64, bytes: &[u8]);
    /// A send of connection `token` finished; [`ConnPort::queued`] says what still waits.
    fn sent(&mut self, net: &mut dyn ConnPort, token: u64);
    /// Connection `token` is gone: the peer closed it, it failed, or [`ConnPort::close`] ended
    /// it.
    fn closed(&mut self, net: &mut dyn ConnPort, token: u64);
    /// After every round of the port, and when another thread woke it.
    fn woken(&mut self, net: &mut dyn ConnPort);
}
