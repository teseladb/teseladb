//! The implementation interfaces behind the networking API, and the API methods that
//! forward to them.
//!
//! Each handle of [`api`](crate::engine::net::api) owns a boxed implementation:
//! [`ConnectedSocketImpl`], [`SocketImpl`], [`ServerSocketImpl`], [`DatagramChannelImpl`] and
//! [`NetworkInterfaceImpl`]. The POSIX stack and the native stack each provide their own. The
//! handle methods here are one virtual call each; calling one on a handle with no
//! implementation (default-constructed or moved from) panics.
//!
//! [`register_net_metrics_for_scheduling_group`] exports the per-scheduling-group byte
//! counters of the current shard's stack.

use crate::engine::core::do_with::do_with;
use crate::engine::core::future::{ExceptionPtr, Future, make_exception_future};
use crate::engine::core::iostream::{
    DataSink, DataSource, InputStream, OutputStream, OutputStreamOptions,
};
use crate::engine::core::metrics::{self as sm, LabelInstance, MetricGroups};
use crate::engine::core::reactor::{engine, engine_is_ready};
use crate::engine::core::shared_ptr::SharedPtr;
use crate::engine::core::sstring::Sstring;
use crate::engine::net::api::{
    AcceptResult, ConnectedSocket, ConnectedSocketInputStreamConfig, Datagram, DatagramChannel,
    KeepaliveParams, NetworkInterface, ServerSocket, Socket,
};
use crate::engine::net::inet_address::InetAddress;
use crate::engine::net::packet::Packet;
use crate::engine::net::socket_defs::{SocketAddress, Transport};

/// The implementation of a [`ConnectedSocket`].
pub trait ConnectedSocketImpl {
    /// The source of the bytes received from the peer.
    fn source(&mut self) -> DataSource;
    /// The source of received bytes, tuned by `csisc`; the default ignores `csisc` and
    /// returns [`ConnectedSocketImpl::source`].
    fn source_with_config(&mut self, csisc: ConnectedSocketInputStreamConfig) -> DataSource {
        // Default implementation falls back to non-parameterized data_source
        let _ = csisc;
        self.source()
    }
    /// The sink of the bytes sent to the peer.
    fn sink(&mut self) -> DataSink;
    /// Makes current and future reads fail.
    fn shutdown_input(&mut self);
    /// Makes current and future unflushed writes fail.
    fn shutdown_output(&mut self);
    /// Sets `TCP_NODELAY`.
    ///
    /// # Errors
    ///
    /// When the option cannot be set.
    fn set_nodelay(&mut self, nodelay: bool) -> Result<(), ExceptionPtr>;
    /// Reads `TCP_NODELAY`.
    ///
    /// # Errors
    ///
    /// When the option cannot be read.
    fn get_nodelay(&self) -> Result<bool, ExceptionPtr>;
    /// Sets `SO_KEEPALIVE`.
    ///
    /// # Errors
    ///
    /// When the option cannot be set.
    fn set_keepalive(&mut self, keepalive: bool) -> Result<(), ExceptionPtr>;
    /// Reads `SO_KEEPALIVE`.
    ///
    /// # Errors
    ///
    /// When the option cannot be read.
    fn get_keepalive(&self) -> Result<bool, ExceptionPtr>;
    /// Sets the keepalive parameters.
    ///
    /// # Errors
    ///
    /// When the parameters do not match the transport or cannot be set.
    fn set_keepalive_parameters(&mut self, p: &KeepaliveParams) -> Result<(), ExceptionPtr>;
    /// Reads the keepalive parameters.
    ///
    /// # Errors
    ///
    /// When they cannot be read.
    fn get_keepalive_parameters(&self) -> Result<KeepaliveParams, ExceptionPtr>;
    /// Sets a raw socket option (`setsockopt`).
    ///
    /// # Errors
    ///
    /// When the option cannot be set.
    ///
    /// # Safety
    /// `data` points to `len` readable bytes.
    unsafe fn set_sockopt(
        &mut self,
        level: i32,
        optname: i32,
        data: *const u8,
        len: usize,
    ) -> Result<(), ExceptionPtr>;
    /// Reads a raw socket option (`getsockopt`) into `data`; returns the option length.
    ///
    /// # Errors
    ///
    /// When the option cannot be read.
    ///
    /// # Safety
    /// `data` points to `len` writable bytes.
    unsafe fn get_sockopt(
        &self,
        level: i32,
        optname: i32,
        data: *mut u8,
        len: usize,
    ) -> Result<i32, ExceptionPtr>;
    /// The local address.
    fn local_address(&self) -> SocketAddress;
    /// The peer's address.
    fn remote_address(&self) -> SocketAddress;
    /// Resolves when the peer closes or shuts down its sending side, or when
    /// [`ConnectedSocketImpl::shutdown_input`] is called.
    fn wait_input_shutdown(&mut self) -> Future<()>;
    /// The concrete implementation as `Any`, so that a layer can recognise its own sockets
    /// (TLS uses it). The default returns `None`; implementations that need it override it.
    fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
        None
    }
}

