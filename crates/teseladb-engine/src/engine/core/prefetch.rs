//! Software prefetch hints for data the caller is about to read or write.
//!
//! A prefetch asks the CPU to start loading cache lines early; it never faults, even on an
//! invalid address, and has no effect other than timing. The reactor uses it to prefetch the
//! next tasks of its run queue, and data structures use it before visiting nodes.
//!
//! Every function comes in two forms: the plain name uses the default temporal locality
//! [`DEFAULT_LOC`] (3, keep in all cache levels) and the `_loc` form takes the locality as a
//! const argument. The variants differ in what they prefetch:
//!
//! | Function                 | Prefetches                                           |
//! |--------------------------|------------------------------------------------------|
//! | [`prefetch`]             | `L` cache lines starting at a byte pointer           |
//! | [`prefetch_object`]      | the cache lines covering one `T`                     |
//! | [`prefetch_objects`]     | the cache lines of each `T` of an iterator           |
//! | [`prefetch_objects_n`]   | the objects behind an array of `C` pointers          |
//! | [`prefetch_n`]           | `L` lines at each pointer of an iterator             |
//! | [`prefetch_lines_n`]     | `L` lines at each of an array of `C` pointers        |
//!
//! The `prefetchw*` functions are the same for data about to be written. For example,
//! `prefetch::<2>(p as *const u8)` prefetches two cache lines at `p` with locality 3.
//!
//! Prefetch instructions are emitted on x86-64 (`prefetcht0/1/2`, `prefetchnta`, `prefetchw`)
//! and AArch64 (`prfm`); on other architectures the functions compile to nothing.

use std::sync::atomic::{Ordering, compiler_fence};

use crate::engine::core::cacheline::CACHE_LINE_SIZE;

