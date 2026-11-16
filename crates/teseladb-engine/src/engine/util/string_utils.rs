//! Case-insensitive string keys.
//!
//! HTTP header names and similar identifiers compare without regard to ASCII case. This
//! module provides the comparator and hash ([`internal::CaseInsensitiveCmp`],
//! [`internal::CaseInsensitiveHash`]), an owned map key ([`internal::CaseInsensitiveKey`])
//! with a borrowed form ([`internal::CaseInsensitiveStr`]) for allocation-free lookups,
//! and a pass-through hasher ([`internal::CaseInsensitiveBuildHasher`]) so that a lookup
//! hashes the key bytes exactly once.

/// Case-insensitive string utilities; not part of the stable API path.
pub mod internal {
    use std::hash::{Hash, Hasher};

    use crate::engine::core::sstring::Sstring;

    /// Case-insensitive equality of two strings (ASCII case folding on each byte).
    #[derive(Clone, Copy, Default)]
    pub struct CaseInsensitiveCmp;

    impl CaseInsensitiveCmp {
        /// Compares `s1` and `s2` ignoring ASCII case.
        #[inline]
        pub fn call(&self, s1: &[u8], s2: &[u8]) -> bool {
            s1.eq_ignore_ascii_case(s2)
        }
    }

    /// Case-insensitive hash of a string: the hash of its ASCII lower-cased bytes.
    #[derive(Clone, Copy, Default)]
    pub struct CaseInsensitiveHash;

    impl CaseInsensitiveHash {
        /// Returns the hash of the lower-cased `s`, computed on the fly without
        /// materializing the lowered copy (see [`std_hash_bytes_lowered`]).
        #[inline]
        pub fn hash_of(&self, s: &[u8]) -> u64 {
            std_hash_bytes_lowered(s)
        }

        /// Hashes `s` lower-cased into `state`: one `write_u64` of
        /// [`CaseInsensitiveHash::hash_of`], which [`CaseInsensitiveBuildHasher`] passes
        /// through unchanged.
        #[inline]
        pub fn call<H: Hasher>(&self, s: &[u8], state: &mut H) {
            state.write_u64(self.hash_of(s));
        }
    }

    /// The 64-bit multiplier of the hash.
    const HASH_MUL: u64 = (0xc6a4a793_u64 << 32) + 0x5bd1e995;
    /// The seed of the hash.
    const HASH_SEED: u64 = 0xc70f6907;

    #[inline(always)]
    const fn shift_mix(v: u64) -> u64 {
        v ^ (v >> 47)
    }

    /// Returns the 64-bit MurmurHash (seed `0xc70f6907`, native-endian 8-byte loads) of
    /// the ASCII lower-cased bytes of `s`.
    ///
    /// O(len), allocation-free: each byte is lower-cased as it is loaded.
    #[inline]
    pub fn std_hash_bytes_lowered(s: &[u8]) -> u64 {
        let len = s.len();
        let mut hash = HASH_SEED ^ (len as u64).wrapping_mul(HASH_MUL);
        let (words, rest) = s.as_chunks::<8>();
        for w in words {
            let mut b = [0u8; 8];
            for (d, c) in b.iter_mut().zip(w) {
                *d = c.to_ascii_lowercase();
            }
            let data =
                shift_mix(u64::from_ne_bytes(b).wrapping_mul(HASH_MUL)).wrapping_mul(HASH_MUL);
            hash ^= data;
            hash = hash.wrapping_mul(HASH_MUL);
        }
        if !rest.is_empty() {
            // The tail bytes, the last one most significant.
            let data =
                rest.iter().rev().fold(0u64, |r, c| (r << 8) + u64::from(c.to_ascii_lowercase()));
            hash ^= data;
            hash = hash.wrapping_mul(HASH_MUL);
        }
        hash = shift_mix(hash).wrapping_mul(HASH_MUL);
        shift_mix(hash)
    }