/// The implementation of a [`Socket`].
pub trait SocketImpl {
    /// Connects to `sa` from `local` over `proto`.
    fn connect(
        &mut self,
        sa: SocketAddress,
        local: SocketAddress,
        proto: Transport,
    ) -> Future<ConnectedSocket>;
    /// Sets `SO_REUSEADDR`.
    ///
    /// # Errors
    ///
    /// When the option cannot be set.
    fn set_reuseaddr(&mut self, reuseaddr: bool) -> Result<(), ExceptionPtr>;
    /// Reads `SO_REUSEADDR`.
    ///
    /// # Errors
    ///
    /// When the option cannot be read.
    fn get_reuseaddr(&self) -> Result<bool, ExceptionPtr>;
    /// Cancels a connection attempt in progress or terminates the established connection.
    ///
    /// # Errors
    ///
    /// When the socket cannot be shut down.
    fn shutdown(&mut self) -> Result<(), ExceptionPtr>;
}

/// The implementation of a [`ServerSocket`].
pub trait ServerSocketImpl {
    /// Accepts the next connection.
    fn accept(&mut self) -> Future<AcceptResult>;
    /// Makes current and future accepts fail.
    fn abort_accept(&mut self);
    /// The bound local address.
    ///
    /// # Errors
    ///
    /// The `getsockname` error of a POSIX listener.
    fn local_address(&self) -> Result<SocketAddress, ExceptionPtr>;
}

/// The implementation of a [`DatagramChannel`].
pub trait DatagramChannelImpl {
    /// The bound local address.
    fn local_address(&self) -> SocketAddress;
    /// Receives the next datagram.
    fn receive(&mut self) -> Future<Datagram>;
    /// Sends `msg` to `dst` without copying it; `'static` because the bytes are referenced
    /// until the send completes.
    fn send_str(&mut self, dst: &SocketAddress, msg: &'static str) -> Future<()>;
    /// Sends the packet `p` to `dst`.
    fn send(&mut self, dst: &SocketAddress, p: Packet) -> Future<()>;
    /// Makes a pending receive complete (possibly with an error).
    fn shutdown_input(&mut self);
    /// Makes a pending send complete (possibly with an error).
    fn shutdown_output(&mut self);
    /// Whether the channel is closed.
    fn is_closed(&self) -> bool;
    /// Closes the channel and releases its resources.
    fn close(&mut self);
}

/// The implementation of a UDP channel.
pub type UdpChannelImpl = dyn DatagramChannelImpl;

/// The implementation of a [`NetworkInterface`].
pub trait NetworkInterfaceImpl {
    /// The interface index.
    fn index(&self) -> u32;
    /// The MTU.
    fn mtu(&self) -> u32;
    /// The name.
    fn name(&self) -> &Sstring;
    /// The display name.
    fn display_name(&self) -> &Sstring;
    /// The addresses assigned to the interface.
    fn addresses(&self) -> &Vec<InetAddress>;
    /// The hardware (MAC) address.
    fn hardware_address(&self) -> Vec<u8>;
    /// Whether it is a loopback interface.
    fn is_loopback(&self) -> bool;
    /// Whether it is a virtual interface.
    fn is_virtual(&self) -> bool;
    /// Whether it is up.
    fn is_up(&self) -> bool;
    /// Whether it has IPv6 support.
    fn supports_ipv6(&self) -> bool;
}

/// The implementation of an API object. An object with no implementation (default-constructed
/// or moved from) panics, as `DataSource` and `DataSink` do, since a safe function cannot
/// have undefined behaviour. The check is one comparison per call; the panic path is cold.
#[inline]
pub(crate) fn impl_of<T>(o: Option<T>) -> T {
    match o {
        Some(v) => v,
        None => no_implementation(),
    }
}

#[cold]
#[inline(never)]
fn no_implementation() -> ! {
    panic!("network API object used without an implementation (default-constructed or moved from)")
}

impl DatagramChannel {
    /// A channel with no implementation.
    pub fn new() -> Self {
        DatagramChannel { _impl: None }
    }

