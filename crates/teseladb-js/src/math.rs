//! `Math` (ECMA-262 §21.3). `Math.random` draws from the generator the runtime gives the
//! invocation (016 FR-008), never from the system.

use crate::builtins::method;
use crate::interp::{Interp, R};
use crate::value::*;

/// The next number in `[0, 1)` of the invocation's generator (SplitMix64).
pub fn next_random(it: &mut Interp) -> f64 {
    it.rng = it.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = it.rng;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    let m = it.object()?;
    it.define(g, "Math", Prop::hidden(Val::Obj(m.clone())))?;
    for (name, v) in [
        ("PI", std::f64::consts::PI),
        ("E", std::f64::consts::E),
        ("LN2", std::f64::consts::LN_2),
        ("LN10", std::f64::consts::LN_10),
        ("LOG2E", std::f64::consts::LOG2_E),
        ("LOG10E", std::f64::consts::LOG10_E),
        ("SQRT2", std::f64::consts::SQRT_2),
        ("SQRT1_2", std::f64::consts::FRAC_1_SQRT_2),
    ] {
        it.define(&m, name, Prop { writable: false, ..Prop::hidden(Val::Num(v)) })?;
    }
    let unary: [(&str, NativeFn); 30] = [
        ("acosh", |it, _, a| one(it, a, acosh)),
        ("asinh", |it, _, a| one(it, a, asinh)),
        ("atanh", |it, _, a| one(it, a, atanh)),
        ("f16round", |it, _, a| one(it, a, f16_round)),
        ("abs", |it, _, a| one(it, a, f64::abs)),
        ("floor", |it, _, a| one(it, a, f64::floor)),
        ("ceil", |it, _, a| one(it, a, f64::ceil)),
        ("round", |it, _, a| one(it, a, round)),
        ("trunc", |it, _, a| one(it, a, f64::trunc)),
        ("sign", |it, _, a| one(it, a, |x| if x.is_nan() || x == 0.0 { x } else { x.signum() })),
        ("sqrt", |it, _, a| one(it, a, f64::sqrt)),
        ("cbrt", |it, _, a| one(it, a, f64::cbrt)),
        ("exp", |it, _, a| one(it, a, f64::exp)),
        ("expm1", |it, _, a| one(it, a, f64::exp_m1)),
        ("log", |it, _, a| one(it, a, f64::ln)),
        ("log2", |it, _, a| one(it, a, f64::log2)),
        ("log10", |it, _, a| one(it, a, f64::log10)),
        ("log1p", |it, _, a| one(it, a, log1p)),
        ("sin", |it, _, a| one(it, a, f64::sin)),
        ("cos", |it, _, a| one(it, a, f64::cos)),
        ("tan", |it, _, a| one(it, a, f64::tan)),
        ("asin", |it, _, a| one(it, a, f64::asin)),
        ("acos", |it, _, a| one(it, a, f64::acos)),
        ("atan", |it, _, a| one(it, a, f64::atan)),
        ("sinh", |it, _, a| one(it, a, f64::sinh)),
        ("cosh", |it, _, a| one(it, a, f64::cosh)),
        ("tanh", |it, _, a| one(it, a, f64::tanh)),
        ("fround", |it, _, a| one(it, a, |x| f64::from(x as f32))),
        ("clz32", |it, _, a| {
            one(it, a, |x| f64::from(crate::number::to_uint32(x).leading_zeros()))
        }),
        ("random", |it, _, _| Ok(Val::Num(next_random(it)))),
    ];
    for (name, f) in unary {
        method(it, &m, name, f, 1)?;
    }
    method(it, &m, "max", |it, _, a| fold(it, a, f64::NEG_INFINITY, true), 2)?;
    method(it, &m, "min", |it, _, a| fold(it, a, f64::INFINITY, false), 2)?;
    method(
        it,
        &m,
        "pow",
        |it, _, a| {
            let x = it.number_of(&a.first().cloned().unwrap_or(Val::Undef))?;
            let y = it.number_of(&a.get(1).cloned().unwrap_or(Val::Undef))?;
            it.binary("**", &Val::Num(x), &Val::Num(y))
        },
        2,
    )?;
    method(
        it,
        &m,
        "atan2",
        |it, _, a| {
            let y = it.number_of(&a.first().cloned().unwrap_or(Val::Undef))?;
            let x = it.number_of(&a.get(1).cloned().unwrap_or(Val::Undef))?;
            Ok(Val::Num(y.atan2(x)))
        },
        2,
    )?;
    method(it, &m, "hypot", hypot, 2)?;
    method(it, &m, "imul", imul, 2)?;
    method(it, &m, "sumPrecise", sum_precise, 1)?;
    Ok(())
}

