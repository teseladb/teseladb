//! Unsigned integers of any size, only as much as comparing a double with a decimal exactly and
//! writing a double's exact decimal digits need: build from a `u128`, multiply by powers of 2, 5
//! and 10, compare, and print in base 10.

use std::cmp::Ordering;

/// An unsigned integer: 32-bit limbs, least significant first, without high zero limbs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Big(Vec<u32>);

impl Big {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a limb is the low 32 bits of a product or quotient and a carry is below 2^32"
    )]
    pub(crate) fn from_u128(mut v: u128) -> Big {
        let mut limbs = Vec::new();
        while v != 0 {
            limbs.push(v as u32);
            v >>= 32;
        }
        Big(limbs)
    }

    /// Multiplies by `m`.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a limb is the low 32 bits of a product or quotient and a carry is below 2^32"
    )]
    fn mul_small(&mut self, m: u32) {
        let mut carry = 0u64;
        for limb in &mut self.0 {
            let p = u64::from(*limb) * u64::from(m) + carry;
            *limb = p as u32;
            carry = p >> 32;
        }
        if carry != 0 {
            self.0.push(carry as u32);
        }
    }

    /// Multiplies by `10^n`.
    pub(crate) fn mul_pow10(&mut self, mut n: u32) {
        while n >= 9 {
            self.mul_small(1_000_000_000);
            n -= 9;
        }
        self.mul_small(10u32.pow(n));
    }

    /// Multiplies by `5^n`.
    pub(crate) fn mul_pow5(&mut self, mut n: u32) {
        // 5^13 is the largest power of 5 below 2^32.
        while n >= 13 {
            self.mul_small(1_220_703_125);
            n -= 13;
        }
        self.mul_small(5u32.pow(n));
    }

    /// Its decimal digits, most significant first (`[0]` for zero).
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a quotient by 10^9 of a 64-bit value with a limb on top fits 32 bits, and so does a remainder"
    )]
    pub(crate) fn decimal_digits(&self) -> Vec<u8> {
        let mut limbs = self.0.clone();
        // Chunks of nine digits, least significant first.
        let mut chunks = Vec::new();
        while !limbs.is_empty() {
            let mut rem = 0u64;
            for limb in limbs.iter_mut().rev() {
                let cur = (rem << 32) | u64::from(*limb);
                *limb = (cur / 1_000_000_000) as u32;
                rem = cur % 1_000_000_000;
            }
            while limbs.last() == Some(&0) {
                limbs.pop();
            }
            chunks.push(rem as u32);
        }
        let mut digits = Vec::with_capacity(chunks.len() * 9);
        for (i, chunk) in chunks.iter().rev().enumerate() {
            let text = if i == 0 { chunk.to_string() } else { format!("{chunk:09}") };
            digits.extend(text.bytes().map(|b| b - b'0'));
        }
        if digits.is_empty() { vec![0] } else { digits }
    }

    /// Multiplies by `2^n`.
    pub(crate) fn shl(&mut self, n: u32) {
        if self.0.is_empty() {
            return;
        }
        let (limbs, bits) = ((n / 32) as usize, n % 32);
        if bits != 0 {
            let mut carry = 0u32;
            for limb in &mut self.0 {
                let next = *limb >> (32 - bits);
                *limb = (*limb << bits) | carry;
                carry = next;
            }
            if carry != 0 {
                self.0.push(carry);
            }
        }
        self.0.splice(0..0, std::iter::repeat_n(0, limbs));
    }
}

impl Ord for Big {
    fn cmp(&self, other: &Big) -> Ordering {
        self.0.len().cmp(&other.0.len()).then_with(|| self.0.iter().rev().cmp(other.0.iter().rev()))
    }
}

impl PartialOrd for Big {
    fn partial_cmp(&self, other: &Big) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn products_and_order_match_u128() {
        let mut a = Big::from_u128(3);
        a.shl(70);
        assert_eq!(a, Big::from_u128(3u128 << 70));
        let mut whole = Big::from_u128(5);
        whole.shl(64);
        assert_eq!(whole, Big::from_u128(5u128 << 64));
        let mut b = Big::from_u128(7);
        b.mul_pow10(20);
        assert_eq!(b, Big::from_u128(7 * 10u128.pow(20)));
        assert!(a > Big::from_u128(3u128 << 69) && a < Big::from_u128(4u128 << 70));
        let mut zero = Big::from_u128(0);
        zero.shl(5);
        zero.mul_pow10(3);
        assert_eq!(zero, Big::from_u128(0));
        // Past 128 bits: 2^200 against 10^60 (2^200 ≈ 1.6 × 10^60).
        let (mut p2, mut p10) = (Big::from_u128(1), Big::from_u128(1));
        p2.shl(200);
        p10.mul_pow10(60);
        assert!(p2 > p10);
        p10.mul_small(2);
        assert!(p2 < p10);
    }

    #[test]
    fn powers_of_five_and_decimal_digits() {
        let mut a = Big::from_u128(3);
        a.mul_pow5(30);
        assert_eq!(a, Big::from_u128(3 * 5u128.pow(30)));
        let text = (3 * 5u128.pow(30)).to_string();
        assert_eq!(a.decimal_digits(), text.bytes().map(|b| b - b'0').collect::<Vec<_>>());
        assert_eq!(Big::from_u128(0).decimal_digits(), [0]);
        assert_eq!(Big::from_u128(1_000_000_000).decimal_digits(), [1, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        // 2^200, past 128 bits.
        let mut p = Big::from_u128(1);
        p.shl(200);
        let expected = "1606938044258990275541962092341162602522202993782792835301376";
        assert_eq!(p.decimal_digits(), expected.bytes().map(|b| b - b'0').collect::<Vec<_>>());
    }
}