    /// Wraps an implementation.
    pub fn from_impl(impl_: Box<dyn DatagramChannelImpl>) -> Self {
        DatagramChannel { _impl: Some(impl_) }
    }

    fn imp(&mut self) -> &mut dyn DatagramChannelImpl {
        impl_of(self._impl.as_deref_mut())
    }

    /// The bound local address; the default address for a channel with no implementation.
    pub fn local_address(&self) -> SocketAddress {
        if let Some(i) = &self._impl { i.local_address() } else { SocketAddress::default() }
    }

    /// Receives the next datagram.
    ///
    /// # Panics
    ///
    /// If the channel has no implementation.
    pub fn receive(&mut self) -> Future<Datagram> {
        self.imp().receive()
    }

    /// Sends `msg` to `dst` without copying it.
    ///
    /// # Panics
    ///
    /// If the channel has no implementation.
    pub fn send_str(&mut self, dst: &SocketAddress, msg: &'static str) -> Future<()> {
        self.imp().send_str(dst, msg)
    }

    /// Sends the packet `p` to `dst`.
    ///
    /// # Panics
    ///
    /// If the channel has no implementation.
    pub fn send(&mut self, dst: &SocketAddress, p: Packet) -> Future<()> {
        self.imp().send(dst, p)
    }

    /// Whether the channel is closed.
    ///
    /// # Panics
    ///
    /// If the channel has no implementation.
    pub fn is_closed(&self) -> bool {
        impl_of(self._impl.as_ref()).is_closed()
    }

    /// Causes a pending receive() to complete (possibly with an exception)
    ///
    /// # Panics
    ///
    /// If the channel has no implementation.
    pub fn shutdown_input(&mut self) {
        self.imp().shutdown_input();
    }

    /// Causes a pending send() to complete (possibly with an exception)
    ///
    /// # Panics
    ///
    /// If the channel has no implementation.
    pub fn shutdown_output(&mut self) {
        self.imp().shutdown_output();
    }

    /// Closes the channel and releases all resources. Call it only when no `send` or
    /// `receive` is pending; the shutdown calls make pending ones complete soon.
    ///
    /// # Panics
    ///
    /// If the channel has no implementation.
    pub fn close(&mut self) {
        self.imp().close()
    }
}

impl ConnectedSocket {
    /// A socket that corresponds to no connection.
    pub fn new() -> Self {
        ConnectedSocket { _csi: None }
    }

    /// Wraps an implementation.
    pub fn from_impl(csi: Box<dyn ConnectedSocketImpl>) -> Self {
        ConnectedSocket { _csi: Some(csi) }
    }

    fn csi(&self) -> &dyn ConnectedSocketImpl {
        impl_of(self._csi.as_deref())
    }

