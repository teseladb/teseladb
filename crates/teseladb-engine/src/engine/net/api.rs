//! The public networking API: sockets, listeners, datagram channels and the network stack
//! interface.
//!
//! Every shard owns one [`NetworkStack`] (the POSIX stack over kernel sockets, or the native
//! user-space stack) and every object here belongs to the shard that created it:
//! [`ConnectedSocket`] (a full-duplex stream), [`Socket`] (a connection being established),
//! [`ServerSocket`] (a listener), [`DatagramChannel`] (UDP or UNIX datagrams) and
//! [`NetworkInterface`]. Each is a thin handle over a boxed implementation trait declared in
//! [`stack`](crate::engine::net::stack), where most of their methods are implemented.
//!
//! How accepted connections are spread over the shards is chosen per listener with
//! [`ListenOptions::lba`] ([`LoadBalancingAlgorithm`]).

use std::time::Duration;

use crate::engine::core::file_types::FilePermissions;
use crate::engine::core::future::{ExceptionPtr, Future, make_ready_future};
use crate::engine::core::shared_ptr::SharedPtr;
use crate::engine::core::sstring::Sstring;
use crate::engine::net::packet::Packet;
use crate::engine::net::socket_defs::{Ipv4Addr, sa_family_t};
/// The address and transport types of this API, re-exported for its users.
pub use crate::engine::net::socket_defs::{SocketAddress, Transport};
use crate::engine::net::stack::{
    ConnectedSocketImpl, DatagramChannelImpl, NetworkInterfaceImpl, ServerSocketImpl, SocketImpl,
};
use crate::engine::util::noncopyable_function::NoncopyableFunction;
use crate::engine::util::program_options::OptionGroup;

/// Whether the IP of `addr` is `0.0.0.0`.
#[inline]
pub fn is_ip_unspecified(addr: &Ipv4Addr) -> bool {
    addr.is_ip_unspecified()
}

/// Whether the port of `addr` is 0.
#[inline]
pub fn is_port_unspecified(addr: &Ipv4Addr) -> bool {
    addr.is_port_unspecified()
}

/// The `AF_INET` socket address of `addr`.
#[inline]
pub fn make_ipv4_address(addr: &Ipv4Addr) -> SocketAddress {
    SocketAddress::from_ipv4_addr(*addr)
}

/// The `AF_INET` socket address of a host-order IP and a port.
#[inline]
pub fn make_ipv4_address_ip_port(ip: u32, port: u16) -> SocketAddress {
    make_ipv4_address(&Ipv4Addr::new(ip, port))
}

/// TCP keepalive parameters (see Linux tcp(7)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct TcpKeepaliveParams {
    /// Idle time before the first probe (`TCP_KEEPIDLE`).
    pub idle: Duration,
    /// Time between probes (`TCP_KEEPINTVL`).
    pub interval: Duration,
    /// Unanswered probes before the connection is dropped (`TCP_KEEPCNT`).
    pub count: u32,
}

/// SCTP heartbeat parameters (see Linux sctp(7)).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct SctpKeepaliveParams {
    /// Heartbeat interval (`spp_hbinterval`).
    pub interval: Duration,
    /// Maximum retransmissions on a path (`spp_pathmaxrt`).
    pub count: u32,
}

/// The keepalive parameters of a connection, by transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeepaliveParams {
    /// For a TCP connection.
    Tcp(TcpKeepaliveParams),
    /// For an SCTP connection.
    Sctp(SctpKeepaliveParams),
}

/// The implementation behind a received [`Datagram`].
pub trait DatagramImpl {
    /// The sender's address.
    fn get_src(&mut self) -> SocketAddress;
    /// The address the datagram was sent to.
    fn get_dst(&mut self) -> SocketAddress;
    /// The destination port.
    ///
    /// # Errors
    ///
    /// When the implementation cannot tell the port (for example a UNIX-domain datagram).
    fn get_dst_port(&mut self) -> Result<u16, ExceptionPtr>;
    /// The payload.
    fn get_data(&mut self) -> &mut Packet;
}

/// The implementation behind a received UDP datagram.
pub type UdpDatagramImpl = dyn DatagramImpl;

