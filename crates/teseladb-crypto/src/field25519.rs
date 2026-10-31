//! Arithmetic modulo p = 2²⁵⁵ − 19 with five 51-bit limbs, in constant time.

use crate::ct::{cswap64, is_zero64, mask64};

const MASK51: u64 = (1 << 51) - 1;

/// A field element; limbs may exceed 51 bits between reductions.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fe(pub(crate) [u64; 5]);

impl Fe {
    pub(crate) const ZERO: Fe = Fe([0; 5]);
    pub(crate) const ONE: Fe = Fe([1, 0, 0, 0, 0]);

    /// Reads 32 little-endian bytes, ignoring the top bit.
    pub(crate) fn from_bytes(b: &[u8; 32]) -> Fe {
        let load = |i: usize| {
            let mut w = [0u8; 8];
            let end = (i + 8).min(32);
            w[..end - i].copy_from_slice(&b[i..end]);
            u64::from_le_bytes(w)
        };
        Fe([
            load(0) & MASK51,
            (load(6) >> 3) & MASK51,
            (load(12) >> 6) & MASK51,
            (load(19) >> 1) & MASK51,
            (load(24) >> 12) & MASK51,
        ])
    }

    fn carry(mut self) -> Fe {
        for _ in 0..2 {
            for i in 0..4 {
                self.0[i + 1] += self.0[i] >> 51;
                self.0[i] &= MASK51;
            }
            self.0[0] += 19 * (self.0[4] >> 51);
            self.0[4] &= MASK51;
        }
        self
    }

    /// The canonical 32-byte encoding.
    pub(crate) fn to_bytes(self) -> [u8; 32] {
        let mut h = self.carry().0;
        // Subtract p when h ≥ p: q is 1 exactly then.
        let mut q = (h[0] + 19) >> 51;
        for limb in &h[1..] {
            q = (limb + q) >> 51;
        }
        h[0] += 19 * q;
        for i in 0..4 {
            h[i + 1] += h[i] >> 51;
            h[i] &= MASK51;
        }
        h[4] &= MASK51;
        let words = [
            h[0] | (h[1] << 51),
            (h[1] >> 13) | (h[2] << 38),
            (h[2] >> 26) | (h[3] << 25),
            (h[3] >> 39) | (h[4] << 12),
        ];
        let mut out = [0u8; 32];
        for (chunk, w) in out.as_chunks_mut::<8>().0.iter_mut().zip(words) {
            *chunk = w.to_le_bytes();
        }
        out
    }

    pub(crate) fn add(self, o: Fe) -> Fe {
        let mut r = [0; 5];
        for (i, x) in r.iter_mut().enumerate() {
            *x = self.0[i] + o.0[i];
        }
        Fe(r).carry()
    }

    pub(crate) fn sub(self, o: Fe) -> Fe {
        // Adds 4p before subtracting so no limb goes negative.
        const P4: [u64; 5] = [
            0x1f_ffff_ffff_ffb4,
            0x1f_ffff_ffff_fffc,
            0x1f_ffff_ffff_fffc,
            0x1f_ffff_ffff_fffc,
            0x1f_ffff_ffff_fffc,
        ];
        let a = self.carry();
        let b = o.carry();
        let mut r = [0; 5];
        for (i, x) in r.iter_mut().enumerate() {
            *x = a.0[i] + P4[i] - b.0[i];
        }
        Fe(r).carry()
    }

    pub(crate) fn neg(self) -> Fe {
        Fe::ZERO.sub(self)
    }

