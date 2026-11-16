//! Low-level helpers shared by the I/O layers; not part of the stable API.
//!
//! [`iovec_utils`] handles scatter/gather vectors for vectored reads and writes;
//! [`magic`] lists the filesystem magic numbers reported by `statfs(2)`.

pub mod iovec_utils;
pub mod magic;
