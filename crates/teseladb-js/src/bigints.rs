//! `BigInt` in the language: the operators on two `BigInt`s, comparisons with other primitives,
//! and the `BigInt` function with its prototype, with SpiderMonkey's messages (measured on a
//! reference router 9.0.4 on 02-11).

use std::cmp::Ordering;
use std::rc::Rc;

use crate::bigint::BigInt;
use crate::builtins::{arg, ctor, method, new_target};
use crate::interp::{Interp, R};
use crate::value::*;

const TOO_LARGE: &str = "BigInt is too large to allocate";

/// `x op y` for two `BigInt`s (`@+` is numeric addition).
pub fn binary(it: &mut Interp, op: &str, x: &BigInt, y: &BigInt) -> R {
    let out = match op {
        "@+" => x.add(y),
        "-" => x.sub(y),
        "*" => {
            if x.bit_len() + y.bit_len() > crate::bigint::MAX_BITS + 1 {
                return Err(it.error("RangeError", TOO_LARGE));
            }
            x.mul(y)
        }
        "/" | "%" => {
            let Some((q, r)) = x.div_rem(y) else {
                return Err(it.error("RangeError", "BigInt division by zero"));
            };
            if op == "/" { q } else { r }
        }
        "**" => {
            if y.is_neg() {
                return Err(it.error("RangeError", "BigInt negative exponent"));
            }
            let small = x.is_zero() || x.abs() == BigInt::from_i64(1);
            let e = if y.bit_len() > 63 && small {
                if y.as_i64_wrapping() & 1 == 0 { 2 } else { 1 }
            } else if y.bit_len() > 63 {
                return Err(it.error("RangeError", TOO_LARGE));
            } else {
                y.as_i64_wrapping() as u64
            };
            match x.pow(e) {
                Some(v) => v,
                None => return Err(it.error("RangeError", TOO_LARGE)),
            }
        }
        "&" => x.and(y),
        "|" => x.or(y),
        "^" => x.xor(y),
        "<<" | ">>" => {
            let n = if op == "<<" { y.clone() } else { y.neg() };
            match x.shift_left(&n) {
                Some(v) => v,
                None => return Err(it.error("RangeError", TOO_LARGE)),
            }
        }
        _ => return it.type_error("can't convert BigInt to number"),
    };
    Ok(Val::BigInt(Rc::new(out)))
}

/// Compares two primitives one of which is a `BigInt`; `None` when they are unordered (NaN, or
/// a string that is not an integer).
pub fn compare(it: &mut Interp, a: &Val, b: &Val) -> R<Option<Ordering>> {
    Ok(match (a, b) {
        (Val::BigInt(x), Val::BigInt(y)) => Some(x.cmp(y)),
        (Val::BigInt(x), Val::Str(s)) => BigInt::parse(s).map(|y| x.cmp(&y)),
        (Val::Str(s), Val::BigInt(y)) => BigInt::parse(s).map(|x| x.cmp(y)),
        (Val::BigInt(x), other) => {
            let n = it.number_of(other)?;
            x.cmp_f64(n)
        }
        (other, Val::BigInt(y)) => {
            let n = it.number_of(other)?;
            y.cmp_f64(n).map(Ordering::reverse)
        }
        _ => None,
    })
}

/// `ToBigInt`.
pub fn to_bigint(it: &mut Interp, v: &Val) -> R<Rc<BigInt>> {
    let p = it.primitive(v, "number")?;
    match &p {
        Val::BigInt(b) => Ok(b.clone()),
        Val::Bool(b) => Ok(Rc::new(BigInt::from_i64(i64::from(*b)))),
        Val::Str(s) => match BigInt::parse(s) {
            Some(b) => Ok(Rc::new(b)),
            None => Err(it.error("SyntaxError", "invalid BigInt syntax")),
        },
        Val::Sym(s) => it.type_error(&format!("can't convert Symbol(\"{}\") to BigInt", s.0)),
        _ => {
            let what = it.show(&p);
            it.type_error(&format!("can't convert {what} to BigInt"))
        }
    }
}

/// `ToIndex` of `BigInt.asIntN`'s width.
fn to_index(it: &mut Interp, v: &Val) -> R<u64> {
    let n = it.number_of(v)?;
    let n = if n.is_nan() { 0.0 } else { n.trunc() };
    if !(0.0..=9_007_199_254_740_991.0).contains(&n) {
        return Err(it.error("RangeError", "invalid or out-of-range index"));
    }
    Ok(n as u64)
}

fn bigint_fn(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    if new_target(this).is_some() {
        return it.type_error("BigInt is not a constructor");
    }
    let v = it.primitive(&arg(a, 0), "number")?;
    if let Val::Num(n) = v {
        return match BigInt::from_f64(n) {
            Some(b) => Ok(Val::BigInt(Rc::new(b))),
            None => {
                let m = format!(
                    "{} can't be converted to BigInt because it isn't an integer",
                    crate::number::to_string(n)
                );
                Err(it.error("RangeError", &m))
            }
        };
    }
    Ok(Val::BigInt(to_bigint(it, &v)?))
}

fn as_int_n(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let bits = to_index(it, &arg(a, 0))?;
    let b = to_bigint(it, &arg(a, 1))?;
    Ok(Val::BigInt(Rc::new(b.as_int_n(bits))))
}

fn as_uint_n(it: &mut Interp, _: &Val, a: &[Val]) -> R {
    let bits = to_index(it, &arg(a, 0))?;
    let b = to_bigint(it, &arg(a, 1))?;
    Ok(Val::BigInt(Rc::new(b.as_uint_n(bits))))
}

fn this_bigint(it: &mut Interp, this: &Val, method: &str) -> R<Rc<BigInt>> {
    match this {
        Val::BigInt(b) => Ok(b.clone()),
        Val::Obj(o) if matches!(o.borrow().kind, Kind::BigInt(_)) => match &o.borrow().kind {
            Kind::BigInt(b) => Ok(b.clone()),
            _ => unreachable!("checked by the guard"),
        },
        _ => it.type_error(&format!(
            "{method} method called on incompatible {}",
            crate::builtins::class_name(this)
        )),
    }
}

fn to_string(it: &mut Interp, this: &Val, a: &[Val]) -> R {
    let b = this_bigint(it, this, "toString")?;
    let radix = match arg(a, 0) {
        Val::Undef => 10.0,
        r => it.number_of(&r)?.trunc(),
    };
    if !(2.0..=36.0).contains(&radix) {
        return Err(
            it.error("RangeError", "radix must be an integer at least 2 and no greater than 36")
        );
    }
    it.string(b.to_string_radix(radix as u32))
}

fn to_locale_string(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    let b = this_bigint(it, this, "toLocaleString")?;
    it.string(b.to_string())
}

fn value_of(it: &mut Interp, this: &Val, _: &[Val]) -> R {
    Ok(Val::BigInt(this_bigint(it, this, "valueOf")?))
}

pub fn install(it: &mut Interp, g: &ObjRef) -> R<()> {
    let proto = it.realm.bigint.clone();
    let c = ctor(it, g, "BigInt", bigint_fn, 1, &proto)?;
    method(it, &c, "asIntN", as_int_n, 2)?;
    method(it, &c, "asUintN", as_uint_n, 2)?;
    method(it, &proto, "toString", to_string, 0)?;
    method(it, &proto, "toLocaleString", to_locale_string, 0)?;
    method(it, &proto, "valueOf", value_of, 0)?;
    crate::builtins::to_string_tag(it, &proto, "BigInt")
}
