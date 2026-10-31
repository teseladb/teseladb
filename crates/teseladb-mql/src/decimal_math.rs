//! The decimal functions of the expressions of 010 (`$sqrt`, `$exp`, `$ln`, `$log10`, `$log`,
//! `$pow`) to 34 digits, rounded half to even, as RefDb's decimal library gives them (measured
//! on a `refrouter` 9.0.4: `crates/teseladb-exec/tests/expr_core.rs`).
//!
//! The square root is exact (an integer square root of the coefficient, scaled); the other
//! functions work in fixed point with [`PRECISION`] decimal digits after the point, far more than
//! the 34 kept, over a small unsigned integer of base 10⁹. An exact result keeps RefDb's short
//! form (`$sqrt` of 4 is 2, `$log10` of 1000 is 3, `$exp` of 0 is 1, `$ln` of 1 is 0); `$pow`
//! always gives 34 digits, as RefDb does (`2^10` is `1024.000000000000000000000000000000`).

use std::cmp::Ordering;

use teseladb_bson::{Decimal, Decimal128};

use crate::arith::{DecOp, dec_binary};

/// Digits after the point of the fixed-point values.
pub(crate) const PRECISION: usize = 90;
/// One limb's base.
const BASE: u64 = 1_000_000_000;

/// An unsigned integer: limbs of base 10⁹, least significant first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Big(Vec<u32>);

impl Big {
    pub(crate) fn zero() -> Big {
        Big(Vec::new())
    }

    pub(crate) fn from_u128(mut v: u128) -> Big {
        let mut l = Vec::new();
        while v > 0 {
            l.push((v % u128::from(BASE)) as u32);
            v /= u128::from(BASE);
        }
        Big(l)
    }

    fn trim(mut self) -> Big {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
        self
    }

    pub(crate) fn is_zero(&self) -> bool {
        self.0.iter().all(|x| *x == 0)
    }

    pub(crate) fn cmp(&self, o: &Big) -> Ordering {
        let (a, b) = (self.clone().trim(), o.clone().trim());
        a.0.len().cmp(&b.0.len()).then_with(|| a.0.iter().rev().cmp(b.0.iter().rev()))
    }

    pub(crate) fn add(&self, o: &Big) -> Big {
        let mut out = Vec::with_capacity(self.0.len().max(o.0.len()) + 1);
        let mut carry = 0u64;
        for i in 0..self.0.len().max(o.0.len()) {
            let s = u64::from(*self.0.get(i).unwrap_or(&0))
                + u64::from(*o.0.get(i).unwrap_or(&0))
                + carry;
            out.push((s % BASE) as u32);
            carry = s / BASE;
        }
        if carry > 0 {
            out.push(carry as u32);
        }
        Big(out).trim()
    }

    /// `self − o` with `self ≥ o`.
    pub(crate) fn sub(&self, o: &Big) -> Big {
        let mut out = Vec::with_capacity(self.0.len());
        let mut borrow = 0i64;
        for i in 0..self.0.len() {
            let mut d = i64::from(self.0[i]) - i64::from(*o.0.get(i).unwrap_or(&0)) - borrow;
            borrow = 0;
            if d < 0 {
                d += BASE as i64;
                borrow = 1;
            }
            out.push(d as u32);
        }
        Big(out).trim()
    }

    pub(crate) fn mul(&self, o: &Big) -> Big {
        if self.is_zero() || o.is_zero() {
            return Big::zero();
        }
        let mut acc = vec![0u64; self.0.len() + o.0.len() + 1];
        for (i, x) in self.0.iter().enumerate() {
            let mut carry = 0u64;
            for (j, y) in o.0.iter().enumerate() {
                let t = acc[i + j] + u64::from(*x) * u64::from(*y) + carry;
                acc[i + j] = t % BASE;
                carry = t / BASE;
            }
            let mut k = i + o.0.len();
            while carry > 0 {
                let t = acc[k] + carry;
                acc[k] = t % BASE;
                carry = t / BASE;
                k += 1;
            }
        }
        Big(acc.into_iter().map(|x| x as u32).collect()).trim()
    }

