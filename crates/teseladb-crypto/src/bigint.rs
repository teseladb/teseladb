//! Multi-precision arithmetic on little-endian `u64` limbs, in constant time for a given length.
//! Montgomery multiplication (CIOS) serves the NIST curves and RSA; numbers have at most
//! [`MAX_LIMBS`] limbs (8192 bits).

use crate::ct::{mask64, select64};

/// The largest supported number of limbs.
pub const MAX_LIMBS: usize = 128;

/// `a += b` over `a.len()` limbs (`b` may be shorter); returns the carry.
pub(crate) fn add_assign(a: &mut [u64], b: &[u64]) -> u64 {
    let mut carry = 0u64;
    for (i, x) in a.iter_mut().enumerate() {
        let y = b.get(i).copied().unwrap_or(0);
        let s = u128::from(*x) + u128::from(y) + u128::from(carry);
        *x = s as u64;
        carry = (s >> 64) as u64;
    }
    carry
}

/// `a -= b` over `a.len()` limbs; returns the borrow (0 or 1).
pub(crate) fn sub_assign(a: &mut [u64], b: &[u64]) -> u64 {
    let mut borrow = 0u64;
    for (i, x) in a.iter_mut().enumerate() {
        let y = b.get(i).copied().unwrap_or(0);
        let d = u128::from(*x).wrapping_sub(u128::from(y)).wrapping_sub(u128::from(borrow));
        *x = d as u64;
        borrow = ((d >> 64) as u64) & 1;
    }
    borrow
}

/// Copies `src` into `dst` when `mask` is all ones.
pub(crate) fn cmov(mask: u64, dst: &mut [u64], src: &[u64]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = select64(mask, *s, *d);
    }
}

/// All ones when `a < b` (same length).
pub(crate) fn ct_lt(a: &[u64], b: &[u64]) -> u64 {
    let mut t = [0u64; MAX_LIMBS + 1];
    let t = &mut t[..a.len()];
    t.copy_from_slice(a);
    mask64(sub_assign(t, b))
}

/// All ones when `a == 0`.
pub(crate) fn ct_is_zero(a: &[u64]) -> u64 {
    let acc = a.iter().fold(0, |acc, x| acc | x);
    crate::ct::is_zero64(acc)
}

/// Reads a big-endian byte string into `out` (zero-extended). Returns false if it does not fit.
pub(crate) fn from_be_bytes(bytes: &[u8], out: &mut [u64]) -> bool {
    out.fill(0);
    for (i, &b) in bytes.iter().rev().enumerate() {
        if i / 8 >= out.len() {
            if b != 0 {
                return false;
            }
            continue;
        }
        out[i / 8] |= u64::from(b) << (8 * (i % 8));
    }
    true
}

/// Writes `a` big-endian into `out` (its low bytes; higher limbs must be zero).
pub(crate) fn to_be_bytes(a: &[u64], out: &mut [u8]) {
    let n = out.len();
    for (i, o) in out.iter_mut().enumerate() {
        let pos = n - 1 - i;
        *o = a.get(pos / 8).map_or(0, |l| (l >> (8 * (pos % 8))) as u8);
    }
}

/// `x mod m` by shifting in one bit at a time (constant time for the lengths). `m` must be
/// non-zero with its top limb non-zero; `out` has `m.len()` limbs.
pub(crate) fn reduce(x: &[u64], m: &[u64], out: &mut [u64]) {
    let n = m.len();
    let mut r = [0u64; MAX_LIMBS + 1];
    let mut t = [0u64; MAX_LIMBS + 1];
    for i in (0..x.len() * 64).rev() {
        let bit = (x[i / 64] >> (i % 64)) & 1;
        let mut carry = bit;
        for limb in r.iter_mut().take(n + 1) {
            let next = *limb >> 63;
            *limb = (*limb << 1) | carry;
            carry = next;
        }
        t[..=n].copy_from_slice(&r[..=n]);
        let borrow = sub_assign(&mut t[..=n], m);
        cmov(mask64(borrow ^ 1), &mut r[..=n], &t[..=n]);
    }
    out.copy_from_slice(&r[..n]);
}

/// Montgomery arithmetic modulo an odd `m`.
#[derive(Clone, Debug)]
pub(crate) struct Mont {
    pub(crate) m: Vec<u64>,
    m0inv: u64,
    r2: Vec<u64>,
}

