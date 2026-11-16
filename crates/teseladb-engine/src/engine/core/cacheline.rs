//! The CPU cache line size of the target architecture and a wrapper that aligns a value to it.
//!
//! Per-core structures that other cores may touch (counters, queue heads, shard state) are
//! padded to a cache line with [`CacheLineAligned`] so that writes on one core do not invalidate
//! a line another core is using (false sharing). Building for an architecture not listed here is
//! a compile error, so a new target must choose its size explicitly.

/// Cache line size in bytes, for x86 and x86-64.
#[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
pub const CACHE_LINE_SIZE: usize = 64;
/// Cache line size in bytes, for s390x.
#[cfg(target_arch = "s390x")]
pub const CACHE_LINE_SIZE: usize = 256;
/// Cache line size in bytes, for 64-bit POWER.
#[cfg(target_arch = "powerpc64")]
pub const CACHE_LINE_SIZE: usize = 128;
/// Cache line size in bytes, for AArch64. 128 is the value the Linux kernel uses for this
/// architecture; some microarchitectures have 64-byte lines, and the larger value is a safe
/// upper bound for padding.
#[cfg(target_arch = "aarch64")]
pub const CACHE_LINE_SIZE: usize = 128;
#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "x86",
    target_arch = "s390x",
    target_arch = "powerpc64",
    target_arch = "aarch64"
)))]
compile_error!("cache_line_size not defined for this architecture");

/// A value aligned (and therefore padded) to [`CACHE_LINE_SIZE`], so that it never shares a
/// cache line with a neighbouring value.
#[cfg_attr(any(target_arch = "x86_64", target_arch = "x86"), repr(C, align(64)))]
#[cfg_attr(target_arch = "s390x", repr(C, align(256)))]
#[cfg_attr(any(target_arch = "powerpc64", target_arch = "aarch64"), repr(C, align(128)))]
#[derive(Debug, Default)]
pub struct CacheLineAligned<T>(pub T);
