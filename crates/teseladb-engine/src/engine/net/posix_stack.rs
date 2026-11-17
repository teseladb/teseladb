//! The POSIX network stack: the networking API over the kernel's sockets.
//!
//! The main shard runs a [`PosixNetworkStack`] and every other shard a
//! [`PosixApNetworkStack`] ([`register_posix_stack`]). Sockets are non-blocking file
//! descriptors driven by the reactor through [`PollableFd`]; reads and writes go through
//! [`PosixDataSourceImpl`] and [`PosixDataSinkImpl`], and the bytes moved are counted per
//! scheduling group for [`NetworkStack::stats`].
//!
//! Listening:
//! - with `SO_REUSEPORT`, each shard opens its own listening socket on the same address
//!   ([`PosixReuseportServerSocketImpl`]) and the kernel spreads the connections;
//! - otherwise the main shard owns the listening socket ([`PosixServerSocketImpl`]) and
//!   hands each accepted connection to the shard chosen by the listener's
//!   [`LoadBalancingAlgorithm`]; the other shards wait in a [`PosixApServerSocketImpl`] for
//!   the connections sent to them. UNIX-domain listeners always work this way.
//!
//! [`Conntrack`] keeps the per-shard connection counts that connection distribution uses,
//! outliving the listener. The module also declares the Linux socket, SCTP and rtnetlink
//! interfaces it needs (`setsockopt` options, `IP_PKTINFO`, the netlink messages that list
//! the network interfaces).

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, c_char, c_int, c_uint, c_ulong, c_void};
use std::hash::{Hash, Hasher};
use std::sync::OnceLock;
use std::time::Duration;

use crate::engine::core::deleter::{Deleter, make_deleter, make_deleter_with};
use crate::engine::core::future::{
    ExceptionPtr, Future, Promise, SystemError, futurize_invoke, make_exception_future,
    make_ready_future,
};
use crate::engine::core::internal::buffer_allocator::BufferAllocator;
use crate::engine::core::internal::pollable_fd::{PollableFd, ShutdownKernelOnly};
use crate::engine::core::iostream::{DataSink, DataSinkImpl, DataSource, DataSourceImpl};
use crate::engine::core::loop_::{StopIteration, repeat};
use crate::engine::core::memory::MALLOC_ALLOCATOR;
use crate::engine::core::on_internal_error::on_internal_error;
use crate::engine::core::polymorphic_temporary_buffer::PolymorphicAllocator;
use crate::engine::core::posix::{FileDesc, throw_system_error_on};
use crate::engine::core::reactor::{engine, engine_logger};
use crate::engine::core::scheduling::internal::scheduling_group_index;
use crate::engine::core::scheduling::{current_scheduling_group, max_scheduling_groups};
use crate::engine::core::shard_id::{ShardId, this_shard_id};
use crate::engine::core::sharded::{ForeignPtr, make_foreign};
use crate::engine::core::shared_ptr::{
    LwSharedPtr, SharedPtr, make_lw_shared, make_shared, shared_ptr_converting_move,
};
use crate::engine::core::smp::Smp;
use crate::engine::core::sstring::Sstring;
use crate::engine::core::temporary_buffer::TemporaryBuffer;
use crate::engine::net::api::{
    AcceptResult, ConnectedSocket, ConnectedSocketInputStreamConfig, Datagram, DatagramChannel,
    DatagramImpl, FactoryFunc, KeepaliveParams, ListenOptions, LoadBalancingAlgorithm,
    NetworkInterface, NetworkStack, NetworkStackEntry, SctpKeepaliveParams, ServerSocket, Socket,
    Statistics, TcpKeepaliveParams, UdpChannel,
};
use crate::engine::net::byteorder::{hton, ntoh};
use crate::engine::net::inet_address::{Family, InetAddress, RuntimeError};
use crate::engine::net::node_ptr::{CrossShard, NodePtr, StaticConst};
use crate::engine::net::packet::{Fragment, Packet};
use crate::engine::net::socket_address::hash_socket_address;
use crate::engine::net::socket_defs::{
    AF_INET, AF_INET6, AF_NETLINK, AF_PACKET, AF_UNIX, AF_UNSPEC, ECONNABORTED, IPPROTO_IP,
    IPPROTO_IPV6, IPPROTO_SCTP, IPPROTO_TCP, Ipv4Addr, Ipv6Addr, SocketAddress, Transport, in_addr,
    in6_addr, inet_pton, sa_family_t, sockaddr, sockaddr_storage, socklen_t,
};
use crate::engine::net::stack::{
    ConnectedSocketImpl, DatagramChannelImpl, NetworkInterfaceImpl, ServerSocketImpl, SocketImpl,
};
use crate::engine::net::unix_address::UnixDomainAddr;
use crate::engine::util::program_options::OptionGroup;

// ---------------------------------------------------------------------------
// Linux declarations (<sys/socket.h>, <netinet/tcp.h>, <netinet/sctp.h>, <net/route.h>,
// <linux/netlink.h>, <linux/rtnetlink.h>, <linux/if.h>, <stdio.h>, <unistd.h>)
// ---------------------------------------------------------------------------

/// An opaque C stdio stream.
#[allow(non_camel_case_types)]
pub type FILE = c_void;

/// `struct iovec` and `struct msghdr`: the reactor backend's, which `PollableFd::sendmsg` and
/// `recvmsg` take.
pub use crate::engine::core::reactor_backend::{Iovec as iovec, Msghdr as msghdr};

/// `struct cmsghdr`: the header of a control message (ancillary data) of `sendmsg`/`recvmsg`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct cmsghdr {
    /// The length of the header plus its data.
    pub cmsg_len: usize,
    /// The originating protocol.
    pub cmsg_level: c_int,
    /// The protocol-specific type.
    pub cmsg_type: c_int,
}

/// `struct in_pktinfo`: the `IP_PKTINFO` control message (destination of a received IPv4 datagram).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct in_pktinfo {
    /// The interface the datagram arrived on.
    pub ipi_ifindex: c_int,
    /// The local address used as source in replies.
    pub ipi_spec_dst: in_addr,
    /// The destination address in the IP header.
    pub ipi_addr: in_addr,
}

/// `struct in6_pktinfo`: the `IPV6_PKTINFO` control message (destination of a received IPv6 datagram).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct in6_pktinfo {
    /// The destination address.
    pub ipi6_addr: in6_addr,
    /// The interface the datagram arrived on.
    pub ipi6_ifindex: c_uint,
}

/// The fields of `struct sctp_paddrparams`, without padding (`__attribute__((packed))`).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct sctp_paddrparams_fields {
    /// The association.
    pub spp_assoc_id: i32,
    /// `struct sockaddr_storage spp_address`, as its 128 bytes: a packed struct cannot hold
    /// the 8-aligned `sockaddr_storage` itself.
    pub spp_address: [u8; size_of::<sockaddr_storage>()],
    /// The heartbeat interval, in milliseconds.
    pub spp_hbinterval: u32,
    /// The maximum number of retransmissions on the path.
    pub spp_pathmaxrxt: u16,
    /// The path MTU.
    pub spp_pathmtu: u32,
    /// The SACK delay, in milliseconds.
    pub spp_sackdelay: u32,
    /// `SPP_*` flags.
    pub spp_flags: u32,
    /// The IPv6 flow label.
    pub spp_ipv6_flowlabel: u32,
    /// The DSCP value.
    pub spp_dscp: u8,
}

/// `struct sctp_paddrparams` of `<linux/sctp.h>`, declared
/// `__attribute__((packed, aligned(4)))`: its fields without padding, and the whole aligned
/// to 4 (155 bytes rounded up to 156). `#[repr(C, packed(4))]` is not the same: it still
/// aligns each field up to 4 and pads after `spp_pathmaxrxt`. The fields are reached through
/// `Deref`.
#[repr(C, align(4))]
#[derive(Clone, Copy)]
pub struct sctp_paddrparams {
    /// The fields.
    pub p: sctp_paddrparams_fields,
}

const _: () = {
    use std::mem::{align_of, offset_of, size_of};
    assert!(size_of::<sockaddr_storage>() == 128);
    assert!(size_of::<sctp_paddrparams>() == 156);
    assert!(align_of::<sctp_paddrparams>() == 4);
    assert!(offset_of!(sctp_paddrparams, p) == 0);
    assert!(offset_of!(sctp_paddrparams_fields, spp_assoc_id) == 0);
    assert!(offset_of!(sctp_paddrparams_fields, spp_address) == 4);
    assert!(offset_of!(sctp_paddrparams_fields, spp_hbinterval) == 132);
    assert!(offset_of!(sctp_paddrparams_fields, spp_pathmaxrxt) == 136);
    assert!(offset_of!(sctp_paddrparams_fields, spp_pathmtu) == 138);
    assert!(offset_of!(sctp_paddrparams_fields, spp_sackdelay) == 142);
    assert!(offset_of!(sctp_paddrparams_fields, spp_flags) == 146);
    assert!(offset_of!(sctp_paddrparams_fields, spp_ipv6_flowlabel) == 150);
    assert!(offset_of!(sctp_paddrparams_fields, spp_dscp) == 154);
};

impl std::ops::Deref for sctp_paddrparams {
    type Target = sctp_paddrparams_fields;
    #[inline]
    fn deref(&self) -> &sctp_paddrparams_fields {
        &self.p
    }
}

impl std::ops::DerefMut for sctp_paddrparams {
    #[inline]
    fn deref_mut(&mut self) -> &mut sctp_paddrparams_fields {
        &mut self.p
    }
}

impl Default for sctp_paddrparams {
    fn default() -> Self {
        // SAFETY: plain old data; all-zero is a valid value of every field.
        unsafe { std::mem::zeroed() }
    }
}

/// `struct sockaddr_nl`: a netlink socket address.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct sockaddr_nl {
    /// `AF_NETLINK`.
    pub nl_family: sa_family_t,
    /// Zero.
    pub nl_pad: u16,
    /// The port id (0 for the kernel).
    pub nl_pid: u32,
    /// The multicast groups mask.
    pub nl_groups: u32,
}

/// `struct nlmsghdr`: the header of a netlink message.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct nlmsghdr {
    /// The length of the message, header included.
    pub nlmsg_len: u32,
    /// The message type (`RTM_*`, `NLMSG_DONE`).
    pub nlmsg_type: u16,
    /// `NLM_F_*` flags.
    pub nlmsg_flags: u16,
    /// The sequence number.
    pub nlmsg_seq: u32,
    /// The sender's port id.
    pub nlmsg_pid: u32,
}

/// `struct rtgenmsg`: the body of a generic rtnetlink dump request.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct rtgenmsg {
    /// The address family to dump.
    pub rtgen_family: u8,
}

/// `struct ifaddrmsg`: an interface address (`RTM_NEWADDR`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ifaddrmsg {
    /// The address family.
    pub ifa_family: u8,
    /// The prefix length.
    pub ifa_prefixlen: u8,
    /// `IFA_F_*` flags.
    pub ifa_flags: u8,
    /// The address scope.
    pub ifa_scope: u8,
    /// The interface index.
    pub ifa_index: u32,
}

/// `struct ifinfomsg`: an interface (`RTM_NEWLINK`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ifinfomsg {
    /// `AF_UNSPEC`.
    pub ifi_family: u8,
    /// Padding.
    pub __ifi_pad: u8,
    /// The device type (`ARPHRD_*`).
    pub ifi_type: u16,
    /// The interface index.
    pub ifi_index: i32,
    /// `IFF_*` flags.
    pub ifi_flags: u32,
    /// The change mask.
    pub ifi_change: u32,
}

/// `struct rtattr`: the header of an rtnetlink attribute.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct rtattr {
    /// The length of the attribute, header included.
    pub rta_len: u16,
    /// The attribute type (`IFLA_*`, `IFA_*`).
    pub rta_type: u16,
}

/// Socket-level option level.
pub const SOL_SOCKET: c_int = 1;
/// Allow reuse of local addresses.
pub const SO_REUSEADDR: c_int = 2;
/// Enable keepalive probes.
pub const SO_KEEPALIVE: c_int = 9;
/// Allow several sockets to bind the same address and port.
pub const SO_REUSEPORT: c_int = 15;
/// Disable Nagle's algorithm.
pub const TCP_NODELAY: c_int = 1;
/// Idle time before the first keepalive probe, in seconds.
pub const TCP_KEEPIDLE: c_int = 4;
/// Time between keepalive probes, in seconds.
pub const TCP_KEEPINTVL: c_int = 5;
/// Unanswered keepalive probes before the connection is dropped.
pub const TCP_KEEPCNT: c_int = 6;
/// SCTP option level.
pub const SOL_SCTP: c_int = 132;
/// Disable the SCTP Nagle-like algorithm.
pub const SCTP_NODELAY: c_int = 3;
/// Per-path parameters ([`sctp_paddrparams`]).
pub const SCTP_PEER_ADDR_PARAMS: c_int = 9;
/// Enable heartbeats on the path.
pub const SPP_HB_ENABLE: u32 = 1;
/// `shutdown`: stop receiving.
pub const SHUT_RD: c_int = 0;
/// `shutdown`: stop sending.
pub const SHUT_WR: c_int = 1;
/// `shutdown`: stop both.
pub const SHUT_RDWR: c_int = 2;
/// IPv4 option level.
pub const SOL_IP: c_int = 0;
/// Receive [`in_pktinfo`] control messages.
pub const IP_PKTINFO: c_int = 8;
/// Receive [`in6_pktinfo`] control messages.
pub const IPV6_PKTINFO: c_int = 50;
/// `IPV6_RECVPKTINFO` (`<netinet/in.h>`): attach `IPV6_PKTINFO` to received datagrams.
pub const IPV6_RECVPKTINFO: c_int = 49;
/// Datagram socket type.
pub const SOCK_DGRAM: c_int = 2;
/// Raw socket type.
pub const SOCK_RAW: c_int = 3;
/// Open the socket in non-blocking mode.
pub const SOCK_NONBLOCK: c_int = 0o4000;
/// Close the socket on `exec`.
pub const SOCK_CLOEXEC: c_int = 0o2000000;
/// The rtnetlink protocol.
pub const NETLINK_ROUTE: c_int = 0;
/// The multicast group of IPv4 address changes.
pub const RTMGRP_IPV4_IFADDR: u32 = 0x10;
/// The multicast group of IPv6 address changes.
pub const RTMGRP_IPV6_IFADDR: u32 = 0x100;
/// An interface (reply to `RTM_GETLINK`).
pub const RTM_NEWLINK: u16 = 16;
/// Request the interfaces.
pub const RTM_GETLINK: u16 = 18;
/// An address (reply to `RTM_GETADDR`).
pub const RTM_NEWADDR: u16 = 20;
/// Request the addresses.
pub const RTM_GETADDR: u16 = 22;
/// The message is a request.
pub const NLM_F_REQUEST: u16 = 1;
/// Return the complete table (dump).
pub const NLM_F_ROOT: u16 = 0x100;
/// The end of a multipart reply.
pub const NLMSG_DONE: u16 = 3;
/// The interface is up.
pub const IFF_UP: u32 = 0x1;
/// The interface is a loopback.
pub const IFF_LOOPBACK: u32 = 0x8;
/// Interface attribute: the hardware address.
pub const IFLA_ADDRESS: u16 = 1;
/// Interface attribute: the name.
pub const IFLA_IFNAME: u16 = 3;
/// Interface attribute: the MTU.
pub const IFLA_MTU: u16 = 4;
/// Address attribute: the (peer) address.
pub const IFA_ADDRESS: u16 = 1;
/// Address attribute: the local address.
pub const IFA_LOCAL: u16 = 2;
/// Route flag: reject route.
pub const RTF_REJECT: c_ulong = 0x0200;
/// Route flag: flow route.
pub const RTF_FLOW: c_ulong = 0x02000000;
/// Route flag: policy route.
pub const RTF_POLICY: c_ulong = 0x04000000;
/// `errno`: address already in use.
pub const EADDRINUSE: i32 = 98;
/// `errno`: address not available.
pub const EADDRNOTAVAIL: i32 = 99;
/// `errno`: the socket is not connected.
pub const ENOTCONN: i32 = 107;