    fn csi_mut(&mut self) -> &mut dyn ConnectedSocketImpl {
        impl_of(self._csi.as_deref_mut())
    }

    /// The stream of the data sent by the peer, buffered as `csisc` suggests.
    ///
    /// The methods below that read or change the connection panic if the socket has no
    /// implementation (see [`ConnectedSocket::is_valid`]).
    pub fn input(&mut self, csisc: ConnectedSocketInputStreamConfig) -> InputStream<u8> {
        InputStream::<u8>::new(self.csi_mut().source_with_config(csisc))
    }

    /// The stream that sends data to the peer, buffering up to `buffer_size` bytes and
    /// batching flushes.
    pub fn output(&mut self, buffer_size: usize) -> OutputStream<u8> {
        let opts = OutputStreamOptions { batch_flushes: true, ..Default::default() };
        // TODO: allow user to determine buffer size etc
        OutputStream::<u8>::with_options(self.csi_mut().sink(), buffer_size, opts)
    }

    /// Sets `TCP_NODELAY` (disables Nagle's algorithm when `true`).
    ///
    /// # Errors
    ///
    /// When the option cannot be set.
    pub fn set_nodelay(&mut self, nodelay: bool) -> Result<(), ExceptionPtr> {
        self.csi_mut().set_nodelay(nodelay)
    }

    /// Whether `TCP_NODELAY` is set.
    ///
    /// # Errors
    ///
    /// When the option cannot be read.
    pub fn get_nodelay(&self) -> Result<bool, ExceptionPtr> {
        self.csi().get_nodelay()
    }

    /// Sets `SO_KEEPALIVE` (enables the keepalive timer).
    ///
    /// # Errors
    ///
    /// When the option cannot be set.
    pub fn set_keepalive(&mut self, keepalive: bool) -> Result<(), ExceptionPtr> {
        self.csi_mut().set_keepalive(keepalive)
    }

    /// Whether `SO_KEEPALIVE` is set.
    ///
    /// # Errors
    ///
    /// When the option cannot be read.
    pub fn get_keepalive(&self) -> Result<bool, ExceptionPtr> {
        self.csi().get_keepalive()
    }

    /// Sets the keepalive parameters.
    ///
    /// # Errors
    ///
    /// When the parameters do not match the transport or cannot be set.
    pub fn set_keepalive_parameters(&mut self, p: &KeepaliveParams) -> Result<(), ExceptionPtr> {
        self.csi_mut().set_keepalive_parameters(p)
    }

    /// The keepalive parameters.
    ///
    /// # Errors
    ///
    /// When they cannot be read.
    pub fn get_keepalive_parameters(&self) -> Result<KeepaliveParams, ExceptionPtr> {
        self.csi().get_keepalive_parameters()
    }

    /// Sets a raw socket option, as `setsockopt` does (see the Linux protocol manuals such
    /// as tcp(7) and ip(7) for the options).
    ///
    /// # Errors
    ///
    /// When the option cannot be set.
    ///
    /// # Safety
    /// `data` points to `len` readable bytes.
    pub unsafe fn set_sockopt(
        &mut self,
        level: i32,
        optname: i32,
        data: *const u8,
        len: usize,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: `data` points to `len` readable bytes (this function's contract), which is
        // the contract of the implementation's method.
        unsafe { self.csi_mut().set_sockopt(level, optname, data, len) }
    }

    /// Reads a raw socket option into `data`, as `getsockopt` does; returns the option length.
    ///
    /// # Errors
    ///
    /// When the option cannot be read.
    ///
    /// # Safety
    /// `data` points to `len` writable bytes.
    pub unsafe fn get_sockopt(
        &self,
        level: i32,
        optname: i32,
        data: *mut u8,
        len: usize,
    ) -> Result<i32, ExceptionPtr> {
        // SAFETY: `data` points to `len` writable bytes (this function's contract), which is
        // the contract of the implementation's method.
        unsafe { self.csi().get_sockopt(level, optname, data, len) }
    }

    /// The local address of the connection.
    pub fn local_address(&self) -> SocketAddress {
        self.csi().local_address()
    }

    /// The peer's address.
    pub fn remote_address(&self) -> SocketAddress {
        self.csi().remote_address()
    }

    /// Disables output: current or future writes not yet flushed fail at once. Useful to
    /// abort operations on a connection that makes no progress because the peer failed.
    pub fn shutdown_output(&mut self) {
        self.csi_mut().shutdown_output();
    }

    /// Disables input: current or future reads fail at once.
    pub fn shutdown_input(&mut self) {
        self.csi_mut().shutdown_input();
    }

    /// Resolves when the peer closes the connection or shuts down its sending side, or when
    /// [`ConnectedSocket::shutdown_input`] is called locally (see `POLLRDHUP` in poll(2)).
    ///
    /// The socket may still be readable when it resolves, so a caller may want to wait for
    /// end-of-file too. Call it at most once per socket.
    pub fn wait_input_shutdown(&mut self) -> Future<()> {
        self.csi_mut().wait_input_shutdown()
    }
}

impl Socket {
    /// Wraps an implementation.
    pub fn from_impl(si: Box<dyn SocketImpl>) -> Self {
        Socket { _si: Some(si) }
    }

