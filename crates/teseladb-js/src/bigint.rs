//! `BigInt` values: integers of any size in sign and magnitude, with the arithmetic, the bitwise
//! operators in two's complement and the conversions ECMAScript defines for them.

use std::cmp::Ordering;

/// The most bits a `BigInt` holds, as SpiderMonkey (`BigInt::MaxBitLength`).
pub const MAX_BITS: u64 = 1 << 20;

/// An integer: sign and magnitude in 32-bit limbs, least significant first, without high zero
/// limbs; zero has no limbs and no sign.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BigInt {
    neg: bool,
    mag: Vec<u32>,
}

fn trim(v: &mut Vec<u32>) {
    while v.last() == Some(&0) {
        v.pop();
    }
}

fn cmp_mag(a: &[u32], b: &[u32]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

fn add_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    let mut out = Vec::with_capacity(long.len() + 1);
    let mut carry = 0u64;
    for (i, &x) in long.iter().enumerate() {
        let s = u64::from(x) + u64::from(short.get(i).copied().unwrap_or(0)) + carry;
        out.push(s as u32);
        carry = s >> 32;
    }
    if carry != 0 {
        out.push(carry as u32);
    }
    out
}

/// `a - b` with `a >= b`.
fn sub_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for (i, &x) in a.iter().enumerate() {
        let mut d = i64::from(x) - i64::from(b.get(i).copied().unwrap_or(0)) - borrow;
        borrow = 0;
        if d < 0 {
            d += 1 << 32;
            borrow = 1;
        }
        out.push(d as u32);
    }
    trim(&mut out);
    out
}

fn mul_mag(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut out = vec![0u32; a.len() + b.len()];
    for (i, &x) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, &y) in b.iter().enumerate() {
            let t = u64::from(x) * u64::from(y) + u64::from(out[i + j]) + carry;
            out[i + j] = t as u32;
            carry = t >> 32;
        }
        out[i + b.len()] = carry as u32;
    }
    trim(&mut out);
    out
}

/// Divides by a one-limb divisor: the quotient and the remainder.
fn divmod_small(a: &[u32], d: u32) -> (Vec<u32>, u32) {
    let mut q = vec![0u32; a.len()];
    let mut r = 0u64;
    for i in (0..a.len()).rev() {
        let cur = (r << 32) | u64::from(a[i]);
        q[i] = (cur / u64::from(d)) as u32;
        r = cur % u64::from(d);
    }
    trim(&mut q);
    (q, r as u32)
}

fn shl_mag(a: &[u32], bits: u32) -> Vec<u32> {
    let mut out = Vec::with_capacity(a.len() + 1);
    let mut carry = 0u32;
    for &x in a {
        out.push((x << bits) | carry);
        carry = if bits == 0 { 0 } else { x >> (32 - bits) };
    }
    out.push(carry);
    out
}