    /// `self / d` for a small divisor, in one pass over the limbs.
    pub(crate) fn div_u32(&self, d: u32) -> Big {
        let mut out = vec![0u32; self.0.len()];
        let mut rem = 0u64;
        for i in (0..self.0.len()).rev() {
            let cur = rem * BASE + u64::from(self.0[i]);
            out[i] = (cur / u64::from(d)) as u32;
            rem = cur % u64::from(d);
        }
        Big(out).trim()
    }

    pub(crate) fn mul_small(&self, m: u32) -> Big {
        self.mul(&Big::from_u128(u128::from(m)))
    }

    /// `self × 10ⁿ`.
    pub(crate) fn shl10(&self, n: usize) -> Big {
        let mut l = vec![0u32; n / 9];
        l.extend_from_slice(&self.0);
        Big(l).mul(&Big::from_u128(10u128.pow((n % 9) as u32)))
    }

    /// `(self / o, self % o)`, by schoolbook long division over decimal digits.
    pub(crate) fn divmod(&self, o: &Big) -> (Big, Big) {
        let digits = self.digits();
        let mut q = Vec::with_capacity(digits.len());
        let mut r = Big::zero();
        for d in digits {
            r = r.mul_small(10).add(&Big::from_u128(u128::from(d)));
            let mut k = 0u8;
            while r.cmp(o) != Ordering::Less {
                r = r.sub(o);
                k += 1;
            }
            q.push(k);
        }
        (Big::from_digits(&q), r)
    }

    pub(crate) fn digits(&self) -> Vec<u8> {
        let t = self.clone().trim();
        let Some((last, rest)) = t.0.split_last() else { return vec![0] };
        let mut s = last.to_string();
        for l in rest.iter().rev() {
            s.push_str(&format!("{l:09}"));
        }
        s.bytes().map(|b| b - b'0').collect()
    }

    pub(crate) fn from_digits(d: &[u8]) -> Big {
        let mut l = Vec::new();
        let mut end = d.len();
        while end > 0 {
            let start = end.saturating_sub(9);
            let v = d[start..end].iter().fold(0u32, |acc, x| acc * 10 + u32::from(*x));
            l.push(v);
            end = start;
        }
        Big(l).trim()
    }

    /// `⌊√self⌋`, by Newton's iteration from above.
    pub(crate) fn isqrt(&self) -> Big {
        if self.is_zero() {
            return Big::zero();
        }
        let n = self.digits().len();
        let mut x = Big::from_u128(1).shl10(n / 2 + 1);
        loop {
            let y = x.add(&self.divmod(&x).0).divmod(&Big::from_u128(2)).0;
            if y.cmp(&x) != Ordering::Less {
                return x;
            }
            x = y;
        }
    }
}

/// A signed fixed-point number: `±magnitude × 10^−PRECISION`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fixed {
    pub(crate) neg: bool,
    pub(crate) mag: Big,
}

pub(crate) fn one() -> Big {
    Big::from_u128(1).shl10(PRECISION)
}

impl Fixed {
    pub(crate) fn new(neg: bool, mag: Big) -> Fixed {
        let zero = mag.is_zero();
        Fixed { neg: neg && !zero, mag }
    }

    pub(crate) fn add(&self, o: &Fixed) -> Fixed {
        if self.neg == o.neg {
            return Fixed::new(self.neg, self.mag.add(&o.mag));
        }
        match self.mag.cmp(&o.mag) {
            Ordering::Less => Fixed::new(o.neg, o.mag.sub(&self.mag)),
            _ => Fixed::new(self.neg, self.mag.sub(&o.mag)),
        }
    }

    pub(crate) fn neg(&self) -> Fixed {
        Fixed::new(!self.neg, self.mag.clone())
    }

    pub(crate) fn mul(&self, o: &Fixed) -> Fixed {
        Fixed::new(self.neg != o.neg, self.mag.mul(&o.mag).divmod(&one()).0)
    }

    pub(crate) fn div(&self, o: &Fixed) -> Fixed {
        Fixed::new(self.neg != o.neg, self.mag.shl10(PRECISION).divmod(&o.mag).0)
    }

    pub(crate) fn div_small(&self, n: u32) -> Fixed {
        Fixed::new(self.neg, self.mag.divmod(&Big::from_u128(u128::from(n))).0)
    }

    pub(crate) fn sqrt(&self) -> Fixed {
        Fixed::new(false, self.mag.shl10(PRECISION).isqrt())
    }
}