unsafe extern "C" {
    fn fopen(path: *const c_char, mode: *const c_char) -> *mut FILE;
    fn fclose(f: *mut FILE) -> c_int;
    fn fscanf(f: *mut FILE, fmt: *const c_char, ...) -> c_int;
    fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    fn bind(fd: c_int, addr: *const c_void, len: socklen_t) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn getpid() -> c_int;
    fn sendmsg(fd: c_int, msg: *const msghdr, flags: c_int) -> isize;
    fn recvmsg(fd: c_int, msg: *mut msghdr, flags: c_int) -> isize;
    fn setsockopt(
        fd: c_int,
        level: c_int,
        optname: c_int,
        optval: *const c_void,
        optlen: socklen_t,
    ) -> c_int;
    fn getsockopt(
        fd: c_int,
        level: c_int,
        optname: c_int,
        optval: *mut c_void,
        optlen: *mut socklen_t,
    ) -> c_int;
    fn getsockname(fd: c_int, addr: *mut c_void, len: *mut socklen_t) -> c_int;
    fn getpeername(fd: c_int, addr: *mut c_void, len: *mut socklen_t) -> c_int;
}

const NLMSG_ALIGNTO: u32 = 4;
#[inline]
fn nlmsg_align(len: u32) -> u32 {
    (len + NLMSG_ALIGNTO - 1) & !(NLMSG_ALIGNTO - 1)
}
#[inline]
fn nlmsg_hdrlen() -> u32 {
    nlmsg_align(std::mem::size_of::<nlmsghdr>() as u32)
}
#[inline]
fn nlmsg_length(len: u32) -> u32 {
    len + nlmsg_hdrlen()
}
/// The size of `struct rtattr`, the header of an attribute (already aligned).
const RTA_HDRLEN: usize = std::mem::size_of::<rtattr>();

#[inline]
fn netlink_align(len: usize) -> usize {
    (len + NLMSG_ALIGNTO as usize - 1) & !(NLMSG_ALIGNTO as usize - 1)
}

/// Reads the native-endian `u16` at `at`; the caller checked that `b` holds it.
#[inline]
fn ne_u16(b: &[u8], at: usize) -> u16 {
    let mut a = [0u8; 2];
    a.copy_from_slice(&b[at..at + 2]);
    u16::from_ne_bytes(a)
}

/// Reads the native-endian `u32` at `at`; the caller checked that `b` holds it.
#[inline]
fn ne_u32(b: &[u8], at: usize) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[at..at + 4]);
    u32::from_ne_bytes(a)
}

/// The records of a netlink buffer: messages (a `nlmsghdr` with a 32-bit length) or
/// attributes (a `rtattr` with a 16-bit length). Each item is the record's type and its
/// payload, both inside the buffer.
///
/// A record is admitted only when the buffer holds its header, its declared length covers
/// that header and does not run past the buffer (`NLMSG_OK` and `RTA_OK`); the walk stops at
/// the first record that fails. The step to the next record is the aligned length, capped at
/// the end of the buffer, so the remaining length never wraps (`NLMSG_NEXT` and `RTA_NEXT`).
struct NetlinkRecords<'a> {
    rest: &'a [u8],
    /// The header size: `NLMSG_HDRLEN` or the size of `rtattr`.
    hdr_len: usize,
}

impl<'a> NetlinkRecords<'a> {
    /// The messages of a netlink reply (the bytes `recvmsg` returned).
    fn messages(reply: &'a [u8]) -> Self {
        NetlinkRecords { rest: reply, hdr_len: nlmsg_hdrlen() as usize }
    }

    /// The attributes in `buf` (the bytes after a message's fixed body).
    fn attributes(buf: &'a [u8]) -> Self {
        NetlinkRecords { rest: buf, hdr_len: RTA_HDRLEN }
    }
}

impl<'a> Iterator for NetlinkRecords<'a> {
    type Item = (u16, &'a [u8]);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.len() < self.hdr_len {
            self.rest = &[];
            return None;
        }
        let (len, ty) = if self.hdr_len == RTA_HDRLEN {
            (ne_u16(self.rest, 0) as usize, ne_u16(self.rest, 2))
        } else {
            (ne_u32(self.rest, 0) as usize, ne_u16(self.rest, 4))
        };
        if len < self.hdr_len || len > self.rest.len() {
            self.rest = &[];
            return None;
        }
        let payload = &self.rest[self.hdr_len..len];
        let step = netlink_align(len).min(self.rest.len());
        self.rest = &self.rest[step..];
        Some((ty, payload))
    }
}

/// The bytes after a fixed body of `body_len` bytes at the start of a message payload, where
/// its attributes begin (`IFLA_RTA`, `IFA_RTA`).
#[inline]
fn attributes_after(payload: &[u8], body_len: usize) -> &[u8] {
    payload.get(netlink_align(body_len)..).unwrap_or(&[])
}

/// Parses the payload of an `RTM_NEWLINK` message; `None` when it is too short for its
/// `ifinfomsg`.
fn parse_netlink_link(payload: &[u8]) -> Option<PosixNetworkInterfaceImpl> {
    const BODY_LEN: usize = std::mem::size_of::<ifinfomsg>();
    if payload.len() < BODY_LEN {
        return None;
    }
    // todo: filter any non-network interfaces (family)

    let mut nwif = PosixNetworkInterfaceImpl::default();
    // ifinfomsg: family (1), pad (1), type (2), index (4), flags (4), change (4).
    let ifi_flags = ne_u32(payload, 8);
    nwif._index = ne_u32(payload, 4);
    nwif._loopback = (ifi_flags & IFF_LOOPBACK) != 0;
    nwif._up = (ifi_flags & IFF_UP) != 0;
    // `_virtual` stays false: the VLAN, bridge and inactive-slave flags that would mark a
    // virtual interface are private to the kernel and not reported in `ifi_flags`.
    for (ty, data) in NetlinkRecords::attributes(attributes_after(payload, BODY_LEN)) {
        match ty {
            IFLA_IFNAME => {
                // The kernel sends a NUL-terminated name; the read stops at the NUL or at the
                // end of the attribute, whichever comes first.
                let name = data.split(|&b| b == 0).next().unwrap_or(&[]);
                nwif._name = Sstring::from(String::from_utf8_lossy(name).as_ref());
            }
            IFLA_MTU => {
                if data.len() >= std::mem::size_of::<u32>() {
                    nwif._mtu = ne_u32(data, 0);
                }
            }
            IFLA_ADDRESS => {
                nwif._hardware_address = data.to_vec();
            }
            _ => {}
        }
    }
    Some(nwif)
}

/// Parses the payload of an `RTM_NEWADDR` message into the interface of `res` it names; a
/// payload too short for its `ifaddrmsg` is ignored.
fn parse_netlink_addr(payload: &[u8], res: &mut [PosixNetworkInterfaceImpl]) {
    const BODY_LEN: usize = std::mem::size_of::<ifaddrmsg>();
    if payload.len() < BODY_LEN {
        return;
    }
    // ifaddrmsg: family, prefixlen, flags, scope (1 each), index (4).
    let ifa_index = ne_u32(payload, 4);
    let Some(nwif) = res.iter_mut().find(|nwif| nwif._index == ifa_index) else {
        return;
    };
    for (ty, data) in NetlinkRecords::attributes(attributes_after(payload, BODY_LEN)) {
        if ty != IFA_LOCAL && ty != IFA_ADDRESS {
            continue;
        }
        // ipv6 addresses are reported only as "ADDRESS"
        let ia = if data.len() == std::mem::size_of::<in_addr>() {
            // The address bytes are in network order, as `s_addr` holds them.
            Some(InetAddress::from_in_addr(in_addr { s_addr: ne_u32(data, 0) }))
        } else if let Ok(s6_addr) = <[u8; 16]>::try_from(data) {
            Some(InetAddress::from_in6_addr(in6_addr { s6_addr }, nwif.index()))
        } else {
            None
        };
        if let Some(ia) = ia
            && !nwif._addresses.contains(&ia)
        {
            nwif._addresses.push(ia);
        }
    }
}

/// Folds one netlink reply (the bytes `recvmsg` returned) into `res`: an interface per
/// `RTM_NEWLINK`, its addresses per `RTM_NEWADDR`. Returns whether the reply held
/// `NLMSG_DONE`, the end of the dump.
fn parse_netlink_reply(reply: &[u8], res: &mut Vec<PosixNetworkInterfaceImpl>) -> bool {
    let mut done = false;
    for (ty, payload) in NetlinkRecords::messages(reply) {
        match ty {
            // that is all
            NLMSG_DONE => done = true,
            RTM_NEWLINK => res.extend(parse_netlink_link(payload)),
            RTM_NEWADDR => parse_netlink_addr(payload, res),
            _ => {}
        }
    }
    done
}
#[inline]
fn cmsg_align(len: usize) -> usize {
    (len + std::mem::size_of::<usize>() - 1) & !(std::mem::size_of::<usize>() - 1)
}
#[inline]
unsafe fn cmsg_firsthdr(mhdr: *const msghdr) -> *mut cmsghdr {
    // SAFETY: guaranteed by the caller.
    unsafe {
        if (*mhdr).msg_controllen >= std::mem::size_of::<cmsghdr>() {
            (*mhdr).msg_control as *mut cmsghdr
        } else {
            std::ptr::null_mut()
        }
    }
}
#[inline]
unsafe fn cmsg_nxthdr(mhdr: *const msghdr, cmsg: *mut cmsghdr) -> *mut cmsghdr {
    // SAFETY: guaranteed by the caller (the glibc __cmsg_nxthdr algorithm).
    unsafe {
        if (*cmsg).cmsg_len < std::mem::size_of::<cmsghdr>() {
            return std::ptr::null_mut();
        }
        let next = (cmsg as *mut u8).add(cmsg_align((*cmsg).cmsg_len)) as *mut cmsghdr;
        let end = ((*mhdr).msg_control as *mut u8).add((*mhdr).msg_controllen);
        if (next as *mut u8).add(1) > end
            || (next as *mut u8).add(cmsg_align((*next).cmsg_len)) > end
        {
            return std::ptr::null_mut();
        }
        next
    }
}
#[inline]
unsafe fn cmsg_data(cmsg: *const cmsghdr) -> *const u8 {
    // SAFETY: guaranteed by the caller.
    unsafe { (cmsg as *const u8).add(cmsg_align(std::mem::size_of::<cmsghdr>())) }
}
/// Whether `a` is an IPv6 link-local address (`fe80::/10`).
#[inline]
fn in6_is_addr_linklocal(a: &in6_addr) -> bool {
    a.s6_addr[0] == 0xfe && (a.s6_addr[1] & 0xc0) == 0x80
}

// ---------------------------------------------------------------------------
// Shared helpers and per-shard counters
// ---------------------------------------------------------------------------

/// A protocol and a listening address: the key under which a shard's
/// [`PosixApServerSocketImpl`] waits for the connections the main shard hands over. Hashes as
/// the protocol XOR the address hash.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ProtocolAndSocketAddress(pub i32, pub SocketAddress);

impl Hash for ProtocolAndSocketAddress {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let h1 = self.0 as usize;
        let h2 = hash_socket_address(&self.1);
        state.write_usize(h1 ^ h2);
    }
}

/// Reads a `T` from a byte pointer by copying it (an unaligned read), instead of casting the
/// pointer and dereferencing it, which could be misaligned and would alias the bytes with a
/// different type.
fn copy_reinterpret_cast<T: Copy>(ptr: *const u8) -> T {
    // SAFETY: the caller passes a pointer to at least size_of::<T>() readable bytes.
    unsafe { std::ptr::read_unaligned(ptr as *const T) }
}

/// One byte counter per scheduling group.
type SgCounters = [Cell<u64>; max_scheduling_groups() as usize];

thread_local! {
    static BYTES_SENT: SgCounters = const { [const { Cell::new(0) }; max_scheduling_groups() as usize] };
    static BYTES_RECEIVED: SgCounters = const { [const { Cell::new(0) }; max_scheduling_groups() as usize] };
}

/// Adds `n` to this shard's bytes-sent counter of the current scheduling group (hot path:
/// no bounds check, no atomic).
#[inline]
fn add_bytes_sent(n: u64) {
    let sg_id = scheduling_group_index(current_scheduling_group()) as usize;
    BYTES_SENT.with(|b| {
        // SAFETY: a scheduling group index is always below max_scheduling_groups().
        let c = unsafe { b.get_unchecked(sg_id) };
        c.set(c.get() + n);
    });
}

