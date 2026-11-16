//! Kernel release parsing and comparison.
//!
//! The engine enables or avoids some kernel features (for example, the behaviour of asynchronous
//! I/O on a given file system) depending on the running kernel. This module parses release strings
//! such as `"3.10.0-957.5.1.el7.x86_64"` into [`UnameT`], formats them back, and answers "is this
//! kernel that release or a later one of the same line?". [`kernel_uname`] reads the running
//! kernel's release with `uname(2)`.

use std::ffi::{CStr, c_char, c_int};
use std::fmt;

use crate::engine::core::internal::uname::UnameT;

impl UnameT {
    /// Returns how many numeric components the release has: 2 for `"4.5"`, 3 for `"4.5.0"`, 4 for
    /// `"4.5.0.1"`, 5 when a distribution patch number is present, as in `"5.1.3-33.3.el7"`. The
    /// trailing text (`".3.el7"`) does not count.
    pub fn component_count(&self) -> i32 {
        if self.distro_patch.is_some() {
            return 5;
        }
        if self.subsublevel.is_some() {
            return 4;
        }
        if self.sublevel.is_some() {
            return 3;
        }
        2
    }

    /// Returns whether the distribution suffix (the text after the patch number, such as
    /// `".el7"`) contains `extra`.
    pub fn has_distro_extra(&self, extra: &str) -> bool {
        self.distro_extra.contains(extra)
    }

    /// Returns whether this kernel is `x` or a later release of the same line.
    ///
    /// The major and minor numbers must be at least those of `x`, the distribution suffix must
    /// contain that of `x`, and the components that `x` gives beyond the minor number must match
    /// exactly except the last one, which may be greater.
    pub fn same_as_or_descendant_of(&self, x: &UnameT) -> bool {
        if self.version < x.version {
            return false; // 4.2 vs. 5.1
        }
        if self.version == x.version && self.patchlevel < x.patchlevel {
            return false; // 4.0 vs 4.1
        }
        if !self.has_distro_extra(&x.distro_extra) {
            return false;
        }
        match x.component_count() {
            5 => {
                self.version == x.version
                    && self.patchlevel == x.patchlevel
                    && cmp(self.sublevel, x.sublevel) == 0
                    && cmp(self.subsublevel, x.subsublevel) == 0
                    && cmp(self.distro_patch, x.distro_patch) >= 0
            }
            4 => {
                self.version == x.version
                    && self.patchlevel == x.patchlevel
                    && cmp(self.sublevel, x.sublevel) == 0
                    && cmp(self.subsublevel, x.subsublevel) >= 0
            }
            3 => {
                self.version == x.version
                    && self.patchlevel == x.patchlevel
                    && cmp(self.sublevel, x.sublevel) >= 0
            }
            2 => true,
            _ => false,
        }
    }

    /// Same as [`same_as_or_descendant_of`](Self::same_as_or_descendant_of), with `x` given as a
    /// release string and parsed with [`parse_uname`].
    pub fn same_as_or_descendant_of_str(&self, x: &str) -> bool {
        self.same_as_or_descendant_of(&parse_uname(x))
    }

    /// Returns whether this kernel is, or descends from, any of the release strings in `wl`.
    pub fn whitelisted(&self, wl: &[&str]) -> bool {
        wl.iter().any(|v| self.same_as_or_descendant_of(&parse_uname(v)))
    }
}

/// Compares two optional components, a missing one counting as `0`; the sign of the result orders
/// them.
fn cmp(u1: Option<i32>, u2: Option<i32>) -> i32 {
    u1.unwrap_or(0).wrapping_sub(u2.unwrap_or(0))
}

