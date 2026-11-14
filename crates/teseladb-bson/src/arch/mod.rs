//! The search of the 0 byte that ends a name (003 FR-023): NEON on `AArch64`, AVX2 or SSE2 on
//! x86-64 chosen at run time, and the byte-by-byte search elsewhere. Every path gives the position
//! the byte-by-byte search gives.
//!
//! It is the only module of the crate with `unsafe`: the vector functions use
//! `#[target_feature]`, load from pointers into the slice they are given, and are only called
//! after checking that the CPU has those extensions.
#![allow(unsafe_code)]

#[cfg(target_arch = "aarch64")]
mod aarch64;
#[cfg(target_arch = "x86_64")]
mod x86_64;

/// Which search [`find_nul`] uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Path {
    #[cfg(target_arch = "aarch64")]
    Neon,
    #[cfg(target_arch = "x86_64")]
    Avx2,
    #[cfg(target_arch = "x86_64")]
    Sse2,
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    Bytewise,
}

fn path() -> Path {
    #[cfg(target_arch = "aarch64")]
    {
        // Every `AArch64` CPU has NEON.
        Path::Neon
    }
    #[cfg(target_arch = "x86_64")]
    {
        static AVX2: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *AVX2.get_or_init(|| std::arch::is_x86_feature_detected!("avx2")) {
            Path::Avx2
        } else {
            Path::Sse2
        }
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        Path::Bytewise
    }
}

/// The name of the search [`find_nul`] uses on this CPU: `neon`, `avx2`, `sse2` or `bytewise`.
#[must_use]
pub fn nul_search() -> &'static str {
    match path() {
        #[cfg(target_arch = "aarch64")]
        Path::Neon => "neon",
        #[cfg(target_arch = "x86_64")]
        Path::Avx2 => "avx2",
        #[cfg(target_arch = "x86_64")]
        Path::Sse2 => "sse2",
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        Path::Bytewise => "bytewise",
    }
}

/// The position of the first 0 byte of `b`, as `b.iter().position(|&c| c == 0)` gives it, found
/// 16 or 32 bytes at a time with the vector instructions of the CPU.
#[must_use]
#[inline]
pub fn find_nul(b: &[u8]) -> Option<usize> {
    match path() {
        #[cfg(target_arch = "aarch64")]
        // SAFETY: every `AArch64` CPU has NEON.
        Path::Neon => unsafe { aarch64::find_nul(b) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: `path` checked AVX2.
        Path::Avx2 => unsafe { x86_64::find_nul_avx2(b) },
        #[cfg(target_arch = "x86_64")]
        // SAFETY: every x86-64 CPU has SSE2.
        Path::Sse2 => unsafe { x86_64::find_nul_sse2(b) },
        #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
        Path::Bytewise => bytewise(b),
    }
}

/// The byte-by-byte search, for the tails shorter than a vector.
#[inline]
pub(crate) fn bytewise(b: &[u8]) -> Option<usize> {
    b.iter().position(|&c| c == 0)
}

#[cfg(test)]
mod tests {
    use super::bytewise;

    /// Slices of every length up to 100 with a zero at every position, and without one, at
    /// every start alignment of their buffer.
    pub(super) fn cases() -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for len in 0..=100 {
            out.push(vec![0xffu8; len]);
            for at in 0..len {
                let mut b: Vec<u8> =
                    (0..len).map(|i| u8::try_from(i % 255 + 1).unwrap_or(1)).collect();
                b[at] = 0;
                if at + 7 < len {
                    b[at + 7] = 0;
                }
                out.push(b);
            }
        }
        out
    }

    pub(super) fn check(f: impl Fn(&[u8]) -> Option<usize>) {
        for b in cases() {
            for start in 0..b.len().min(33) {
                assert_eq!(f(&b[start..]), bytewise(&b[start..]), "{:?}", &b[start..]);
            }
        }
    }

    #[test]
    fn the_dispatched_search_matches_the_bytewise_one() {
        check(super::find_nul);
    }
}