/// A datagram received on a [`DatagramChannel`]: its addresses and its payload.
pub struct Datagram {
    pub(crate) _impl: Box<dyn DatagramImpl>,
}

impl Datagram {
    /// Wraps an implementation.
    pub fn new(impl_: Box<dyn DatagramImpl>) -> Self {
        Datagram { _impl: impl_ }
    }
    /// The sender's address.
    pub fn get_src(&mut self) -> SocketAddress {
        self._impl.get_src()
    }
    /// The address the datagram was sent to.
    pub fn get_dst(&mut self) -> SocketAddress {
        self._impl.get_dst()
    }
    /// The destination port.
    ///
    /// # Errors
    ///
    /// As [`DatagramImpl::get_dst_port`].
    pub fn get_dst_port(&mut self) -> Result<u16, ExceptionPtr> {
        self._impl.get_dst_port()
    }
    /// The payload.
    pub fn get_data(&mut self) -> &mut Packet {
        self._impl.get_data()
    }
}

/// A received UDP datagram.
pub type UdpDatagram = Datagram;

/// A datagram channel (UDP or UNIX-domain datagrams), bound or unbound.
///
/// A default-constructed channel has no implementation. The operations (`local_address`,
/// `receive`, `send`, `shutdown_input`, `shutdown_output`, `close`, `is_closed`) are
/// implemented in [`stack`](crate::engine::net::stack). `close` must be called only when no
/// `send` or `receive` is pending; the shutdown calls make pending ones complete soon.
#[derive(Default)]
pub struct DatagramChannel {
    pub(crate) _impl: Option<Box<dyn DatagramChannelImpl>>,
}

/// A UDP channel.
pub type UdpChannel = DatagramChannel;

/// Configuration for buffered connected_socket input operations
///
/// This structure allows tuning of buffered input operations done via
/// connected_socket. It is a hint to the implementation and may be
/// ignored (e.g. the zero-copy native stack does not allocate buffers,
/// so it ignores buffer-size parameters).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConnectedSocketInputStreamConfig {
    /// Initial buffer size to use for input buffering
    pub buffer_size: u32,
    /// Minimum buffer size to use for input buffering. The system will decrease
    /// buffer sizes if it sees a tendency towards small requests, but will not go
    /// below this buffer size.
    pub min_buffer_size: u32,
    /// Maximum buffer size to use for input buffering. The system will increase
    /// buffer sizes if it sees a tendency towards large requests, but will not go
    /// above this buffer size.
    pub max_buffer_size: u32,
}

/// 8 KiB initial, 512 bytes minimum, 128 KiB maximum.
impl Default for ConnectedSocketInputStreamConfig {
    fn default() -> Self {
        ConnectedSocketInputStreamConfig {
            buffer_size: 8192,
            min_buffer_size: 512,
            max_buffer_size: 128 * 1024,
        }
    }
}

/// The distinguished names of a TLS peer certificate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionDn {
    /// The subject.
    pub subject: Sstring,
    /// The issuer.
    pub issuer: Sstring,
}

/// A TCP (or other stream-based protocol) connection.
///
/// A `ConnectedSocket` represents a full-duplex stream between a local endpoint and a remote
/// endpoint. A default-constructed one corresponds to no connection. Its input and output
/// streams, socket options, addresses and shutdown operations are implemented in
/// [`stack`](crate::engine::net::stack).
#[derive(Default)]
pub struct ConnectedSocket {
    pub(crate) _csi: Option<Box<dyn ConnectedSocketImpl>>,
}

impl ConnectedSocket {
    /// Whether this socket corresponds to a connection (it was returned by `connect` or
    /// `accept` rather than default-constructed).
    #[inline]
    pub fn is_valid(&self) -> bool {
        self._csi.is_some()
    }
}

/// A socket that establishes a connection to a remote endpoint.
///
/// `connect`, `set_reuseaddr`, `get_reuseaddr` and `shutdown` (which cancels an attempt in
/// progress or terminates the established connection) are implemented in
/// [`stack`](crate::engine::net::stack).
#[derive(Default)]
pub struct Socket {
    pub(crate) _si: Option<Box<dyn SocketImpl>>,
}

