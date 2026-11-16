//! Ethernet layer types of the native network stack: the 48-bit MAC address
//! ([`EthernetAddress`]), the Ethernet II header ([`EthHdr`]), the link-layer constants
//! ([`Ethernet`]) and MAC-address parsing.
//!
//! The address and header are `#[repr(C, packed)]` so they can be read from and written to
//! packet buffers directly; fields are kept in network byte order on the wire and converted
//! with [`crate::engine::net::byteorder::ntoh_struct`] / `hton_struct`.

use std::fmt;

use crate::engine::core::future::ExceptionPtr;
use crate::engine::net::byteorder::{AdjustEndianness, EndianVisitor};
use crate::engine::net::inet_address::RuntimeError;

/// A 48-bit Ethernet MAC address, laid out as on the wire.
#[repr(C, packed)]
#[derive(Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct EthernetAddress {
    /// The six address bytes, in transmission order.
    pub mac: [u8; 6],
}

impl EthernetAddress {
    /// Returns the all-zero address.
    pub fn new() -> Self {
        EthernetAddress { mac: [0; 6] }
    }

    /// Copies the address from the first six bytes of `eaddr`.
    ///
    /// # Panics
    /// If `eaddr` is shorter than six bytes.
    pub fn from_ptr(eaddr: &[u8]) -> Self {
        let mut mac = [0u8; 6];
        mac.copy_from_slice(&eaddr[..6]);
        EthernetAddress { mac }
    }

    /// Builds the address from exactly six bytes.
    ///
    /// # Panics
    /// If `eaddr` does not hold exactly six bytes.
    pub fn from_list(eaddr: &[u8]) -> Self {
        assert!(eaddr.len() == 6);
        Self::from_ptr(eaddr)
    }

    /// Reads an address from the front of `p`.
    ///
    /// # Panics
    /// If `p` is shorter than six bytes.
    pub fn read(p: &[u8]) -> EthernetAddress {
        let mut ea = EthernetAddress::new();
        let mut mac = ea.mac;
        mac.copy_from_slice(&p[..Self::size()]);
        ea.mac = mac;
        ea
    }

    /// Reads an address from the front of `p` and advances `p` past it.
    ///
    /// # Panics
    /// If `p` is shorter than six bytes.
    pub fn consume(p: &mut &[u8]) -> EthernetAddress {
        let ea = Self::read(p);
        *p = &p[Self::size()..];
        ea
    }

    /// Writes the address to the front of `p`.
    ///
    /// # Panics
    /// If `p` is shorter than six bytes.
    pub fn write(&self, p: &mut [u8]) {
        let mac = self.mac;
        p[..Self::size()].copy_from_slice(&mac);
    }

    /// Writes the address to the front of `p` and advances `p` past it.
    ///
    /// # Panics
    /// If `p` is shorter than six bytes.
    pub fn produce(&self, p: &mut &mut [u8]) {
        let buf = std::mem::take(p);
        self.write(buf);
        *p = &mut buf[Self::size()..];
    }

    /// Encoded size of an address: six bytes.
    pub const fn size() -> usize {
        6
    }
}

impl AdjustEndianness for EthernetAddress {
    fn adjust_endianness<V: EndianVisitor>(&mut self, _a: &mut V) {}
}

impl fmt::Display for EthernetAddress {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        let m = self.mac;
        write!(
            os,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            m[0] as u32, m[1] as u32, m[2] as u32, m[3] as u32, m[4] as u32, m[5] as u32
        )
    }
}

impl fmt::Debug for EthernetAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Link-layer description of Ethernet used by the generic L3 and ARP code.
pub struct Ethernet;

/// The address type of the Ethernet link layer.
pub type EthernetAddressType = EthernetAddress;

impl Ethernet {
    /// The broadcast address `ff:ff:ff:ff:ff:ff`.
    pub fn broadcast_address() -> EthernetAddress {
        EthernetAddress::from_list(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff])
    }
    /// ARP hardware type of Ethernet (1, RFC 826).
    pub const fn arp_hardware_type() -> u16 {
        1
    }
}

/// Ethernet II frame header (without VLAN tag).
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct EthHdr {
    /// Destination MAC address.
    pub dst_mac: EthernetAddress,
    /// Source MAC address.
    pub src_mac: EthernetAddress,
    /// EtherType of the payload (network byte order on the wire).
    pub eth_proto: u16,
}

impl AdjustEndianness for EthHdr {
    fn adjust_endianness<V: EndianVisitor>(&mut self, a: &mut V) {
        crate::adjust_fields!(a, self.eth_proto);
    }
}

/// Parses a hexadecimal integer prefix of `x`: leading whitespace, an optional sign and an
/// optional `0x`/`0X` prefix are accepted, and parsing stops at the first non-hex digit.
/// Fails with `InvalidArgument` when there are no digits and with a runtime error when the
/// value does not fit in an `i32`.
fn stoi16(x: &str) -> Result<i32, ExceptionPtr> {
    let t = x.trim_start();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let t = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")).unwrap_or(t);
    let end = t.bytes().position(|b| !b.is_ascii_hexdigit()).unwrap_or(t.len());
    if end == 0 {
        return Err(ExceptionPtr::from(crate::engine::core::future::InvalidArgument(
            "stoi".to_string(),
        )));
    }
    let v = i64::from_str_radix(&t[..end], 16).map_err(|_| {
        ExceptionPtr::from(crate::engine::core::future::InvalidArgument("stoi".to_string()))
    })?;
    let v = if neg { -v } else { v };
    if v < i32::MIN as i64 || v > i32::MAX as i64 {
        return Err(ExceptionPtr::from(RuntimeError("stoi: out of range".to_string())));
    }
    Ok(v as i32)
}

/// Parses a MAC address written as six colon-separated hexadecimal groups
/// (`"aa:bb:cc:dd:ee:ff"`).
///
/// # Errors
/// Returns a runtime error if the string does not have exactly six groups, an
/// `InvalidArgument` error for a group without hexadecimal digits and a runtime error for a
/// group out of `i32` range. Each group is truncated to its low byte.
pub fn parse_ethernet_address(addr: String) -> Result<EthernetAddress, ExceptionPtr> {
    let v: Vec<&str> = addr.split(':').collect();

    if v.len() != 6 {
        return Err(ExceptionPtr::from(RuntimeError("invalid mac address\n".to_string())));
    }

    let mut a = EthernetAddress::new();
    let mut mac = a.mac;
    for (i, x) in v.into_iter().enumerate() {
        mac[i] = stoi16(x)? as u8;
    }
    a.mac = mac;
    Ok(a)
}