/// Adds `n` to this shard's bytes-received counter of the current scheduling group (hot
/// path: no bounds check, no atomic).
#[inline]
fn add_bytes_received(n: u64) {
    let sg_id = scheduling_group_index(current_scheduling_group()) as usize;
    BYTES_RECEIVED.with(|b| {
        // SAFETY: a scheduling group index is always below max_scheduling_groups().
        let c = unsafe { b.get_unchecked(sg_id) };
        c.set(c.get() + n);
    });
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Connection tracking of a listener: how many of its connections each shard holds.
///
/// It cannot live in the server socket as plain members, because a connection can outlive
/// the server socket; rather than tracking the whole server socket through a shared pointer,
/// the counts are in a shared [`LoadBalancer`] that each connection's [`ConntrackHandle`]
/// keeps alive. Used by [`PosixServerSocketImpl`].
pub struct Conntrack {
    _lb: LwSharedPtr<LoadBalancer>,
}

/// The number of open connections per shard. It is shared through an [`LwSharedPtr`] and
/// mutated through it, so each counter is a `Cell`; only its home shard touches it.
pub struct LoadBalancer {
    _cpu_load: Vec<Cell<u32>>,
}

impl LoadBalancer {
    /// One zero counter per shard.
    pub fn new() -> Self {
        LoadBalancer { _cpu_load: (0..Smp::count()).map(|_| Cell::new(0)).collect() }
    }
    /// A connection placed on `cpu` was closed.
    #[inline]
    pub fn closed_cpu(&self, cpu: ShardId) {
        let c = &self._cpu_load[cpu as usize];
        c.set(c.get() - 1);
    }
    /// The least loaded shard, which takes the new connection.
    pub fn next_cpu(&self) -> ShardId {
        // FIXME: The naive algorithm will just round robin the connections around the shards.
        // A more complex version can keep track of the amount of activity in each connection,
        // and use that information.
        // The first of the minimal elements.
        let mut min_idx = 0usize;
        for (i, v) in self._cpu_load.iter().enumerate() {
            if v.get() < self._cpu_load[min_idx].get() {
                min_idx = i;
            }
        }
        let cpu = min_idx as ShardId;
        let c = &self._cpu_load[min_idx];
        c.set(c.get() + 1);
        cpu
    }
    /// Places the new connection on `cpu`.
    #[inline]
    pub fn force_cpu(&self, cpu: ShardId) -> ShardId {
        let c = &self._cpu_load[cpu as usize];
        c.set(c.get() + 1);
        cpu
    }
}

impl Default for LoadBalancer {
    fn default() -> Self {
        Self::new()
    }
}

/// The tracking of one connection: the shard it was placed on. Dropping it (on any shard)
/// decrements that shard's count on the load balancer's home shard.
#[derive(Default)]
pub struct ConntrackHandle {
    _host_cpu: ShardId,
    _target_cpu: ShardId,
    _lb: ForeignPtr<LwSharedPtr<LoadBalancer>>,
}

impl ConntrackHandle {
    /// The handle of a connection placed on `cpu`, counted in `lb` (which lives on the
    /// current shard).
    pub fn new(cpu: ShardId, lb: LwSharedPtr<LoadBalancer>) -> Self {
        ConntrackHandle { _host_cpu: this_shard_id(), _target_cpu: cpu, _lb: make_foreign(lb) }
    }
    /// The shard the connection was placed on.
    pub fn cpu(&self) -> ShardId {
        self._target_cpu
    }
}

impl Drop for ConntrackHandle {
    fn drop(&mut self) {
        if !self._lb.is_some() {
            return;
        }
        let cpu = self._target_cpu;
        let lb = std::mem::take(&mut self._lb);
        // FIXME: future is discarded
        let _ = Smp::submit_to(self._host_cpu, move || {
            // The load balancer belongs to the host shard, where this runs.
            if let Some(lb) = lb.get_local() {
                lb.closed_cpu(cpu);
            }
        });
    }
}

impl Default for Conntrack {
    fn default() -> Self {
        Self::new()
    }
}

impl Conntrack {
    /// No connections on any shard.
    pub fn new() -> Self {
        Conntrack { _lb: make_lw_shared(LoadBalancer::new()) }
    }
    /// Places a new connection on the least loaded shard.
    pub fn get_handle(&self) -> ConntrackHandle {
        ConntrackHandle::new(self._lb.next_cpu(), self._lb.clone())
    }
    /// Places a new connection on `cpu`.
    pub fn get_handle_for(&self, cpu: ShardId) -> ConntrackHandle {
        ConntrackHandle::new(self._lb.force_cpu(cpu), self._lb.clone())
    }
}

/// The state of a [`PosixDataSourceImpl`] that its continuation and the reactor (through
/// the buffer allocator) reach after `get` returns. It lives in its own node: the source
/// sits in the `Box` of a `DataSource`, which reasserts its uniqueness each time it moves,
/// so a pointer to the source taken in `get` would not stay valid.
pub struct PosixSourceState {
    _buffer_allocator: AllocatorPtr,
    _config: ConnectedSocketInputStreamConfig,
}

/// The data source of a connected socket: each `get` reads into a buffer from the
/// configured allocator, sized adaptively within the input stream configuration.
pub struct PosixDataSourceImpl {
    _fd: PollableFd,
    _s: NodePtr<PosixSourceState>,
}

impl PosixDataSourceImpl {
    /// A source reading `fd` with buffers from `allocator`, sized according to `config`.
    pub fn new(
        fd: PollableFd,
        config: ConnectedSocketInputStreamConfig,
        allocator: AllocatorPtr,
    ) -> Self {
        PosixDataSourceImpl {
            _fd: fd,
            _s: NodePtr::new(PosixSourceState { _buffer_allocator: allocator, _config: config }),
        }
    }
}

/// The data sink of a connected socket: writes each buffer or packet completely before the
/// put resolves, and supports batched flushes.
pub struct PosixDataSinkImpl {
    _fd: PollableFd,
    // In its own node: the write keeps a pointer to it, and the continuation resets it,
    // after `put` returns (see `PosixSourceState`).
    _p: NodePtr<Packet>,
}

impl PosixDataSinkImpl {
    /// A sink writing to `fd`.
    pub fn new(fd: PollableFd) -> Self {
        PosixDataSinkImpl { _fd: fd, _p: NodePtr::new(Packet::make_null_packet()) }
    }
}

struct Connection {
    fd: PollableFd,
    addr: SocketAddress,
    connection_tracking_handle: ConntrackHandle,
}

impl Connection {
    fn new(xfd: PollableFd, xaddr: SocketAddress, cth: ConntrackHandle) -> Self {
        Connection { fd: xfd, addr: xaddr, connection_tracking_handle: cth }
    }
}

type PortMapT = HashSet<ProtocolAndSocketAddress>;
type SocketsMapT = HashMap<ProtocolAndSocketAddress, Promise<AcceptResult>>;
type ConnMapT = HashMap<ProtocolAndSocketAddress, Vec<Connection>>; // several per key

thread_local! {
    static PORTS: RefCell<PortMapT> = RefCell::new(HashSet::new());
    static SOCKETS: RefCell<SocketsMapT> = RefCell::new(HashMap::new());
    static CONN_Q: RefCell<ConnMapT> = RefCell::new(HashMap::new());
}

/// The listener of a shard other than the main one when the main shard owns the listening
/// socket: its `accept` resolves with the connections the main shard hands to this shard
/// for the same protocol and address.
pub struct PosixApServerSocketImpl {
    _protocol: i32,
    _sa: SocketAddress,
    _allocator: AllocatorPtr,
}

/// The state of a [`PosixServerSocketImpl`], in its own node: `accept`'s continuation
/// points to it and runs after the call returns (see `PosixSourceState`).
pub struct PosixServerSocketState {
    _sa: SocketAddress,
    _protocol: i32,
    _lfd: PollableFd,
    _conntrack: Conntrack,
    _lba: LoadBalancingAlgorithm,
    _fixed_cpu: ShardId,
    _allocator: AllocatorPtr,
}

/// The listener that owns the listening socket and spreads accepted connections over the
/// shards with its [`LoadBalancingAlgorithm`] and [`Conntrack`].
pub struct PosixServerSocketImpl {
    _s: NodePtr<PosixServerSocketState>,
}

impl PosixServerSocketImpl {
    /// A listener on the listening descriptor `lfd` bound to `sa`; `fixed_cpu` is the target
    /// shard under [`LoadBalancingAlgorithm::Fixed`].
    pub fn new(
        protocol: i32,
        sa: SocketAddress,
        lfd: PollableFd,
        lba: LoadBalancingAlgorithm,
        fixed_cpu: ShardId,
        allocator: AllocatorPtr,
    ) -> Self {
        PosixServerSocketImpl {
            _s: NodePtr::new(PosixServerSocketState {
                _sa: sa,
                _protocol: protocol,
                _lfd: lfd,
                _conntrack: Conntrack::new(),
                _lba: lba,
                _fixed_cpu: fixed_cpu,
                _allocator: allocator,
            }),
        }
    }

    /// The state, for a short access from a socket method.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    fn st(&self) -> &mut PosixServerSocketState {
        // SAFETY: the socket's methods run from the reactor, and no accept continuation is
        // running (they run between them), so no other reference to the state is live.
        unsafe { self._s.get_mut() }
    }
}

/// A shard's own listener on an `SO_REUSEPORT` socket: connections stay on the shard that
/// accepts them.
pub struct PosixReuseportServerSocketImpl {
    _sa: SocketAddress,
    _protocol: i32,
    _lfd: PollableFd,
    _allocator: AllocatorPtr,
}

impl PosixReuseportServerSocketImpl {
    /// A listener on the listening descriptor `lfd` bound to `sa`.
    pub fn new(protocol: i32, sa: SocketAddress, lfd: PollableFd, allocator: AllocatorPtr) -> Self {
        PosixReuseportServerSocketImpl {
            _sa: sa,
            _protocol: protocol,
            _lfd: lfd,
            _allocator: allocator,
        }
    }
}

/// The POSIX stack of the main shard: it owns the listening sockets when `SO_REUSEPORT` is
/// not available.
pub struct PosixNetworkStack {
    _reuseport: bool,
    /// The allocator of receive buffers.
    pub _allocator: AllocatorPtr,
}

impl PosixNetworkStack {
    /// A ready future holding a new stack.
    pub fn create(opts: &OptionGroup, allocator: AllocatorPtr) -> Future<Box<dyn NetworkStack>> {
        make_ready_future::<Box<dyn NetworkStack>>(Box::new(PosixNetworkStack::new(
            opts, allocator,
        )))
    }
}

/// The POSIX stack of the shards other than the main one: without `SO_REUSEPORT`, its
/// listeners receive their connections from the main shard. Everything else is the
/// [`PosixNetworkStack`] behaviour.
pub struct PosixApNetworkStack {
    base: PosixNetworkStack,
    _reuseport: bool,
}

impl PosixApNetworkStack {
    /// A ready future holding a new stack.
    pub fn create(opts: &OptionGroup, allocator: AllocatorPtr) -> Future<Box<dyn NetworkStack>> {
        make_ready_future::<Box<dyn NetworkStack>>(Box::new(PosixApNetworkStack::new(
            opts, allocator,
        )))
    }
}

// ---------------------------------------------------------------------------
// Implementation
// ---------------------------------------------------------------------------

/// The socket options of a connected socket that depend on its protocol (TCP, SCTP or
/// UNIX stream): one stateless implementation per protocol, chosen when the socket is made.
///
/// The option methods return the `setsockopt`/`getsockopt` error, or an error for an option
/// the protocol does not have.
pub trait PosixConnectedSocketOperations: Sync {
    /// Sets the protocol's no-delay option.
    fn set_nodelay(&self, fd: &FileDesc, nodelay: bool) -> Result<(), ExceptionPtr>;
    /// Reads the protocol's no-delay option.
    fn get_nodelay(&self, fd: &FileDesc) -> Result<bool, ExceptionPtr>;
    /// Sets `SO_KEEPALIVE` (or the protocol's equivalent).
    fn set_keepalive(&self, _fd: &FileDesc, keepalive: bool) -> Result<(), ExceptionPtr>;
    /// Reads `SO_KEEPALIVE` (or the protocol's equivalent).
    fn get_keepalive(&self, _fd: &FileDesc) -> Result<bool, ExceptionPtr>;
    /// Sets the keepalive parameters of the protocol.
    fn set_keepalive_parameters(
        &self,
        _fd: &FileDesc,
        params: &KeepaliveParams,
    ) -> Result<(), ExceptionPtr>;
    /// Reads the keepalive parameters of the protocol.
    fn get_keepalive_parameters(&self, _fd: &FileDesc) -> Result<KeepaliveParams, ExceptionPtr>;
    /// Sets a raw socket option with `setsockopt`.
    ///
    /// # Errors
    ///
    /// The `setsockopt` error.
    ///
    /// # Safety
    /// `data` must be valid for reads of `len` bytes.
    unsafe fn set_sockopt(
        &self,
        _fd: &FileDesc,
        level: i32,
        optname: i32,
        data: *const u8,
        len: usize,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: guaranteed by the caller.
        let r = unsafe {
            setsockopt(_fd.get(), level, optname, data as *const c_void, len as socklen_t)
        };
        throw_system_error_on(r == -1, "setsockopt")
    }
    /// Reads a raw socket option with `getsockopt` into `data`; returns the call's result.
    ///
    /// # Errors
    ///
    /// The `getsockopt` error.
    ///
    /// # Safety
    /// `data` must be valid for writes of `len` bytes.
    unsafe fn get_sockopt(
        &self,
        _fd: &FileDesc,
        level: i32,
        optname: i32,
        data: *mut u8,
        len: usize,
    ) -> Result<i32, ExceptionPtr> {
        let mut len = len as socklen_t;
        // SAFETY: guaranteed by the caller.
        let r = unsafe { getsockopt(_fd.get(), level, optname, data as *mut c_void, &mut len) };
        throw_system_error_on(r == -1, "getsockopt")?;
        Ok(r)
    }
    /// The local address (`getsockname`).
    ///
    /// The address accessors of a connected socket cannot fail; a failing `getsockname`
    /// (ENOTCONN after the peer reset, for one) is fatal: the process prints the error and
    /// aborts, without unwinding.
    fn local_address(&self, _fd: &FileDesc) -> SocketAddress {
        match get_address(_fd) {
            Ok(a) => a,
            Err(e) => terminate_in_noexcept("getsockname", &e),
        }
    }
    /// The peer's address (`getpeername`). A socket the kernel no longer has connected (the
    /// peer reset it, say) has no peer: its address is the unspecified one. Any other failure
    /// aborts the process, as for [`PosixConnectedSocketOperations::local_address`].
    fn remote_address(&self, _fd: &FileDesc) -> SocketAddress {
        match get_remote_address(_fd) {
            Ok(a) => a,
            Err(e) if system_error_code(&e) == Some(ENOTCONN) => SocketAddress::default(),
            Err(e) => terminate_in_noexcept("getpeername", &e),
        }
    }
}

/// The local address of `fd` (`getsockname`), through a shared descriptor (the call does
/// not change it).
fn get_address(fd: &FileDesc) -> Result<SocketAddress, ExceptionPtr> {
    let mut addr = SocketAddress::default();
    // SAFETY: `addr` provides the storage and its length.
    let r = unsafe {
        getsockname(
            fd.get(),
            addr.as_posix_sockaddr_mut() as *mut sockaddr as *mut c_void,
            &mut addr.addr_length,
        )
    };
    throw_system_error_on(r == -1, "getsockname")?;
    Ok(addr)
}

/// The peer address of `fd` (`getpeername`), through a shared descriptor, as `get_address`.
fn get_remote_address(fd: &FileDesc) -> Result<SocketAddress, ExceptionPtr> {
    let mut addr = SocketAddress::default();
    // SAFETY: as above.
    let r = unsafe {
        getpeername(
            fd.get(),
            addr.as_posix_sockaddr_mut() as *mut sockaddr as *mut c_void,
            &mut addr.addr_length,
        )
    };
    throw_system_error_on(r == -1, "getpeername")?;
    Ok(addr)
}

/// The options of a TCP connection (`TCP_NODELAY`, `SO_KEEPALIVE`, `TCP_KEEP*`).
pub struct PosixTcpConnectedSocketOperations;

impl PosixConnectedSocketOperations for PosixTcpConnectedSocketOperations {
    fn set_nodelay(&self, _fd: &FileDesc, nodelay: bool) -> Result<(), ExceptionPtr> {
        _fd.setsockopt::<c_int>(IPPROTO_TCP, TCP_NODELAY, nodelay as c_int).map(drop)
    }
    fn get_nodelay(&self, _fd: &FileDesc) -> Result<bool, ExceptionPtr> {
        // SAFETY: a C integer: every bit pattern is valid.
        Ok(unsafe { _fd.getsockopt::<c_int>(IPPROTO_TCP, TCP_NODELAY) }? != 0)
    }
    fn set_keepalive(&self, _fd: &FileDesc, keepalive: bool) -> Result<(), ExceptionPtr> {
        _fd.setsockopt::<c_int>(SOL_SOCKET, SO_KEEPALIVE, keepalive as c_int).map(drop)
    }
    fn get_keepalive(&self, _fd: &FileDesc) -> Result<bool, ExceptionPtr> {
        // SAFETY: a C integer: every bit pattern is valid.
        Ok(unsafe { _fd.getsockopt::<c_int>(SOL_SOCKET, SO_KEEPALIVE) }? != 0)
    }
    fn set_keepalive_parameters(
        &self,
        _fd: &FileDesc,
        params: &KeepaliveParams,
    ) -> Result<(), ExceptionPtr> {
        let KeepaliveParams::Tcp(pms) = params else {
            // SCTP parameters on a TCP socket.
            return Err(ExceptionPtr::from(RuntimeError("bad_variant_access".to_string())));
        };
        _fd.setsockopt::<c_uint>(IPPROTO_TCP, TCP_KEEPCNT, pms.count)?;
        _fd.setsockopt::<c_int>(IPPROTO_TCP, TCP_KEEPIDLE, pms.idle.as_secs() as c_int)?;
        _fd.setsockopt::<c_int>(IPPROTO_TCP, TCP_KEEPINTVL, pms.interval.as_secs() as c_int)
            .map(drop)
    }
    fn get_keepalive_parameters(&self, _fd: &FileDesc) -> Result<KeepaliveParams, ExceptionPtr> {
        Ok(KeepaliveParams::Tcp(TcpKeepaliveParams {
            // SAFETY: a C integer: every bit pattern is valid.
            idle: Duration::from_secs(
                unsafe { _fd.getsockopt::<c_int>(IPPROTO_TCP, TCP_KEEPIDLE) }? as u64,
            ),
            // SAFETY: a C integer: every bit pattern is valid.
            interval: Duration::from_secs(unsafe {
                _fd.getsockopt::<c_int>(IPPROTO_TCP, TCP_KEEPINTVL)
            }? as u64),
            // SAFETY: a C integer: every bit pattern is valid.
            count: unsafe { _fd.getsockopt::<c_uint>(IPPROTO_TCP, TCP_KEEPCNT) }?,
        }))
    }
}

/// The options of an SCTP association (`SCTP_NODELAY`, heartbeats through
/// `SCTP_PEER_ADDR_PARAMS`).
pub struct PosixSctpConnectedSocketOperations;

