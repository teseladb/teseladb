//! Host/network byte-order conversion for the native network stack.
//!
//! Provides the scalar conversions (`ntohs`, `htonl`, `ntohq`, ...), the [`NetOrder`] trait
//! that gives every wire scalar an `ntoh`/`hton` pair, and the [`AdjustEndianness`] machinery
//! that converts a whole `#[repr(C, packed)]` protocol header in one call
//! ([`ntoh_struct`], [`hton_struct`]). It also has bounds-checked big-endian reads and writes
//! over byte slices for the address and ARP codecs.
//!
//! All conversions are pure, allocation-free and `#[inline]`; on a big-endian target they
//! compile to nothing.

use crate::engine::core::unaligned::Unaligned;

/// Converts a 64-bit value from network (big-endian) to host byte order.
#[inline]
pub fn ntohq(v: u64) -> u64 {
    #[cfg(target_endian = "big")]
    {
        // big endian, nothing to do
        v
    }
    #[cfg(target_endian = "little")]
    {
        // little endian, reverse bytes
        v.swap_bytes()
    }
}

/// Converts a 64-bit value from host to network (big-endian) byte order.
#[inline]
pub fn htonq(v: u64) -> u64 {
    // htonq and ntohq have identical implementations
    ntohq(v)
}

/// Converts a 16-bit value from network to host byte order.
#[inline]
pub fn ntohs(x: u16) -> u16 {
    u16::from_be(x)
}
/// Converts a 16-bit value from host to network byte order.
#[inline]
pub fn htons(x: u16) -> u16 {
    x.to_be()
}
/// Converts a 32-bit value from network to host byte order.
#[inline]
pub fn ntohl(x: u32) -> u32 {
    u32::from_be(x)
}
/// Converts a 32-bit value from host to network byte order.
#[inline]
pub fn htonl(x: u32) -> u32 {
    x.to_be()
}

/// A value with a network byte-order representation: one implementation per wire scalar
/// type (and for `()` and byte arrays, which have no byte order).
pub trait NetOrder: Sized {
    /// Converts `self` from network to host byte order.
    fn ntoh(self) -> Self;
    /// Converts `self` from host to network byte order.
    fn hton(self) -> Self;
}

/// Converts `x` from network to host byte order.
#[inline]
pub fn ntoh<T: NetOrder>(x: T) -> T {
    x.ntoh()
}

/// Converts `x` from host to network byte order.
#[inline]
pub fn hton<T: NetOrder>(x: T) -> T {
    x.hton()
}

impl NetOrder for () {
    #[inline]
    fn ntoh(self) {}
    #[inline]
    fn hton(self) {}
}

impl NetOrder for u8 {
    #[inline]
    fn ntoh(self) -> u8 {
        self
    }
    #[inline]
    fn hton(self) -> u8 {
        self
    }
}
impl NetOrder for u16 {
    #[inline]
    fn ntoh(self) -> u16 {
        ntohs(self)
    }
    #[inline]
    fn hton(self) -> u16 {
        htons(self)
    }
}
impl NetOrder for u32 {
    #[inline]
    fn ntoh(self) -> u32 {
        ntohl(self)
    }
    #[inline]
    fn hton(self) -> u32 {
        htonl(self)
    }
}
impl NetOrder for u64 {
    #[inline]
    fn ntoh(self) -> u64 {
        ntohq(self)
    }
    #[inline]
    fn hton(self) -> u64 {
        htonq(self)
    }
}

impl NetOrder for i8 {
    #[inline]
    fn ntoh(self) -> i8 {
        self
    }
    #[inline]
    fn hton(self) -> i8 {
        self
    }
}
impl NetOrder for i16 {
    #[inline]
    fn ntoh(self) -> i16 {
        ntohs(self as u16) as i16
    }
    #[inline]
    fn hton(self) -> i16 {
        htons(self as u16) as i16
    }
}
impl NetOrder for i32 {
    #[inline]
    fn ntoh(self) -> i32 {
        ntohl(self as u32) as i32
    }
    #[inline]
    fn hton(self) -> i32 {
        htonl(self as u32) as i32
    }
}
impl NetOrder for i64 {
    #[inline]
    fn ntoh(self) -> i64 {
        ntohq(self as u64) as i64
    }
    #[inline]
    fn hton(self) -> i64 {
        htonq(self as u64) as i64
    }
}

/// An unaligned value as it appears inside a packed wire header; alias of [`Unaligned`].
// TODO: get rid of this alias.
pub type Packed<T> = Unaligned<T>;

/// Reads an unaligned wire field and converts it from network to host byte order.
#[inline]
pub fn ntoh_packed<T: NetOrder + Copy>(x: &Packed<T>) -> T {
    let v: T = x.get();
    ntoh(v)
}

/// Reads an unaligned host-order field and converts it to network byte order.
#[inline]
pub fn hton_packed<T: NetOrder + Copy>(x: &Packed<T>) -> T {
    let v: T = x.get();
    hton(v)
}

impl<T: Copy + std::fmt::Display> std::fmt::Display for Packed<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let x = self.get();
        write!(f, "{}", x)
    }
}

/// A per-field conversion applied by [`AdjustEndianness::adjust_endianness`]; it is called
/// once for every field of the header.
pub trait EndianVisitor {
    /// Converts one field in place.
    fn visit<T: NetOrder + Copy>(&mut self, what: &mut T);
}

