//! Constructors, comparison, hashing and formatting of
//! [`SocketAddress`] that are independent of
//! the address family: IPv4, IPv6 and UNIX-domain addresses all go through here.
//!
//! The type itself (a `sockaddr` union plus its length) is declared in
//! [`socket_defs`](crate::engine::net::socket_defs). Equality compares the family, port and
//! address (or the UNIX path, tolerating a counted or uncounted trailing null); `Display`
//! prints `ip:port`, `[ipv6]:port`, a filesystem path, `@abstract-name` or `{unnamed}`.

use std::fmt;
use std::hash::{Hash, Hasher};

use crate::engine::net::byteorder::{htonl, htons, ntohs};
use crate::engine::net::inet_address::{Family, InetAddress, hash_inet_address};
use crate::engine::net::socket_defs::{
    AF_INET, AF_INET6, AF_UNIX, AF_UNSPEC, Ipv4Addr, Ipv6Addr, SOCKADDR_UN_SUN_PATH_OFFSET,
    SocketAddress, hash_combine, in6_are_addr_equal, sockaddr_in, sockaddr_in6, sockaddr_storage,
    sockaddr_un, socklen_t,
};
use crate::engine::net::unix_address::UnixDomainAddr;

/// The hash of a socket address: the hash of its IP address combined with its port.
pub fn hash_socket_address(a: &SocketAddress) -> usize {
    let mut h = hash_inet_address(&a.addr());
    hash_combine(&mut h, a.as_posix_sockaddr_in().sin_port as usize);
    h
}

impl Hash for SocketAddress {
    fn hash<H: Hasher>(&self, state: &mut H) {
        state.write_usize(hash_socket_address(self));
    }
}

/// An unspecified address whose length is the full `sockaddr_storage`, ready to be filled by
/// `accept` or `getsockname`.
impl Default for SocketAddress {
    fn default() -> Self {
        // set max addr_length, as we (probably) want to use the constructed object
        // in accept() or get_address()
        const _: () = assert!(AF_UNSPEC == 0, "just checking");
        let mut s = SocketAddress::zeroed();
        s.addr_length = std::mem::size_of::<sockaddr_storage>() as socklen_t;
        s
    }
}

impl SocketAddress {
    /// The same as [`SocketAddress::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// The IPv4 wildcard address `0.0.0.0` with port `p`.
    pub fn from_port(p: u16) -> Self {
        Self::from_ipv4_addr(Ipv4Addr::from_port(p))
    }

    /// An `AF_INET` address from a host-order [`Ipv4Addr`].
    pub fn from_ipv4_addr(addr: Ipv4Addr) -> Self {
        let mut s = SocketAddress::zeroed();
        s.addr_length = std::mem::size_of::<sockaddr_in>() as socklen_t;
        let in_ = s.as_posix_sockaddr_in_mut();
        in_.sin_family = AF_INET;
        in_.sin_port = htons(addr.port);
        in_.sin_addr.s_addr = htonl(addr.ip);
        s
    }

    /// An `AF_INET6` address from `addr` with the scope (interface) id `scope`.
    pub fn from_ipv6_addr_scope(addr: &Ipv6Addr, scope: u32) -> Self {
        let mut s = SocketAddress::zeroed();
        s.addr_length = std::mem::size_of::<sockaddr_in6>() as socklen_t;
        let in6 = s.as_posix_sockaddr_in6_mut();
        in6.sin6_family = AF_INET6;
        in6.sin6_port = htons(addr.port);
        in6.sin6_flowinfo = 0;
        in6.sin6_scope_id = scope;
        in6.sin6_addr.s6_addr.copy_from_slice(&addr.ip);
        s
    }

    /// An `AF_INET6` address from `addr` with no scope id.
    pub fn from_ipv6_addr(addr: &Ipv6Addr) -> Self {
        Self::from_ipv6_addr_scope(addr, InetAddress::INVALID_SCOPE)
    }

    /// An `AF_INET` address from a host-order IPv4 integer and a port.
    pub fn from_ipv4_port(ipv4: u32, p: u16) -> Self {
        crate::engine::net::api::make_ipv4_address_ip_port(ipv4, p)
    }

    /// The address `a` with port `p`, `AF_INET6` (keeping the scope of `a`) or `AF_INET`
    /// according to the family of `a`.
    pub fn from_inet_address(a: &InetAddress, p: u16) -> Self {
        if a.is_ipv6() {
            Self::from_ipv6_addr_scope(&Ipv6Addr::from_inet_address(a, p), a.scope())
        } else {
            Self::from_ipv4_addr(
                Ipv4Addr::from_inet_address(a, p).expect("ipv4_addr of an ipv4 inet_address"),
            )
        }
    }

