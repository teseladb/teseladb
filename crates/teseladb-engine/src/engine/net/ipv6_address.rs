//! The IPv6 address type of the native stack's wire headers.
//!
//! [`Ipv6Address`] stores the 16 address bytes in network order. Its constructors, parsing,
//! `is_unspecified`, the `read`/`consume`/`write`/`produce` accessors, `Display` and `Hash` are
//! implemented in [`inet_address`](crate::engine::net::inet_address), next to the
//! address-family-independent type that uses them.

use crate::engine::net::byteorder::{AdjustEndianness, EndianVisitor};

/// The 16 bytes of an IPv6 address, in network byte order.
pub type Ipv6Bytes = [u8; 16];

const _: () = assert!(std::mem::align_of::<Ipv6Bytes>() == 1, "ipv6_bytes should be byte-aligned");
const _: () = assert!(std::mem::size_of::<Ipv6Bytes>() == 16, "ipv6_bytes should be 16 bytes");

/// An IPv6 address as it appears in a header (16 bytes, byte-aligned).
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Ipv6Address {
    // Stored as a byte array, so `packed` changes nothing; reading it as integers needs a copy.
    /// The address bytes in network byte order.
    pub ip: Ipv6Bytes,
}

impl AdjustEndianness for Ipv6Address {
    fn adjust_endianness<V: EndianVisitor>(&mut self, a: &mut V) {
        crate::adjust_fields!(a, self.ip);
    }
}

impl Ipv6Address {
    /// A copy of the address bytes.
    #[inline]
    pub fn bytes(&self) -> Ipv6Bytes {
        self.ip
    }

    /// The size of the address on the wire, in bytes (16).
    #[inline]
    pub const fn size() -> usize {
        std::mem::size_of::<Ipv6Bytes>()
    }
}

impl PartialEq for Ipv6Address {
    #[inline]
    fn eq(&self, y: &Ipv6Address) -> bool {
        self.bytes() == y.bytes()
    }
}
impl Eq for Ipv6Address {}