/// The result of [`ServerSocket`]'s `accept`.
pub struct AcceptResult {
    /// The newly-accepted connection
    pub connection: ConnectedSocket,
    /// The address of the peer that connected to us
    pub remote_address: SocketAddress,
}

/// How a listener spreads accepted connections over the shards.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum LoadBalancingAlgorithm {
    /// Distribute connections evenly: each new connection goes to the shard with the fewest
    /// connections.
    #[default]
    ConnectionDistribution,
    /// The destination shard is the peer's port modulo the number of shards, so a client
    /// that knows the shard count can reach a given shard by choosing its source port.
    Port,
    /// Every connection goes to [`ListenOptions::fixed_cpu`].
    Fixed,
}

impl LoadBalancingAlgorithm {
    /// The default algorithm, [`LoadBalancingAlgorithm::ConnectionDistribution`].
    pub const DEFAULT_: LoadBalancingAlgorithm = LoadBalancingAlgorithm::ConnectionDistribution;
}

/// A listening socket, waiting to accept incoming network connections.
///
/// `accept`, `abort_accept` (which makes current and future `accept` calls fail at once) and
/// `local_address` are implemented in [`stack`](crate::engine::net::stack).
#[derive(Default)]
pub struct ServerSocket {
    pub(crate) _ssi: Option<Box<dyn ServerSocketImpl>>,
    pub(crate) _aborted: bool,
}

impl ServerSocket {
    /// Whether this listener is bound to an address (it was returned by `listen` rather than
    /// default-constructed).
    #[inline]
    pub fn is_valid(&self) -> bool {
        self._ssi.is_some()
    }
}

/// Options for creating a listening socket.
///
/// WARNING: these options currently only have an effect when using
/// the POSIX stack: all options are ignored on the native stack as they
/// are not implemented there.
#[derive(Clone, Debug)]
pub struct ListenOptions {
    /// Set `SO_REUSEADDR` on the listening socket.
    pub reuse_address: bool,
    /// How accepted connections are spread over the shards.
    pub lba: LoadBalancingAlgorithm,
    /// The transport protocol.
    pub proto: Transport,
    /// The `listen` backlog.
    pub listen_backlog: i32,
    /// The shard that receives every connection under [`LoadBalancingAlgorithm::Fixed`].
    pub fixed_cpu: u32,
    /// For a UNIX-domain listener, the permissions given to the socket file.
    pub unix_domain_socket_permissions: Option<FilePermissions>,

    /// If set, the SO_SNDBUF size will be set to the given value on the listening socket
    /// via setsockopt. This buffer size is inherited by the sockets returned by
    /// accept and is the preferred way to set the buffer size for these sockets since
    /// setting it directly on the already-accepted socket is ineffective (see TCP(7)).
    pub so_sndbuf: Option<i32>,

    /// If set, the SO_RCVBUF size will be set to the given value on the listening socket
    /// via setsockopt. This buffer size is inherited by the sockets returned by
    /// accept and is the preferred way to set the buffer size for these sockets since
    /// setting it directly on the already-accepted socket is ineffective (see TCP(7)).
    pub so_rcvbuf: Option<i32>,
}

/// No `SO_REUSEADDR`, connection distribution, TCP, a backlog of 100.
impl Default for ListenOptions {
    fn default() -> Self {
        ListenOptions {
            reuse_address: false,
            lba: LoadBalancingAlgorithm::DEFAULT_,
            proto: Transport::TCP,
            listen_backlog: 100,
            fixed_cpu: 0,
            unix_domain_socket_permissions: None,
            so_sndbuf: None,
            so_rcvbuf: None,
        }
    }
}

impl ListenOptions {
    /// Sends every accepted connection to shard `cpu` ([`LoadBalancingAlgorithm::Fixed`]).
    pub fn set_fixed_cpu(&mut self, cpu: u32) {
        self.lba = LoadBalancingAlgorithm::Fixed;
        self.fixed_cpu = cpu;
    }
}

/// A network interface of the host (or of the native stack's device): index, MTU, names,
/// addresses, hardware address and flags, implemented in
/// [`stack`](crate::engine::net::stack).
pub struct NetworkInterface {
    pub(crate) _impl: SharedPtr<dyn NetworkInterfaceImpl>,
}