    fn si(&self) -> &dyn SocketImpl {
        impl_of(self._si.as_deref())
    }

    fn si_mut(&mut self) -> &mut dyn SocketImpl {
        impl_of(self._si.as_deref_mut())
    }

    /// Attempts to connect to `sa` from `local` (unspecified to let the system pick) over
    /// `proto`, resolving to the connection.
    ///
    /// # Panics
    ///
    /// If the socket has no implementation (as do the other methods of `Socket`).
    pub fn connect(
        &mut self,
        sa: SocketAddress,
        local: SocketAddress,
        proto: Transport,
    ) -> Future<ConnectedSocket> {
        self.si_mut().connect(sa, local, proto)
    }

    /// Sets `SO_REUSEADDR`.
    ///
    /// # Errors
    ///
    /// When the option cannot be set.
    pub fn set_reuseaddr(&mut self, reuseaddr: bool) -> Result<(), ExceptionPtr> {
        self.si_mut().set_reuseaddr(reuseaddr)
    }

    /// Whether `SO_REUSEADDR` is set.
    ///
    /// # Errors
    ///
    /// When the option cannot be read.
    pub fn get_reuseaddr(&self) -> Result<bool, ExceptionPtr> {
        self.si().get_reuseaddr()
    }

    /// Stops any in-flight connection attempt: cancels it if still in progress, or
    /// terminates the connection if it is already established.
    ///
    /// # Errors
    ///
    /// When the socket cannot be shut down.
    pub fn shutdown(&mut self) -> Result<(), ExceptionPtr> {
        self.si_mut().shutdown()
    }
}

impl ServerSocket {
    /// A listener bound to no address.
    pub fn new() -> Self {
        ServerSocket { _ssi: None, _aborted: false }
    }

    /// Wraps an implementation.
    pub fn from_impl(ssi: Box<dyn ServerSocketImpl>) -> Self {
        ServerSocket { _ssi: Some(ssi), _aborted: false }
    }

    /// Accepts the next connection, resolving to it and the peer's address. After
    /// [`ServerSocket::abort_accept`] it fails at once with `ECONNABORTED`.
    ///
    /// # Panics
    ///
    /// If the listener has no implementation (as do `abort_accept` and `local_address`).
    pub fn accept(&mut self) -> Future<AcceptResult> {
        if self._aborted {
            return make_exception_future::<AcceptResult>(ExceptionPtr::from(
                std::io::Error::from_raw_os_error(crate::engine::net::socket_defs::ECONNABORTED),
            ));
        }
        impl_of(self._ssi.as_deref_mut()).accept()
    }

    /// Stops any accept in progress: current and future accepts fail at once. Safe on a
    /// listener with no implementation (built by [`ServerSocket::new`]).
    pub fn abort_accept(&mut self) {
        if let Some(ssi) = self._ssi.as_deref_mut() {
            ssi.abort_accept();
        }
        self._aborted = true;
    }

