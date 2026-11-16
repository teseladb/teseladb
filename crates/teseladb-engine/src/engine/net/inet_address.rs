//! Family-independent IP addresses.
//!
//! [`InetAddress`] holds an IPv4 or an IPv6 address (with an optional IPv6 scope id) and is
//! the address type of the public networking API: DNS results, [`SocketAddress::addr`],
//! listener and connect targets. This module also implements, for the header types of
//! [`ipv4_address`](crate::engine::net::ipv4_address) and
//! [`ipv6_address`](crate::engine::net::ipv6_address) and for the
//! [`Ipv4Addr`]/[`Ipv6Addr`] pairs of [`socket_defs`](crate::engine::net::socket_defs),
//! their text parsing (through `inet_pton`), `Display` (through `inet_ntop`) and `Hash`.
//!
//! Host-name resolution is in `net::dns` (feature `dns`); this module only parses
//! numerical addresses. Parsing a `%scope` suffix asks the current shard's network stack for
//! its interfaces, so it must run on a reactor thread.

use std::ffi::{CStr, CString, c_char, c_void};
use std::fmt;
use std::hash::{Hash, Hasher};

use crate::engine::core::future::{ExceptionPtr, InvalidArgument};
use crate::engine::core::reactor::engine;
use crate::engine::core::sstring::Sstring;
use crate::engine::net::byteorder::{hton, htonl, ntoh};
use crate::engine::net::ipv4_address::Ipv4Address;
use crate::engine::net::ipv6_address::{Ipv6Address, Ipv6Bytes};
use crate::engine::net::socket_defs::{
    AF_INET, AF_INET6, AF_UNIX, IN6ADDR_ANY, IN6ADDR_LOOPBACK, INADDR_ANY, Ipv4Addr, Ipv6Addr,
    SocketAddress, hash_combine, in_addr, in6_addr, in6_is_addr_v4mapped, inet_ntop, inet_pton,
    sa_family_t, sockaddr_in6, socklen_t,
};

/// The runtime error of the future library, re-exported for the address and DNS code.
pub use crate::engine::core::future::RuntimeError;

/// A host name that could not be resolved, or an address of a family the caller did not
/// accept; carries the message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownHost(pub String);
impl fmt::Display for UnknownHost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for UnknownHost {}

/// The family of an [`InetAddress`]; the discriminants are the `AF_*` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum Family {
    /// IPv4 (`AF_INET`).
    INET = AF_INET,
    /// IPv6 (`AF_INET6`).
    INET6 = AF_INET6,
}

/// The storage of an [`InetAddress`]: an IPv4 address in the first bytes, or an IPv6 one.
#[repr(C)]
#[derive(Clone, Copy)]
union InetAddressU {
    _in: in_addr,
    _in6: in6_addr,
}

/// An IPv4 or IPv6 address, in network byte order, with an IPv6 scope id.
///
/// Equality compares the family and the address bytes, not the scope. An IPv4 address
/// converts to its IPv4-mapped IPv6 form with [`InetAddress::to_in6_addr`], and an
/// IPv4-mapped IPv6 address converts back with [`InetAddress::to_in_addr`].
#[derive(Clone, Copy)]
pub struct InetAddress {
    _in_family: Family,
    u: InetAddressU,
    _scope: u32,
}

impl InetAddress {
    /// The scope id of an address that has none.
    pub const INVALID_SCOPE: u32 = u32::MAX;

    fn in_(&self) -> in_addr {
        // SAFETY: both union members are plain old data of which `_in` is a prefix.
        unsafe { self.u._in }
    }
    fn in6(&self) -> in6_addr {
        // SAFETY: both union members are plain old data; `_in6` covers the whole union.
        unsafe { self.u._in6 }
    }

    /// The family of the address.
    #[inline]
    pub fn in_family(&self) -> Family {
        self._in_family
    }

    /// Whether the address is IPv6.
    #[inline]
    pub fn is_ipv6(&self) -> bool {
        self._in_family == Family::INET6
    }
    /// Whether the address is IPv4.
    #[inline]
    pub fn is_ipv4(&self) -> bool {
        self._in_family == Family::INET
    }

