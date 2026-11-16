//! UNIX-domain socket addresses.
//!
//! [`UnixDomainAddr`] holds the name of a UNIX-domain socket and is what a
//! [`SocketAddress`](crate::engine::net::socket_defs::SocketAddress) of family `AF_UNIX` is
//! built from. The name may start with a null byte (the Linux abstract namespace), so it is
//! kept as bytes with an explicit length, never as a C string.

use std::fmt;
use std::hash::{Hash, Hasher};

/// The address of a UNIX-domain socket.
///
/// A UNIX-domain socket is either named or unnamed. If named, the name is either a path in
/// the filesystem namespace, or an abstract-namespace identifier. Abstract names start with
/// a null byte and may contain non-printable characters, so the name is stored as bytes with
/// its own length rather than relying on `strlen`.
#[derive(Clone, Debug)]
pub struct UnixDomainAddr {
    /// The name bytes, without a terminating null.
    pub name: Vec<u8>,
    /// The length of the address as passed to the kernel: `name.len()` for an unnamed or
    /// abstract address, `name.len() + 1` (counting the terminating null) for a filesystem
    /// path. See [`UnixDomainAddr::path_length_aux`].
    pub path_count: i32,
    /// The name followed by its terminating null.
    c_name: Vec<u8>,
}

impl UnixDomainAddr {
    /// The address named by the bytes `fn_` (which may contain null bytes).
    pub fn new(fn_: &[u8]) -> Self {
        let name = fn_.to_vec();
        let mut c_name = name.clone();
        c_name.push(0);
        let mut a = UnixDomainAddr { name, path_count: 0, c_name };
        a.path_count = a.path_length_aux();
        a
    }

    /// The address named by `fn_` up to its first null byte, as a C string would be read.
    pub fn from_c_str(fn_: &str) -> Self {
        let bytes = fn_.as_bytes();
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        Self::new(&bytes[..end])
    }

    /// The length of the address as passed to the kernel (see [`UnixDomainAddr::path_count`]).
    pub fn path_length(&self) -> i32 {
        self.path_count
    }

    /// Computes the kernel-visible length of the name.
    ///
    /// For an abstract name, `name.len()` is already the number of meaningful bytes (the
    /// leading null included). For a filesystem path it does not count the implicit
    /// terminating null, which the kernel expects, so one is added.
    pub fn path_length_aux(&self) -> i32 {
        let pl = self.name.len() as i32;
        if pl == 0 || self.name[0] == b'\0' {
            // unnamed, or abstract-namespace
            return pl;
        }
        1 + pl
    }

    /// The name followed by a terminating null byte.
    pub fn path_bytes(&self) -> &[u8] {
        &self.c_name
    }
}

impl PartialEq for UnixDomainAddr {
    fn eq(&self, a: &UnixDomainAddr) -> bool {
        self.name == a.name
    }
}
impl Eq for UnixDomainAddr {}

/// Prints `{unnamed}`, the filesystem path, or `@` followed by the abstract name with each
/// non-printable byte shown as `_`.
impl fmt::Display for UnixDomainAddr {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path_length() == 0 {
            return write!(os, "{{unnamed}}");
        }
        if self.name[0] != 0 {
            // regular (filesystem-namespace) path
            return write!(os, "{}", String::from_utf8_lossy(&self.name));
        }

        write!(os, "@")?;
        let bytes = self.path_bytes();
        let mut src = 1usize;

        let mut k = self.path_length();
        loop {
            k -= 1;
            if k <= 0 {
                break;
            }
            let c = bytes[src];
            let printable = (0x20..0x7f).contains(&c);
            write!(os, "{}", if printable { c as char } else { '_' })?;
            src += 1;
        }
        Ok(())
    }
}

impl Hash for UnixDomainAddr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}
