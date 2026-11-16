//! The IPv4 address type of the native stack's wire headers.
//!
//! [`Ipv4Address`] is a packed 32-bit address held in host byte order; [`Ipv4Address::read`],
//! [`Ipv4Address::write`] and their cursor forms convert to and from the network byte order
//! of a packet. Parsing from text and `Display` live in
//! [`inet_address`](crate::engine::net::inet_address).

use std::fmt;
use std::hash::{Hash, Hasher};

use crate::engine::net::byteorder::{AdjustEndianness, EndianVisitor};
use crate::engine::net::byteorder::{read_be_slice, write_be_slice};
use crate::engine::net::socket_defs::Ipv4Addr;

/// An IPv4 address as it appears in a header, packed so that it can be a field of a
/// `#[repr(C, packed)]` wire structure.
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct Ipv4Address {
    /// The address in host byte order.
    pub ip: u32,
}

impl Ipv4Address {
    /// The unspecified address `0.0.0.0`.
    #[inline]
    pub fn new() -> Self {
        Ipv4Address { ip: 0 }
    }
    /// The address whose host-order value is `ip`.
    #[inline]
    pub fn from_u32(ip: u32) -> Self {
        Ipv4Address { ip }
    }
    /// The address held by a socket-level [`Ipv4Addr`].
    #[inline]
    pub fn from_ipv4_addr(addr: Ipv4Addr) -> Self {
        Ipv4Address { ip: addr.ip }
    }

    /// Reads a big-endian address from the first 4 bytes of `p`.
    ///
    /// # Panics
    ///
    /// If `p` is shorter than 4 bytes.
    #[inline]
    pub fn read(p: &[u8]) -> Ipv4Address {
        let mut ia = Ipv4Address::new();
        ia.ip = read_be_slice::<u32>(p);
        ia
    }
    /// Reads a big-endian address from `p` and advances `p` past it.
    ///
    /// # Panics
    ///
    /// If `p` is shorter than 4 bytes.
    #[inline]
    pub fn consume(p: &mut &[u8]) -> Ipv4Address {
        let ia = Self::read(p);
        *p = &p[4..];
        ia
    }
    /// Writes the address big-endian into the first 4 bytes of `p`.
    ///
    /// # Panics
    ///
    /// If `p` is shorter than 4 bytes.
    #[inline]
    pub fn write(&self, p: &mut [u8]) {
        write_be_slice::<u32>(p, self.ip);
    }
    /// Writes the address big-endian into `p` and advances `p` past it.
    ///
    /// # Panics
    ///
    /// If `p` is shorter than 4 bytes.
    #[inline]
    pub fn produce(&self, p: &mut &mut [u8]) {
        let buf = std::mem::take(p);
        write_be_slice::<u32>(buf, self.ip);
        *p = &mut buf[4..];
    }
    /// The size of the address on the wire, in bytes.
    #[inline]
    pub const fn size() -> usize {
        4
    }
}

impl AdjustEndianness for Ipv4Address {
    fn adjust_endianness<V: EndianVisitor>(&mut self, a: &mut V) {
        crate::adjust_fields!(a, self.ip);
    }
}

impl PartialEq for Ipv4Address {
    #[inline]
    fn eq(&self, y: &Ipv4Address) -> bool {
        // Copy the packed fields out: references to them would be unaligned.
        let (a, b) = (self.ip, y.ip);
        a == b
    }
}
impl Eq for Ipv4Address {}

impl fmt::Debug for Ipv4Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Whether `addr` is the unspecified address `0.0.0.0`.
#[inline]
pub fn is_unspecified(addr: Ipv4Address) -> bool {
    let ip = addr.ip;
    ip == 0
}

/// Hashes the host-order integer, so that [`IdentityHash`](crate::engine::net::hash::IdentityHash)
/// of an address is the address itself.
impl Hash for Ipv4Address {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let ip = self.ip;
        state.write_usize(ip as usize);
    }
}