impl PosixConnectedSocketOperations for PosixSctpConnectedSocketOperations {
    fn set_nodelay(&self, _fd: &FileDesc, nodelay: bool) -> Result<(), ExceptionPtr> {
        _fd.setsockopt::<c_int>(SOL_SCTP, SCTP_NODELAY, nodelay as c_int).map(drop)
    }
    fn get_nodelay(&self, _fd: &FileDesc) -> Result<bool, ExceptionPtr> {
        // SAFETY: a C integer: every bit pattern is valid.
        Ok(unsafe { _fd.getsockopt::<c_int>(SOL_SCTP, SCTP_NODELAY) }? != 0)
    }
    fn set_keepalive(&self, _fd: &FileDesc, keepalive: bool) -> Result<(), ExceptionPtr> {
        // SAFETY: `sctp_paddrparams` is the kernel's plain struct: every bit pattern is valid.
        let mut heartbeat =
            unsafe { _fd.getsockopt::<sctp_paddrparams>(SOL_SCTP, SCTP_PEER_ADDR_PARAMS) }?;
        if keepalive {
            heartbeat.spp_flags |= SPP_HB_ENABLE;
        } else {
            heartbeat.spp_flags &= !SPP_HB_ENABLE;
        }
        _fd.setsockopt::<sctp_paddrparams>(SOL_SCTP, SCTP_PEER_ADDR_PARAMS, heartbeat).map(drop)
    }
    fn get_keepalive(&self, _fd: &FileDesc) -> Result<bool, ExceptionPtr> {
        // SAFETY: `sctp_paddrparams` is the kernel's plain struct: every bit pattern is valid.
        let p = unsafe { _fd.getsockopt::<sctp_paddrparams>(SOL_SCTP, SCTP_PEER_ADDR_PARAMS) }?;
        Ok(p.spp_flags & SPP_HB_ENABLE != 0)
    }
    fn set_keepalive_parameters(
        &self,
        _fd: &FileDesc,
        kpms: &KeepaliveParams,
    ) -> Result<(), ExceptionPtr> {
        let KeepaliveParams::Sctp(pms) = kpms else {
            // TCP parameters on an SCTP socket.
            return Err(ExceptionPtr::from(RuntimeError("bad_variant_access".to_string())));
        };
        // SAFETY: `sctp_paddrparams` is the kernel's plain struct: every bit pattern is valid.
        let mut params =
            unsafe { _fd.getsockopt::<sctp_paddrparams>(SOL_SCTP, SCTP_PEER_ADDR_PARAMS) }?;
        params.spp_hbinterval = (pms.interval.as_secs() * 1000) as u32; // in milliseconds
        params.spp_pathmaxrxt = pms.count as u16;
        _fd.setsockopt::<sctp_paddrparams>(SOL_SCTP, SCTP_PEER_ADDR_PARAMS, params).map(drop)
    }
    fn get_keepalive_parameters(&self, _fd: &FileDesc) -> Result<KeepaliveParams, ExceptionPtr> {
        // SAFETY: `sctp_paddrparams` is the kernel's plain struct: every bit pattern is valid.
        let params =
            unsafe { _fd.getsockopt::<sctp_paddrparams>(SOL_SCTP, SCTP_PEER_ADDR_PARAMS) }?;
        let hb = params.spp_hbinterval;
        let maxrxt = params.spp_pathmaxrxt;
        Ok(KeepaliveParams::Sctp(SctpKeepaliveParams {
            interval: Duration::from_secs((hb / 1000) as u64), // in seconds
            count: maxrxt as u32,
        }))
    }
}

/// The options of a UNIX stream socket, which has none: no-delay is always on (setting it
/// off panics), keepalive is always off, and keepalive parameters are ignored.
pub struct PosixUnixStreamConnectedSocketOperations;

impl PosixConnectedSocketOperations for PosixUnixStreamConnectedSocketOperations {
    fn set_nodelay(&self, _fd: &FileDesc, nodelay: bool) -> Result<(), ExceptionPtr> {
        assert!(nodelay); // make sure nobody actually tries to use this non-existing functionality
        Ok(())
    }
    fn get_nodelay(&self, _fd: &FileDesc) -> Result<bool, ExceptionPtr> {
        Ok(true)
    }
    fn set_keepalive(&self, _fd: &FileDesc, _keepalive: bool) -> Result<(), ExceptionPtr> {
        Ok(())
    }
    fn get_keepalive(&self, _fd: &FileDesc) -> Result<bool, ExceptionPtr> {
        Ok(false)
    }
    fn set_keepalive_parameters(
        &self,
        _fd: &FileDesc,
        _p: &KeepaliveParams,
    ) -> Result<(), ExceptionPtr> {
        Ok(())
    }
    fn get_keepalive_parameters(&self, _fd: &FileDesc) -> Result<KeepaliveParams, ExceptionPtr> {
        // Default TCP parameters (all zero).
        Ok(KeepaliveParams::Tcp(TcpKeepaliveParams::default()))
    }
}

/// The option implementation of a socket of `family` and `protocol`; aborts the process on
/// any other combination.
fn get_posix_connected_socket_ops(
    family: sa_family_t,
    protocol: i32,
) -> &'static dyn PosixConnectedSocketOperations {
    static TCP_OPS: PosixTcpConnectedSocketOperations = PosixTcpConnectedSocketOperations;
    static SCTP_OPS: PosixSctpConnectedSocketOperations = PosixSctpConnectedSocketOperations;
    static UNIX_OPS: PosixUnixStreamConnectedSocketOperations =
        PosixUnixStreamConnectedSocketOperations;
    match family {
        AF_INET | AF_INET6 => match protocol {
            IPPROTO_TCP => &TCP_OPS,
            IPPROTO_SCTP => &SCTP_OPS,
            _ => std::process::abort(),
        },
        AF_UNIX => &UNIX_OPS,
        _ => std::process::abort(),
    }
}

/// Shuts down one direction of a connected socket. The shutdown ignores ENOTCONN; any other
/// failure is an internal error and aborts the process.
fn shutdown_socket_fd(fd: &mut PollableFd, how: i32) {
    // The shutdown ignores ENOTCONN. The other possible errors:
    // EINVAL (wrong "how") -- impossible
    // ENOTSOCK (not a socket) -- incredible
    // EBADF (invalid file descriptor) -- irretrievable
    if let Err(e) = fd.shutdown(how, ShutdownKernelOnly::YES) {
        let msg =
            format!("socket shutdown({}, {}) failed: {}", fd.get_file_desc().fdinfo(), how, e);
        // on_internal_error logs and either aborts or returns the error, which is fatal here.
        let ex = on_internal_error(engine_logger(), &msg);
        terminate_in_noexcept("shutdown", &ex);
    }
}

/// A connected kernel socket: its descriptor, the option implementation of its protocol,
/// and the connection tracking handle of the listener that accepted it, if any.
pub struct PosixConnectedSocketImpl {
    _fd: PollableFd,
    _ops: &'static dyn PosixConnectedSocketOperations,
    _handle: ConntrackHandle,
    _allocator: AllocatorPtr,
}

impl PosixConnectedSocketImpl {
    fn new(family: sa_family_t, protocol: i32, fd: PollableFd, allocator: AllocatorPtr) -> Self {
        PosixConnectedSocketImpl {
            _fd: fd,
            _ops: get_posix_connected_socket_ops(family, protocol),
            _handle: ConntrackHandle::default(),
            _allocator: allocator,
        }
    }
    fn with_handle(
        family: sa_family_t,
        protocol: i32,
        fd: PollableFd,
        handle: ConntrackHandle,
        allocator: AllocatorPtr,
    ) -> Self {
        PosixConnectedSocketImpl {
            _fd: fd,
            _ops: get_posix_connected_socket_ops(family, protocol),
            _handle: handle,
            _allocator: allocator,
        }
    }
}

impl ConnectedSocketImpl for PosixConnectedSocketImpl {
    fn source(&mut self) -> DataSource {
        self.source_with_config(ConnectedSocketInputStreamConfig::default())
    }
    fn source_with_config(&mut self, csisc: ConnectedSocketInputStreamConfig) -> DataSource {
        DataSource::new(Box::new(PosixDataSourceImpl::new(
            self._fd.clone(),
            csisc,
            self._allocator,
        )))
    }
    fn sink(&mut self) -> DataSink {
        DataSink::new(Box::new(PosixDataSinkImpl::new(self._fd.clone())))
    }
    fn shutdown_input(&mut self) {
        shutdown_socket_fd(&mut self._fd, SHUT_RD);
    }
    fn shutdown_output(&mut self) {
        shutdown_socket_fd(&mut self._fd, SHUT_WR);
    }
    fn set_nodelay(&mut self, nodelay: bool) -> Result<(), ExceptionPtr> {
        self._ops.set_nodelay(self._fd.get_file_desc_ref(), nodelay)
    }
    fn get_nodelay(&self) -> Result<bool, ExceptionPtr> {
        self._ops.get_nodelay(self._fd.get_file_desc_ref())
    }
    fn set_keepalive(&mut self, keepalive: bool) -> Result<(), ExceptionPtr> {
        self._ops.set_keepalive(self._fd.get_file_desc_ref(), keepalive)
    }
    fn get_keepalive(&self) -> Result<bool, ExceptionPtr> {
        self._ops.get_keepalive(self._fd.get_file_desc_ref())
    }
    fn set_keepalive_parameters(&mut self, p: &KeepaliveParams) -> Result<(), ExceptionPtr> {
        self._ops.set_keepalive_parameters(self._fd.get_file_desc_ref(), p)
    }
    fn get_keepalive_parameters(&self) -> Result<KeepaliveParams, ExceptionPtr> {
        self._ops.get_keepalive_parameters(self._fd.get_file_desc_ref())
    }
    unsafe fn set_sockopt(
        &mut self,
        level: i32,
        optname: i32,
        data: *const u8,
        len: usize,
    ) -> Result<(), ExceptionPtr> {
        // SAFETY: `data` points to `len` readable bytes, the contract of this trait method
        // (and of `ConnectedSocket::set_sockopt`, its only caller).
        unsafe { self._ops.set_sockopt(self._fd.get_file_desc_ref(), level, optname, data, len) }
    }
    unsafe fn get_sockopt(
        &self,
        level: i32,
        optname: i32,
        data: *mut u8,
        len: usize,
    ) -> Result<i32, ExceptionPtr> {
        // SAFETY: `data` points to `len` writable bytes, the contract of this trait method
        // (and of `ConnectedSocket::get_sockopt`, its only caller).
        unsafe { self._ops.get_sockopt(self._fd.get_file_desc_ref(), level, optname, data, len) }
    }
    fn local_address(&self) -> SocketAddress {
        self._ops.local_address(self._fd.get_file_desc_ref())
    }
    fn remote_address(&self) -> SocketAddress {
        self._ops.remote_address(self._fd.get_file_desc_ref())
    }
    fn wait_input_shutdown(&mut self) -> Future<()> {
        self._fd.poll_rdhup()
    }
}

/// Gives a link-local IPv6 destination without a scope id the index of the interface whose
/// route covers it, read from `/proc/net/ipv6_route`; any other address is left unchanged.
///
/// # Errors
///
/// When the route table cannot be opened or the interfaces cannot be listed.
fn resolve_outgoing_address(a: &mut SocketAddress) -> Result<(), ExceptionPtr> {
    if a.family() != AF_INET6
        || a.as_posix_sockaddr_in6().sin6_scope_id != InetAddress::INVALID_SCOPE
        || !in6_is_addr_linklocal(&a.as_posix_sockaddr_in6().sin6_addr)
    {
        return Ok(());
    }

    // SAFETY: both arguments are valid C strings.
    let f: *mut FILE = unsafe { fopen(c"/proc/net/ipv6_route".as_ptr(), c"r".as_ptr()) };
    if f.is_null() {
        // A system error with the errno of fopen, as the reference throws
        // (std::system_error(errno, std::system_category(), "resolve_address")).
        let code = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        return Err(ExceptionPtr::from(SystemError::new(code, "resolve_address")));
    }

    struct Holder(*mut FILE);
    impl Drop for Holder {
        fn drop(&mut self) {
            // SAFETY: the FILE was opened by fopen and is closed once.
            unsafe { fclose(self.0) };
        }
    }
    let _holder = Holder(f);

    /*
      Here all configured IPv6 routes are shown in a special format. The example displays for loopback interface only. The meaning is shown below (see net/ipv6/route.c for more).

    # cat /proc/net/ipv6_route
    00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000001 00200200 lo
    +------------------------------+ ++ +------------------------------+ ++ +------------------------------+ +------+ +------+ +------+ +------+ ++
    |                                |  |                                |  |                                |        |        |        |        |
    1                                2  3                                4  5                                6        7        8        9        10

    1: IPv6 destination network displayed in 32 hexadecimal chars without colons as separator

    2: IPv6 destination prefix length in hexadecimal

    3: IPv6 source network displayed in 32 hexadecimal chars without colons as separator

    4: IPv6 source prefix length in hexadecimal

    5: IPv6 next hop displayed in 32 hexadecimal chars without colons as separator

    6: Metric in hexadecimal

    7: Reference counter

    8: Use counter

    9: Flags

    10: Device name

    */

    let mut prefix_len: u32 = 0;
    let mut src_prefix_len: u32 = 0;
    let mut flags: c_ulong = 0;
    let mut device = [0 as c_char; 16];
    let mut dest_str = [0 as c_char; 40];

    loop {
        let d = dest_str.as_mut_ptr();
        // SAFETY: the format writes at most 4+1 bytes at each dest_str offset, 8+1 into
        // device, and one integer into each of prefix_len, src_prefix_len and flags.
        let n = unsafe {
            fscanf(
                f,
                c"%4s%4s%4s%4s%4s%4s%4s%4s %02x %*4s%*4s%*4s%*4s%*4s%*4s%*4s%*4s %02x %*4s%*4s%*4s%*4s%*4s%*4s%*4s%*4s %*08x %*08x %*08x %08lx %8s".as_ptr(),
                d.add(0), d.add(5), d.add(10), d.add(15),
                d.add(20), d.add(25), d.add(30), d.add(35),
                &mut prefix_len as *mut u32,
                &mut src_prefix_len as *mut u32,
                &mut flags as *mut c_ulong,
                device.as_mut_ptr(),
            )
        };
        if n != 12 {
            break;
        }

        if (prefix_len > 128)
            || (src_prefix_len != 0)
            || (flags & (RTF_POLICY | RTF_FLOW)) != 0
            || ((flags & RTF_REJECT) != 0 && prefix_len == 0)
        /* reject all */
        {
            continue;
        }

        for i in [4usize, 9, 14, 19, 24, 29, 34] {
            dest_str[i] = b':' as c_char;
        }
        dest_str[39] = 0;

        let mut addr = in6_addr::default();
        // SAFETY: dest_str is null-terminated and addr has room for an in6_addr.
        if unsafe {
            inet_pton(AF_INET6 as i32, dest_str.as_ptr(), &mut addr as *mut in6_addr as *mut c_void)
        } < 0
        {
            /* not an Ipv6 address */
            continue;
        }

        let bytes = (prefix_len / 8) as usize;
        let bits = prefix_len % 8;

        let src = a.as_posix_sockaddr_in6().sin6_addr;

        if bytes > 0 && src.s6_addr[..bytes] != addr.s6_addr[..bytes] {
            continue;
        }
        if bits > 0 {
            let c1 = src.s6_addr[bytes] as u32;
            let c2 = addr.s6_addr[bytes] as u32;
            let mask = 0xffu32 << (8 - bits);
            if (c1 & mask) != (c2 & mask) {
                continue;
            }
        }

        // found the route.
        // SAFETY: fscanf null-terminated `device`.
        let dev = unsafe { CStr::from_ptr(device.as_ptr()) }.to_string_lossy().into_owned();
        // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
        for nif in unsafe { (*engine()).net() }.network_interfaces()? {
            if nif.name().as_str() == dev || nif.display_name().as_str() == dev {
                a.as_posix_sockaddr_in6_mut().sin6_scope_id = nif.index();
                return Ok(());
            }
        }
    }
    Ok(())
}

/// The state of a [`PosixSocketImpl`] that the connection attempts write after `connect`
/// returns, in its own node (see `PosixSourceState`).
struct PosixSocketState {
    _fd: PollableFd,
    _reuseaddr: bool,
}

/// A socket being connected. For TCP without an explicit local port it picks a random local
/// port congruent to the shard id modulo the shard count (what port-based load balancing
/// expects), trying up to five such ports when one is taken before letting the system pick.
pub struct PosixSocketImpl {
    _s: NodePtr<PosixSocketState>,
    _allocator: AllocatorPtr,
}

thread_local! {
    /// This thread's random engine for local ports: a Lehmer generator (multiplier 16807,
    /// modulus 2^31 - 1) seeded from `/dev/urandom`.
    static RANDOM_ENGINE: Cell<u64> = Cell::new({
        let mut seed = [0u8; 4];
        let _ = std::fs::File::open("/dev/urandom").and_then(|mut f| {
            use std::io::Read;
            f.read_exact(&mut seed)
        });
        let s = u32::from_ne_bytes(seed) as u64 % 2147483647;
        if s == 0 { 1 } else { s }
    });
}

/// A pseudo-random value in `lo..=hi` from this thread's engine.
fn random_port_base(lo: u16, hi: u16) -> u16 {
    RANDOM_ENGINE.with(|e| {
        // x = x * 16807 % (2^31 - 1)
        let x = (e.get() * 16807) % 2147483647;
        e.set(x);
        let range = (hi - lo) as u64 + 1;
        lo + ((x - 1) % range) as u16
    })
}