    /// The IPv6 scope id, or [`InetAddress::INVALID_SCOPE`].
    #[inline]
    pub fn scope(&self) -> u32 {
        self._scope
    }
}

/// The IPv6 wildcard address `::` with no scope.
impl Default for InetAddress {
    fn default() -> Self {
        InetAddress::from_in6_addr(in6_addr::default(), Self::INVALID_SCOPE)
    }
}

impl InetAddress {
    /// The same as [`InetAddress::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// The all-zero (wildcard) address of family `f`, with no scope.
    pub fn from_family(f: Family) -> Self {
        InetAddress {
            _in_family: f,
            u: InetAddressU { _in6: in6_addr::default() },
            _scope: Self::INVALID_SCOPE,
        }
    }

    /// An IPv4 address.
    pub fn from_in_addr(i: in_addr) -> Self {
        let mut u = InetAddressU { _in6: in6_addr::default() };
        u._in = i;
        InetAddress { _in_family: Family::INET, u, _scope: Self::INVALID_SCOPE }
    }

    /// An IPv6 address with the scope id `scope`.
    pub fn from_in6_addr(i: in6_addr, scope: u32) -> Self {
        InetAddress { _in_family: Family::INET6, u: InetAddressU { _in6: i }, _scope: scope }
    }

    /// Parses a numerical IPv4 or IPv6 address; an IPv6 address may carry a `%scope` suffix,
    /// which is matched against the index, name or display name of the shard's network
    /// interfaces (an unmatched scope leaves the address without one).
    ///
    /// Returns `Ok(None)` if `addr` is not a numerical address. Must run on a reactor thread
    /// when `addr` has a `%` suffix.
    ///
    /// # Errors
    ///
    /// The error of listing the network interfaces, when a `%scope` suffix needs them.
    pub fn parse_numerical(addr: &Sstring) -> Result<Option<InetAddress>, ExceptionPtr> {
        let mut in_ = InetAddress::new();
        let c = match CString::new(addr.as_bytes()) {
            Ok(c) => c,
            Err(_) => return Ok(None),
        };
        // SAFETY: `c` is a valid C string and `in_.u` has room for an in_addr.
        if unsafe {
            inet_pton(AF_INET as i32, c.as_ptr(), &mut in_.u._in as *mut in_addr as *mut c_void)
        } != 0
        {
            in_._in_family = Family::INET;
            return Ok(Some(in_));
        }
        let s = addr.as_str();
        if let Some(i) = s.rfind('%') {
            let ext = &s[i + 1..];
            let src = &s[..i];
            let res = Self::parse_numerical(&Sstring::from(src))?;

            if let Some(mut res) = res {
                let mut index: u32 = u32::MAX;
                if let Ok(v) = parse_stoul(ext) {
                    index = v as u32;
                }
                // SAFETY: engine() is this shard's running reactor; the reference lives for this expression only.
                for nwif in unsafe { (*engine()).net() }.network_interfaces()? {
                    if nwif.index() == index
                        || nwif.name().as_str() == ext
                        || nwif.display_name().as_str() == ext
                    {
                        res._scope = nwif.index();
                        break;
                    }
                }
                return Ok(Some(res));
            }
        }
        // SAFETY: `c` is a valid C string and `in_.u` has room for an in6_addr.
        if unsafe {
            inet_pton(AF_INET6 as i32, c.as_ptr(), &mut in_.u._in6 as *mut in6_addr as *mut c_void)
        } != 0
        {
            in_._in_family = Family::INET6;
            return Ok(Some(in_));
        }
        Ok(None)
    }

    /// Parses a numerical IPv4 or IPv6 address, as [`InetAddress::parse_numerical`] does. It
    /// does not resolve host names.
    ///
    /// # Errors
    ///
    /// An `InvalidArgument` carrying `addr` if it is not a numerical address, or the error of
    /// [`InetAddress::parse_numerical`].
    pub fn from_sstring(addr: &Sstring) -> Result<Self, ExceptionPtr> {
        let res = Self::parse_numerical(addr)?;
        if let Some(res) = res {
            return Ok(res);
        }
        Err(ExceptionPtr::from(InvalidArgument(addr.to_string())))
    }