/// Knuth's algorithm D: the quotient and remainder of magnitudes, `b` not zero.
fn divmod_mag(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    if cmp_mag(a, b) == Ordering::Less {
        return (Vec::new(), a.to_vec());
    }
    if b.len() == 1 {
        let (q, r) = divmod_small(a, b[0]);
        let mut r = vec![r];
        trim(&mut r);
        return (q, r);
    }
    let s = b[b.len() - 1].leading_zeros();
    let v = {
        let mut v = shl_mag(b, s);
        v.pop();
        v
    };
    let mut u = shl_mag(a, s);
    let n = v.len();
    let m = u.len() - n - 1;
    let mut q = vec![0u32; m + 1];
    let base = 1u64 << 32;
    for j in (0..=m).rev() {
        let num = (u64::from(u[j + n]) << 32) | u64::from(u[j + n - 1]);
        let mut qhat = num / u64::from(v[n - 1]);
        let mut rhat = num % u64::from(v[n - 1]);
        while qhat >= base || qhat * u64::from(v[n - 2]) > ((rhat << 32) | u64::from(u[j + n - 2]))
        {
            qhat -= 1;
            rhat += u64::from(v[n - 1]);
            if rhat >= base {
                break;
            }
        }
        let mut borrow = 0i64;
        let mut carry = 0u64;
        for i in 0..n {
            let p = qhat * u64::from(v[i]) + carry;
            carry = p >> 32;
            let t = i64::from(u[i + j]) - borrow - i64::from(p as u32);
            u[i + j] = t as u32;
            borrow = i64::from(t < 0);
        }
        let t = i64::from(u[j + n]) - borrow - carry as i64;
        u[j + n] = t as u32;
        if t < 0 {
            qhat -= 1;
            let mut c = 0u64;
            for i in 0..n {
                let s2 = u64::from(u[i + j]) + u64::from(v[i]) + c;
                u[i + j] = s2 as u32;
                c = s2 >> 32;
            }
            u[j + n] = u[j + n].wrapping_add(c as u32);
        }
        q[j] = qhat as u32;
    }
    trim(&mut q);
    u.truncate(n);
    let mut r: Vec<u32> = if s == 0 {
        u
    } else {
        (0..n).map(|i| (u[i] >> s) | u.get(i + 1).map_or(0, |&hi| hi << (32 - s))).collect()
    };
    trim(&mut r);
    (q, r)
}

/// Two's complement limbs of `x` over `len` limbs.
fn twos(x: &BigInt, len: usize) -> Vec<u32> {
    let mut out = x.mag.clone();
    out.resize(len, 0);
    if x.neg {
        let mut carry = 1u64;
        for l in &mut out {
            let s = u64::from(!*l) + carry;
            *l = s as u32;
            carry = s >> 32;
        }
    }
    out
}

fn from_twos(mut v: Vec<u32>) -> BigInt {
    let neg = v.last().is_some_and(|&l| l & 0x8000_0000 != 0);
    if neg {
        let mut carry = 1u64;
        for l in &mut v {
            let s = u64::from(!*l) + carry;
            *l = s as u32;
            carry = s >> 32;
        }
    }
    BigInt::make(neg, v)
}

impl BigInt {
    fn make(neg: bool, mut mag: Vec<u32>) -> BigInt {
        trim(&mut mag);
        BigInt { neg: neg && !mag.is_empty(), mag }
    }

    pub fn from_i64(v: i64) -> BigInt {
        BigInt::make(v < 0, BigInt::from_u64(v.unsigned_abs()).mag)
    }

    pub fn from_u64(v: u64) -> BigInt {
        BigInt::make(false, vec![v as u32, (v >> 32) as u32])
    }

    /// The integer a finite integral double holds; `None` for other doubles.
    pub fn from_f64(d: f64) -> Option<BigInt> {
        if !d.is_finite() || d.trunc() != d {
            return None;
        }
        let bits = d.abs().to_bits();
        let exp = ((bits >> 52) & 0x7ff) as i64;
        if exp == 0 {
            return Some(BigInt::default());
        }
        let mant = (bits & ((1 << 52) - 1)) | (1 << 52);
        let shift = exp - 1075;
        let m = BigInt::from_u64(mant);
        let r = if shift >= 0 { m.shl_bits(shift as u64) } else { m.shr_bits((-shift) as u64) };
        Some(if d < 0.0 { r.neg() } else { r })
    }

    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    pub fn is_neg(&self) -> bool {
        self.neg
    }

    /// How many bits the magnitude takes.
    pub fn bit_len(&self) -> u64 {
        match self.mag.last() {
            None => 0,
            Some(&top) => self.mag.len() as u64 * 32 - u64::from(top.leading_zeros()),
        }
    }