/// The OS error code of a failed `bind` or `connect`, which `find_port_and_connect` inspects:
/// it surfaces as a `SystemError` (from the file descriptor wrapper) or as a
/// `std::io::Error` with an OS code. `None` for any other error.
fn system_error_code(err: &ExceptionPtr) -> Option<i32> {
    err.downcast_ref::<SystemError>()
        .map(SystemError::code)
        .or_else(|| err.downcast_ref::<std::io::Error>().and_then(|e| e.raw_os_error()))
}

impl PosixSocketImpl {
    /// Connects to `sa` from `local`; see [`PosixSocketImpl`] for the choice of local port.
    fn find_port_and_connect(
        &mut self,
        sa: SocketAddress,
        mut local: SocketAddress,
        proto: Transport,
    ) -> Future<()> {
        let lo = (49152 / Smp::count() + 1) as u16;
        let hi = (65535 / Smp::count() - 1) as u16;
        // If no explicit local address, set to dest address family wildcard.
        if local.is_unspecified() {
            local = SocketAddress::from_inet_address(
                &InetAddress::from_family(sa.addr().in_family()),
                0,
            );
        }
        let mut sa = sa;
        if let Err(e) = resolve_outgoing_address(&mut sa) {
            return make_exception_future(e);
        }
        // The state's root pointer: `repeat` runs the later attempts after this call.
        let this: *mut PosixSocketState = self._s.as_ptr();
        let mut attempts = 0;
        let requested_port: u16 = ntoh(local.as_posix_sockaddr_in().sin_port);
        repeat(move || {
            // SAFETY: the caller keeps the socket alive until its connection attempt completes
            // (a contract of the socket API); the state is reached through its root pointer with
            // no other reference to it live.
            let me = unsafe { &mut *this };
            // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
            me._fd = match unsafe { (*engine()).make_pollable_fd(sa, proto as i32) } {
                Ok(fd) => fd,
                Err(e) => return make_exception_future(e),
            };
            if let Err(e) = me._fd.get_file_desc().setsockopt::<c_int>(
                SOL_SOCKET,
                SO_REUSEADDR,
                me._reuseaddr as c_int,
            ) {
                return make_exception_future(e);
            }
            let port: u16 = if {
                let a = attempts;
                attempts += 1;
                a
            } < 5
                && requested_port == 0
                && proto == Transport::TCP
            {
                (random_port_base(lo, hi) as u32 * Smp::count() + this_shard_id()) as u16
            } else {
                requested_port
            };
            local.as_posix_sockaddr_in_mut().sin_port = hton(port);
            let fd = me._fd.clone();
            // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
            futurize_invoke(move || unsafe { (*engine()).posix_connect(fd, sa, local) })
                .then_wrapped(move |f: Future<()>| match f.get() {
                    Ok(()) => make_ready_future(StopIteration::YES),
                    Err(err) => {
                        if let Some(code) = system_error_code(&err)
                            && port != requested_port
                            && (code == EADDRINUSE || code == EADDRNOTAVAIL)
                        {
                            return make_ready_future(StopIteration::NO);
                        }
                        make_exception_future(err)
                    }
                })
        })
    }

    /// an aux function to handle unix-domain-specific requests
    fn connect_unix_domain(
        &mut self,
        sa: SocketAddress,
        mut local: SocketAddress,
    ) -> Future<ConnectedSocket> {
        // note that if the 'local' address was not set by the client, it was created as an undefined address
        if local.is_unspecified() {
            local = SocketAddress::from_unix_domain_addr(&UnixDomainAddr::new(b""));
        }

        // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
        self.st()._fd = match unsafe { (*engine()).make_pollable_fd(sa, 0) } {
            Ok(fd) => fd,
            Err(e) => return make_exception_future(e),
        };
        let fd = self.st()._fd.clone();
        let allocator = self._allocator;
        // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
        unsafe { (*engine()).posix_connect(self.st()._fd.clone(), sa, local) }.then(move |()| {
            let csi: Box<dyn ConnectedSocketImpl> =
                Box::new(PosixConnectedSocketImpl::new(AF_UNIX, 0, fd, allocator));
            make_ready_future::<ConnectedSocket>(ConnectedSocket::from_impl(csi))
        })
    }

    /// An unconnected socket whose connections read into buffers from `allocator`.
    pub fn new(allocator: AllocatorPtr) -> Self {
        PosixSocketImpl {
            _s: NodePtr::new(PosixSocketState { _fd: PollableFd::default(), _reuseaddr: false }),
            _allocator: allocator,
        }
    }

    /// The state, for a short access from a socket method.
    #[inline]
    #[allow(clippy::mut_from_ref)]
    fn st(&self) -> &mut PosixSocketState {
        // SAFETY: the socket's methods run from the reactor; no connection attempt is running
        // its body (it runs between them), so no other reference to the state is live.
        unsafe { self._s.get_mut() }
    }
}

impl SocketImpl for PosixSocketImpl {
    fn connect(
        &mut self,
        sa: SocketAddress,
        local: SocketAddress,
        proto: Transport,
    ) -> Future<ConnectedSocket> {
        if sa.is_af_unix() {
            return self.connect_unix_domain(sa, local);
        }
        let this: *mut PosixSocketState = self._s.as_ptr();
        let allocator = self._allocator;
        self.find_port_and_connect(sa, local, proto).then(move |()| {
            // SAFETY: the caller keeps the socket alive until its connection attempt completes;
            // the state's root pointer, with no other reference to it live.
            let fd = unsafe { (*this)._fd.clone() };
            let csi: Box<dyn ConnectedSocketImpl> =
                Box::new(PosixConnectedSocketImpl::new(sa.family(), proto as i32, fd, allocator));
            make_ready_future::<ConnectedSocket>(ConnectedSocket::from_impl(csi))
        })
    }

    fn set_reuseaddr(&mut self, reuseaddr: bool) -> Result<(), ExceptionPtr> {
        self.st()._reuseaddr = reuseaddr;
        if self.st()._fd.is_open() {
            self.st()._fd.get_file_desc().setsockopt::<c_int>(
                SOL_SOCKET,
                SO_REUSEADDR,
                reuseaddr as c_int,
            )?;
        }
        Ok(())
    }

    fn get_reuseaddr(&self) -> Result<bool, ExceptionPtr> {
        if self.st()._fd.is_open() {
            // SAFETY: a C integer: every bit pattern is valid.
            Ok(unsafe {
                self.st()._fd.get_file_desc().getsockopt::<c_int>(SOL_SOCKET, SO_REUSEADDR)
            }? != 0)
        } else {
            Ok(self.st()._reuseaddr)
        }
    }

    fn shutdown(&mut self) -> Result<(), ExceptionPtr> {
        if self.st()._fd.is_open() {
            // `PollableFd::shutdown` ignores ENOTCONN and reports any other error.
            self.st()._fd.shutdown(SHUT_RDWR, ShutdownKernelOnly::YES)?;
        }
        Ok(())
    }
}

/// Handles an error in an operation that cannot fail: prints the failed call and the error
/// to standard error and aborts the process, without unwinding.
#[cold]
fn terminate_in_noexcept(what: &str, e: &ExceptionPtr) -> ! {
    eprintln!("fatal system error in an operation that cannot fail: {what}: {e}");
    std::process::abort()
}

impl ServerSocketImpl for PosixServerSocketImpl {
    fn accept(&mut self) -> Future<AcceptResult> {
        // SAFETY: the state's root pointer, with no reference to it live.
        unsafe { PosixServerSocketState::accept(self._s.as_ptr()) }
    }

    fn abort_accept(&mut self) {
        // ENOTCONN is ignored below; EINVAL, ENOTSOCK and EBADF cannot happen on this open
        // listening socket, and the trait method returns nothing.
        let _ = self.st()._lfd.shutdown(SHUT_RD, ShutdownKernelOnly::NO);
    }

    fn local_address(&self) -> Result<SocketAddress, ExceptionPtr> {
        self.st()._lfd.get_file_desc().get_address()
    }
}

impl PosixServerSocketState {
    /// Accepts connections until one belongs to this shard: each accepted connection gets a
    /// tracking handle from the load-balancing algorithm, and one placed on another shard is
    /// handed to that shard's [`PosixApServerSocketImpl`] before accepting again.
    ///
    /// # Safety
    /// `this` is the state's root pointer, with no reference to it live.
    unsafe fn accept(this: *mut Self) -> Future<AcceptResult> {
        // SAFETY: guaranteed by the caller; the reference ends with the call.
        let f = unsafe { (*this)._lfd.accept() };
        f.then(move |fd_sa: (PollableFd, SocketAddress)| {
            // SAFETY: the caller keeps the server socket alive until its accept() completes; the
            // continuation (in line or later) reaches the state through its root pointer,
            // with no other reference to it live.
            let me = unsafe { &mut *this };
            let (mut fd, sa) = fd_sa;
            let cth = match me._lba {
                LoadBalancingAlgorithm::ConnectionDistribution => me._conntrack.get_handle(),
                LoadBalancingAlgorithm::Port => me
                    ._conntrack
                    .get_handle_for(ntoh(sa.as_posix_sockaddr_in().sin_port) as u32 % Smp::count()),
                LoadBalancingAlgorithm::Fixed => me._conntrack.get_handle_for(me._fixed_cpu),
            };
            let cpu = cth.cpu();
            if cpu == this_shard_id() {
                let csi: Box<dyn ConnectedSocketImpl> =
                    Box::new(PosixConnectedSocketImpl::with_handle(
                        sa.family(),
                        me._protocol,
                        fd,
                        cth,
                        me._allocator,
                    ));
                make_ready_future::<AcceptResult>(AcceptResult {
                    connection: ConnectedSocket::from_impl(csi),
                    remote_address: sa,
                })
            } else {
                let protocol = me._protocol;
                let ssa = me._sa;
                // Take the descriptor out, leaving -1 behind, so that the pollable fd no
                // longer closes it.
                // SAFETY: `fd` was just accepted and has no other copy.
                let fdesc =
                    std::mem::replace(unsafe { fd.get_file_desc_mut() }, FileDesc::from_fd(-1));
                let allocator = me._allocator;
                // SAFETY: the descriptor and the handle move into the work run on `cpu`: the
                // descriptor is a plain fd owned by the work, the
                // handle reaches its load balancer through a `foreign_ptr` (released back on
                // the listening shard), and the allocator outlives every shard's sockets.
                let moved = unsafe { CrossShard::new((fdesc, cth, allocator)) };
                // FIXME: future is discarded
                let _ = Smp::submit_to(cpu, move || {
                    let (fdesc, cth, allocator) = moved.into_inner();
                    PosixApServerSocketImpl::move_connected_socket(
                        protocol,
                        ssa,
                        PollableFd::from_file_desc(fdesc),
                        sa,
                        cth,
                        allocator,
                    );
                });
                // SAFETY: `me` is not used again; `this` is the root pointer.
                unsafe { Self::accept(this) }
            }
        })
    }
}

impl PosixApServerSocketImpl {
    /// Registers this shard as a listener for `protocol` on `sa`.
    ///
    /// # Errors
    ///
    /// `EADDRINUSE` if the shard already listens on that protocol and address.
    pub fn new(
        protocol: i32,
        sa: SocketAddress,
        allocator: AllocatorPtr,
    ) -> Result<Self, ExceptionPtr> {
        let inserted =
            PORTS.with(|p| p.borrow_mut().insert(ProtocolAndSocketAddress(protocol, sa)));
        if !inserted {
            return Err(ExceptionPtr::from(std::io::Error::from_raw_os_error(EADDRINUSE)));
        }
        Ok(PosixApServerSocketImpl { _protocol: protocol, _sa: sa, _allocator: allocator })
    }
}

impl Drop for PosixApServerSocketImpl {
    fn drop(&mut self) {
        let t_sa = ProtocolAndSocketAddress(self._protocol, self._sa);
        // Connections forwarded to this listener after its last accept would never be taken:
        // drop (and close) them with it.
        let queued = CONN_Q.with(|q| q.borrow_mut().remove(&t_sa));
        drop(queued);
        PORTS.with(|p| p.borrow_mut().remove(&t_sa));
    }
}

impl ServerSocketImpl for PosixApServerSocketImpl {
    fn accept(&mut self) -> Future<AcceptResult> {
        let t_sa = ProtocolAndSocketAddress(self._protocol, self._sa);
        let conni = CONN_Q.with(|q| {
            let mut q = q.borrow_mut();
            let c =
                q.get_mut(&t_sa).and_then(|v| if v.is_empty() { None } else { Some(v.remove(0)) });
            if q.get(&t_sa).map(|v| v.is_empty()).unwrap_or(false) {
                q.remove(&t_sa);
            }
            c
        });
        if let Some(c) = conni {
            let csi: Box<dyn ConnectedSocketImpl> =
                Box::new(PosixConnectedSocketImpl::with_handle(
                    self._sa.family(),
                    self._protocol,
                    c.fd,
                    c.connection_tracking_handle,
                    self._allocator,
                ));
            make_ready_future::<AcceptResult>(AcceptResult {
                connection: ConnectedSocket::from_impl(csi),
                remote_address: c.addr,
            })
        } else {
            SOCKETS.with(|s| {
                let mut s = s.borrow_mut();
                assert!(!s.contains_key(&t_sa));
                let pr = s.entry(t_sa).or_insert_with(Promise::new);
                pr.get_future()
            })
        }
    }

    fn abort_accept(&mut self) {
        let t_sa = ProtocolAndSocketAddress(self._protocol, self._sa);
        CONN_Q.with(|q| q.borrow_mut().remove(&t_sa));
        let i = SOCKETS.with(|s| s.borrow_mut().remove(&t_sa));
        if let Some(mut pr) = i {
            pr.set_exception(ExceptionPtr::from(std::io::Error::from_raw_os_error(ECONNABORTED)));
        }
    }

    fn local_address(&self) -> Result<SocketAddress, ExceptionPtr> {
        Ok(self._sa)
    }
}

impl ServerSocketImpl for PosixReuseportServerSocketImpl {
    fn accept(&mut self) -> Future<AcceptResult> {
        let allocator = self._allocator;
        let protocol = self._protocol;
        self._lfd.accept().then(move |fd_sa: (PollableFd, SocketAddress)| {
            let (fd, sa) = fd_sa;
            let csi: Box<dyn ConnectedSocketImpl> =
                Box::new(PosixConnectedSocketImpl::new(sa.family(), protocol, fd, allocator));
            make_ready_future::<AcceptResult>(AcceptResult {
                connection: ConnectedSocket::from_impl(csi),
                remote_address: sa,
            })
        })
    }

    fn abort_accept(&mut self) {
        // As in PosixServerSocketImpl::abort_accept.
        let _ = self._lfd.shutdown(SHUT_RD, ShutdownKernelOnly::NO);
    }

    fn local_address(&self) -> Result<SocketAddress, ExceptionPtr> {
        get_address(self._lfd.get_file_desc_ref())
    }
}

impl PosixApServerSocketImpl {
    /// Delivers a connection the main shard accepted for this shard's listener on
    /// (`protocol`, `sa`): it completes a pending `accept`, or is queued for the next one.
    pub fn move_connected_socket(
        protocol: i32,
        sa: SocketAddress,
        fd: PollableFd,
        addr: SocketAddress,
        cth: ConntrackHandle,
        allocator: AllocatorPtr,
    ) {
        let t_sa = ProtocolAndSocketAddress(protocol, sa);
        let i = SOCKETS.with(|s| s.borrow_mut().remove(&t_sa));
        if let Some(mut pr) = i {
            let csi: Box<dyn ConnectedSocketImpl> = Box::new(
                PosixConnectedSocketImpl::with_handle(sa.family(), protocol, fd, cth, allocator),
            );
            pr.set_value(AcceptResult {
                connection: ConnectedSocket::from_impl(csi),
                remote_address: addr,
            });
        } else if PORTS.with(|p| p.borrow().contains(&t_sa)) {
            CONN_Q.with(|q| {
                q.borrow_mut().entry(t_sa).or_default().push(Connection::new(fd, addr, cth));
            });
        } else {
            // No one listens on this address on this shard any more: close the connection
            // instead of queueing it where it would never be taken.
            drop(fd);
        }
    }
}