    /// The IPv4 address of a wire header (host order converted to network order).
    pub fn from_ipv4_address(in_: &Ipv4Address) -> Self {
        let ip = in_.ip;
        Self::from_in_addr(in_addr { s_addr: hton(ip) })
    }

    /// The IPv6 address of a wire header, with the scope id `scope`.
    pub fn from_ipv6_address(in_: &Ipv6Address, scope: u32) -> Self {
        let mut tmp = in6_addr::default();
        tmp.s6_addr.copy_from_slice(&in_.bytes());
        Self::from_in6_addr(tmp, scope)
    }

    /// The address as a wire-header IPv4 address (host order).
    ///
    /// # Errors
    ///
    /// As [`InetAddress::to_in_addr`]: an IPv6 address that is not IPv4-mapped.
    pub fn as_ipv4_address(&self) -> Result<Ipv4Address, ExceptionPtr> {
        let in_: in_addr = self.to_in_addr()?;
        Ok(Ipv4Address::from_u32(ntoh(in_.s_addr)))
    }

    /// The address as a wire-header IPv6 address (IPv4 addresses are mapped).
    pub fn as_ipv6_address(&self) -> Ipv6Address {
        let in6: in6_addr = self.to_in6_addr();
        ipv6_address_from_in6_addr(&in6)
    }

    /// The address as an `in_addr`; an IPv4-mapped IPv6 address gives its IPv4 part.
    ///
    /// # Errors
    ///
    /// An `InvalidArgument` ("Not an IPv4 address") for any other IPv6 address.
    pub fn to_in_addr(&self) -> Result<in_addr, ExceptionPtr> {
        if self._in_family != Family::INET {
            let in6 = self.in6();
            if in6_is_addr_v4mapped(&in6) {
                let in_ = in_addr { s_addr: in6.s6_addr32(3) };
                return Ok(in_);
            }
            return Err(ExceptionPtr::from(InvalidArgument("Not an IPv4 address".to_string())));
        }
        Ok(self.in_())
    }

    /// The address as an `in6_addr`; an IPv4 address gives its IPv4-mapped form.
    pub fn to_in6_addr(&self) -> in6_addr {
        if self._in_family == Family::INET {
            let mut in6 = IN6ADDR_ANY;
            in6.set_s6_addr32(2, htonl(0xffff));
            in6.set_s6_addr32(3, self.in_().s_addr);
            return in6;
        }
        self.in6()
    }

    /// The same as [`InetAddress::as_ipv6_address`].
    pub fn to_ipv6_address(&self) -> Ipv6Address {
        self.as_ipv6_address()
    }

    /// The size of the address bytes: 4 for IPv4, 16 for IPv6.
    pub fn size(&self) -> usize {
        match self._in_family {
            Family::INET => std::mem::size_of::<in_addr>(),
            Family::INET6 => std::mem::size_of::<in6_addr>(),
        }
    }

    /// A pointer to the address bytes (network order), valid while `self` is; `size()` of
    /// them are meaningful.
    pub fn data(&self) -> *const c_void {
        &self.u as *const InetAddressU as *const c_void
    }

    /// The bytes `data()` points to, `size()` of them.
    pub fn data_bytes(&self) -> &[u8] {
        // SAFETY: `u` is at least `size()` bytes of plain old data.
        unsafe { std::slice::from_raw_parts(self.data() as *const u8, self.size()) }
    }

    /// Whether the address is a loopback address (`127.0.0.0/8` or `::1`).
    pub fn is_loopback(&self) -> bool {
        match self._in_family {
            Family::INET => (ntoh(self.in_().s_addr) & 0xff000000) == 0x7f000000,
            Family::INET6 => self.in6().s6_addr == IN6ADDR_LOOPBACK.s6_addr,
        }
    }