    pub(crate) fn mul(self, o: Fe) -> Fe {
        let a = self.carry().0.map(u128::from);
        let b = o.carry().0.map(u128::from);
        let b19: [u128; 5] = b.map(|x| x * 19);
        let r0 = a[0] * b[0] + a[1] * b19[4] + a[2] * b19[3] + a[3] * b19[2] + a[4] * b19[1];
        let r1 = a[0] * b[1] + a[1] * b[0] + a[2] * b19[4] + a[3] * b19[3] + a[4] * b19[2];
        let r2 = a[0] * b[2] + a[1] * b[1] + a[2] * b[0] + a[3] * b19[4] + a[4] * b19[3];
        let r3 = a[0] * b[3] + a[1] * b[2] + a[2] * b[1] + a[3] * b[0] + a[4] * b19[4];
        let r4 = a[0] * b[4] + a[1] * b[3] + a[2] * b[2] + a[3] * b[1] + a[4] * b[0];
        let mut r = [r0, r1, r2, r3, r4];
        let mut out = [0u64; 5];
        for i in 0..4 {
            r[i + 1] += r[i] >> 51;
            out[i] = (r[i] as u64) & MASK51;
        }
        out[4] = (r[4] as u64) & MASK51;
        out[0] += (19 * (r[4] >> 51)) as u64;
        Fe(out).carry()
    }

    pub(crate) fn square(self) -> Fe {
        self.mul(self)
    }

    pub(crate) fn mul_small(self, k: u64) -> Fe {
        let a = self.carry().0;
        let mut r = [0u128; 5];
        for i in 0..5 {
            r[i] = u128::from(a[i]) * u128::from(k);
        }
        let mut out = [0u64; 5];
        for i in 0..4 {
            r[i + 1] += r[i] >> 51;
            out[i] = (r[i] as u64) & MASK51;
        }
        out[4] = (r[4] as u64) & MASK51;
        out[0] += (19 * (r[4] >> 51)) as u64;
        Fe(out).carry()
    }

    fn pow2k(mut self, k: u32) -> Fe {
        for _ in 0..k {
            self = self.square();
        }
        self
    }

    /// Returns (x^(2²⁵⁰−1), x¹¹), the shared prefix of inversion and square roots.
    fn pow22501(self) -> (Fe, Fe) {
        let t0 = self.square();
        let t1 = t0.pow2k(2).mul(self);
        let t0 = t0.mul(t1); // x^11
        let t2 = t0.square().mul(t1); // x^31 = 2^5-1
        let t2 = t2.pow2k(5).mul(t2); // 2^10-1
        let t3 = t2.pow2k(10).mul(t2); // 2^20-1
        let t4 = t3.pow2k(20).mul(t3); // 2^40-1
        let t4 = t4.pow2k(10).mul(t2); // 2^50-1
        let t5 = t4.pow2k(50).mul(t4); // 2^100-1
        let t6 = t5.pow2k(100).mul(t5); // 2^200-1
        let t6 = t6.pow2k(50).mul(t4); // 2^250-1
        (t6, t0)
    }

    /// x^(p−2): the inverse, and 0 for 0.
    pub(crate) fn invert(self) -> Fe {
        let (t, x11) = self.pow22501();
        t.pow2k(5).mul(x11)
    }

    /// x^((p−5)/8) = x^(2²⁵²−3).
    pub(crate) fn pow_p58(self) -> Fe {
        let (t, _) = self.pow22501();
        t.pow2k(2).mul(self)
    }

    /// All ones when the element is zero.
    pub(crate) fn is_zero_mask(self) -> u64 {
        let b = self.to_bytes();
        let mut acc = 0u64;
        for x in b {
            acc |= u64::from(x);
        }
        is_zero64(acc)
    }

    pub(crate) fn ct_eq_mask(self, o: Fe) -> u64 {
        self.sub(o).is_zero_mask()
    }

    /// Lowest bit of the canonical encoding.
    pub(crate) fn is_negative(self) -> u64 {
        u64::from(self.to_bytes()[0] & 1)
    }

    pub(crate) fn select(mask: u64, a: Fe, b: Fe) -> Fe {
        let mut r = b.0;
        for (i, x) in r.iter_mut().enumerate() {
            *x ^= mask & (a.0[i] ^ b.0[i]);
        }
        Fe(r)
    }

    pub(crate) fn cswap(bit: u64, a: &mut Fe, b: &mut Fe) {
        cswap64(mask64(bit), &mut a.0, &mut b.0);
    }
}