/// Issues one prefetch instruction for the cache line containing `ptr`.
///
/// `RW` is 0 for a read and 1 for a write; `LOC` is the temporal locality from 0 (no reuse
/// expected, do not pollute the cache) to 3 (keep in every cache level). On x86-64 a write
/// prefetch ignores `LOC`.
#[inline(always)]
pub fn builtin_prefetch<const RW: i32, const LOC: i32>(ptr: *const u8) {
    // SAFETY: prefetch instructions never fault, whatever the address, and have no
    // architectural effect; the asm reads no memory and touches no flags or stack.
    unsafe {
        #[cfg(target_arch = "x86_64")]
        {
            if RW == 1 {
                std::arch::asm!("prefetchw [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly));
            } else {
                match LOC {
                    0 => {
                        std::arch::asm!("prefetchnta [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly))
                    }
                    1 => {
                        std::arch::asm!("prefetcht2 [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly))
                    }
                    2 => {
                        std::arch::asm!("prefetcht1 [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly))
                    }
                    _ => {
                        std::arch::asm!("prefetcht0 [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly))
                    }
                }
            }
        }
        #[cfg(target_arch = "aarch64")]
        {
            match (RW, LOC) {
                (1, 0) => {
                    std::arch::asm!("prfm pstl1strm, [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly))
                }
                (1, 1) => {
                    std::arch::asm!("prfm pstl3keep, [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly))
                }
                (1, 2) => {
                    std::arch::asm!("prfm pstl2keep, [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly))
                }
                (1, _) => {
                    std::arch::asm!("prfm pstl1keep, [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly))
                }
                (_, 0) => {
                    std::arch::asm!("prfm pldl1strm, [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly))
                }
                (_, 1) => {
                    std::arch::asm!("prfm pldl3keep, [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly))
                }
                (_, 2) => {
                    std::arch::asm!("prfm pldl2keep, [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly))
                }
                (_, _) => {
                    std::arch::asm!("prfm pldl1keep, [{0}]", in(reg) ptr, options(nostack, preserves_flags, readonly))
                }
            }
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        {
            let _ = ptr;
        }
    }
}

/// Prefetches the `N` bytes at address `ptr`, one cache line at a time (nothing when `N` is
/// 0). `RW` and `LOC` are as for [`builtin_prefetch`].
#[inline(always)]
pub fn prefetcher<const N: usize, const RW: i32, const LOC: i32>(ptr: usize) {
    prefetch_bytes::<RW, LOC>(ptr, N);
}

/// `size_of::<T>()` rounded up to a whole number of cache lines.
#[inline(always)]
const fn aligned_size<T>() -> usize {
    std::mem::size_of::<T>().div_ceil(CACHE_LINE_SIZE) * CACHE_LINE_SIZE
}

/// Prefetches `n` bytes starting at `ptr`, one cache line at a time; a constant `n` lets the
/// compiler unroll the loop. A prefetch never faults, so the addresses may wrap.
#[inline(always)]
fn prefetch_bytes<const RW: i32, const LOC: i32>(ptr: usize, n: usize) {
    let mut off = 0;
    while off < n {
        builtin_prefetch::<RW, LOC>(ptr.wrapping_add(off) as *const u8);
        compiler_fence(Ordering::SeqCst);
        off += CACHE_LINE_SIZE;
    }
}

/// The default temporal locality: 3, keep the data in every cache level.
pub const DEFAULT_LOC: i32 = 3;

// LOC is the temporal locality, a constant from 0 to 3: 0 means the data will not be reused
// soon and need not stay in the cache after the access; 3 means a high degree of reuse and the
// data should stay in every cache level; 1 and 2 are low and moderate reuse.

/// Prefetches for reading the cache lines covering the `T` at `ptr`, with locality `LOC`.
#[inline(always)]
pub fn prefetch_object_loc<T, const LOC: i32>(ptr: *const T) {
    prefetch_bytes::<0, LOC>(ptr as usize, aligned_size::<T>());
}

/// Prefetches for reading the cache lines covering the `T` at `ptr`.
#[inline(always)]
pub fn prefetch_object<T>(ptr: *const T) {
    prefetch_object_loc::<T, DEFAULT_LOC>(ptr);
}

/// Prefetches for reading each object an iterator of pointers yields, with locality `LOC`.
#[inline(always)]
pub fn prefetch_objects_loc<T, I: IntoIterator<Item = *const T>, const LOC: i32>(it: I) {
    for v in it {
        prefetch_object_loc::<T, LOC>(v);
    }
}

/// Prefetches for reading each object an iterator of pointers yields.
#[inline(always)]
pub fn prefetch_objects<T, I: IntoIterator<Item = *const T>>(it: I) {
    prefetch_objects_loc::<T, I, DEFAULT_LOC>(it);
}

/// Prefetches for reading the objects behind the `C` pointers at `pptr`, with locality `LOC`.
///
/// # Safety
///
/// `pptr` must point to `C` readable pointers. The pointed-to objects need not be valid: a
/// prefetch never faults.
#[inline(always)]
pub unsafe fn prefetch_objects_n_loc<const C: usize, T, const LOC: i32>(pptr: *const *const T) {
    for x in 0..C {
        // SAFETY: the caller guarantees `pptr` points to `C` readable pointers and `x < C`.
        prefetch_object_loc::<T, LOC>(unsafe { *pptr.add(x) });
    }
}

/// Prefetches for reading the objects behind the `C` pointers at `pptr`.
///
/// # Safety
///
/// As [`prefetch_objects_n_loc`].
#[inline(always)]
pub unsafe fn prefetch_objects_n<const C: usize, T>(pptr: *const *const T) {
    // SAFETY: the caller upholds the same contract as the `_loc` function requires.
    unsafe { prefetch_objects_n_loc::<C, T, DEFAULT_LOC>(pptr) };
}

/// Prefetches for reading `L` cache lines starting at `ptr`, with locality `LOC`.
#[inline(always)]
pub fn prefetch_loc<const L: usize, const LOC: i32>(ptr: *const u8) {
    // `L * CACHE_LINE_SIZE` cannot be a const generic argument on stable Rust, so the length
    // is passed as a value; being constant, it unrolls the same way.
    prefetch_bytes::<0, LOC>(ptr as usize, L * CACHE_LINE_SIZE);
}

/// Prefetches for reading `L` cache lines starting at `ptr`, with locality 3.
#[inline(always)]
pub fn prefetch<const L: usize>(ptr: *const u8) {
    prefetch_loc::<L, DEFAULT_LOC>(ptr);
}

/// Prefetches for reading `L` cache lines at each pointer of an iterator, with locality `LOC`.
#[inline(always)]
pub fn prefetch_n_loc<const L: usize, I: IntoIterator<Item = *const u8>, const LOC: i32>(it: I) {
    for v in it {
        prefetch_loc::<L, LOC>(v);
    }
}

/// Prefetches for reading `L` cache lines at each pointer of an iterator.
#[inline(always)]
pub fn prefetch_n<const L: usize, I: IntoIterator<Item = *const u8>>(it: I) {
    prefetch_n_loc::<L, I, DEFAULT_LOC>(it);
}

/// Prefetches for reading `L` cache lines at each of the `C` pointers at `pptr`, with locality
/// `LOC`.
///
/// # Safety
///
/// `pptr` must point to `C` readable pointers. The pointed-to objects need not be valid: a
/// prefetch never faults.
#[inline(always)]
pub unsafe fn prefetch_lines_n_loc<const L: usize, const C: usize, T, const LOC: i32>(
    pptr: *const *const T,
) {
    for x in 0..C {
        // SAFETY: the caller guarantees `pptr` points to `C` readable pointers and `x < C`.
        prefetch_loc::<L, LOC>(unsafe { *pptr.add(x) } as *const u8);
    }
}

/// Prefetches for reading `L` cache lines at each of the `C` pointers at `pptr`.
///
/// # Safety
///
/// As [`prefetch_lines_n_loc`].
#[inline(always)]
pub unsafe fn prefetch_lines_n<const L: usize, const C: usize, T>(pptr: *const *const T) {
    // SAFETY: the caller upholds the same contract as the `_loc` function requires.
    unsafe { prefetch_lines_n_loc::<L, C, T, DEFAULT_LOC>(pptr) };
}

/// Prefetches for writing the cache lines covering the `T` at `ptr`, with locality `LOC`.
#[inline(always)]
pub fn prefetchw_object_loc<T, const LOC: i32>(ptr: *const T) {
    prefetch_bytes::<1, LOC>(ptr as usize, aligned_size::<T>());
}

/// Prefetches for writing the cache lines covering the `T` at `ptr`.
#[inline(always)]
pub fn prefetchw_object<T>(ptr: *const T) {
    prefetchw_object_loc::<T, DEFAULT_LOC>(ptr);
}

/// Prefetches for writing each object an iterator of pointers yields, with locality `LOC`.
#[inline(always)]
pub fn prefetchw_objects_loc<T, I: IntoIterator<Item = *const T>, const LOC: i32>(it: I) {
    for v in it {
        prefetchw_object_loc::<T, LOC>(v);
    }
}

/// Prefetches for writing each object an iterator of pointers yields.
#[inline(always)]
pub fn prefetchw_objects<T, I: IntoIterator<Item = *const T>>(it: I) {
    prefetchw_objects_loc::<T, I, DEFAULT_LOC>(it);
}

/// Prefetches for writing the objects behind the `C` pointers at `pptr`, with locality `LOC`.
///
/// # Safety
///
/// `pptr` must point to `C` readable pointers. The pointed-to objects need not be valid: a
/// prefetch never faults.
#[inline(always)]
pub unsafe fn prefetchw_objects_n_loc<const C: usize, T, const LOC: i32>(pptr: *const *const T) {
    for x in 0..C {
        // SAFETY: the caller guarantees `pptr` points to `C` readable pointers and `x < C`.
        prefetchw_object_loc::<T, LOC>(unsafe { *pptr.add(x) });
    }
}

/// Prefetches for writing the objects behind the `C` pointers at `pptr`.
///
/// # Safety
///
/// As [`prefetchw_objects_n_loc`].
#[inline(always)]
pub unsafe fn prefetchw_objects_n<const C: usize, T>(pptr: *const *const T) {
    // SAFETY: the caller upholds the same contract as the `_loc` function requires.
    unsafe { prefetchw_objects_n_loc::<C, T, DEFAULT_LOC>(pptr) };
}

/// Prefetches for writing `L` cache lines starting at `ptr`, with locality `LOC`.
#[inline(always)]
pub fn prefetchw_loc<const L: usize, const LOC: i32>(ptr: *const u8) {
    // The length is a value for the same reason as in `prefetch_loc`.
    prefetch_bytes::<1, LOC>(ptr as usize, L * CACHE_LINE_SIZE);
}

/// Prefetches for writing `L` cache lines starting at `ptr`, with locality 3.
#[inline(always)]
pub fn prefetchw<const L: usize>(ptr: *const u8) {
    prefetchw_loc::<L, DEFAULT_LOC>(ptr);
}

/// Prefetches for writing `L` cache lines at each pointer of an iterator, with locality `LOC`.
#[inline(always)]
pub fn prefetchw_n_loc<const L: usize, I: IntoIterator<Item = *const u8>, const LOC: i32>(it: I) {
    for v in it {
        prefetchw_loc::<L, LOC>(v);
    }
}

/// Prefetches for writing `L` cache lines at each pointer of an iterator.
#[inline(always)]
pub fn prefetchw_n<const L: usize, I: IntoIterator<Item = *const u8>>(it: I) {
    prefetchw_n_loc::<L, I, DEFAULT_LOC>(it);
}

/// Prefetches for writing `L` cache lines at each of the `C` pointers at `pptr`, with locality
/// `LOC`.
///
/// # Safety
///
/// `pptr` must point to `C` readable pointers. The pointed-to objects need not be valid: a
/// prefetch never faults.
#[inline(always)]
pub unsafe fn prefetchw_lines_n_loc<const L: usize, const C: usize, T, const LOC: i32>(
    pptr: *const *const T,
) {
    for x in 0..C {
        // SAFETY: the caller guarantees `pptr` points to `C` readable pointers and `x < C`.
        prefetchw_loc::<L, LOC>(unsafe { *pptr.add(x) } as *const u8);
    }
}

/// Prefetches for writing `L` cache lines at each of the `C` pointers at `pptr`.
///
/// # Safety
///
/// As [`prefetchw_lines_n_loc`].
#[inline(always)]
pub unsafe fn prefetchw_lines_n<const L: usize, const C: usize, T>(pptr: *const *const T) {
    // SAFETY: the caller upholds the same contract as the `_loc` function requires.
    unsafe { prefetchw_lines_n_loc::<L, C, T, DEFAULT_LOC>(pptr) };
}