impl DataSourceImpl for PosixDataSourceImpl {
    fn get(&mut self) -> Future<TemporaryBuffer<u8>> {
        // The state's root pointer: the reactor and the continuation use it after this call.
        let this: *mut PosixSourceState = self._s.as_ptr();
        let ba: *mut dyn BufferAllocator = this;
        // SAFETY: the allocator is the source state, which outlives its get() calls.
        unsafe { self._fd.recv_some(ba) }.then(move |b: TemporaryBuffer<u8>| {
            // SAFETY: the input stream keeps the data source alive until get() completes; the state
            // is reached through its root pointer with no other reference to it live.
            let me = unsafe { &mut *this };
            if b.size() as u32 >= me._config.buffer_size {
                me._config.buffer_size *= 2;
                me._config.buffer_size =
                    std::cmp::min(me._config.buffer_size, me._config.max_buffer_size);
            } else if b.size() as u32 <= me._config.buffer_size / 4 {
                me._config.buffer_size /= 2;
                me._config.buffer_size =
                    std::cmp::max(me._config.buffer_size, me._config.min_buffer_size);
            }
            add_bytes_received(b.size() as u64);
            make_ready_future(b)
        })
    }

    fn close(&mut self) -> Future<()> {
        // A shutdown error is the returned future's.
        if let Err(e) = self._fd.shutdown(SHUT_RD, ShutdownKernelOnly::YES) {
            return make_exception_future(e);
        }
        make_ready_future(())
    }
}

/// The allocator of the receive buffers, which outlives the stack (the default is the
/// static [`MALLOC_ALLOCATOR`]).
pub type AllocatorPtr = &'static dyn PolymorphicAllocator;

/// A buffer of `size` bytes from `allocator`, given back to it when the buffer is dropped;
/// with [`MALLOC_ALLOCATOR`], an ordinary heap buffer. The generic constructor of
/// [`polymorphic_temporary_buffer`](crate::engine::core::polymorphic_temporary_buffer) wants
/// a concrete allocator type; this one takes the pointer the stack keeps.
fn make_temporary_buffer_dyn(allocator: AllocatorPtr, size: usize) -> TemporaryBuffer<u8> {
    if std::ptr::addr_eq(allocator as *const dyn PolymorphicAllocator, MALLOC_ALLOCATOR as *const _)
    {
        return TemporaryBuffer::new(size);
    }
    let buffer = allocator.allocate(size);
    let buffer_addr = buffer as usize;
    // SAFETY: `buffer` holds `size` bytes until the deleter gives them back to the allocator;
    // the deleter runs once, so `deallocate` gets the pointer and size of that
    // `allocate(size)` exactly once.
    unsafe {
        TemporaryBuffer::with_deleter(
            buffer,
            size,
            make_deleter_with(Deleter::default(), move || {
                allocator.deallocate(buffer_addr as *mut u8, size)
            }),
        )
    }
}

impl BufferAllocator for PosixSourceState {
    fn allocate_buffer(&mut self) -> TemporaryBuffer<u8> {
        make_temporary_buffer_dyn(self._buffer_allocator, self._config.buffer_size as usize)
    }
}

/// The I/O vector of the fragments of `p` (one allocation).
pub fn to_iovec(p: &Packet) -> Vec<iovec> {
    let mut v = Vec::with_capacity(p.nr_frags() as usize);
    for f in p.fragments().iter() {
        v.push(iovec { iov_base: f.base as *mut c_void, iov_len: f.size });
    }
    v
}

/// The I/O vector of `buf_vec`, writable (one allocation).
pub fn to_iovec_buffers(buf_vec: &mut [TemporaryBuffer<u8>]) -> Vec<iovec> {
    let mut v = Vec::with_capacity(buf_vec.len());
    for buf in buf_vec.iter_mut() {
        v.push(iovec { iov_base: buf.get_write() as *mut c_void, iov_len: buf.size() });
    }
    v
}

impl DataSinkImpl for PosixDataSinkImpl {
    fn put_buffer(&mut self, buf: TemporaryBuffer<u8>) -> Future<()> {
        add_bytes_sent(buf.size() as u64);
        let mut buf = buf;
        let (ptr, len) = (buf.get(), buf.size());
        let d = buf.release();
        // SAFETY: the deleter `d`, which the continuation keeps until the write is done,
        // keeps the `len` bytes at `ptr` alive and unchanged.
        unsafe { self._fd.write_all_bytes(ptr, len) }.then(move |()| {
            drop(d);
            make_ready_future(())
        })
    }

    fn put(&mut self, p: Packet) -> Future<()> {
        // The packet's root pointer: the write and the continuation use it after this call.
        let pp: *mut Packet = self._p.as_ptr();
        // SAFETY: no reference to the packet is live (the previous put has completed, which the
        // data sink contract requires before the next put).
        unsafe {
            *pp = p;
            add_bytes_sent((*pp).len() as u64);
        }
        // SAFETY: as above; the write keeps the pointer while it runs.
        self._fd.write_all(unsafe { &mut *pp }).then(move |()| {
            // SAFETY: the output stream keeps the data sink alive until put() completes; the write is
            // done, so no other reference to the packet is live.
            unsafe { (*pp).reset() };
            make_ready_future(())
        })
    }

    fn close(&mut self) -> Future<()> {
        // A shutdown error is the returned future's.
        if let Err(e) = self._fd.shutdown(SHUT_WR, ShutdownKernelOnly::YES) {
            return make_exception_future(e);
        }
        make_ready_future(())
    }

    fn can_batch_flushes(&self) -> bool {
        true
    }

    fn on_batch_flush_error(&mut self) {
        shutdown_socket_fd(&mut self._fd, SHUT_RD);
    }
}

impl PosixNetworkStack {
    /// A stack whose sockets read into buffers from `allocator`; it uses `SO_REUSEPORT` when
    /// the reactor found it available. Must run on a reactor thread.
    pub fn new(_opts: &OptionGroup, allocator: AllocatorPtr) -> Self {
        PosixNetworkStack {
            // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
            _reuseport: unsafe { (*engine()).posix_reuseport_available() },
            _allocator: allocator,
        }
    }

    /// A stack with the default heap allocator.
    pub fn new_default(opts: &OptionGroup) -> Self {
        Self::new(opts, MALLOC_ALLOCATOR)
    }
}

impl NetworkStack for PosixNetworkStack {
    fn listen(
        &mut self,
        mut sa: SocketAddress,
        opt: ListenOptions,
    ) -> Result<ServerSocket, ExceptionPtr> {
        // allow unspecified bind address -> default to ipv4 wildcard
        if sa.is_unspecified() {
            sa = SocketAddress::from_inet_address(&InetAddress::from_family(Family::INET), 0);
        }
        if sa.is_af_unix() {
            return Ok(ServerSocket::from_impl(Box::new(PosixServerSocketImpl::new(
                0,
                sa,
                // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
                unsafe { (*engine()).posix_listen(sa, opt.clone()) }?,
                opt.lba,
                opt.fixed_cpu,
                self._allocator,
            ))));
        }
        let protocol = opt.proto as i32;
        Ok(if self._reuseport {
            ServerSocket::from_impl(Box::new(PosixReuseportServerSocketImpl::new(
                protocol,
                sa,
                // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
                unsafe { (*engine()).posix_listen(sa, opt.clone()) }?,
                self._allocator,
            )))
        } else {
            ServerSocket::from_impl(Box::new(PosixServerSocketImpl::new(
                protocol,
                sa,
                // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
                unsafe { (*engine()).posix_listen(sa, opt.clone()) }?,
                opt.lba,
                opt.fixed_cpu,
                self._allocator,
            )))
        })
    }

    fn socket(&mut self) -> Socket {
        Socket::from_impl(Box::new(PosixSocketImpl::new(self._allocator)))
    }

    fn make_udp_channel(&mut self, addr: &SocketAddress) -> Result<UdpChannel, ExceptionPtr> {
        if !addr.is_unspecified() {
            self.make_bound_datagram_channel(addr)
        } else {
            // Preserve the default behavior of make_udp_channel({}) which is to
            // create an unbound channel that can be used to send UDP datagrams.
            self.make_unbound_datagram_channel(AF_INET)
        }
    }

    fn make_unbound_datagram_channel(
        &mut self,
        family: sa_family_t,
    ) -> Result<DatagramChannel, ExceptionPtr> {
        Ok(DatagramChannel::from_impl(PosixDatagramChannel::new_unbound(family)?))
    }

    fn make_bound_datagram_channel(
        &mut self,
        local: &SocketAddress,
    ) -> Result<DatagramChannel, ExceptionPtr> {
        Ok(DatagramChannel::from_impl(PosixDatagramChannel::new_bound(*local)?))
    }

    fn has_per_core_namespace(&mut self) -> bool {
        self._reuseport
    }

    fn supports_ipv6(&self) -> bool {
        posix_network_stack_supports_ipv6()
    }

    fn network_interfaces(&mut self) -> Result<Vec<NetworkInterface>, ExceptionPtr> {
        posix_network_stack_network_interfaces()
    }

    fn stats(&mut self, scheduling_group_id: u32) -> Statistics {
        Statistics {
            bytes_sent: BYTES_SENT.with(|b| b[scheduling_group_id as usize].get()),
            bytes_received: BYTES_RECEIVED.with(|b| b[scheduling_group_id as usize].get()),
        }
    }

    fn clear_stats(&mut self, scheduling_group_id: u32) {
        BYTES_SENT.with(|b| b[scheduling_group_id as usize].set(0));
        BYTES_RECEIVED.with(|b| b[scheduling_group_id as usize].set(0));
    }
}

impl PosixApNetworkStack {
    /// A stack whose sockets read into buffers from `allocator`. Must run on a reactor
    /// thread.
    pub fn new(opts: &OptionGroup, allocator: AllocatorPtr) -> Self {
        PosixApNetworkStack {
            base: PosixNetworkStack::new(opts, allocator),
            // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
            _reuseport: unsafe { (*engine()).posix_reuseport_available() },
        }
    }
}

impl NetworkStack for PosixApNetworkStack {
    fn listen(
        &mut self,
        mut sa: SocketAddress,
        opt: ListenOptions,
    ) -> Result<ServerSocket, ExceptionPtr> {
        // allow unspecified bind address -> default to ipv4 wildcard
        if sa.is_unspecified() {
            sa = SocketAddress::from_inet_address(&InetAddress::from_family(Family::INET), 0);
        }
        if sa.is_af_unix() {
            return Ok(ServerSocket::from_impl(Box::new(PosixApServerSocketImpl::new(
                0,
                sa,
                self.base._allocator,
            )?)));
        }
        let protocol = opt.proto as i32;
        Ok(if self._reuseport {
            ServerSocket::from_impl(Box::new(PosixReuseportServerSocketImpl::new(
                protocol,
                sa,
                // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
                unsafe { (*engine()).posix_listen(sa, opt.clone()) }?,
                self.base._allocator,
            )))
        } else {
            ServerSocket::from_impl(Box::new(PosixApServerSocketImpl::new(
                protocol,
                sa,
                self.base._allocator,
            )?))
        })
    }

    // The other members forward to the base stack.
    fn socket(&mut self) -> Socket {
        self.base.socket()
    }
    #[allow(deprecated)]
    fn make_udp_channel(&mut self, addr: &SocketAddress) -> Result<UdpChannel, ExceptionPtr> {
        self.base.make_udp_channel(addr)
    }
    fn make_unbound_datagram_channel(
        &mut self,
        family: sa_family_t,
    ) -> Result<DatagramChannel, ExceptionPtr> {
        self.base.make_unbound_datagram_channel(family)
    }
    fn make_bound_datagram_channel(
        &mut self,
        local: &SocketAddress,
    ) -> Result<DatagramChannel, ExceptionPtr> {
        self.base.make_bound_datagram_channel(local)
    }
    fn has_per_core_namespace(&mut self) -> bool {
        self.base.has_per_core_namespace()
    }
    fn supports_ipv6(&self) -> bool {
        self.base.supports_ipv6()
    }
    fn network_interfaces(&mut self) -> Result<Vec<NetworkInterface>, ExceptionPtr> {
        self.base.network_interfaces()
    }
    fn stats(&mut self, scheduling_group_id: u32) -> Statistics {
        self.base.stats(scheduling_group_id)
    }
    fn clear_stats(&mut self, scheduling_group_id: u32) {
        self.base.clear_stats(scheduling_group_id)
    }
}

/// Room for either packet-info control message.
#[repr(C)]
#[derive(Clone, Copy)]
union CmsgPktinfoU {
    pktinfo: in_pktinfo,
    pkt6info: in6_pktinfo,
}

/// The control buffer of a receive: one packet-info message.
#[repr(C)]
#[derive(Clone, Copy)]
struct CmsgWithPktinfo {
    cmh: cmsghdr,
    u: CmsgPktinfoU,
}

/// The largest UDP payload over IPv4 (65535 - 8 - 20).
const MAX_DATAGRAM_SIZE: usize = 65507;

/// The `recvmsg` state of a datagram channel: header, I/O vector, source address, the
/// buffer of the datagram being received and the packet-info control buffer.
struct RecvCtx {
    _hdr: msghdr,
    _iov: iovec,
    _src_addr: SocketAddress,
    _buffer: *mut u8,
    _cmsg: CmsgWithPktinfo,
    use_pktinfo: bool,
}

impl RecvCtx {
    fn new(use_pktinfo: bool) -> Self {
        RecvCtx {
            _hdr: msghdr::default(),
            _iov: iovec { iov_base: std::ptr::null_mut(), iov_len: 0 },
            _src_addr: SocketAddress::default(),
            _buffer: std::ptr::null_mut(),
            // SAFETY: plain old data, for which all-zero bytes are a valid value.
            _cmsg: unsafe { std::mem::zeroed() },
            use_pktinfo,
        }
    }

    /// Points the header at the context's own I/O vector, source address and (with
    /// `use_pktinfo`) control buffer, once the context has its final address; the context
    /// must not move afterwards. The pointers
    /// stored in the header are projected from `this`, so they keep the provenance of the
    /// context's node and stay valid when references to the context are made later.
    ///
    /// # Safety
    /// `this` is the context's root pointer (projected from its node), with no reference to
    /// the context live.
    unsafe fn init(this: *mut RecvCtx) {
        use std::ptr::addr_of_mut;
        // SAFETY: guaranteed by the caller; place accesses and projections only. `_src_addr.u`
        // is a `repr(C)` union of plain old data, so its `sa` member is at its start.
        unsafe {
            (*this)._hdr = msghdr::default();
            (*this)._hdr.msg_iov = addr_of_mut!((*this)._iov);
            (*this)._hdr.msg_iovlen = 1;
            (*this)._hdr.msg_name = addr_of_mut!((*this)._src_addr.u.sa) as *mut c_void;
            (*this)._hdr.msg_namelen = std::mem::size_of::<sockaddr_storage>() as socklen_t;

            if (*this).use_pktinfo {
                // Plain old data.
                (*this)._cmsg = std::mem::zeroed();
                (*this)._hdr.msg_control = addr_of_mut!((*this)._cmsg) as *mut c_void;
                (*this)._hdr.msg_controllen = std::mem::size_of::<CmsgWithPktinfo>();
            } else {
                (*this)._hdr.msg_control = std::ptr::null_mut();
                (*this)._hdr.msg_controllen = 0;
            }
        }
    }

    /// Allocates the buffer of the next datagram (`MAX_DATAGRAM_SIZE` bytes) and points the
    /// I/O vector at it.
    fn prepare(&mut self) {
        // Left uninitialized: recvmsg writes the
        // datagram before any byte of it is read, and only the received bytes are read.
        let buf: Box<[std::mem::MaybeUninit<u8>]> = Box::new_uninit_slice(MAX_DATAGRAM_SIZE);
        self._buffer = Box::into_raw(buf) as *mut u8;
        self._iov.iov_base = self._buffer as *mut c_void;
        self._iov.iov_len = MAX_DATAGRAM_SIZE;
        // recvmsg() overwrites these with the lengths it used: give it the whole buffers
        // again for every datagram.
        self._hdr.msg_namelen = std::mem::size_of::<sockaddr_storage>() as socklen_t;
        self._hdr.msg_controllen =
            if self.use_pktinfo { std::mem::size_of::<CmsgWithPktinfo>() } else { 0 };
    }
}

/// Frees a buffer made by `RecvCtx::prepare`.
fn delete_recv_buffer(buf: *mut u8) {
    // SAFETY: `buf` came from Box::into_raw of a MAX_DATAGRAM_SIZE boxed slice of
    // MaybeUninit<u8>, which is freed with the same type.
    unsafe {
        drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
            buf as *mut std::mem::MaybeUninit<u8>,
            MAX_DATAGRAM_SIZE,
        )))
    };
}

/// The `sendmsg` state of a datagram channel: header, I/O vector, destination and the
/// packet being sent.
struct SendCtx {
    _hdr: msghdr,
    _iovecs: Vec<iovec>,
    _dst: SocketAddress,
    _p: Packet,
}