    /// Whether the address is the wildcard (`0.0.0.0` or `::`).
    pub fn is_addr_any(&self) -> bool {
        match self._in_family {
            Family::INET => self.in_().s_addr == INADDR_ANY,
            Family::INET6 => self.in6().s6_addr == IN6ADDR_ANY.s6_addr,
        }
    }
}

/// Parses a leading unsigned decimal: leading whitespace, an optional `+`, then digits up to
/// the first non-digit; fails when there is no digit or the value overflows `u64`.
fn parse_stoul(s: &str) -> Result<u64, ()> {
    let t = s.trim_start();
    let t = t.strip_prefix('+').unwrap_or(t);
    let end = t.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(t.len());
    if end == 0 {
        return Err(());
    }
    t[..end].parse::<u64>().map_err(|_| ())
}

impl PartialEq for InetAddress {
    fn eq(&self, o: &InetAddress) -> bool {
        if o._in_family != self._in_family {
            return false;
        }
        match self._in_family {
            Family::INET => self.in_().s_addr == o.in_().s_addr,
            Family::INET6 => self.in6().s6_addr == o.in6().s6_addr,
        }
    }
}
impl Eq for InetAddress {}

/// The wire-header IPv6 address of `in_`.
pub fn ipv6_address_from_in6_addr(in_: &in6_addr) -> Ipv6Address {
    Ipv6Address { ip: in_.s6_addr }
}

impl Ipv6Address {
    /// The address of `in_`.
    pub fn from_in6_addr(in_: &in6_addr) -> Self {
        ipv6_address_from_in6_addr(in_)
    }

    /// The address with the bytes `in_` (network order).
    pub fn from_bytes(in_: &Ipv6Bytes) -> Self {
        Ipv6Address { ip: *in_ }
    }

    /// The address part of `addr`.
    pub fn from_ipv6_addr(addr: &Ipv6Addr) -> Self {
        Self::from_bytes(&addr.ip)
    }

    /// The unspecified address `::`.
    pub fn new() -> Self {
        Self::from_in6_addr(&IN6ADDR_ANY)
    }

    /// Parses an IPv6 address in colon-hex notation.
    ///
    /// # Errors
    ///
    /// A `RuntimeError` if `addr` is not a valid IPv6 address (or contains a null byte).
    pub fn from_string(addr: &str) -> Result<Self, ExceptionPtr> {
        let mut ip: Ipv6Bytes = [0; 16];
        let c = CString::new(addr).map_err(|_| {
            ExceptionPtr::from(RuntimeError(format!(
                "Wrong format for IPv6 address {}. Please ensure it's in colon-hex format",
                addr
            )))
        })?;
        // SAFETY: `c` is a valid C string and `ip` has room for 16 bytes.
        if unsafe { inet_pton(AF_INET6 as i32, c.as_ptr(), ip.as_mut_ptr() as *mut c_void) } == 0 {
            return Err(ExceptionPtr::from(RuntimeError(format!(
                "Wrong format for IPv6 address {}. Please ensure it's in colon-hex format",
                addr
            ))));
        }
        Ok(Ipv6Address { ip })
    }

    /// Reads an address from the first 16 bytes of `s`.
    ///
    /// # Panics
    ///
    /// If `s` is shorter than 16 bytes.
    pub fn read(s: &[u8]) -> Ipv6Address {
        let mut in_ = Ipv6Address::new();
        let mut ip = in_.ip;
        ip.copy_from_slice(&s[..Ipv6Address::size()]);
        in_.ip = ip;
        in_
    }

    /// Reads an address from `p` and advances `p` past it.
    ///
    /// # Panics
    ///
    /// If `p` is shorter than 16 bytes.
    pub fn consume(p: &mut &[u8]) -> Ipv6Address {
        let res = Self::read(p);
        *p = &p[Self::size()..];
        res
    }

