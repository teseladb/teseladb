//! Socket address definitions shared by the POSIX and native stacks.
//!
//! The first part declares the POSIX socket structures and constants (`<sys/socket.h>`,
//! `<sys/un.h>`, `<netinet/in.h>`, `<arpa/inet.h>`) with their Linux layout, so that the
//! crate needs no binding crate for them. The second part defines the engine's own address
//! types: [`SocketAddress`] (a `sockaddr` union plus its length, passed to and from the
//! kernel unchanged), [`Ipv4Addr`] and [`Ipv6Addr`] (an IP address and a port in host
//! order), and [`Transport`].
//!
//! Most constructors, `Display` and `Hash` of these types live in
//! [`socket_address`](crate::engine::net::socket_address),
//! [`inet_address`](crate::engine::net::inet_address) and [`ip`](crate::engine::net::ip).

use std::fmt;
use std::hash::{Hash, Hasher};

// ---------------------------------------------------------------------------
// <sys/socket.h>, <sys/un.h>, <netinet/in.h>, <arpa/inet.h> (Linux)
// ---------------------------------------------------------------------------

/// An address family (`AF_*`).
#[allow(non_camel_case_types)]
pub type sa_family_t = u16;
/// The length of a socket address, in bytes.
#[allow(non_camel_case_types)]
pub type socklen_t = u32;
/// A port in network byte order.
#[allow(non_camel_case_types)]
pub type in_port_t = u16;
/// An IPv4 address in network byte order.
#[allow(non_camel_case_types)]
pub type in_addr_t = u32;

/// Unspecified address family.
pub const AF_UNSPEC: sa_family_t = 0;
/// UNIX-domain sockets.
pub const AF_UNIX: sa_family_t = 1;
/// IPv4.
pub const AF_INET: sa_family_t = 2;
/// IPv6.
pub const AF_INET6: sa_family_t = 10;
/// Raw link-level packets.
pub const AF_PACKET: sa_family_t = 17;
/// Netlink (kernel/user-space messaging).
pub const AF_NETLINK: sa_family_t = 16;

/// The IP protocol (socket option level).
pub const IPPROTO_IP: i32 = 0;
/// ICMP.
pub const IPPROTO_ICMP: i32 = 1;
/// TCP.
pub const IPPROTO_TCP: i32 = 6;
/// UDP.
pub const IPPROTO_UDP: i32 = 17;
/// IPv6 (socket option level).
pub const IPPROTO_IPV6: i32 = 41;
/// SCTP.
pub const IPPROTO_SCTP: i32 = 132;

/// Linux `errno` value for "software caused connection abort".
pub const ECONNABORTED: i32 = 103;
/// The IPv4 wildcard address `0.0.0.0`.
pub const INADDR_ANY: in_addr_t = 0;

/// `struct in_addr`: an IPv4 address.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct in_addr {
    /// The address in network byte order.
    pub s_addr: in_addr_t,
}

/// `struct in6_addr`: an IPv6 address.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct in6_addr {
    /// The 16 address bytes in network byte order.
    pub s6_addr: [u8; 16],
}

impl in6_addr {
    /// The `i`-th 32-bit word of the address, in network byte order (`s6_addr32[i]`).
    ///
    /// # Panics
    ///
    /// If `i > 3`.
    #[inline]
    pub fn s6_addr32(&self, i: usize) -> u32 {
        let b = &self.s6_addr[i * 4..i * 4 + 4];
        u32::from_ne_bytes([b[0], b[1], b[2], b[3]])
    }
    /// Sets the `i`-th 32-bit word of the address; `v` is in network byte order.
    ///
    /// # Panics
    ///
    /// If `i > 3`.
    #[inline]
    pub fn set_s6_addr32(&mut self, i: usize, v: u32) {
        self.s6_addr[i * 4..i * 4 + 4].copy_from_slice(&v.to_ne_bytes());
    }
}

/// The IPv6 wildcard address `::`.
pub const IN6ADDR_ANY: in6_addr = in6_addr { s6_addr: [0; 16] };
/// The IPv6 loopback address `::1`.
pub const IN6ADDR_LOOPBACK: in6_addr =
    in6_addr { s6_addr: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1] };

/// Whether `a` is an IPv4-mapped IPv6 address (`::ffff:a.b.c.d`).
#[inline]
pub fn in6_is_addr_v4mapped(a: &in6_addr) -> bool {
    a.s6_addr32(0) == 0 && a.s6_addr32(1) == 0 && a.s6_addr32(2) == 0xffffu32.to_be()
}

/// Whether two IPv6 addresses are equal.
#[inline]
pub fn in6_are_addr_equal(a: &in6_addr, b: &in6_addr) -> bool {
    a.s6_addr == b.s6_addr
}

/// `struct sockaddr`: the generic socket address header.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct sockaddr {
    /// The address family.
    pub sa_family: sa_family_t,
    /// Family-specific data.
    pub sa_data: [u8; 14],
}