/// Visitor that converts each field from network to host byte order.
pub struct NtohInplace;
/// Visitor that converts each field from host to network byte order.
pub struct HtonInplace;

impl EndianVisitor for NtohInplace {
    #[inline]
    fn visit<T: NetOrder + Copy>(&mut self, first: &mut T) {
        *first = ntoh(*first);
    }
}

impl EndianVisitor for HtonInplace {
    #[inline]
    fn visit<T: NetOrder + Copy>(&mut self, first: &mut T) {
        *first = hton(*first);
    }
}

/// Converts each place expression given, in order, from network to host byte order
/// (`x = ntoh(x)` for every argument).
#[macro_export]
macro_rules! ntoh_inplace {
    () => {};
    ($first:expr $(, $rest:expr)* $(,)?) => {
        $first = $crate::engine::net::byteorder::ntoh($first);
        $crate::ntoh_inplace!($($rest),*);
    };
}

/// Converts each place expression given, in order, from host to network byte order
/// (`x = hton(x)` for every argument).
#[macro_export]
macro_rules! hton_inplace {
    () => {};
    ($first:expr $(, $rest:expr)* $(,)?) => {
        $first = $crate::engine::net::byteorder::hton($first);
        $crate::hton_inplace!($($rest),*);
    };
}

/// A wire header whose fields can be converted between host and network byte order.
pub trait AdjustEndianness: Clone {
    /// Applies `f` to every multi-byte field of the header, typically through
    /// [`adjust_fields!`](crate::adjust_fields).
    fn adjust_endianness<V: EndianVisitor>(&mut self, f: &mut V);
}

/// Applies the visitor `a` to each listed field, inside an `adjust_endianness` body. The header
/// structs are `#[repr(C, packed)]`, so each field is copied out, visited and written back
/// (a reference to a packed field is not allowed).
#[macro_export]
macro_rules! adjust_fields {
    ($a:expr $(, $field:expr)* $(,)?) => {
        $(
            {
                let mut tmp = $field;
                $crate::engine::net::byteorder::EndianVisitor::visit($a, &mut tmp);
                $field = tmp;
            }
        )*
    };
}

/// Byte arrays have no byte order: the adjusters leave them as they are.
impl<const N: usize> NetOrder for [u8; N] {
    #[inline]
    fn ntoh(self) -> Self {
        self
    }
    #[inline]
    fn hton(self) -> Self {
        self
    }
}

/// Returns a copy of the header `x` with every field converted to host byte order.
#[inline]
pub fn ntoh_struct<T: AdjustEndianness>(x: &T) -> T {
    let mut tmp = x.clone();
    tmp.adjust_endianness(&mut NtohInplace);
    tmp
}

/// Returns a copy of the header `x` with every field converted to network byte order.
#[inline]
pub fn hton_struct<T: AdjustEndianness>(x: &T) -> T {
    let mut tmp = x.clone();
    tmp.adjust_endianness(&mut HtonInplace);
    tmp
}

// Big-endian reads and writes on byte slices, for the address and ARP codecs. Unlike the
// raw-pointer `read_be`/`write_be` of `core::byteorder`, these check the length (one
// comparison per field, outside the per-packet data path).
/// Reads a big-endian `T` from the first `size_of::<T>()` bytes of `p`.
///
/// # Panics
/// If `p` is shorter than `T`.
#[inline]
pub fn read_be_slice<T: crate::engine::core::byteorder::ByteOrder>(p: &[u8]) -> T {
    assert!(p.len() >= std::mem::size_of::<T>(), "read_be_slice: short buffer");
    // SAFETY: `p` holds at least `size_of::<T>()` readable bytes.
    unsafe { crate::engine::core::byteorder::read_be::<T>(p.as_ptr()) }
}

/// Writes `v` as big-endian into the first `size_of::<T>()` bytes of `p`.
///
/// # Panics
/// If `p` is shorter than `T`.
#[inline]
pub fn write_be_slice<T: crate::engine::core::byteorder::ByteOrder>(p: &mut [u8], v: T) {
    assert!(p.len() >= std::mem::size_of::<T>(), "write_be_slice: short buffer");
    // SAFETY: `p` holds at least `size_of::<T>()` writable bytes.
    unsafe { crate::engine::core::byteorder::write_be::<T>(p.as_mut_ptr(), v) }
}

/// Reads a big-endian `T` from the front of `p` and advances `p` past it.
///
/// # Panics
/// If `p` is shorter than `T`.
#[inline]
pub fn consume_be_slice<T: crate::engine::core::byteorder::ByteOrder>(p: &mut &[u8]) -> T {
    let v = read_be_slice::<T>(p);
    *p = &p[std::mem::size_of::<T>()..];
    v
}

/// Writes `v` as big-endian at the front of `p` and advances `p` past it.
///
/// # Panics
/// If `p` is shorter than `T`.
#[inline]
pub fn produce_be_slice<T: crate::engine::core::byteorder::ByteOrder>(p: &mut &mut [u8], v: T) {
    let buf = std::mem::take(p);
    write_be_slice::<T>(buf, v);
    *p = &mut buf[std::mem::size_of::<T>()..];
}