fn one(it: &mut Interp, a: &[Val], f: fn(f64) -> f64) -> R {
    let x = it.number_of(&a.first().cloned().unwrap_or(Val::Undef))?;
    Ok(Val::Num(f(x)))
}

fn fold(it: &mut Interp, a: &[Val], start: f64, max: bool) -> R {
    let mut acc = start;
    let mut nan = false;
    for x in a {
        let n = it.number_of(x)?;
        if n.is_nan() {
            nan = true;
        } else if (max && (n > acc || (n == 0.0 && acc == 0.0 && n.is_sign_positive())))
            || (!max && (n < acc || (n == 0.0 && acc == 0.0 && n.is_sign_negative())))
        {
            acc = n;
        }
    }
    Ok(Val::Num(if nan { f64::NAN } else { acc }))
}

/// `Math.round`: the nearest integer, halves up, with the sign of zero kept.
fn round(x: f64) -> f64 {
    if !x.is_finite() || x.fract() == 0.0 {
        return x;
    }
    if (-0.5..0.0).contains(&x) {
        return -0.0;
    }
    let r = x.floor();
    if x - r >= 0.5 { r + 1.0 } else { r }
}

/// `Math.f16round`: the nearest binary16 value, ties to even.
fn f16_round(x: f64) -> f64 {
    if x.is_nan() || x.is_infinite() || x == 0.0 {
        return x;
    }
    /// Half the gap past the largest binary16 value: from here on the result is infinite.
    const OVERFLOW: f64 = 65520.0;
    if x.abs() >= OVERFLOW {
        return f64::INFINITY.copysign(x);
    }
    let exp = ((x.abs().to_bits() >> 52) & 0x7ff) as i32 - 1023;
    // The gap between binary16 values near `x`: 2^-24 below the normal range.
    let quantum = 2f64.powi(exp.max(-14) - 10);
    let r = (x / quantum).round_ties_even() * quantum;
    if r == 0.0 { 0.0f64.copysign(x) } else { r }
}

fn hypot(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let mut nums = Vec::with_capacity(a.len());
    for x in a {
        nums.push(it.number_of(x)?);
    }
    if nums.iter().any(|n| n.is_infinite()) {
        return Ok(Val::Num(f64::INFINITY));
    }
    if nums.iter().any(|n| n.is_nan()) {
        return Ok(Val::Num(f64::NAN));
    }
    // Scaled by the largest value so the squares neither overflow nor vanish.
    let max = nums.iter().fold(0.0f64, |m, n| m.max(n.abs()));
    if max == 0.0 {
        return Ok(Val::Num(0.0));
    }
    let s: f64 = nums.iter().map(|n| (n / max) * (n / max)).sum();
    Ok(Val::Num(s.sqrt() * max))
}

fn imul(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let x = crate::number::to_uint32(it.number_of(&a.first().cloned().unwrap_or(Val::Undef))?);
    let y = crate::number::to_uint32(it.number_of(&a.get(1).cloned().unwrap_or(Val::Undef))?);
    Ok(Val::Num(f64::from(x.wrapping_mul(y) as i32)))
}

/// Bits of the exact accumulator: every finite double is an integer number of 2^-1074, below
/// 2^2098, and the 64 bits above leave room for the carries of any number of terms.
const LIMBS: usize = 35;

/// An unsigned integer in units of 2^-1074.
#[derive(Clone)]
struct Exact([u64; LIMBS]);

impl Exact {
    /// Adds the magnitude of the finite `x`.
    fn add(&mut self, x: f64) {
        let bits = x.to_bits();
        let biased = (bits >> 52) & 0x7ff;
        let frac = bits & ((1 << 52) - 1);
        let (m, shift) = if biased == 0 { (frac, 0) } else { (frac | (1 << 52), biased - 1) };
        let (limb, off) = ((shift / 64) as usize, shift % 64);
        let wide = u128::from(m) << off;
        let mut carry = 0u128;
        for (i, part) in [wide as u64, (wide >> 64) as u64].into_iter().enumerate() {
            let s = u128::from(self.0[limb + i]) + u128::from(part) + carry;
            self.0[limb + i] = s as u64;
            carry = s >> 64;
        }
        let mut i = limb + 2;
        while carry != 0 && i < LIMBS {
            let s = u128::from(self.0[i]) + carry;
            self.0[i] = s as u64;
            carry = s >> 64;
            i += 1;
        }
    }