impl Mont {
    /// Prepares the modulus (odd, top limb non-zero, at most [`MAX_LIMBS`] limbs).
    pub(crate) fn new(m: &[u64]) -> Mont {
        // −m⁻¹ mod 2⁶⁴ by Newton's iteration.
        let mut inv = 1u64;
        for _ in 0..6 {
            inv = inv.wrapping_mul(2u64.wrapping_sub(m[0].wrapping_mul(inv)));
        }
        let n = m.len();
        let mut x = vec![0u64; 2 * n + 1];
        x[2 * n] = 1;
        let mut r2 = vec![0u64; n];
        reduce(&x, m, &mut r2);
        Mont { m: m.to_vec(), m0inv: inv.wrapping_neg(), r2 }
    }

    pub(crate) fn limbs(&self) -> usize {
        self.m.len()
    }

    /// `out = a·b·R⁻¹ mod m` for `a, b < m`.
    pub(crate) fn mul(&self, a: &[u64], b: &[u64], out: &mut [u64]) {
        let n = self.m.len();
        let mut t = [0u64; MAX_LIMBS + 2];
        for &bi in &b[..n] {
            let mut c: u128 = 0;
            for j in 0..n {
                let s = u128::from(t[j]) + u128::from(a[j]) * u128::from(bi) + c;
                t[j] = s as u64;
                c = s >> 64;
            }
            let s = u128::from(t[n]) + c;
            t[n] = s as u64;
            t[n + 1] = (s >> 64) as u64;
            let q = t[0].wrapping_mul(self.m0inv);
            let s = u128::from(t[0]) + u128::from(q) * u128::from(self.m[0]);
            let mut c = s >> 64;
            for j in 1..n {
                let s = u128::from(t[j]) + u128::from(q) * u128::from(self.m[j]) + c;
                t[j - 1] = s as u64;
                c = s >> 64;
            }
            let s = u128::from(t[n]) + c;
            t[n - 1] = s as u64;
            t[n] = t[n + 1] + ((s >> 64) as u64);
        }
        let mut sub = [0u64; MAX_LIMBS + 1];
        sub[..=n].copy_from_slice(&t[..=n]);
        let borrow = sub_assign(&mut sub[..=n], &self.m);
        cmov(mask64(borrow ^ 1), &mut t[..n], &sub[..n]);
        out.copy_from_slice(&t[..n]);
    }

    pub(crate) fn to_mont(&self, a: &[u64], out: &mut [u64]) {
        self.mul(a, &self.r2, out);
    }

    pub(crate) fn mont_to_plain(&self, a: &[u64], out: &mut [u64]) {
        let mut one = [0u64; MAX_LIMBS];
        one[0] = 1;
        self.mul(a, &one[..self.m.len()], out);
    }

    /// `R mod m`, the Montgomery form of 1.
    pub(crate) fn one(&self, out: &mut [u64]) {
        let mut one = [0u64; MAX_LIMBS];
        one[0] = 1;
        self.to_mont(&one[..self.m.len()], out);
    }

    /// `out = base^exp` in Montgomery form, with a fixed 4-bit window and a table read by
    /// scanning every entry: the time depends only on the length of `exp`.
    pub(crate) fn pow(&self, base: &[u64], exp: &[u64], out: &mut [u64]) {
        let n = self.m.len();
        let mut table = vec![0u64; 16 * n];
        self.one(&mut table[..n]);
        table[n..2 * n].copy_from_slice(base);
        for i in 2..16 {
            let (lo, hi) = table.split_at_mut(i * n);
            self.mul(&lo[(i - 1) * n..], base, &mut hi[..n]);
        }
        let mut acc = vec![0u64; n];
        self.one(&mut acc);
        let mut tmp = vec![0u64; n];
        let mut pick = vec![0u64; n];
        for i in (0..exp.len() * 16).rev() {
            for _ in 0..4 {
                self.mul(&acc, &acc, &mut tmp);
                acc.copy_from_slice(&tmp);
            }
            let w = (exp[i / 16] >> (4 * (i % 16))) & 0xf;
            pick.fill(0);
            for (j, entry) in table.chunks_exact(n).enumerate() {
                cmov(crate::ct::is_zero64(w ^ j as u64), &mut pick, entry);
            }
            self.mul(&acc, &pick, &mut tmp);
            acc.copy_from_slice(&tmp);
        }
        out.copy_from_slice(&acc);
    }

    /// `out = a⁻¹` in Montgomery form for a prime modulus (Fermat), in constant time.
    pub(crate) fn invert_prime(&self, a: &[u64], out: &mut [u64]) {
        let mut e = self.m.clone();
        sub_assign(&mut e, &[2]);
        self.pow(a, &e, out);
    }
}