    /// The bound local address; an unspecified address for a listener with no implementation.
    ///
    /// # Errors
    ///
    /// The `getsockname` error of a POSIX listener.
    pub fn local_address(&self) -> Result<SocketAddress, ExceptionPtr> {
        match self._ssi.as_deref() {
            Some(ssi) => ssi.local_address(),
            None => Ok(SocketAddress::default()),
        }
    }
}

impl NetworkInterface {
    /// Wraps an implementation.
    pub fn new(impl_: SharedPtr<dyn NetworkInterfaceImpl>) -> Self {
        NetworkInterface { _impl: impl_ }
    }

    /// The interface index.
    pub fn index(&self) -> u32 {
        self._impl.index()
    }

    /// The MTU.
    pub fn mtu(&self) -> u32 {
        self._impl.mtu()
    }

    /// The name.
    pub fn name(&self) -> &Sstring {
        self._impl.name()
    }

    /// The display name.
    pub fn display_name(&self) -> &Sstring {
        self._impl.display_name()
    }

    /// The addresses assigned to the interface.
    pub fn addresses(&self) -> &Vec<InetAddress> {
        self._impl.addresses()
    }

    /// The hardware (MAC) address.
    pub fn hardware_address(&self) -> Vec<u8> {
        self._impl.hardware_address()
    }

    /// Whether it is a loopback interface.
    pub fn is_loopback(&self) -> bool {
        self._impl.is_loopback()
    }

    /// Whether it is a virtual interface.
    pub fn is_virtual(&self) -> bool {
        self._impl.is_virtual()
    }

    /// Whether it is up.
    pub fn is_up(&self) -> bool {
        self._impl.is_up()
    }

    /// Whether it has IPv6 support.
    pub fn supports_ipv6(&self) -> bool {
        self._impl.supports_ipv6()
    }
}

/// The default body of [`NetworkStack::connect`](crate::engine::net::api::NetworkStack::connect):
/// connects `s`, keeping it alive until the connection resolves.
pub fn network_stack_connect(
    s: Socket,
    sa: SocketAddress,
    local: SocketAddress,
    proto: Transport,
) -> Future<ConnectedSocket> {
    do_with(s, move |s: &mut Socket| s.connect(sa, local, proto))
}

/// The default body of
/// [`NetworkStack::network_interfaces`](crate::engine::net::api::NetworkStack::network_interfaces):
/// no interfaces.
pub fn network_stack_network_interfaces() -> Vec<NetworkInterface> {
    Vec::new()
}

/// Registers the `network` metric group (`bytes_sent`, `bytes_received`) of scheduling group
/// `sg_id` under the label `name`, read from the current shard's stack, and clears the
/// group's statistics unless the reactor is still starting (a group recreated with the same
/// id must start from zero).
pub fn register_net_metrics_for_scheduling_group(
    metrics: &mut MetricGroups,
    sg_id: u32,
    name: &LabelInstance,
) {
    // Registration fails only on a double registration of the same group, which the
    // reactor, the only caller, never does; the result is ignored.
    let _ = metrics.add_group(
        "network",
        vec![
            sm::make_counter(
                "bytes_sent",
                // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
                move || unsafe { (*engine()).net() }.stats(sg_id).bytes_sent,
                sm::Description::new("Counts the number of bytes written to network sockets."),
                vec![name.clone()],
            ),
            sm::make_counter(
                "bytes_received",
                // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
                move || unsafe { (*engine()).net() }.stats(sg_id).bytes_received,
                sm::Description::new("Counts the number of bytes received from network sockets."),
                vec![name.clone()],
            ),
        ],
    );
    // need to clear stats in case we recreated a SG with the same id
    // but avoid during reactor startup
    if engine_is_ready() {
        // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
        unsafe { (*engine()).net() }.clear_stats(sg_id);
    }
}
