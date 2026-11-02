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
    let unary: [(&str, NativeFn); 26] = [
        ("abs", |it, _, a| one(it, a, f64::abs)),
        ("floor", |it, _, a| one(it, a, f64::floor)),
        ("ceil", |it, _, a| one(it, a, f64::ceil)),
        ("round", |it, _, a| {
            one(it, a, |x| if x.fract() == -0.5 { x.ceil() } else { (x + 0.5).floor().copysign(x) })
        }),
        ("trunc", |it, _, a| one(it, a, f64::trunc)),
        ("sign", |it, _, a| one(it, a, |x| if x.is_nan() || x == 0.0 { x } else { x.signum() })),
        ("sqrt", |it, _, a| one(it, a, f64::sqrt)),
        ("cbrt", |it, _, a| one(it, a, f64::cbrt)),
        ("exp", |it, _, a| one(it, a, f64::exp)),
        ("expm1", |it, _, a| one(it, a, f64::exp_m1)),
        ("log", |it, _, a| one(it, a, f64::ln)),
        ("log2", |it, _, a| one(it, a, f64::log2)),
        ("log10", |it, _, a| one(it, a, f64::log10)),
        ("log1p", |it, _, a| one(it, a, f64::ln_1p)),
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
    method(
        it,
        &m,
        "hypot",
        |it, _, a| {
            let mut s = 0.0f64;
            for x in a {
                let n = it.number_of(x)?;
                s += n * n;
            }
            Ok(Val::Num(s.sqrt()))
        },
        2,
    )?;
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
