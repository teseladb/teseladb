//! Explicit prefetch (009 FR-024): a walk over an index or a list of keys in memory asks the CPU
//! to bring into its cache the element a few places ahead, so that its load has finished when the
//! walk gets there. `_mm_prefetch` on x86_64 and `prfm pldl1keep` on aarch64; a hint that never
//! faults and changes no value, and nothing on other targets and under Miri, which runs no
//! assembly.
//!
//! This module holds the `unsafe` of the crate (D-151): the prefetch instructions. Each block
//! says why it holds, and the tests run them over live and boundary addresses.

#![allow(unsafe_code)]

/// How many places ahead a walk prefetches: three or four elements of a list (FR-024).
pub const AHEAD: usize = 4;

/// Asks the CPU to bring the cache line of `value`'s first byte into its first-level cache.
#[inline(always)]
pub fn prefetch<T: ?Sized>(value: &T) {
    prefetch_ptr(std::ptr::from_ref(value).cast::<u8>());
}

/// Prefetches `bytes[at]` when `at` is inside `bytes`.
#[inline(always)]
pub fn prefetch_byte(bytes: &[u8], at: usize) {
    if let Some(b) = bytes.get(at) {
        prefetch(b);
    }
}

/// Prefetches element `at` of `items` when there is one.
#[inline(always)]
pub fn prefetch_item<T>(items: &[T], at: usize) {
    if let Some(item) = items.get(at) {
        prefetch(item);
    }
}

#[inline(always)]
fn prefetch_ptr(p: *const u8) {
    #[cfg(all(target_arch = "x86_64", not(miri)))]
    {
        use std::arch::x86_64::{_MM_HINT_T0, _mm_prefetch};
        // SAFETY: a prefetch reads nothing the program sees and never faults, whatever the
        // address; `p` comes from a reference, so it is a live address of this process. SSE, which
        // has the instruction, is part of every x86_64 target.
        unsafe { _mm_prefetch::<{ _MM_HINT_T0 }>(p.cast::<i8>()) }
    }
    #[cfg(all(target_arch = "aarch64", not(miri)))]
    {
        // SAFETY: `prfm` is a hint: it loads a cache line, writes no register, memory or flag
        // and never faults, whatever the address; `p` comes from a reference. The options say
        // so to the compiler: no stack, flags kept, memory only read.
        unsafe {
            std::arch::asm!(
                "prfm pldl1keep, [{0}]",
                in(reg) p,
                options(nostack, preserves_flags, readonly)
            );
        }
    }
    #[cfg(any(miri, not(any(target_arch = "x86_64", target_arch = "aarch64"))))]
    let _ = p;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prefetch_changes_nothing_it_touches() {
        let bytes: Vec<u8> = (0..=255).collect();
        for at in [0, 1, 63, 64, 255, 256, usize::MAX] {
            prefetch_byte(&bytes, at);
        }
        let words: Vec<u64> = (0..100).collect();
        for at in 0..words.len() + AHEAD {
            prefetch_item(&words, at);
        }
        prefetch(&words[..]);
        prefetch("text");
        let empty: [u8; 0] = [];
        prefetch_byte(&empty, 0);
        prefetch(&empty);
        assert_eq!(bytes, (0..=255).collect::<Vec<u8>>());
        assert_eq!(words.iter().sum::<u64>(), 4950);
    }
}