    /// Writes the address into the first 16 bytes of `p`.
    ///
    /// # Panics
    ///
    /// If `p` is shorter than 16 bytes.
    pub fn write(&self, p: &mut [u8]) {
        let ip = self.ip;
        p[..Self::size()].copy_from_slice(&ip);
    }

    /// Writes the address into `p` and advances `p` past it.
    ///
    /// # Panics
    ///
    /// If `p` is shorter than 16 bytes.
    pub fn produce(&self, p: &mut &mut [u8]) {
        let buf = std::mem::take(p);
        self.write(buf);
        *p = &mut buf[Self::size()..];
    }

    /// Whether the address is `::`.
    pub fn is_unspecified(&self) -> bool {
        let ip = self.ip;
        ip.iter().all(|&b| b == 0)
    }
}

impl Default for Ipv6Address {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for Ipv4Address {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ip = self.ip;
        write!(os, "{}.{}.{}.{}", (ip >> 24) & 0xff, (ip >> 16) & 0xff, (ip >> 8) & 0xff, ip & 0xff)
    }
}

/// The text of the address of family `af` at `src` (`inet_ntop`), or an empty string if the
/// family is not supported.
fn ntop(af: i32, src: *const c_void) -> String {
    let mut buffer = [0 as c_char; 64];
    // SAFETY: `src` points to an address of family `af` and `buffer` has 64 bytes.
    let r = unsafe { inet_ntop(af, src, buffer.as_mut_ptr(), buffer.len() as socklen_t) };
    if r.is_null() {
        return String::new();
    }
    // SAFETY: inet_ntop wrote a null-terminated string into `buffer`.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned()
}

impl fmt::Display for Ipv6Address {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        let ip = self.ip;
        os.write_str(&ntop(AF_INET6 as i32, ip.as_ptr() as *const c_void))
    }
}

impl fmt::Debug for Ipv6Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl Ipv6Addr {
    /// The address with bytes `b` and port `p`.
    pub fn new(b: &Ipv6Bytes, p: u16) -> Self {
        Ipv6Addr { ip: *b, port: p }
    }

    /// The wildcard address `::` with port `p`.
    pub fn from_port(p: u16) -> Self {
        Self::from_inet_address(&InetAddress::new(), p)
    }

    /// The address `in6` with port `p`.
    pub fn from_in6_addr(in6: &in6_addr, p: u16) -> Self {
        Self::new(&Ipv6Address::from_in6_addr(in6).bytes(), p)
    }

    /// Parses `[addr]` or `[addr]:port` (port 0 when absent). Without brackets, the text
    /// after the first `:` must parse as a port, so an unbracketed IPv6 address is rejected.
    ///
    /// # Errors
    ///
    /// An `InvalidArgument` if the port is not a number, or the error of
    /// [`Ipv6Address::from_string`] if the address is invalid.
    pub fn from_string(s: &str) -> Result<Self, ExceptionPtr> {
        let lc = s.rfind(']');
        let cp = match lc {
            Some(lc) => s[lc..].find(':').map(|i| i + lc),
            None => s.find(':'),
        };
        let port = match cp {
            Some(cp) => parse_stoul(&s[cp + 1..])
                .map_err(|_| ExceptionPtr::from(InvalidArgument("stoul".to_string())))?,
            None => 0,
        };
        let ss = match lc {
            Some(lc) => &s[1..lc],
            None => s,
        };
        Ok(Self::new(&Ipv6Address::from_string(ss)?.bytes(), port as u16))
    }

    /// Parses the IPv6 address `s` (no brackets, no port) and pairs it with port `p`.
    ///
    /// # Errors
    ///
    /// The error of [`Ipv6Address::from_string`].
    pub fn from_string_port(s: &str, p: u16) -> Result<Self, ExceptionPtr> {
        Ok(Self::new(&Ipv6Address::from_string(s)?.bytes(), p))
    }

    /// The address `i` (IPv4 addresses are mapped) with port `p`.
    pub fn from_inet_address(i: &InetAddress, p: u16) -> Self {
        Self::new(&i.as_ipv6_address().bytes(), p)
    }