    fn less_than(&self, other: &Exact) -> bool {
        self.0.iter().rev().cmp(other.0.iter().rev()) == std::cmp::Ordering::Less
    }

    /// `self - other`, with `other` not larger.
    fn minus(&self, other: &Exact) -> Exact {
        let mut out = [0u64; LIMBS];
        let mut borrow = 0u64;
        for (i, slot) in out.iter_mut().enumerate() {
            let (d, b1) = self.0[i].overflowing_sub(other.0[i]);
            let (d, b2) = d.overflowing_sub(borrow);
            *slot = d;
            borrow = u64::from(b1 || b2);
        }
        Exact(out)
    }

    fn bit(&self, i: usize) -> bool {
        self.0[i / 64] >> (i % 64) & 1 == 1
    }

    fn bit_len(&self) -> usize {
        (0..LIMBS)
            .rev()
            .find(|&i| self.0[i] != 0)
            .map_or(0, |i| 64 * i + 64 - self.0[i].leading_zeros() as usize)
    }

    /// The nearest double, ties to even.
    fn to_f64(&self) -> f64 {
        let len = self.bit_len();
        let field = |from: usize, n: usize| {
            (from..from + n).rev().fold(0u64, |acc, i| (acc << 1) | u64::from(self.bit(i)))
        };
        if len <= 53 {
            return field(0, len) as f64 * 2f64.powi(-1074);
        }
        let shift = len - 53;
        let mut m = field(shift, 53);
        let half = self.bit(shift - 1);
        let sticky = (0..shift - 1).any(|i| self.bit(i));
        if half && (sticky || m & 1 == 1) {
            m += 1;
        }
        let exp = shift as i32 - 1074;
        // Two steps keep each power of two finite.
        m as f64 * 2f64.powi(exp / 2) * 2f64.powi(exp - exp / 2)
    }
}

/// `Math.sumPrecise(items)`: the exact sum of the numbers, rounded once.
fn sum_precise(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let items = a.first().cloned().unwrap_or(Val::Undef);
    let mut rec = it.open_iter(&items)?;
    let (mut pos, mut neg) = (Exact([0; LIMBS]), Exact([0; LIMBS]));
    let (mut nan, mut plus_inf, mut minus_inf, mut all_minus_zero) = (false, false, false, true);
    while let Some(v) = it.iter_step(&mut rec)? {
        let Val::Num(x) = v else {
            if let Err(fatal @ crate::interp::Ctrl::Fatal(_)) = it.iter_close(&rec) {
                return Err(fatal);
            }
            return it.type_error("values passed to Math.sumPrecise must be numbers");
        };
        all_minus_zero &= x == 0.0 && x.is_sign_negative();
        if x.is_nan() {
            nan = true;
        } else if x.is_infinite() {
            if x > 0.0 { plus_inf = true } else { minus_inf = true }
        } else if x > 0.0 {
            pos.add(x);
        } else if x < 0.0 {
            neg.add(x);
        }
    }
    let r = if nan || (plus_inf && minus_inf) {
        f64::NAN
    } else if plus_inf {
        f64::INFINITY
    } else if minus_inf {
        f64::NEG_INFINITY
    } else if all_minus_zero {
        -0.0
    } else if pos.less_than(&neg) {
        -neg.minus(&pos).to_f64()
    } else {
        pos.minus(&neg).to_f64()
    };
    Ok(Val::Num(r))
}

// The inverse hyperbolic functions as fdlibm computes them (SpiderMonkey uses fdlibm), so the
// last bit matches the engine of the reference server.

fn acosh(x: f64) -> f64 {
    if x.is_nan() || x < 1.0 {
        return f64::NAN;
    }
    if x >= 2f64.powi(28) {
        return x.ln() + std::f64::consts::LN_2;
    }
    if x > 2.0 {
        return (2.0 * x - 1.0 / (x + (x * x - 1.0).sqrt())).ln();
    }
    let t = x - 1.0;
    log1p(t + (2.0 * t + t * t).sqrt())
}