    /// The nearest double, ties to even.
    pub fn to_f64(&self) -> f64 {
        let bits = self.bit_len();
        let v = if bits <= 64 {
            self.low_u64() as f64
        } else {
            let shift = bits - 64;
            let top = self.abs().shr_bits(shift);
            let sticky = self.abs() != top.shl_bits(shift);
            let m = top.low_u64() | u64::from(sticky);
            let s = i32::try_from(shift).unwrap_or(i32::MAX);
            if s > 1100 { f64::INFINITY } else { (m as f64) * 2f64.powi(s) }
        };
        if self.neg { -v } else { v }
    }

    fn low_u64(&self) -> u64 {
        u64::from(self.mag.first().copied().unwrap_or(0))
            | (u64::from(self.mag.get(1).copied().unwrap_or(0)) << 32)
    }

    /// The low 64 bits in two's complement.
    pub fn as_i64_wrapping(&self) -> i64 {
        let v = self.low_u64();
        if self.neg { (v as i64).wrapping_neg() } else { v as i64 }
    }

    pub fn abs(&self) -> BigInt {
        BigInt { neg: false, mag: self.mag.clone() }
    }

    pub fn neg(&self) -> BigInt {
        BigInt::make(!self.neg, self.mag.clone())
    }

    pub fn add(&self, o: &BigInt) -> BigInt {
        if self.neg == o.neg {
            return BigInt::make(self.neg, add_mag(&self.mag, &o.mag));
        }
        match cmp_mag(&self.mag, &o.mag) {
            Ordering::Less => BigInt::make(o.neg, sub_mag(&o.mag, &self.mag)),
            _ => BigInt::make(self.neg, sub_mag(&self.mag, &o.mag)),
        }
    }

    pub fn sub(&self, o: &BigInt) -> BigInt {
        self.add(&o.neg())
    }

    pub fn mul(&self, o: &BigInt) -> BigInt {
        BigInt::make(self.neg != o.neg, mul_mag(&self.mag, &o.mag))
    }

    /// The quotient truncated toward zero and the remainder with the dividend's sign; `None`
    /// when `o` is zero.
    pub fn div_rem(&self, o: &BigInt) -> Option<(BigInt, BigInt)> {
        if o.is_zero() {
            return None;
        }
        let (q, r) = divmod_mag(&self.mag, &o.mag);
        Some((BigInt::make(self.neg != o.neg, q), BigInt::make(self.neg, r)))
    }

    /// `self ** e` for `e >= 0`, `None` past [`MAX_BITS`].
    pub fn pow(&self, mut e: u64) -> Option<BigInt> {
        let mut base = self.clone();
        let mut out = BigInt::from_i64(1);
        if e > 0 && base.bit_len().saturating_sub(1).saturating_mul(e) > MAX_BITS {
            return None;
        }
        while e > 0 {
            if e & 1 == 1 {
                out = out.mul(&base);
            }
            e >>= 1;
            if e > 0 {
                base = base.mul(&base);
            }
        }
        Some(out)
    }

    pub fn shl_bits(&self, n: u64) -> BigInt {
        if self.is_zero() {
            return self.clone();
        }
        let limbs = (n / 32) as usize;
        let mut mag = vec![0u32; limbs];
        mag.extend(shl_mag(&self.mag, (n % 32) as u32));
        BigInt::make(self.neg, mag)
    }

    /// Shifts right rounding toward negative infinity, as `>>` does.
    pub fn shr_bits(&self, n: u64) -> BigInt {
        let limbs = usize::try_from(n / 32).unwrap_or(usize::MAX);
        let bits = (n % 32) as u32;
        let src = self.mag.get(limbs..).unwrap_or(&[]);
        let mut mag: Vec<u32> = (0..src.len())
            .map(|i| {
                let hi =
                    if bits == 0 { 0 } else { src.get(i + 1).map_or(0, |&h| h << (32 - bits)) };
                (src[i] >> bits) | hi
            })
            .collect();
        trim(&mut mag);
        let r = BigInt::make(self.neg, mag);
        if self.neg && r.abs().shl_bits(n) != self.abs() { r.sub(&BigInt::from_i64(1)) } else { r }
    }