    /// An `AF_UNIX` address; a name longer than `sun_path` is truncated.
    pub fn from_unix_domain_addr(s: &UnixDomainAddr) -> Self {
        let mut r = SocketAddress::zeroed();
        // SAFETY: the union is plain old data; `un` is written as a whole member.
        let un: &mut sockaddr_un = unsafe { &mut r.u.un };
        un.sun_family = AF_UNIX;
        un.sun_path.fill(0);
        let path_length = std::cmp::min(un.sun_path.len() as i32, s.path_length()) as usize;
        un.sun_path[..path_length].copy_from_slice(&s.path_bytes()[..path_length]);
        r.addr_length = (path_length + SOCKADDR_UN_SUN_PATH_OFFSET) as socklen_t;
        r
    }

    /// Whether the family is `AF_UNSPEC`.
    pub fn is_unspecified(&self) -> bool {
        self.as_posix_sockaddr().sa_family == AF_UNSPEC
    }
}

fn adjusted_path_length(a: &SocketAddress) -> i32 {
    let mut l = std::cmp::max(0, a.addr_length as i32 - SOCKADDR_UN_SUN_PATH_OFFSET as i32);
    // "un-count" a trailing null in filesystem-namespace paths
    let path = &a.as_posix_sockaddr_un().sun_path;
    if path[0] != 0 && l > 1 && path[(l - 1) as usize] == 0 {
        l -= 1;
    }
    l
}

impl PartialEq for SocketAddress {
    fn eq(&self, a: &SocketAddress) -> bool {
        if self.as_posix_sockaddr().sa_family != a.as_posix_sockaddr().sa_family {
            return false;
        }
        if self.as_posix_sockaddr().sa_family == AF_UNIX {
            // tolerate counting/not counting a terminating null in filesystem-namespace paths
            let adjusted_len = adjusted_path_length(self);
            let a_adjusted_len = adjusted_path_length(a);
            if adjusted_len != a_adjusted_len {
                return false;
            }
            let n = adjusted_len as usize;
            return self.as_posix_sockaddr_un().sun_path[..n]
                == a.as_posix_sockaddr_un().sun_path[..n];
        }

        // an INET address
        if self.as_posix_sockaddr_in().sin_port != a.as_posix_sockaddr_in().sin_port {
            return false;
        }
        match self.as_posix_sockaddr().sa_family {
            AF_INET => {
                return self.as_posix_sockaddr_in().sin_addr.s_addr
                    == a.as_posix_sockaddr_in().sin_addr.s_addr;
            }
            AF_UNSPEC | AF_INET6 => {
                // handled below
            }
            _ => return false,
        }

        let in1 = self.as_posix_sockaddr_in6();
        let in2 = a.as_posix_sockaddr_in6();

        in6_are_addr_equal(&in1.sin6_addr, &in2.sin6_addr)
    }
}
impl Eq for SocketAddress {}

/// The text of an `AF_UNIX` address: `{unnamed}`, the filesystem path, or `@` followed by
/// the abstract name with each non-printable byte shown as `_`.
pub fn unix_domain_addr_text(sa: &SocketAddress) -> String {
    if sa.length() as usize <= SOCKADDR_UN_SUN_PATH_OFFSET {
        return "{unnamed}".to_string();
    }
    let path = &sa.as_posix_sockaddr_un().sun_path;
    if path[0] != 0 {
        // regular (filesystem-namespace) path
        let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
        return String::from_utf8_lossy(&path[..end]).into_owned();
    }

    let path_length = sa.length() as usize - SOCKADDR_UN_SUN_PATH_OFFSET;
    let mut ud_path: Vec<u8> = vec![0; path_length];
    let mut targ = 0usize;
    ud_path[targ] = b'@';
    targ += 1;
    let mut src = 1usize;
    let mut k = path_length as i32;

    loop {
        k -= 1;
        if k <= 0 {
            break;
        }
        let c = path[src];
        ud_path[targ] = if (0x20..0x7f).contains(&c) { c } else { b'_' };
        targ += 1;
        src += 1;
    }
    String::from_utf8_lossy(&ud_path).into_owned()
}

impl fmt::Display for SocketAddress {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_af_unix() {
            return os.write_str(&unix_domain_addr_text(self));
        }

        let addr = self.addr();
        // IPv4-mapped IPv6 addresses are bracketed too.
        let bracket = addr.in_family() == Family::INET6;

        if bracket {
            os.write_str("[")?;
        }
        write!(os, "{}", addr)?;
        if bracket {
            os.write_str("]")?;
        }

        write!(os, ":{}", ntohs(self.as_posix_sockaddr_in().sin_port))
    }
}

impl fmt::Debug for SocketAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}
