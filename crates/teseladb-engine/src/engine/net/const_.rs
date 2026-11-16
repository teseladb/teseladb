//! Protocol numbers and fixed header sizes shared by the native network stack layers.

/// IP protocol number (the IPv4 `protocol` / IPv6 `next header` field).
///
/// Only the protocols the stack handles have a name; every other value maps to
/// [`IpProtocolNum::unused`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
// Lower-case variant names match the protocol names used across the stack.
#[allow(non_camel_case_types)]
pub enum IpProtocolNum {
    /// ICMP (1).
    icmp = 1,
    /// TCP (6).
    tcp = 6,
    /// UDP (17).
    udp = 17,
    /// Any protocol the stack does not handle (255 is reserved by IANA).
    #[default]
    unused = 255,
}

impl IpProtocolNum {
    /// Maps a wire protocol number to its variant; numbers without a name map to
    /// [`IpProtocolNum::unused`].
    pub fn from_u8(v: u8) -> IpProtocolNum {
        match v {
            1 => IpProtocolNum::icmp,
            6 => IpProtocolNum::tcp,
            17 => IpProtocolNum::udp,
            _ => IpProtocolNum::unused,
        }
    }
}

/// EtherType of an Ethernet frame payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
// Lower-case variant names match the protocol names used across the stack.
#[allow(non_camel_case_types)]
pub enum EthProtocolNum {
    /// IPv4 (0x0800).
    ipv4 = 0x0800,
    /// ARP (0x0806).
    arp = 0x0806,
    /// IPv6 (0x86dd).
    ipv6 = 0x86dd,
}

/// Length of an Ethernet header without VLAN tag, in bytes.
pub const ETH_HDR_LEN: u8 = 14;
/// Minimum TCP header length (no options), in bytes.
pub const TCP_HDR_LEN_MIN: u8 = 20;
/// Minimum IPv4 header length (no options), in bytes.
pub const IPV4_HDR_LEN_MIN: u8 = 20;
/// IPv6 fixed header length, in bytes.
pub const IPV6_HDR_LEN_MIN: u8 = 40;
/// Largest IP packet length the 16-bit total-length field allows, in bytes.
pub const IP_PACKET_LEN_MAX: u16 = 65535;