    /// `<<` with a signed count, `None` past [`MAX_BITS`].
    pub fn shift_left(&self, n: &BigInt) -> Option<BigInt> {
        let count = n.abs();
        if n.neg {
            if count.bit_len() > 63 {
                return Some(if self.neg { BigInt::from_i64(-1) } else { BigInt::default() });
            }
            return Some(self.shr_bits(count.low_u64()));
        }
        if self.is_zero() {
            return Some(self.clone());
        }
        if count.bit_len() > 63 || self.bit_len() + count.low_u64() > MAX_BITS {
            return None;
        }
        Some(self.shl_bits(count.low_u64()))
    }

    fn bitwise(&self, o: &BigInt, f: fn(u32, u32) -> u32) -> BigInt {
        let len = self.mag.len().max(o.mag.len()) + 1;
        let (a, b) = (twos(self, len), twos(o, len));
        from_twos(a.iter().zip(&b).map(|(x, y)| f(*x, *y)).collect())
    }

    pub fn and(&self, o: &BigInt) -> BigInt {
        self.bitwise(o, |x, y| x & y)
    }

    pub fn or(&self, o: &BigInt) -> BigInt {
        self.bitwise(o, |x, y| x | y)
    }

    pub fn xor(&self, o: &BigInt) -> BigInt {
        self.bitwise(o, |x, y| x ^ y)
    }

    /// `~x`, that is `-x - 1`.
    pub fn not(&self) -> BigInt {
        self.neg().sub(&BigInt::from_i64(1))
    }

    /// `BigInt.asUintN(bits, self)`.
    pub fn as_uint_n(&self, bits: u64) -> BigInt {
        if bits == 0 {
            return BigInt::default();
        }
        if !self.neg && self.bit_len() <= bits {
            return self.clone();
        }
        let len = usize::try_from(bits.div_ceil(32)).unwrap_or(usize::MAX).max(self.mag.len()) + 1;
        let mut v = twos(self, len);
        v.truncate(usize::try_from(bits.div_ceil(32)).unwrap_or(usize::MAX));
        if !bits.is_multiple_of(32)
            && let Some(top) = v.last_mut()
        {
            *top &= (1u32 << (bits % 32)) - 1;
        }
        BigInt::make(false, v)
    }

    /// `BigInt.asIntN(bits, self)`.
    pub fn as_int_n(&self, bits: u64) -> BigInt {
        if bits == 0 {
            return BigInt::default();
        }
        let u = self.as_uint_n(bits);
        if u.bit_len() == bits { u.sub(&BigInt::from_i64(1).shl_bits(bits)) } else { u }
    }