/// `struct sockaddr_in`: an IPv4 socket address.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct sockaddr_in {
    /// `AF_INET`.
    pub sin_family: sa_family_t,
    /// The port in network byte order.
    pub sin_port: in_port_t,
    /// The address.
    pub sin_addr: in_addr,
    /// Padding to the size of `sockaddr`.
    pub sin_zero: [u8; 8],
}

/// `struct sockaddr_in6`: an IPv6 socket address.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct sockaddr_in6 {
    /// `AF_INET6`.
    pub sin6_family: sa_family_t,
    /// The port in network byte order.
    pub sin6_port: in_port_t,
    /// The flow information.
    pub sin6_flowinfo: u32,
    /// The address.
    pub sin6_addr: in6_addr,
    /// The scope (interface) id.
    pub sin6_scope_id: u32,
}

/// `struct sockaddr_un`: a UNIX-domain socket address.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct sockaddr_un {
    /// `AF_UNIX`.
    pub sun_family: sa_family_t,
    /// The path or abstract name; its length is given by the address length, not by a null.
    pub sun_path: [u8; 108],
}

impl fmt::Debug for sockaddr_un {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("sockaddr_un").field("sun_family", &self.sun_family).finish()
    }
}

/// The offset of `sun_path` in `sockaddr_un`: the length of an unnamed UNIX address.
pub const SOCKADDR_UN_SUN_PATH_OFFSET: usize = std::mem::size_of::<sa_family_t>();

/// `struct sockaddr_storage`: big and aligned enough for any socket address.
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct sockaddr_storage {
    /// The address family.
    pub ss_family: sa_family_t,
    /// Padding.
    pub __ss_padding: [u8; 118],
    /// Alignment.
    pub __ss_align: u64,
}

unsafe extern "C" {
    /// Converts the text address `src` of family `af` to binary form in `dst`; returns 1 on
    /// success, 0 if `src` is not a valid address, -1 (with `errno`) on a bad family.
    pub fn inet_pton(af: i32, src: *const std::ffi::c_char, dst: *mut std::ffi::c_void) -> i32;
    /// Converts the binary address `src` of family `af` to text in `dst` (of `size` bytes);
    /// returns `dst`, or null with `errno` set.
    pub fn inet_ntop(
        af: i32,
        src: *const std::ffi::c_void,
        dst: *mut std::ffi::c_char,
        size: socklen_t,
    ) -> *const std::ffi::c_char;
}

/// Mixes the hash `h` of a further value into `seed` (the golden-ratio combine:
/// `seed ^= h + 0x9e3779b9 + (seed << 6) + (seed >> 2)`).
#[inline]
pub fn hash_combine(seed: &mut usize, h: usize) {
    *seed ^= h.wrapping_add(0x9e3779b9).wrapping_add(*seed << 6).wrapping_add(*seed >> 2);
}

// ---------------------------------------------------------------------------
// Engine address types
// ---------------------------------------------------------------------------

/// The storage of a [`SocketAddress`]: one of the kernel's socket address structures.
#[repr(C)]
#[derive(Clone, Copy)]
pub union SocketAddressU {
    /// Room for any family.
    pub sas: sockaddr_storage,
    /// The generic header (family).
    pub sa: sockaddr,
    /// An IPv4 address.
    pub in_: sockaddr_in,
    /// An IPv6 address.
    pub in6: sockaddr_in6,
    /// A UNIX-domain address.
    pub un: sockaddr_un,
}

/// A socket address of any family, in the form the kernel takes and returns
/// (`bind`, `connect`, `accept`, `getsockname`, ...).
///
/// The family is read from the common `sa_family` header of the union. Every member is plain
/// old data, so reading any member is defined; the accessors below give each view.
#[derive(Clone, Copy)]
pub struct SocketAddress {
    /// The size of the meaningful part of `u`, in bytes.
    pub addr_length: socklen_t,
    /// The address itself.
    pub u: SocketAddressU,
}

impl SocketAddress {
    /// An `AF_INET` address copied from `sa`.
    pub fn from_sockaddr_in(sa: &sockaddr_in) -> Self {
        let mut s = Self::zeroed();
        s.addr_length = std::mem::size_of::<sockaddr_in>() as socklen_t;
        s.u.in_ = *sa;
        s
    }

    /// An `AF_INET6` address copied from `sa`.
    pub fn from_sockaddr_in6(sa: &sockaddr_in6) -> Self {
        let mut s = Self::zeroed();
        s.addr_length = std::mem::size_of::<sockaddr_in6>() as socklen_t;
        s.u.in6 = *sa;
        s
    }

    /// An all-zero address of length 0 (family `AF_UNSPEC`), the starting point of the
    /// constructors before they write one member of the union.
    pub(crate) fn zeroed() -> Self {
        // SAFETY: every member of the union is plain old data for which all-zero bytes
        // is a valid value.
        let u: SocketAddressU = unsafe { std::mem::zeroed() };
        SocketAddress { addr_length: 0, u }
    }