impl SendCtx {
    fn new() -> Self {
        SendCtx {
            _hdr: msghdr::default(),
            _iovecs: Vec::new(),
            _dst: SocketAddress::default(),
            _p: Packet::make_null_packet(),
        }
    }

    /// Points the header at the context's own destination address, projected from `this`
    /// as in `RecvCtx::init`.
    ///
    /// # Safety
    /// As for `RecvCtx::init`.
    unsafe fn init(this: *mut SendCtx) {
        // SAFETY: as in `RecvCtx::init`.
        unsafe {
            (*this)._hdr = msghdr::default();
            (*this)._hdr.msg_name = std::ptr::addr_of_mut!((*this)._dst.u.sa) as *mut c_void;
            (*this)._hdr.msg_namelen = (*this)._dst.addr_length;
        }
    }

    /// Sets up the send of `p` to `dst`.
    ///
    /// # Errors
    ///
    /// The error of resolving a link-local destination's scope.
    fn prepare(&mut self, dst: &SocketAddress, p: Packet) -> Result<(), ExceptionPtr> {
        self._dst = *dst;
        self._hdr.msg_namelen = self._dst.addr_length;
        self._p = p;
        self._iovecs = to_iovec(&self._p);
        self._hdr.msg_iov = self._iovecs.as_mut_ptr();
        self._hdr.msg_iovlen = self._iovecs.len();
        resolve_outgoing_address(&mut self._dst)
    }
}

/// The receive and send state of a [`PosixDatagramChannel`], in its own node: their message
/// headers point into themselves (`init`), the reactor keeps a pointer to them while a
/// receive or send runs, and the receive continuation reads them after the call. A `Box`
/// that moves would invalidate those pointers.
struct DatagramCtx {
    _recv: RecvCtx,
    _send: SendCtx,
}

/// A datagram channel over a kernel datagram socket (UDP or UNIX). On IP sockets it asks for
/// `IP_PKTINFO`, so that each received datagram reports the address it was sent to, and it
/// sets `SO_REUSEPORT` when available. One receive and one send may be in flight at a time.
pub struct PosixDatagramChannel {
    _fd: PollableFd,
    _address: SocketAddress,
    _ctx: NodePtr<DatagramCtx>,
    _closed: bool,
}

impl PosixDatagramChannel {
    /// Whether `family` is IPv4 or IPv6.
    fn is_inet(family: sa_family_t) -> bool {
        family == AF_INET || family == AF_INET6
    }

    /// A non-blocking datagram socket of `family`, with the IP options above.
    fn create_socket(family: sa_family_t) -> Result<FileDesc, ExceptionPtr> {
        let fd = FileDesc::socket(family as i32, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0)?;

        if Self::is_inet(family) {
            // The kernel attaches IP_PKTINFO to IPv4 datagrams only: an IPv6 socket asks for
            // IPV6_PKTINFO, which also covers the IPv4 datagrams of a dual-stack socket
            // (reported as v4-mapped addresses, like their source).
            if family == AF_INET6 {
                fd.setsockopt::<c_int>(IPPROTO_IPV6, IPV6_RECVPKTINFO, 1)?;
            } else {
                fd.setsockopt::<c_int>(SOL_IP, IP_PKTINFO, 1)?;
            }
            // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
            if unsafe { (*engine()).posix_reuseport_available() } {
                fd.setsockopt::<c_int>(SOL_SOCKET, SO_REUSEPORT, 1)?;
            }
        }

        Ok(fd)
    }

    /// The channel of `fd`, its contexts initialized in place.
    fn boxed(family: sa_family_t, fd: FileDesc) -> Result<Box<PosixDatagramChannel>, ExceptionPtr> {
        let address = fd.get_address()?;
        let mut this = Box::new(PosixDatagramChannel {
            _fd: PollableFd::default(),
            _address: address,
            _ctx: NodePtr::new(DatagramCtx {
                _recv: RecvCtx::new(Self::is_inet(family)),
                _send: SendCtx::new(),
            }),
            _closed: false,
        });
        let c = this._ctx.as_ptr();
        // SAFETY: the context was just made in its node; nothing else refers to it. The
        // pointers `init` stores into it are derived from its root pointer.
        unsafe {
            RecvCtx::init(std::ptr::addr_of_mut!((*c)._recv));
            SendCtx::init(std::ptr::addr_of_mut!((*c)._send));
        }
        this._fd = PollableFd::from_file_desc(fd);
        Ok(this)
    }

    /// Creates a channel that is not bound to any socket address. The channel can be used
    /// to communicate with addresses that belong to `family`.
    ///
    /// # Errors
    ///
    /// When the socket cannot be created or configured.
    pub fn new_unbound(family: sa_family_t) -> Result<Box<PosixDatagramChannel>, ExceptionPtr> {
        let fd = Self::create_socket(family)?;
        Self::boxed(family, fd)
    }

    /// Creates a channel that is bound to the specified local address. It can be used to
    /// communicate with addresses that belong to the family of `local`.
    ///
    /// # Errors
    ///
    /// When the socket cannot be created, configured or bound.
    pub fn new_bound(local: SocketAddress) -> Result<Box<PosixDatagramChannel>, ExceptionPtr> {
        let fd = Self::create_socket(local.family())?;
        // SAFETY: the address passed is at least as long as the length passed.
        unsafe { fd.bind(local.as_posix_sockaddr(), local.addr_length) }?;
        Self::boxed(local.family(), fd)
    }
}

impl Drop for PosixDatagramChannel {
    fn drop(&mut self) {
        if !self._closed {
            self.close();
        }
    }
}

impl DatagramChannelImpl for PosixDatagramChannel {
    fn receive(&mut self) -> Future<Datagram> {
        // The context's root pointer: the reactor and the continuation use it after this call.
        let c: *mut DatagramCtx = self._ctx.as_ptr();
        let address = self._address;
        // SAFETY: no other reference to the context is live (a receive completes before the
        // next, which the channel contract requires).
        let (p, hdr) = unsafe {
            (*c)._recv.prepare();
            ((*c)._recv._buffer, std::ptr::addr_of_mut!((*c)._recv._hdr))
        };
        // SAFETY: the header and its buffers are in the context node, which the channel keeps
        // and nothing else uses until the receive is done.
        unsafe { self._fd.recvmsg(hdr) }
            .then(move |size: usize| {
                // SAFETY: the caller keeps the channel alive until its receive()
                // completes; the context is reached through its root pointer, the receive being
                // done, with short accesses only: the control buffer is read through the header's
                // `msg_control`, which `init` projected from that pointer.
                let hdr: *const msghdr = unsafe { std::ptr::addr_of!((*c)._recv._hdr) };
                // The source address has the length the kernel returned. An unbound AF_UNIX
                // sender has no name and recvmsg() leaves the name untouched: report it as an
                // unnamed AF_UNIX address, not as the previous sender.
                // SAFETY: short accesses through the root pointer (see above).
                unsafe {
                    let namelen = (*hdr).msg_namelen;
                    let src = &mut *std::ptr::addr_of_mut!((*c)._recv._src_addr);
                    if (namelen as usize) < std::mem::size_of::<sa_family_t>() {
                        *src = SocketAddress::zeroed();
                        src.as_posix_sockaddr_mut().sa_family = address.family();
                        src.addr_length = std::mem::size_of::<sa_family_t>() as socklen_t;
                    } else {
                        src.addr_length = namelen;
                    }
                }
                let mut dst: Option<SocketAddress> = None;
                // SAFETY: the control buffer was filled by recvmsg.
                let mut cmsg = unsafe { cmsg_firsthdr(hdr) };
                while !cmsg.is_null() {
                    // SAFETY: cmsg points into the control buffer.
                    let (level, ty, len) =
                        unsafe { ((*cmsg).cmsg_level, (*cmsg).cmsg_type, (*cmsg).cmsg_len) };
                    // A truncated message keeps its header but not all its data.
                    let covers = |payload: usize| {
                        len >= cmsg_align(std::mem::size_of::<cmsghdr>()) + payload
                    };
                    if level == IPPROTO_IP && ty == IP_PKTINFO && covers(std::mem::size_of::<in_pktinfo>()) {
                        // SAFETY: an IP_PKTINFO message carries an in_pktinfo.
                        let pi: in_pktinfo = copy_reinterpret_cast(unsafe { cmsg_data(cmsg) });
                        dst = Some(SocketAddress::from_ipv4_addr(Ipv4Addr::from_in_addr(
                            &pi.ipi_addr,
                            address.port(),
                        )));
                        break;
                    } else if level == IPPROTO_IPV6
                        && ty == IPV6_PKTINFO
                        && covers(std::mem::size_of::<in6_pktinfo>())
                    {
                        // SAFETY: an IPV6_PKTINFO message carries an in6_pktinfo.
                        let pi: in6_pktinfo = copy_reinterpret_cast(unsafe { cmsg_data(cmsg) });
                        dst = Some(SocketAddress::from_ipv6_addr(&Ipv6Addr::from_in6_addr(
                            &pi.ipi6_addr,
                            address.port(),
                        )));
                        break;
                    }
                    // SAFETY: as above.
                    cmsg = unsafe { cmsg_nxthdr(hdr, cmsg) };
                }
                add_bytes_received(size as u64);
                // SAFETY: short accesses through the root pointer (see above).
                let (buf, src_addr) = unsafe { ((*c)._recv._buffer, (*c)._recv._src_addr) };
                make_ready_future::<Datagram>(Datagram::new(Box::new(PosixDatagram::new(
                    &src_addr,
                    &dst.unwrap_or(address),
                    Packet::from_fragment_deleter(
                        Fragment { base: buf, size },
                        make_deleter(move || delete_recv_buffer(buf)),
                    ),
                ))))
            })
            .handle_exception(move |ep: ExceptionPtr| {
                delete_recv_buffer(p);
                make_exception_future::<Datagram>(ep)
            })
    }

    fn send_str(&mut self, dst: &SocketAddress, message: &'static str) -> Future<()> {
        // The message ends at its first NUL byte, if any; it is not copied.
        let len = message.bytes().position(|b| b == 0).unwrap_or(message.len());
        let mut a = *dst;
        if let Err(e) = resolve_outgoing_address(&mut a) {
            return make_exception_future(e);
        }
        add_bytes_sent(len as u64);
        // SAFETY: the message is `'static`: its `len` bytes stay valid and unchanged.
        unsafe { self._fd.sendto(a, message.as_ptr() as *const c_void, len) }.then(
            move |size: usize| {
                assert!(size == len);
                make_ready_future(())
            },
        )
    }

    fn send(&mut self, dst: &SocketAddress, p: Packet) -> Future<()> {
        let len = p.len() as usize;
        let c: *mut DatagramCtx = self._ctx.as_ptr();
        // SAFETY: no other reference to the context is live (a send completes before the
        // next, which the channel contract requires).
        if let Err(e) = unsafe { (*c)._send.prepare(dst, p) } {
            return make_exception_future(e);
        }
        add_bytes_sent(len as u64);
        // SAFETY: a projection from the context's root pointer, kept by the reactor while the
        // send runs; the packet buffers it describes stay in the context until then.
        unsafe { self._fd.sendmsg(std::ptr::addr_of_mut!((*c)._send._hdr)) }.then(
            move |size: usize| {
                assert!(size == len);
                make_ready_future(())
            },
        )
    }

    fn shutdown_input(&mut self) {
        // ENOTCONN is ignored below; EINVAL, ENOTSOCK and EBADF cannot happen on this open
        // socket, and the trait method returns nothing.
        let _ = self._fd.shutdown(SHUT_RD, ShutdownKernelOnly::NO);
    }

    fn shutdown_output(&mut self) {
        // As in shutdown_input().
        let _ = self._fd.shutdown(SHUT_WR, ShutdownKernelOnly::NO);
    }

    fn close(&mut self) {
        self._closed = true;
        self._fd = PollableFd::default();
    }

    fn is_closed(&self) -> bool {
        self._closed
    }

    fn local_address(&self) -> SocketAddress {
        assert!(self._address.family() != AF_INET6 || self._address.addr_length > 20);
        self._address
    }
}

/// The IPv6 loopback address `::1` with port 0 (any port), the address the IPv6 probe binds.
///
/// The address is parsed without brackets or port: the bracket-aware parser would read the
/// text after the first `:` as a port and reject it.
///
/// # Errors
///
/// The error of [`Ipv6Addr::from_string_port`], which cannot happen for this constant input.
fn ipv6_loopback_any_port() -> Result<Ipv6Addr, ExceptionPtr> {
    Ipv6Addr::from_string_port("::1", 0)
}

/// Whether a datagram socket can be bound to the IPv6 loopback address; computed once per
/// process.
fn posix_network_stack_supports_ipv6() -> bool {
    static HAS_IPV6: OnceLock<bool> = OnceLock::new();
    *HAS_IPV6.get_or_init(|| {
        let Ok(addr) = ipv6_loopback_any_port() else {
            return false;
        };
        match PosixDatagramChannel::new_bound(SocketAddress::from_ipv6_addr(&addr)) {
            Ok(mut c) => {
                c.close();
                true
            }
            Err(_) => false,
        }
    })
}

/// A datagram received by a [`PosixDatagramChannel`].
pub struct PosixDatagram {
    _src: SocketAddress,
    _dst: SocketAddress,
    _p: Packet,
}

impl PosixDatagram {
    /// A datagram from `src` to `dst` carrying `p`.
    pub fn new(src: &SocketAddress, dst: &SocketAddress, p: Packet) -> Self {
        PosixDatagram { _src: *src, _dst: *dst, _p: p }
    }
}

impl DatagramImpl for PosixDatagram {
    fn get_src(&mut self) -> SocketAddress {
        self._src
    }
    fn get_dst(&mut self) -> SocketAddress {
        self._dst
    }
    fn get_dst_port(&mut self) -> Result<u16, ExceptionPtr> {
        if self._dst.family() != AF_INET && self._dst.family() != AF_INET6 {
            return Err(ExceptionPtr::from(RuntimeError(format!(
                "get_dst_port() called on non-IP address: {}",
                self._dst
            ))));
        }
        Ok(self._dst.port())
    }
    fn get_data(&mut self) -> &mut Packet {
        &mut self._p
    }
}

/// The registration of the POSIX stack (`posix`, the default): a [`PosixNetworkStack`] on the
/// main shard and a [`PosixApNetworkStack`] on the others, both with the heap allocator.
pub fn register_posix_stack() -> NetworkStackEntry {
    NetworkStackEntry {
        name: Sstring::from("posix"),
        // A root option group with no options, so nothing to link.
        opts: Box::new(OptionGroup::new("Posix")),
        factory: FactoryFunc::new(|ops: &OptionGroup| {
            if Smp::main_thread() {
                PosixNetworkStack::create(ops, MALLOC_ALLOCATOR)
            } else {
                PosixApNetworkStack::create(ops, MALLOC_ALLOCATOR)
            }
        }),
        is_default: true,
    }
}

// Network interfaces

/// A host network interface as read from rtnetlink at the first query.
#[derive(Clone, Default)]
pub struct PosixNetworkInterfaceImpl {
    /// The interface index.
    pub _index: u32,
    /// The MTU.
    pub _mtu: u32,
    /// The name.
    pub _name: Sstring,
    /// The display name; the name is used when empty.
    pub _display_name: Sstring,
    /// The assigned addresses.
    pub _addresses: Vec<InetAddress>,
    /// The hardware address.
    pub _hardware_address: Vec<u8>,
    /// Whether it is a loopback interface.
    pub _loopback: bool,
    /// Whether it is a virtual interface (never set: the kernel reports no such flag).
    pub _virtual: bool,
    /// Whether it was up when read.
    pub _up: bool,
}

impl NetworkInterfaceImpl for PosixNetworkInterfaceImpl {
    fn index(&self) -> u32 {
        self._index
    }
    fn mtu(&self) -> u32 {
        self._mtu
    }
    fn name(&self) -> &Sstring {
        &self._name
    }
    fn display_name(&self) -> &Sstring {
        if self._display_name.empty() { self.name() } else { &self._display_name }
    }
    fn addresses(&self) -> &Vec<InetAddress> {
        &self._addresses
    }
    fn hardware_address(&self) -> Vec<u8> {
        self._hardware_address.clone()
    }
    fn is_loopback(&self) -> bool {
        self._loopback
    }
    fn is_virtual(&self) -> bool {
        self._virtual
    }
    fn is_up(&self) -> bool {
        // TODO: should be checked on query?
        self._up
    }
    fn supports_ipv6(&self) -> bool {
        // TODO: this is not 100% correct.
        self._addresses.iter().any(|a| a.is_ipv6())
    }
}

