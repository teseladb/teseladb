//! Linux kernel version numbers, used to pick features that depend on the running kernel.

/// A parsed Linux kernel version number, as reported by `uname(2)`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UnameT {
    /// 4 in "4.5"
    pub version: i32,
    /// 5 in "4.5"
    pub patchlevel: i32,
    /// 1 in "4.5.1"
    pub sublevel: Option<i32>,
    /// 33 in "2.6.44.33"
    pub subsublevel: Option<i32>,
    /// 957 in "3.10.0-957.5.1.el7.x86_64"
    pub distro_patch: Option<i32>,
    /// .5.1.el7.x86_64
    pub distro_extra: String,
}

// The methods, `kernel_uname()` and `parse_uname()` live in `crate::engine::core::uname`.
pub use crate::engine::core::uname::{kernel_uname, parse_uname};