    pub fn cmp(&self, o: &BigInt) -> Ordering {
        match (self.neg, o.neg) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => cmp_mag(&self.mag, &o.mag),
            (true, true) => cmp_mag(&o.mag, &self.mag),
        }
    }

    /// Compares with a double; `None` when it is NaN.
    pub fn cmp_f64(&self, d: f64) -> Option<Ordering> {
        if d.is_nan() {
            return None;
        }
        if d.is_infinite() {
            return Some(if d > 0.0 { Ordering::Less } else { Ordering::Greater });
        }
        let t = d.trunc();
        let i = BigInt::from_f64(t).unwrap_or_default();
        Some(self.cmp(&i).then(if d > t {
            Ordering::Less
        } else if d < t {
            Ordering::Greater
        } else {
            Ordering::Equal
        }))
    }

    /// The digits in `radix` (2 to 36).
    pub fn to_string_radix(&self, radix: u32) -> String {
        if self.is_zero() {
            return "0".into();
        }
        let mut digits = Vec::new();
        let mut cur = self.mag.clone();
        while !cur.is_empty() {
            let (q, r) = divmod_small(&cur, radix);
            digits.push(char::from_digit(r, radix).unwrap_or('0'));
            cur = q;
        }
        if self.neg {
            digits.push('-');
        }
        digits.iter().rev().collect()
    }

    /// Reads digits in `radix` with an optional sign (`allow_sign`) and `_` not allowed.
    pub fn parse_radix(s: &str, radix: u32) -> Option<BigInt> {
        if s.is_empty() {
            return None;
        }
        let mut mag: Vec<u32> = Vec::new();
        for c in s.chars() {
            let d = c.to_digit(radix)?;
            let mut carry = u64::from(d);
            for l in &mut mag {
                let t = u64::from(*l) * u64::from(radix) + carry;
                *l = t as u32;
                carry = t >> 32;
            }
            if carry != 0 {
                mag.push(carry as u32);
            }
        }
        Some(BigInt::make(false, mag))
    }

    /// `StringToBigInt`: whitespace around, an optional sign for decimals, or a `0x`, `0o` or
    /// `0b` prefix without a sign; an empty string is zero.
    pub fn parse(s: &str) -> Option<BigInt> {
        let t = s.trim_matches(crate::number::is_space);
        if t.is_empty() {
            return Some(BigInt::default());
        }
        for (p, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
            if let Some(rest) = t.strip_prefix(p) {
                return BigInt::parse_radix(rest, radix);
            }
        }
        let (neg, digits) = match t.as_bytes()[0] {
            b'-' => (true, &t[1..]),
            b'+' => (false, &t[1..]),
            _ => (false, t),
        };
        BigInt::parse_radix(digits, 10).map(|b| if neg { b.neg() } else { b })
    }
}