/// The body of a dump request.
#[repr(C)]
#[derive(Clone, Copy)]
union NlReqU {
    r#gen: rtgenmsg,
    addr: ifaddrmsg,
}

/// An rtnetlink dump request.
#[repr(C)]
#[derive(Clone, Copy)]
struct NlReq {
    hdr: nlmsghdr,
    u: NlReqU,
}

/// Reads the host's interfaces and their addresses over rtnetlink (`RTM_GETLINK`, then
/// `RTM_GETADDR`).
///
/// # Errors
///
/// When the netlink socket cannot be opened, bound or written.
fn read_global_interfaces() -> Result<Vec<PosixNetworkInterfaceImpl>, ExceptionPtr> {
    // SAFETY: plain socket(2).
    let fd = unsafe { socket(AF_NETLINK as c_int, SOCK_RAW, NETLINK_ROUTE) };
    throw_system_error_on(fd < 0, "could not open netlink socket")?;

    struct FdGuard(c_int);
    impl Drop for FdGuard {
        fn drop(&mut self) {
            // SAFETY: the descriptor was opened above and is closed once.
            unsafe { close(self.0) };
        }
    }
    let _fd_guard = FdGuard(fd);

    // SAFETY: plain getpid(2).
    let pid = unsafe { getpid() };

    let local = sockaddr_nl {
        nl_family: AF_NETLINK,
        nl_pad: 0,
        nl_pid: pid as u32,
        nl_groups: RTMGRP_IPV6_IFADDR | RTMGRP_IPV4_IFADDR,
    };

    throw_system_error_on(
        // SAFETY: `local` is a valid sockaddr_nl of the given size and `fd` an open socket.
        unsafe {
            bind(
                fd,
                &local as *const sockaddr_nl as *const c_void,
                std::mem::size_of::<sockaddr_nl>() as socklen_t,
            )
        } < 0,
        "could not bind netlink socket",
    )?;

    /* RTNL socket is ready for use, prepare and send requests */

    let mut res: Vec<PosixNetworkInterfaceImpl> = Vec::new();

    for msg in [RTM_GETLINK, RTM_GETADDR] {
        // SAFETY: plain old data, for which all-zero bytes are a valid value.
        let mut req: NlReq = unsafe { std::mem::zeroed() };

        req.hdr.nlmsg_len = nlmsg_length(std::mem::size_of::<rtgenmsg>() as u32);
        req.hdr.nlmsg_type = msg;
        req.hdr.nlmsg_flags = NLM_F_REQUEST | NLM_F_ROOT;
        req.hdr.nlmsg_seq = 1;
        req.hdr.nlmsg_pid = pid as u32;

        if msg == RTM_GETLINK {
            req.u.r#gen.rtgen_family = AF_PACKET as u8; /*  no preferred AF, we will get *all* interfaces */
        } else {
            req.u.addr.ifa_family = AF_UNSPEC as u8;
        }

        let mut kernel = sockaddr_nl {
            nl_family: AF_NETLINK, /* fill-in kernel address (destination of our message) */
            ..Default::default()
        };
        let mut io = iovec {
            iov_base: &mut req as *mut NlReq as *mut c_void,
            iov_len: req.hdr.nlmsg_len as usize,
        };
        let rtnl_msg = msghdr {
            msg_name: &mut kernel as *mut sockaddr_nl as *mut c_void,
            msg_namelen: std::mem::size_of::<sockaddr_nl>() as socklen_t,
            msg_iov: &mut io,
            msg_iovlen: 1,
            ..Default::default()
        };

        throw_system_error_on(
            // SAFETY: rtnl_msg points to valid buffers.
            unsafe { sendmsg(fd, &rtnl_msg, 0) } < 0,
            "could not send netlink request",
        )?;
        /* parse reply */

        const REPLY_BUFFER_SIZE: usize = 8192;
        // The parser reads the reply byte-wise, so the buffer needs no alignment.
        let mut reply = [0u8; REPLY_BUFFER_SIZE];

        let mut done = false;

        while !done {
            let mut io_reply =
                iovec { iov_base: reply.as_mut_ptr() as *mut c_void, iov_len: REPLY_BUFFER_SIZE };
            let mut rtnl_reply = msghdr {
                msg_name: &mut kernel as *mut sockaddr_nl as *mut c_void,
                msg_namelen: std::mem::size_of::<sockaddr_nl>() as socklen_t,
                msg_iov: &mut io_reply,
                msg_iovlen: 1,
                ..Default::default()
            };

            // SAFETY: rtnl_reply points to valid buffers.
            let len = unsafe { recvmsg(fd, &mut rtnl_reply, 0) }; /* read as much data as fits in the receive buffer */
            if len <= 0 {
                return Ok(res);
            }

            // recvmsg returns at most the iovec's length; the cap keeps the slice in bounds
            // regardless.
            let n = (len as usize).min(REPLY_BUFFER_SIZE);
            done = parse_netlink_reply(&reply[..n], &mut res);
        }
    }

    Ok(res)
}

/// The host's network interfaces: read once per process (on success) and shared by the
/// shards, each shard keeping its own reference-counted copies to hand out cheaply.
///
/// # Errors
///
/// The error of reading the interfaces, until a read succeeds.
fn posix_network_stack_network_interfaces() -> Result<Vec<NetworkInterface>, ExceptionPtr> {
    // For now, keep an immutable set of interfaces created on start, shared across
    // shards
    // SAFETY: the interfaces (names, numbers, addresses and bytes) have no interior
    // mutability; their heap blocks are only read once built.
    static GLOBAL_INTERFACES: StaticConst<Vec<PosixNetworkInterfaceImpl>> =
        unsafe { StaticConst::new() };
    // A failed read is retried on the next call: only a success is stored, and the error
    // reaches the caller. Two shards racing here both read the
    // interfaces; the second result is dropped.
    let global_interfaces = match GLOBAL_INTERFACES.get() {
        Some(v) => v,
        None => {
            let v = read_global_interfaces()?;
            let _ = GLOBAL_INTERFACES.set(v);
            // SAFETY: set just above (by this call or by a racing one).
            unsafe { GLOBAL_INTERFACES.get().unwrap_unchecked() }
        }
    };

    // And a similarly immutable set of shared pointers to the interfaces per shard, ready
    // to be handed out to callers with minimal overhead
    thread_local! {
        static THREAD_LOCAL_INTERFACES: std::cell::OnceCell<Vec<SharedPtr<PosixNetworkInterfaceImpl>>> =
            const { std::cell::OnceCell::new() };
    }
    THREAD_LOCAL_INTERFACES.with(|t| {
        let v = t.get_or_init(|| {
            let mut res = Vec::with_capacity(global_interfaces.len());
            for impl_ in global_interfaces.iter() {
                res.push(make_shared(impl_.clone()));
            }
            res
        });
        Ok(v.iter()
            .map(|i| {
                // SAFETY: the cast keeps the pointer into the same object.
                NetworkInterface::new(unsafe {
                    shared_ptr_converting_move(i.clone(), |p| p as *mut dyn NetworkInterfaceImpl)
                })
            })
            .collect())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_error_code_reads_a_failed_bind() {
        // file_desc::bind() fails with a SystemError; the random-port retry must see its code.
        let err = ExceptionPtr::from(SystemError::new(EADDRINUSE, "bind"));
        assert_eq!(system_error_code(&err), Some(EADDRINUSE));
    }

    #[test]
    fn system_error_code_reads_an_io_error() {
        let err = ExceptionPtr::from(std::io::Error::from_raw_os_error(EADDRNOTAVAIL));
        assert_eq!(system_error_code(&err), Some(EADDRNOTAVAIL));
    }

    #[test]
    fn system_error_code_is_none_for_other_exceptions() {
        let err = ExceptionPtr::new(crate::engine::core::future::InvalidArgument::new("x"));
        assert_eq!(system_error_code(&err), None);
    }

    #[test]
    fn ipv6_probe_address_is_the_loopback_with_any_port() {
        let addr = ipv6_loopback_any_port().expect("::1 must parse as an IPv6 address");
        assert_eq!(addr.ip, std::net::Ipv6Addr::LOCALHOST.octets());
        assert_eq!(addr.port, 0);
    }

    /// A netlink record (message when `msg`, else attribute) with its declared length set to
    /// the header plus `body` unless `declared` overrides it, padded to the alignment.
    fn record(msg: bool, ty: u16, body: &[u8], declared: Option<usize>) -> Vec<u8> {
        let hdr_len = if msg { 16 } else { 4 };
        let len = declared.unwrap_or(hdr_len + body.len());
        let mut v = Vec::new();
        if msg {
            v.extend_from_slice(&(len as u32).to_ne_bytes());
            v.extend_from_slice(&ty.to_ne_bytes());
            v.extend_from_slice(&[0u8; 10]);
        } else {
            v.extend_from_slice(&(len as u16).to_ne_bytes());
            v.extend_from_slice(&ty.to_ne_bytes());
        }
        v.extend_from_slice(body);
        while v.len() % 4 != 0 {
            v.push(0);
        }
        v
    }

    fn msg(ty: u16, body: &[u8]) -> Vec<u8> {
        record(true, ty, body, None)
    }

    fn attr(ty: u16, data: &[u8]) -> Vec<u8> {
        record(false, ty, data, None)
    }

    fn ifinfo(index: i32, flags: u32) -> Vec<u8> {
        let mut v = vec![0u8, 0, 1, 0];
        v.extend_from_slice(&index.to_ne_bytes());
        v.extend_from_slice(&flags.to_ne_bytes());
        v.extend_from_slice(&0u32.to_ne_bytes());
        v
    }

    fn ifaddr(index: u32) -> Vec<u8> {
        let mut v = vec![AF_INET as u8, 24, 0, 0];
        v.extend_from_slice(&index.to_ne_bytes());
        v
    }

    fn link_msg(index: i32, attrs: &[Vec<u8>]) -> Vec<u8> {
        let mut body = ifinfo(index, IFF_UP);
        for a in attrs {
            body.extend_from_slice(a);
        }
        msg(RTM_NEWLINK, &body)
    }

    #[test]
    fn netlink_reply_with_a_link_and_its_address_is_parsed() {
        let mut reply = link_msg(
            2,
            &[
                attr(IFLA_IFNAME, b"eth0\0"),
                attr(IFLA_MTU, &1500u32.to_ne_bytes()),
                attr(IFLA_ADDRESS, &[1, 2, 3, 4, 5, 6]),
            ],
        );
        let mut body = ifaddr(2);
        body.extend_from_slice(&attr(IFA_LOCAL, &[10, 0, 0, 7]));
        body.extend_from_slice(&attr(
            IFA_ADDRESS,
            &[0xfe, 0x80, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1],
        ));
        reply.extend_from_slice(&msg(RTM_NEWADDR, &body));
        reply.extend_from_slice(&msg(NLMSG_DONE, &[0; 4]));

        let mut res = Vec::new();
        assert!(parse_netlink_reply(&reply, &mut res));
        assert_eq!(res.len(), 1);
        let nwif = &res[0];
        assert_eq!(nwif._index, 2);
        assert!(nwif._up && !nwif._loopback);
        assert_eq!(nwif._name, Sstring::from("eth0"));
        assert_eq!(nwif._mtu, 1500);
        assert_eq!(nwif._hardware_address, vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(nwif._addresses.len(), 2);
        assert!(nwif._addresses[0].is_ipv4());
        assert!(nwif._addresses[1].is_ipv6());
    }

    #[test]
    fn netlink_message_shorter_than_its_body_is_skipped() {
        // nlmsg_len covers the header and 8 bytes: too short for an ifinfomsg (16) or for an
        // ifaddrmsg with attributes. The length subtraction of the walk used to wrap here.
        let mut reply = msg(RTM_NEWLINK, &[0; 8]);
        reply.extend_from_slice(&msg(RTM_NEWADDR, &[0; 4]));
        reply.extend_from_slice(&link_msg(3, &[attr(IFLA_IFNAME, b"lo\0")]));
        let mut res = Vec::new();
        assert!(!parse_netlink_reply(&reply, &mut res));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0]._name, Sstring::from("lo"));
    }

    #[test]
    fn netlink_message_with_a_bad_length_ends_the_walk() {
        let valid = link_msg(4, &[]);
        // The bad message (32 bytes) and the DONE after it (20 bytes) are what is left of the
        // reply when the bad header is read.
        for declared in [0usize, 15, 32 + 20 + 1, 1 << 20] {
            let mut reply = valid.clone();
            reply.extend_from_slice(&record(true, RTM_NEWLINK, &ifinfo(5, 0), Some(declared)));
            reply.extend_from_slice(&msg(NLMSG_DONE, &[0; 4]));
            let mut res = Vec::new();
            // The DONE after the bad message is not reached.
            assert!(!parse_netlink_reply(&reply, &mut res), "declared {declared}");
            assert_eq!(res.len(), 1, "declared {declared}");
            assert_eq!(res[0]._index, 4);
        }
        // A header cut short by the end of the reply.
        let mut reply = valid.clone();
        reply.extend_from_slice(&[32, 0, 0]);
        let mut res = Vec::new();
        assert!(!parse_netlink_reply(&reply, &mut res));
        assert_eq!(res.len(), 1);
    }

    #[test]
    fn netlink_attributes_are_read_within_their_bounds() {
        // A name with no NUL is cut at the end of its attribute; an MTU too short to hold a
        // u32 is ignored; an attribute whose length runs past the message ends the attribute
        // walk, as does one shorter than its header.
        let mut over = attr(IFLA_MTU, &[0; 4]);
        over[0..2].copy_from_slice(&64u16.to_ne_bytes());
        let mut res = Vec::new();
        let reply = link_msg(
            6,
            &[
                attr(IFLA_IFNAME, b"abcdefgh"),
                attr(IFLA_MTU, &[1, 2]),
                over,
                attr(IFLA_ADDRESS, &[9]),
            ],
        );
        parse_netlink_reply(&reply, &mut res);
        assert_eq!(res[0]._name, Sstring::from("abcdefgh"));
        assert_eq!(res[0]._mtu, 0);
        assert!(res[0]._hardware_address.is_empty());

        let mut res = Vec::new();
        let reply = link_msg(
            7,
            &[record(false, IFLA_IFNAME, b"x\0", Some(2)), attr(IFLA_MTU, &9000u32.to_ne_bytes())],
        );
        parse_netlink_reply(&reply, &mut res);
        assert!(res[0]._name.is_empty());
        assert_eq!(res[0]._mtu, 0);
    }

    #[test]
    fn netlink_unaligned_last_attribute_does_not_wrap_the_length() {
        // The message ends right after a 5-byte attribute (its aligned size, 8, is past the
        // end): the walk stops there instead of wrapping the remaining length.
        let mut body = ifinfo(8, 0);
        body.extend_from_slice(&attr(IFLA_ADDRESS, &[0xab]));
        let mut reply = record(true, RTM_NEWLINK, &body, Some(16 + 16 + 5));
        reply.extend_from_slice(&msg(NLMSG_DONE, &[0; 4]));
        let mut res = Vec::new();
        assert!(parse_netlink_reply(&reply, &mut res));
        assert_eq!(res.len(), 1);
        assert_eq!(res[0]._hardware_address, vec![0xab]);
    }

    #[test]
    fn netlink_reply_of_arbitrary_bytes_does_not_panic() {
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        };
        let seed =
            link_msg(1, &[attr(IFLA_IFNAME, b"eth0\0"), attr(IFLA_MTU, &1500u32.to_ne_bytes())]);
        for _ in 0..5_000 {
            let mut reply = seed.clone();
            for _ in 0..(next() % 4) {
                let at = next() as usize % reply.len();
                reply[at] = next();
            }
            let cut = next() as usize % (reply.len() + 1);
            let mut res = Vec::new();
            parse_netlink_reply(&reply[..cut], &mut res);
        }
    }

    #[test]
    fn bracket_parser_rejects_an_unbracketed_ipv6_address() {
        // The defect the probe had: without brackets the text after the first ':' is a port.
        assert!(Ipv6Addr::from_string("::1").is_err());
        let bracketed = Ipv6Addr::from_string("[::1]").expect("[::1] must parse");
        assert_eq!(bracketed.ip, std::net::Ipv6Addr::LOCALHOST.octets());
    }
}