/// Matches `(\d+)\.(\d+)(?:\.(\d+)(?:\.(\d+))?)?(?:-(\d*)(.+))?` against the whole of `u`.
fn match_uname(u: &str) -> Option<(&str, &str, &str, &str, &str, &str)> {
    let b = u.as_bytes();
    let digits = |i: usize| -> usize {
        let mut j = i;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        j
    };
    let e1 = digits(0);
    if e1 == 0 || b.get(e1) != Some(&b'.') {
        return None;
    }
    let s2 = e1 + 1;
    let e2 = digits(s2);
    if e2 == s2 {
        return None;
    }
    let (mut g3, mut g4) = ("", "");
    let mut i = e2;
    if b.get(i) == Some(&b'.') && digits(i + 1) > i + 1 {
        let e3 = digits(i + 1);
        g3 = &u[i + 1..e3];
        i = e3;
        if b.get(i) == Some(&b'.') && digits(i + 1) > i + 1 {
            let e4 = digits(i + 1);
            g4 = &u[i + 1..e4];
            i = e4;
        }
    }
    let (mut g5, mut g6) = ("", "");
    if i < b.len() {
        if b[i] != b'-' || i + 1 >= b.len() {
            return None;
        }
        let s5 = i + 1;
        // (\d*) is greedy but must leave at least one character for (.+)
        let e5 = digits(s5).min(b.len() - 1);
        g5 = &u[s5..e5];
        g6 = &u[e5..];
    }
    Some((&u[..e1], &u[s2..e2], g3, g4, g5, g6))
}

/// Parses a kernel release string such as `"3.10.0-957.5.1.el7.x86_64"`.
///
/// A string that does not have the form `major.minor[.sub[.subsub]][-[patch]suffix]` gives a
/// release of `0.0` with no further components; it never fails.
pub fn parse_uname(u: &str) -> UnameT {
    let num = |sm: &str| -> Option<i32> {
        if !sm.is_empty() {
            // Out-of-range numbers read as 0.
            Some(sm.parse::<i64>().map(|v| v as i32).unwrap_or(0))
        } else {
            None
        }
    };
    match match_uname(u) {
        Some((m1, m2, m3, m4, m5, m6)) => UnameT {
            version: num(m1).unwrap_or(0),
            patchlevel: num(m2).unwrap_or(0),
            sublevel: num(m3),
            subsublevel: num(m4),
            distro_patch: num(m5),
            distro_extra: m6.to_string(),
        },
        None => UnameT {
            version: 0,
            patchlevel: 0,
            sublevel: None,
            subsublevel: None,
            distro_patch: None,
            distro_extra: String::new(),
        },
    }
}

impl fmt::Display for UnameT {
    fn fmt(&self, os: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(os, "{}.{}", self.version, self.patchlevel)?;
        if let Some(s) = self.sublevel {
            write!(os, ".{}", s)?;
        }
        if let Some(s) = self.subsublevel {
            write!(os, ".{}", s)?;
        }
        if self.distro_patch.is_some() || !self.distro_extra.is_empty() {
            os.write_str("-")?;
        }
        if let Some(p) = self.distro_patch {
            write!(os, "{}", p)?;
        }
        os.write_str(&self.distro_extra)
    }
}

/// `struct utsname` of Linux (six 65-byte fields).
#[cfg(target_os = "linux")]
#[repr(C)]
struct Utsname {
    sysname: [c_char; 65],
    nodename: [c_char; 65],
    release: [c_char; 65],
    version: [c_char; 65],
    machine: [c_char; 65],
    domainname: [c_char; 65],
}

/// `struct utsname` of Darwin (five 256-byte fields).
#[cfg(not(target_os = "linux"))]
#[repr(C)]
struct Utsname {
    sysname: [c_char; 256],
    nodename: [c_char; 256],
    release: [c_char; 256],
    version: [c_char; 256],
    machine: [c_char; 256],
}

unsafe extern "C" {
    fn uname(buf: *mut Utsname) -> c_int;
}

/// Returns the release of the running kernel, read with `uname(2)` and parsed with
/// [`parse_uname`].
pub fn kernel_uname() -> UnameT {
    let mut buf = std::mem::MaybeUninit::<Utsname>::zeroed();
    // SAFETY: `buf` is a zeroed `utsname` of the platform's layout, so even if `uname` fails the
    // `release` field is a NUL-terminated (empty) string; on success the kernel fills it with a
    // NUL-terminated string that fits the field.
    let release = unsafe {
        uname(buf.as_mut_ptr());
        CStr::from_ptr((*buf.as_ptr()).release.as_ptr())
    };
    parse_uname(&release.to_string_lossy())
}