/// A finite decimal as `(negative, coefficient, exponent)`; `None` for NaN or an infinity.
pub(crate) fn parts(d: Decimal128) -> Option<(bool, u128, i32)> {
    match d.classify() {
        Decimal::Finite { negative, coefficient, exponent } => {
            Some((negative, coefficient, exponent))
        }
        Decimal::NaN | Decimal::Infinity { .. } => None,
    }
}

/// `c × 10^e` as a fixed-point number, with `c × 10^e = m × 10^q`, `1 ≤ m < 10`: `(m, q)`.
pub(crate) fn normalized(c: u128, e: i32) -> (Fixed, i64) {
    let digits = Big::from_u128(c).digits();
    let q = i64::from(e) + digits.len() as i64 - 1;
    // m = c / 10^(len−1), as fixed point.
    let mut mag = Big::from_digits(&digits);
    let len = digits.len();
    mag = if len - 1 <= PRECISION {
        mag.shl10(PRECISION - (len - 1))
    } else {
        mag.divmod(&Big::from_u128(1).shl10(len - 1 - PRECISION)).0
    };
    (Fixed::new(false, mag), q)
}

/// `ln m` for `m > 0` in fixed point: square roots bring it near 1, then `2·atanh((m−1)/(m+1))`.
pub(crate) fn ln_fixed(m: &Fixed) -> Fixed {
    let mut x = m.clone();
    let mut halvings = 0u32;
    let near = Fixed::new(false, Big::from_u128(1).shl10(PRECISION - 3));
    while x.add(&Fixed::new(false, one()).neg()).mag.cmp(&near.mag) == Ordering::Greater {
        x = x.sqrt();
        halvings += 1;
    }
    let one_f = Fixed::new(false, one());
    let z = x.add(&one_f.neg()).div(&x.add(&one_f));
    let z2 = z.mul(&z);
    let mut term = z.clone();
    let mut sum = z.clone();
    let mut k = 1u32;
    while !term.mag.is_zero() {
        term = term.mul(&z2);
        k += 2;
        sum = sum.add(&term.div_small(k));
    }
    let mut out = sum.add(&sum);
    for _ in 0..halvings {
        out = out.add(&out);
    }
    out
}

pub(crate) fn ln10() -> Fixed {
    ln_fixed(&Fixed::new(false, Big::from_u128(10).shl10(PRECISION)))
}

/// `ln(c × 10^e)` in fixed point.
pub(crate) fn ln_of(c: u128, e: i32) -> Fixed {
    let (m, q) = normalized(c, e);
    let tens = Fixed::new(q < 0, ln10().mag.mul(&Big::from_u128(u128::from(q.unsigned_abs()))));
    ln_fixed(&m).add(&tens)
}

/// `exp x` as `(m, n)` with `exp x = m × 10^n` and `m` in fixed point.
pub(crate) fn exp_fixed(x: &Fixed) -> (Fixed, i64) {
    let l10 = ln10();
    // n = round(x / ln 10), r = x − n·ln 10.
    let ratio = x.div(&l10);
    let (n_big, rem) = ratio.mag.divmod(&one());
    let mut n = n_big
        .digits()
        .iter()
        .fold(0i64, |acc, d| acc.saturating_mul(10).saturating_add(i64::from(*d)));
    if rem.cmp(&one().divmod(&Big::from_u128(2)).0) == Ordering::Greater {
        n += 1;
    }
    let n = if ratio.neg { -n } else { n };
    let r = x.add(&Fixed::new(n > 0, l10.mag.mul(&Big::from_u128(u128::from(n.unsigned_abs())))));
    // exp r = (exp(r / 2^k))^(2^k), Taylor series near 0.
    let k = 16u32;
    let mut y = r.clone();
    for _ in 0..k {
        y = y.div_small(2);
    }
    let one_f = Fixed::new(false, one());
    let mut term = one_f.clone();
    let mut sum = one_f;
    let mut i = 1u32;
    while !term.mag.is_zero() {
        term = term.mul(&y).div_small(i);
        sum = sum.add(&term);
        i += 1;
    }
    for _ in 0..k {
        sum = sum.mul(&sum);
    }
    (sum, n)
}