    /// The hasher of the case-insensitive maps.
    ///
    /// The keys give their whole hash in one `write_u64` ([`CaseInsensitiveHash`]), which
    /// it keeps as is, so a lookup costs one pass of the key hash and nothing more (no
    /// second hashing of the bytes).
    #[derive(Clone, Copy, Default)]
    pub struct CaseInsensitiveHasher(u64);

    impl Hasher for CaseInsensitiveHasher {
        #[inline]
        fn finish(&self) -> u64 {
            self.0
        }

        #[inline]
        fn write_u64(&mut self, h: u64) {
            self.0 = h;
        }

        /// Not used by the keys of these maps; any other bytes are hashed the same way.
        #[inline]
        fn write(&mut self, bytes: &[u8]) {
            self.0 ^= std_hash_bytes_lowered(bytes);
        }
    }

    /// `BuildHasher` of [`CaseInsensitiveHasher`].
    pub type CaseInsensitiveBuildHasher = std::hash::BuildHasherDefault<CaseInsensitiveHasher>;

    #[cfg(test)]
    mod hash_tests {
        use super::*;

        #[test]
        fn hash_ignores_ascii_case() {
            assert_eq!(
                std_hash_bytes_lowered(b"Content-Length"),
                std_hash_bytes_lowered(b"content-length")
            );
            assert_ne!(std_hash_bytes_lowered(b"Host"), std_hash_bytes_lowered(b"Hosts"));
        }

        /// Known hash values of `""` and of `"hello"` (64-bit).
        #[test]
        fn known_hash_values() {
            assert_eq!(std_hash_bytes_lowered(b""), 6142509188972423790);
            assert_eq!(std_hash_bytes_lowered(b"HeLLo"), 2762169579135187400);
        }
    }

    /// A string key compared and hashed case-insensitively, for the hash maps of the HTTP
    /// code (header names and similar).
    #[derive(Clone, Debug, Default)]
    pub struct CaseInsensitiveKey(pub Sstring);

    impl PartialEq for CaseInsensitiveKey {
        #[inline]
        fn eq(&self, o: &Self) -> bool {
            CaseInsensitiveCmp.call(self.0.as_bytes(), o.0.as_bytes())
        }
    }
    impl Eq for CaseInsensitiveKey {}

    impl Hash for CaseInsensitiveKey {
        #[inline]
        fn hash<H: Hasher>(&self, state: &mut H) {
            CaseInsensitiveHash.call(self.0.as_bytes(), state);
        }
    }

    /// The borrowed form of [`CaseInsensitiveKey`], so that a map keyed by it is searched
    /// with a plain `&str` without building an owned key.
    #[repr(transparent)]
    pub struct CaseInsensitiveStr([u8]);

    impl CaseInsensitiveStr {
        /// Views `s` as a case-insensitive key.
        #[inline]
        pub fn new(s: &str) -> &CaseInsensitiveStr {
            Self::from_bytes(s.as_bytes())
        }

        /// Views `s` as a case-insensitive key.
        #[inline]
        pub fn from_bytes(s: &[u8]) -> &CaseInsensitiveStr {
            // SAFETY: CaseInsensitiveStr is a repr(transparent) wrapper of [u8], so the
            // pointer cast keeps layout, metadata (length) and lifetime.
            unsafe { &*(s as *const [u8] as *const CaseInsensitiveStr) }
        }
    }

    impl PartialEq for CaseInsensitiveStr {
        #[inline]
        fn eq(&self, o: &Self) -> bool {
            CaseInsensitiveCmp.call(&self.0, &o.0)
        }
    }
    impl Eq for CaseInsensitiveStr {}

    impl Hash for CaseInsensitiveStr {
        #[inline]
        fn hash<H: Hasher>(&self, state: &mut H) {
            CaseInsensitiveHash.call(&self.0, state);
        }
    }

    impl std::borrow::Borrow<CaseInsensitiveStr> for CaseInsensitiveKey {
        #[inline]
        fn borrow(&self) -> &CaseInsensitiveStr {
            CaseInsensitiveStr::from_bytes(self.0.as_bytes())
        }
    }
}