fn asinh(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 {
        return x;
    }
    let a = x.abs();
    let w = if a > 2f64.powi(28) {
        a.ln() + std::f64::consts::LN_2
    } else if a > 2.0 {
        (2.0 * a + 1.0 / ((x * x + 1.0).sqrt() + a)).ln()
    } else {
        let t = x * x;
        log1p(a + t / (1.0 + (1.0 + t).sqrt()))
    };
    w.copysign(x)
}

fn atanh(x: f64) -> f64 {
    let a = x.abs();
    if x.is_nan() || a > 1.0 {
        return f64::NAN;
    }
    if a == 1.0 {
        return f64::INFINITY.copysign(x);
    }
    if x == 0.0 {
        return x;
    }
    let t = if a < 0.5 {
        let t = a + a;
        0.5 * log1p(t + t * a / (1.0 - a))
    } else {
        0.5 * log1p((a + a) / (1.0 - a))
    };
    t.copysign(x)
}

/// fdlibm's `log1p` (s_log1p.c), which SpiderMonkey uses: `ln(1 + x)` accurate near zero.
#[allow(clippy::excessive_precision)]
fn log1p(x: f64) -> f64 {
    const LN2_HI: f64 = 6.93147180369123816490e-01;
    const LN2_LO: f64 = 1.90821492927058770002e-10;
    const LP: [f64; 7] = [
        6.666666666666735130e-01,
        3.999999999940941908e-01,
        2.857142874366239149e-01,
        2.222219843214978396e-01,
        1.818357216161805012e-01,
        1.531383769920937332e-01,
        1.479819860511658591e-01,
    ];
    let hx = (x.to_bits() >> 32) as i32;
    let ax = hx & 0x7fff_ffff;
    let mut k = 1;
    let (mut f, mut hu, mut c) = (0.0, 0i32, 0.0);
    if hx < 0x3FDA_827A {
        if ax >= 0x3ff0_0000 {
            return if x == -1.0 { f64::NEG_INFINITY } else { f64::NAN };
        }
        if ax < 0x3e20_0000 {
            if x.abs() < 2f64.powi(-54) {
                return x;
            }
            return x - x * x * 0.5;
        }
        if hx > 0 || hx <= 0xbfd2_bec3_u32 as i32 {
            k = 0;
            f = x;
            hu = 1;
        }
    }
    if hx >= 0x7ff0_0000 {
        return x + x;
    }
    if k != 0 {
        let u;
        if hx < 0x4340_0000 {
            u = 1.0 + x;
            hu = (u.to_bits() >> 32) as i32;
            k = (hu >> 20) - 1023;
            c = if k > 0 { 1.0 - (u - x) } else { x - (u - 1.0) };
            c /= u;
        } else {
            u = x;
            hu = (u.to_bits() >> 32) as i32;
            k = (hu >> 20) - 1023;
            c = 0.0;
        }
        hu &= 0x000f_ffff;
        let lo = u.to_bits() & 0xffff_ffff;
        let bits = if hu < 0x6_a09e {
            (((hu | 0x3ff0_0000) as u64) << 32) | lo
        } else {
            k += 1;
            let b = (((hu as u32 | 0x3fe0_0000) as u64) << 32) | lo;
            hu = (0x0010_0000 - hu) >> 2;
            b
        };
        f = f64::from_bits(bits) - 1.0;
    }
    let hfsq = 0.5 * f * f;
    let kf = f64::from(k);
    if hu == 0 {
        if f == 0.0 {
            return if k == 0 { 0.0 } else { kf * LN2_HI + (c + kf * LN2_LO) };
        }
        let r = hfsq * (1.0 - 0.66666666666666666 * f);
        return if k == 0 { f - r } else { kf * LN2_HI - ((r - (kf * LN2_LO + c)) - f) };
    }
    let s = f / (2.0 + f);
    let z = s * s;
    let r = z
        * (LP[0] + z * (LP[1] + z * (LP[2] + z * (LP[3] + z * (LP[4] + z * (LP[5] + z * LP[6]))))));
    if k == 0 {
        f - (hfsq - s * (hfsq + r))
    } else {
        kf * LN2_HI - ((hfsq - (s * (hfsq + r) + (kf * LN2_LO + c))) - f)
    }
}