/// Bytes moved by a stack for one scheduling group.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Statistics {
    /// Bytes sent.
    pub bytes_sent: u64,
    /// Bytes received.
    pub bytes_received: u64,
}

/// Registers the network metrics of a scheduling group.
pub use crate::engine::net::stack::register_net_metrics_for_scheduling_group;

/// A network stack: the per-shard provider of sockets, listeners and datagram channels.
pub trait NetworkStack {
    /// Creates a listener bound to `sa`.
    ///
    /// # Errors
    ///
    /// When the address cannot be bound or listened on.
    fn listen(
        &mut self,
        sa: SocketAddress,
        opts: ListenOptions,
    ) -> Result<ServerSocket, ExceptionPtr>;
    /// Connects to `sa` from `local` (unspecified to let the system pick) over `proto`.
    ///
    /// The default makes a [`Socket`] with [`NetworkStack::socket`] and connects it.
    // FIXME: `local` assumes IPv4 for now; fix when adding other address families.
    fn connect(
        &mut self,
        sa: SocketAddress,
        local: SocketAddress,
        proto: Transport,
    ) -> Future<ConnectedSocket> {
        crate::engine::net::stack::network_stack_connect(self.socket(), sa, local, proto)
    }
    /// A new unconnected [`Socket`].
    fn socket(&mut self) -> Socket;

    /// A UDP channel bound to `addr` (an unspecified address makes it unbound).
    ///
    /// # Errors
    ///
    /// When the channel cannot be created or bound.
    #[deprecated(note = "Use `make_[un]bound_datagram_channel` instead")]
    fn make_udp_channel(&mut self, addr: &SocketAddress) -> Result<UdpChannel, ExceptionPtr>;

    /// An unbound datagram channel of the address family `family`.
    ///
    /// # Errors
    ///
    /// When the channel cannot be created.
    fn make_unbound_datagram_channel(
        &mut self,
        family: sa_family_t,
    ) -> Result<DatagramChannel, ExceptionPtr>;
    /// A datagram channel bound to `local`.
    ///
    /// # Errors
    ///
    /// When the channel cannot be created or bound.
    fn make_bound_datagram_channel(
        &mut self,
        local: &SocketAddress,
    ) -> Result<DatagramChannel, ExceptionPtr>;
    /// Completes the stack's start-up; the default is ready at once.
    fn initialize(&mut self) -> Future<()> {
        make_ready_future(())
    }
    /// Whether each shard has its own port namespace, so that every shard can listen on the
    /// same address independently: true for the native stack, and for the POSIX stack when
    /// it uses `SO_REUSEPORT`.
    fn has_per_core_namespace(&mut self) -> bool;
    /// Whether the stack supports IPv6. This is a stack-wide answer to what is really a
    /// per-interface question.
    fn supports_ipv6(&self) -> bool {
        false
    }

    /// The bytes sent and received by this stack for a scheduling group.
    fn stats(&mut self, scheduling_group_id: u32) -> Statistics;
    /// Resets the statistics of a scheduling group.
    fn clear_stats(&mut self, scheduling_group_id: u32);

    /// A snapshot of the network interfaces available at call time.
    ///
    /// # Errors
    ///
    /// When the interfaces cannot be listed.
    fn network_interfaces(&mut self) -> Result<Vec<NetworkInterface>, ExceptionPtr> {
        Ok(crate::engine::net::stack::network_stack_network_interfaces())
    }
}

/// Builds a network stack from its parsed options.
pub type FactoryFunc =
    NoncopyableFunction<dyn FnMut(&OptionGroup) -> Future<Box<dyn NetworkStack>>>;

/// A registered network stack: its name, options and factory.
pub struct NetworkStackEntry {
    /// The name selected on the command line.
    pub name: Sstring,
    /// The stack's options, boxed so that the option group keeps a stable address.
    pub opts: Box<dyn crate::engine::util::program_options::AsOptionGroup>,
    /// Builds the stack.
    pub factory: FactoryFunc,
    /// Whether this stack is used when none is selected.
    pub is_default: bool,
}