    /// The address and port of `s`.
    pub fn from_sockaddr_in6(s: &sockaddr_in6) -> Self {
        Self::from_in6_addr(&s.sin6_addr, ntoh(s.sin6_port))
    }

    /// The address and port of `s`, read as an `AF_INET6` address.
    pub fn from_socket_address(s: &SocketAddress) -> Self {
        Self::from_sockaddr_in6(s.as_posix_sockaddr_in6())
    }

    /// Whether the address is `::`.
    pub fn is_ip_unspecified(&self) -> bool {
        self.ip.iter().all(|&b| b == 0)
    }
}

impl SocketAddress {
    /// The IP address of an `AF_INET` or `AF_INET6` address (with its scope id); `::` for
    /// any other family.
    pub fn addr(&self) -> InetAddress {
        match self.family() {
            AF_INET => InetAddress::from_in_addr(self.as_posix_sockaddr_in().sin_addr),
            AF_INET6 => InetAddress::from_in6_addr(
                self.as_posix_sockaddr_in6().sin6_addr,
                self.as_posix_sockaddr_in6().sin6_scope_id,
            ),
            _ => InetAddress::new(),
        }
    }

    /// The port in host byte order (meaningful for `AF_INET` and `AF_INET6`).
    pub fn port(&self) -> u16 {
        ntoh(self.as_posix_sockaddr_in().sin_port)
    }

    /// Whether the address is a wildcard: address and port both unspecified, or an unnamed
    /// UNIX-domain address.
    pub fn is_wildcard(&self) -> bool {
        match self.family() {
            AF_INET => {
                let addr = Ipv4Addr::from_socket_address(self);
                addr.is_ip_unspecified() && addr.is_port_unspecified()
            }
            AF_UNIX => self.length() as usize <= std::mem::size_of::<sa_family_t>(),
            // AF_INET6, and any other family
            _ => {
                let addr = Ipv6Addr::from_socket_address(self);
                addr.is_ip_unspecified() && addr.is_port_unspecified()
            }
        }
    }
}

impl fmt::Display for InetAddress {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        os.write_str(&ntop(self.in_family() as i32, self.data()))?;
        if self.scope() != InetAddress::INVALID_SCOPE {
            write!(os, "%{}", self.scope())?;
        }
        Ok(())
    }
}

impl fmt::Debug for InetAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl fmt::Display for Family {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Family::INET => os.write_str("INET"),
            Family::INET6 => os.write_str("INET6"),
        }
    }
}

impl fmt::Display for Ipv4Addr {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(os, "{}", SocketAddress::from_ipv4_addr(*self))
    }
}

impl fmt::Display for Ipv6Addr {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(os, "{}", SocketAddress::from_ipv6_addr(self))
    }
}

/// The hash of an IP address: the host-order integer for IPv4, [`hash_ipv6_address`] for
/// IPv6. The scope is not hashed.
pub fn hash_inet_address(a: &InetAddress) -> usize {
    match a.in_family() {
        Family::INET => {
            let ip = a.as_ipv4_address().map(|x| x.ip).unwrap_or(0);
            ip as usize
        }
        Family::INET6 => hash_ipv6_address(&a.as_ipv6_address()),
    }
}

impl Hash for InetAddress {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(hash_inet_address(self));
    }
}

/// The hash of an IPv6 address: each byte folded in with
/// [`hash_combine`], starting from 0.
pub fn hash_ipv6_address(a: &Ipv6Address) -> usize {
    let mut seed = 0usize;
    let ip = a.ip;
    for b in ip.iter() {
        hash_combine(&mut seed, *b as usize);
    }
    seed
}

impl Hash for Ipv6Address {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(hash_ipv6_address(self));
    }
}

/// The hash of an IPv4 address and port: the address combined with the port.
pub fn hash_ipv4_addr(x: &Ipv4Addr) -> usize {
    let mut h = x.ip as usize;
    hash_combine(&mut h, x.port as usize);
    h
}

impl Hash for Ipv4Addr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(hash_ipv4_addr(self));
    }
}