/// A decimal of fixed-point value `f × 10^n` rounded to 34 digits, keeping them all (`$pow`'s
/// and every inexact result's form).
pub(crate) fn to_decimal(f: &Fixed, n: i64) -> Decimal128 {
    let digits = f.mag.digits();
    let exp = n - PRECISION as i64;
    let exp = exp.clamp(i64::from(i32::MIN / 2), i64::from(i32::MAX / 2)) as i32;
    crate::arith::encode_digits(f.neg, digits, exp)
}

/// The value as `Decimal128`, an exact integer `k` as `k` itself.
pub(crate) fn integer_decimal(k: i64) -> Decimal128 {
    Decimal128::from_parts(k < 0, u128::from(k.unsigned_abs()), 0).unwrap_or_else(Decimal128::nan)
}

/// `√d`; `None` for a negative `d` (the caller's error).
pub fn sqrt(d: Decimal128) -> Option<Decimal128> {
    let Some((neg, c, e)) = parts(d) else {
        return Some(if d.to_string() == "-Infinity" { Decimal128::nan() } else { d });
    };
    if c == 0 {
        return Some(Decimal128::from_parts(false, 0, e.div_euclid(2)).unwrap_or(d));
    }
    if neg {
        return None;
    }
    // c·10^m with (e − m) even and at least 70 digits: the root has at least 35.
    let len = Big::from_u128(c).digits().len() as i64;
    let mut m = (70 - len).max(0);
    if (i64::from(e) - m).rem_euclid(2) != 0 {
        m += 1;
    }
    let scaled = Big::from_u128(c).shl10(m as usize);
    let root = scaled.isqrt();
    let mut exp = (i64::from(e) - m) / 2;
    if root.mul(&root) == scaled.clone().trim() {
        // Exact: the shortest form down to RefDb's preferred exponent ⌊e/2⌋.
        let preferred = i64::from(e).div_euclid(2);
        let mut digits = root.digits();
        while exp < preferred && digits.len() > 1 && digits.last() == Some(&0) {
            digits.pop();
            exp += 1;
        }
        let exp = i32::try_from(exp).unwrap_or(0);
        return Some(crate::arith::encode_digits(false, digits, exp));
    }
    // Inexact: a sticky digit decides the rounding.
    let mut digits = root.digits();
    digits.push(1);
    let exp = i32::try_from(exp - 1).unwrap_or(0);
    Some(crate::arith::encode_digits(false, digits, exp))
}

/// `e^d`.
pub fn exp(d: Decimal128) -> Decimal128 {
    let Some((neg, c, e)) = parts(d) else {
        return match d.to_string().as_str() {
            "-Infinity" => integer_decimal(0),
            _ => d,
        };
    };
    if c == 0 {
        return integer_decimal(1);
    }
    let (m, q) = normalized(c, e);
    // A magnitude past 10^5 overflows or underflows any decimal.
    if q > 5 {
        return if neg {
            Decimal128::from_parts(false, 0, -6176).unwrap_or_else(Decimal128::nan)
        } else {
            Decimal128::infinity(false)
        };
    }
    let mut x = m;
    for _ in 0..q.max(0) {
        x = Fixed::new(false, x.mag.mul_small(10));
    }
    for _ in 0..(-q).max(0) {
        x = x.div_small(10);
    }
    let (r, n) = exp_fixed(&Fixed::new(neg, x.mag));
    to_decimal(&r, n)
}

/// `ln d` for `d > 0`; `None` otherwise (the caller's error).
pub fn ln(d: Decimal128) -> Option<Decimal128> {
    let Some((neg, c, e)) = parts(d) else {
        return (d.to_string() != "-Infinity").then_some(d);
    };
    if neg || c == 0 {
        return None;
    }
    if is_one(c, e) {
        return Some(integer_decimal(0));
    }
    Some(to_decimal(&ln_of(c, e), 0))
}

/// Whether `c × 10^e` is 1.
fn is_one(c: u128, e: i32) -> bool {
    power_of_ten(c, e) == Some(0)
}

/// `k` when `c × 10^e = 10^k`.
pub(crate) fn power_of_ten(c: u128, e: i32) -> Option<i64> {
    let digits = Big::from_u128(c).digits();
    (digits[0] == 1 && digits[1..].iter().all(|x| *x == 0))
        .then(|| i64::from(e) + digits.len() as i64 - 1)
}