    /// The generic `sockaddr` view, writable.
    pub fn as_posix_sockaddr_mut(&mut self) -> &mut sockaddr {
        // SAFETY: the union is repr(C) and every member is plain old data, so any bit
        // pattern is a valid value of any member.
        unsafe { &mut self.u.sa }
    }
    /// The `sockaddr_in` view, writable.
    pub fn as_posix_sockaddr_in_mut(&mut self) -> &mut sockaddr_in {
        // SAFETY: as for `as_posix_sockaddr_mut`: all members are plain old data.
        unsafe { &mut self.u.in_ }
    }
    /// The `sockaddr_in6` view, writable.
    pub fn as_posix_sockaddr_in6_mut(&mut self) -> &mut sockaddr_in6 {
        // SAFETY: as for `as_posix_sockaddr_mut`: all members are plain old data.
        unsafe { &mut self.u.in6 }
    }
    /// The generic `sockaddr` view.
    pub fn as_posix_sockaddr(&self) -> &sockaddr {
        // SAFETY: as for `as_posix_sockaddr_mut`: all members are plain old data.
        unsafe { &self.u.sa }
    }
    /// The `sockaddr_in` view (meaningful when the family is `AF_INET`; the port is at the
    /// same offset for `AF_INET6`).
    pub fn as_posix_sockaddr_in(&self) -> &sockaddr_in {
        // SAFETY: as for `as_posix_sockaddr_mut`: all members are plain old data.
        unsafe { &self.u.in_ }
    }
    /// The `sockaddr_in6` view (meaningful when the family is `AF_INET6`).
    pub fn as_posix_sockaddr_in6(&self) -> &sockaddr_in6 {
        // SAFETY: as for `as_posix_sockaddr_mut`: all members are plain old data.
        unsafe { &self.u.in6 }
    }
    /// The `sockaddr_un` view (meaningful when the family is `AF_UNIX`).
    pub fn as_posix_sockaddr_un(&self) -> &sockaddr_un {
        // SAFETY: as for `as_posix_sockaddr_mut`: all members are plain old data.
        unsafe { &self.u.un }
    }

    /// The length of the address, in bytes.
    #[inline]
    pub fn length(&self) -> socklen_t {
        self.addr_length
    }

    /// Whether the family is `AF_UNIX`.
    #[inline]
    pub fn is_af_unix(&self) -> bool {
        self.as_posix_sockaddr().sa_family == AF_UNIX
    }

    /// The address family.
    #[inline]
    pub fn family(&self) -> sa_family_t {
        self.as_posix_sockaddr().sa_family
    }
}

/// The transport protocol of a stream socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Transport {
    /// TCP.
    TCP = IPPROTO_TCP,
    /// SCTP.
    SCTP = IPPROTO_SCTP,
}

/// An IPv4 address and port, both in host byte order.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ipv4Addr {
    /// The address.
    pub ip: u32,
    /// The port.
    pub port: u16,
}

impl Ipv4Addr {
    /// The address `ip` with port `port`.
    #[inline]
    pub fn new(ip: u32, port: u16) -> Self {
        Ipv4Addr { ip, port }
    }
    /// The wildcard address `0.0.0.0` with port `port`.
    #[inline]
    pub fn from_port(port: u16) -> Self {
        Ipv4Addr { ip: 0, port }
    }

    /// Whether the address is `0.0.0.0`.
    #[inline]
    pub fn is_ip_unspecified(&self) -> bool {
        self.ip == 0
    }
    /// Whether the port is 0.
    #[inline]
    pub fn is_port_unspecified(&self) -> bool {
        self.port == 0
    }
}

/// The 16 bytes of an IPv6 address, in network byte order.
pub type Ipv6Bytes = [u8; 16];

/// An IPv6 address (network byte order) and a port (host byte order).
#[derive(Clone, Copy, Debug)]
pub struct Ipv6Addr {
    /// The address bytes.
    pub ip: Ipv6Bytes,
    /// The port.
    pub port: u16,
}

impl Ipv6Addr {
    /// Whether the port is 0.
    #[inline]
    pub fn is_port_unspecified(&self) -> bool {
        self.port == 0
    }
}

impl PartialEq for Ipv4Addr {
    #[inline]
    fn eq(&self, rhs: &Ipv4Addr) -> bool {
        self.ip == rhs.ip && self.port == rhs.port
    }
}
impl Eq for Ipv4Addr {}

/// The hash of a UNIX-domain address: the hash of its path up to the first null byte.
pub fn hash_sockaddr_un(a: &sockaddr_un) -> usize {
    let end = a.sun_path.iter().position(|&b| b == 0).unwrap_or(a.sun_path.len());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    a.sun_path[..end].hash(&mut h);
    h.finish() as usize
}

impl Hash for Transport {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(*self as i32 as usize);
    }
}
