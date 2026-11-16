//! What the code that serves a port's connections sees of a port whose TCP is its own: the port
//! on AF_XDP with the TCP stack of [`crate::user_tcp`] ([`crate::xdp_port`], 012 FR-033). The
//! port gives the handler each connection, each arrival and each close, and the handler answers
//! through the port: [`ConnPort::send`] queues bytes, [`ConnPort::pause`] and
//! [`ConnPort::resume`] stop and restart the reading of a connection (backpressure), and
//! [`ConnPort::close`] ends it. On the engine the handler is
//! `engine::xdp_clients`, which runs each connection's session in its shard (023 FR-021).

use std::net::SocketAddr;

/// What a handler asks of a port's connections.
pub trait ConnPort {
    /// Queues `bytes` on connection `token`; `false` if it is gone or closing.
    fn send(&mut self, token: u64, bytes: &[u8]) -> bool;
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
    /// After every round of the port.
    fn woken(&mut self, net: &mut dyn ConnPort);
}