/// `log₁₀ d` for `d > 0`; an exact power of ten gives its exponent.
pub fn log10(d: Decimal128) -> Option<Decimal128> {
    let Some((neg, c, e)) = parts(d) else {
        return (d.to_string() != "-Infinity").then_some(d);
    };
    if neg || c == 0 {
        return None;
    }
    if let Some(k) = power_of_ten(c, e) {
        return Some(integer_decimal(k));
    }
    Some(to_decimal(&ln_of(c, e).div(&ln10()), 0))
}

/// `log_b d` correctly rounded (`log(8, 2)` is `3.000000000000000000000000000000000`); for two
/// powers of ten, the quotient of their exponents (`log(100, 10)` is 2), as RefDb writes them.
pub fn log(d: Decimal128, b: Decimal128) -> Option<Decimal128> {
    let (Some((dn, dc, de)), Some((bn, bc, be))) = (parts(d), parts(b)) else {
        // NaN and infinities: as their logarithms divide.
        let (x, y) = (ln(d)?, ln(b)?);
        return dec_binary(
            DecOp::Div,
            &teseladb_bson::Value::Decimal128(x),
            &teseladb_bson::Value::Decimal128(y),
        );
    };
    if dn || bn || dc == 0 || bc == 0 {
        return None;
    }
    // Powers of ten divide their exponents exactly, as RefDb writes `log(100, 10)` as 2.
    if let (Some(k), Some(j)) = (power_of_ten(dc, de), power_of_ten(bc, be)) {
        return dec_binary(
            DecOp::Div,
            &teseladb_bson::Value::Decimal128(integer_decimal(k)),
            &teseladb_bson::Value::Decimal128(integer_decimal(j)),
        );
    }
    // Otherwise the quotient of the logarithms, rounded once.
    Some(to_decimal(&ln_of(dc, de).div(&ln_of(bc, be)), 0))
}

/// `b^x`; NaN for a negative base and an exponent that is not a whole number.
pub fn pow(b: Decimal128, x: Decimal128) -> Decimal128 {
    let (Some((bn, bc, be)), Some((xn, xc, xe))) = (parts(b), parts(x)) else {
        return Decimal128::nan();
    };
    let whole =
        xe >= 0 || Big::from_u128(xc).digits().iter().rev().take((-xe) as usize).all(|d| *d == 0);
    if bc == 0 {
        return if xn { Decimal128::infinity(false) } else { integer_decimal(0) };
    }
    if bn && !whole {
        return Decimal128::nan();
    }
    let odd = whole && {
        let digits = Big::from_u128(xc).digits();
        let units = if xe >= 0 {
            if xe == 0 { digits.last().copied().unwrap_or(0) } else { 0 }
        } else {
            let keep = digits.len().saturating_sub((-xe) as usize);
            if keep == 0 { 0 } else { digits[keep - 1] }
        };
        units % 2 == 1
    };
    // b^x = exp(x · ln |b|).
    let lnb = ln_of(bc, be);
    let (xm, xq) = normalized(xc, xe);
    let mut xf = xm;
    for _ in 0..xq.max(0) {
        xf = Fixed::new(false, xf.mag.mul_small(10));
    }
    for _ in 0..(-xq).max(0) {
        xf = xf.div_small(10);
    }
    let power = Fixed::new(xn, xf.mag).mul(&lnb);
    let (m, n) = exp_fixed(&power);
    to_decimal(&Fixed::new(bn && odd, m.mag), n)
}

/// The exact value of double `x` as a decimal, rounded to 34 digits half to even, in its
/// shortest form (`1.0` is `1`, `0.1` is `0.1000000000000000055511151231257827`): how RefDb's
/// `$sum` and `$avg` take a double into a decimal total.
pub fn from_f64_exact(x: f64) -> Decimal128 {
    if x.is_nan() {
        return Decimal128::nan();
    }
    if x.is_infinite() {
        return Decimal128::infinity(x < 0.0);
    }
    let text = format!("{x:.33e}");
    let (mantissa, exp) = text.split_once('e').unwrap_or((&text, "0"));
    // `{:.33e}` always writes a point: the zeros after the last digit go, and the point with them.
    let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
    format!("{mantissa}E{exp}").parse().unwrap_or_else(|_| Decimal128::nan())
}