impl std::fmt::Display for BigInt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_string_radix(10))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(s: &str) -> BigInt {
        BigInt::parse(s).unwrap()
    }

    #[test]
    fn arithmetic_matches_known_values() {
        let x = b("123456789012345678901234567890");
        let y = b("987654321098765432109876543210");
        assert_eq!(
            x.mul(&y).to_string(),
            "121932631137021795226185032733622923332237463801111263526900"
        );
        let big = BigInt::from_i64(10).pow(40).unwrap();
        let (q, r) = big.div_rem(&BigInt::from_i64(7)).unwrap();
        assert_eq!(q.to_string(), "1428571428571428571428571428571428571428");
        assert_eq!(r.to_string(), "4");
        let (q, r) = b("-5").div_rem(&b("2")).unwrap();
        assert_eq!((q.to_string(), r.to_string()), ("-2".into(), "-1".into()));
        let (q, r) = x.mul(&y).add(&b("17")).div_rem(&y).unwrap();
        assert_eq!((q, r), (x.clone(), b("17")));
        assert!(b("1").div_rem(&BigInt::default()).is_none());
        assert_eq!(y.sub(&x).add(&x), y);
        assert_eq!(x.sub(&y).to_string(), "-864197532086419753208641975320");
    }

    #[test]
    fn division_by_many_limbs_corrects_the_estimate() {
        let d = b("0xffffffff00000001");
        let n = d.mul(&b("0xfffffffffffffffffffffffe")).add(&b("0xffffffff00000000"));
        let (q, r) = n.div_rem(&d).unwrap();
        assert_eq!(q, b("0xfffffffffffffffffffffffe"));
        assert_eq!(r, b("0xffffffff00000000"));
    }

    #[test]
    fn bitwise_operators_use_twos_complement() {
        assert_eq!(b("5").or(&b("-3")).to_string(), "-3");
        assert_eq!(b("5").xor(&b("-3")).to_string(), "-8");
        assert_eq!(b("-5").and(&b("3")).to_string(), "3");
        assert_eq!(b("0").not().to_string(), "-1");
        assert_eq!(b("-5").shr_bits(1).to_string(), "-3");
        assert_eq!(b("-1").shift_left(&b("-1")).unwrap().to_string(), "-1");
        assert_eq!(b("8").shift_left(&b("-2")).unwrap().to_string(), "2");
        assert_eq!(b("1").shift_left(&b("70")).unwrap().to_string(), "1180591620717411303424");
        assert!(b("1").shift_left(&b("2000000")).is_none());
        assert_eq!(b("5").shift_left(&b("-99999999999999999999")).unwrap(), BigInt::default());
        assert_eq!(b("-5").shift_left(&b("-99999999999999999999")).unwrap().to_string(), "-1");
    }

    #[test]
    fn as_int_n_and_as_uint_n_wrap() {
        assert_eq!(b("257").as_uint_n(8).to_string(), "1");
        assert_eq!(b("255").as_int_n(8).to_string(), "-1");
        assert_eq!(b("-1").as_uint_n(64).to_string(), "18446744073709551615");
        assert_eq!(b("9223372036854775808").as_int_n(64).to_string(), "-9223372036854775808");
        assert_eq!(b("-1").as_uint_n(3).to_string(), "7");
        assert_eq!(b("4").as_int_n(3).to_string(), "-4");
        assert_eq!(b("5").as_int_n(0).to_string(), "0");
        assert_eq!(b("1").as_int_n(1).to_string(), "-1");
    }

    #[test]
    fn conversions_with_doubles_round_to_even() {
        assert_eq!(BigInt::from_i64(2).pow(53).unwrap().add(&b("1")).to_f64(), 9007199254740992.0);
        assert_eq!(BigInt::from_i64(2).pow(1024).unwrap().to_f64(), f64::INFINITY);
        assert_eq!(b("-12").to_f64(), -12.0);
        assert_eq!(BigInt::from_f64(1e21).unwrap().to_string(), "1000000000000000000000");
        assert_eq!(BigInt::from_f64(-0.0).unwrap(), BigInt::default());
        assert_eq!(BigInt::from_f64(-3.0).unwrap().to_string(), "-3");
        assert!(BigInt::from_f64(1.5).is_none());
        assert!(BigInt::from_f64(f64::NAN).is_none());
        assert_eq!(b("10").cmp_f64(9.5), Some(Ordering::Greater));
        assert_eq!(b("1").cmp_f64(1.5), Some(Ordering::Less));
        assert_eq!(b("-1").cmp_f64(-1.5), Some(Ordering::Greater));
        assert_eq!(b("1").cmp_f64(f64::INFINITY), Some(Ordering::Less));
        assert_eq!(b("1").cmp_f64(f64::NEG_INFINITY), Some(Ordering::Greater));
        assert_eq!(b("1").cmp_f64(f64::NAN), None);
        assert_eq!(b("3").cmp_f64(3.0), Some(Ordering::Equal));
        assert_eq!(b("-3").cmp(&b("2")), Ordering::Less);
        assert_eq!(b("-3").cmp(&b("-2")), Ordering::Less);
        assert_eq!(b("-1").as_i64_wrapping(), -1);
    }

    #[test]
    fn strings_parse_and_print_in_every_radix() {
        assert_eq!(b("0x1f").to_string(), "31");
        assert_eq!(b("0o17").to_string(), "15");
        assert_eq!(b("0b101").to_string(), "5");
        assert_eq!(b(" -12 ").to_string(), "-12");
        assert_eq!(b("+7").to_string(), "7");
        assert_eq!(b("").to_string(), "0");
        assert_eq!(b("123").to_string_radix(36), "3f");
        assert_eq!(b("-255").to_string_radix(16), "-ff");
        for bad in ["-0x1", "1e3", "1.0", "12n", "0x", "- 1"] {
            assert!(BigInt::parse(bad).is_none(), "{bad}");
        }
        assert!(BigInt::from_i64(10).pow(1 << 20).is_none());
        assert_eq!(BigInt::from_i64(0).pow(0).unwrap().to_string(), "1");
    }
}
