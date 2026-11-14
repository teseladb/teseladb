//! Numbers as ECMAScript writes and reads them (§6.1.6.1.20 Number::toString, §7.1.4.1.1
//! StringToNumber).

/// The shortest decimal digits of `x` (finite, positive) and its exponent `n` such that
/// `x = 0.d1d2… × 10^n`.
fn digits(x: f64) -> (String, i32) {
    // Rust's `{:e}` writes the shortest representation that reads back as `x`.
    let s = format!("{x:e}");
    let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    // `x` is positive, so a digit is not zero.
    let digits = digits.trim_end_matches('0');
    (digits.to_owned(), exp.parse::<i32>().unwrap_or(0) + 1)
}

/// `Number::toString(x)` in base 10.
pub fn to_string(x: f64) -> String {
    if x.is_nan() {
        return "NaN".to_owned();
    }
    if x == 0.0 {
        return "0".to_owned();
    }
    if x.is_infinite() {
        return if x > 0.0 { "Infinity" } else { "-Infinity" }.to_owned();
    }
    if x < 0.0 {
        return format!("-{}", to_string(-x));
    }
    let (d, n) = digits(x);
    let k = d.len() as i32;
    if k <= n && n <= 21 {
        return format!("{d}{}", "0".repeat((n - k) as usize));
    }
    if 0 < n && n <= 21 {
        return format!("{}.{}", &d[..n as usize], &d[n as usize..]);
    }
    if -6 < n && n <= 0 {
        return format!("0.{}{d}", "0".repeat((-n) as usize));
    }
    let e = n - 1;
    let sign = if e < 0 { '-' } else { '+' };
    if k == 1 {
        return format!("{d}e{sign}{}", e.abs());
    }
    format!("{}.{}e{sign}{}", &d[..1], &d[1..], e.abs())
}

/// `x.toString(radix)` for a radix other than 10.
pub fn to_radix(x: f64, radix: u32) -> String {
    if !x.is_finite() || radix == 10 {
        return to_string(x);
    }
    let neg = x < 0.0;
    let int = x.abs().trunc();
    let frac = x.abs() - int;
    // The integer part exactly, also past 2^53 (`(1e21).toString(36)` is `5v1j4f4ds79m9s`).
    let whole = crate::bigint::BigInt::from_f64(int)
        .map_or_else(|| "0".into(), |b| b.to_string_radix(radix));
    let mut out: Vec<char> = whole.chars().collect();
    if frac != 0.0 {
        out.push('.');
        let digits = fraction_digits(x.abs(), frac, radix);
        out.extend(digits.into_iter().map(|d| char::from_digit(d, radix).unwrap_or('0')));
    }
    let s: String = out.into_iter().collect();
    if neg { format!("-{s}") } else { s }
}

/// The digits in `radix` of `frac`, the fraction of `x`: the fewest that still read back as `x`,
/// as David Gay's conversion to any base finds them (SpiderMonkey's `js_dtobasestr`; measured on
/// a reference router 9.0.4 on 03-11: `(0.99999999999999).toString(3)` ends in `202211`).
fn fraction_digits(x: f64, frac: f64, radix: u32) -> Vec<u32> {
    use crate::bigint::BigInt;
    use std::cmp::Ordering;
    let bits = x.to_bits();
    let biased = (bits >> 52) & 0x7ff;
    // 1 / 2^s2 is half the distance from `x` to the next double.
    let mut s2 = if biased == 0 { 1075 } else { 1076 - biased };
    let mut mhi = BigInt::from_u64(1);
    // A power of two is closer to the double below it: a quarter of a digit above.
    if bits & ((1 << 52) - 1) == 0 && biased >= 2 {
        s2 += 1;
        mhi = BigInt::from_u64(2);
    }
    let mut mlo = BigInt::from_u64(1);
    // `frac` is `m * 2^e` with `m` whole, and `b / s` is `frac`.
    let fbits = frac.to_bits();
    let fexp = (fbits >> 52) & 0x7ff;
    let (m, e) = if fexp == 0 {
        (fbits & ((1 << 52) - 1), -1074)
    } else {
        ((fbits & ((1 << 52) - 1)) | (1 << 52), fexp as i64 - 1075)
    };
    // Without its low zero bits `m · 2^e` has `e` above `-s2`: `frac` is a multiple of the
    // spacing of the doubles around `x`.
    let zeros = m.trailing_zeros();
    let (m, e) = (m >> zeros, e + i64::from(zeros));
    let mut b = BigInt::from_u64(m).shl_bits((e + s2 as i64) as u64);
    let s = BigInt::from_u64(1).shl_bits(s2);
    let base = BigInt::from_u64(u64::from(radix));
    let odd = bits & 1 == 1;
    let mut out = Vec::new();
    loop {
        b = b.mul(&base);
        let (q, r) = b.div_rem(&s).unwrap_or_default();
        b = r;
        let mut digit = q.to_f64() as u32;
        mlo = mlo.mul(&base);
        mhi = mhi.mul(&base);
        let j = b.cmp(&mlo);
        let delta = s.sub(&mhi);
        let j1 = if delta.is_neg() { Ordering::Greater } else { b.cmp(&delta) };
        let done = if j1 == Ordering::Equal && !odd {
            digit += u32::from(j == Ordering::Greater);
            true
        } else if j == Ordering::Less || (j == Ordering::Equal && !odd) {
            // Either digit reads back as `x`: the one nearer to it.
            if j1 == Ordering::Greater && b.shl_bits(1).cmp(&s) == Ordering::Greater {
                digit += 1;
            }
            true
        } else if j1 == Ordering::Greater {
            digit += 1;
            true
        } else {
            false
        };
        out.push(digit);
        if done {
            return out;
        }
    }
}

/// `x.toFixed(digits)`: the nearest number with `digits` decimals, and the larger one of two
/// at the same distance (ECMA-262 §21.1.3.3: `(0.5).toFixed(0)` is `1`, `(2.5).toFixed(0)`
/// is `3`; Rust's formatting rounds those to even).
pub fn to_fixed(x: f64, digits: usize) -> String {
    // Every decimal digit of a double: the smallest one has 1074.
    const EXACT: usize = 1100;
    if !x.is_finite() || x.abs() >= 1e21 {
        return to_string(x);
    }
    let exact = format!("{:.*}", digits + EXACT, x.abs());
    let (keep, rest) = exact.split_at(exact.len() - EXACT);
    let mut out: Vec<u8> = keep.trim_end_matches('.').bytes().collect();
    if rest.as_bytes()[0] >= b'5' {
        increment(&mut out);
    }
    let s = String::from_utf8(out).unwrap_or_default();
    if x < 0.0 { format!("-{s}") } else { s }
}

/// Adds one in the last place of the decimal digits `d` (a `.` may be among them).
fn increment(d: &mut Vec<u8>) {
    for i in (0..d.len()).rev() {
        match d[i] {
            b'.' => {}
            b'9' => d[i] = b'0',
            c => {
                d[i] = c + 1;
                return;
            }
        }
    }
    d.insert(0, b'1');
}

/// Whether `c` is white space or a line terminator for StringToNumber.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

/// `ToNumber` of a string.
pub fn parse(s: &str) -> f64 {
    let t = s.trim_matches(is_space);
    if t.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(rest) = t.strip_prefix(prefix) {
            if rest.is_empty() {
                return f64::NAN;
            }
            let mut v = 0f64;
            for c in rest.chars() {
                let Some(d) = c.to_digit(radix) else { return f64::NAN };
                v = v * f64::from(radix) + f64::from(d);
            }
            return v;
        }
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let ok = t.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'));
    if !ok || t.ends_with(['e', 'E']) {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// `parseFloat`: the longest prefix that is a decimal literal.
pub fn parse_float(s: &str) -> f64 {
    let t = s.trim_start_matches(is_space);
    let b = t.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    if t[i..].starts_with("Infinity") {
        return if b.first() == Some(&b'-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    let start_digits = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == start_digits || (i == start_digits + 1 && b[start_digits] == b'.') {
        return f64::NAN;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let d = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > d {
            i = j;
        }
    }
    t[..i].parse::<f64>().unwrap_or(f64::NAN)
}

/// `parseInt(s, radix)`.
pub fn parse_int(s: &str, radix: i32) -> f64 {
    let mut t = s.trim_start_matches(is_space);
    let neg = t.starts_with('-');
    if neg || t.starts_with('+') {
        t = &t[1..];
    }
    let mut radix = radix;
    if (radix == 0 || radix == 16)
        && let Some(rest) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))
    {
        t = rest;
        radix = 16;
    }
    if radix == 0 {
        radix = 10;
    }
    if !(2..=36).contains(&radix) {
        return f64::NAN;
    }
    let mut v = 0f64;
    let mut any = false;
    for c in t.chars() {
        let Some(d) = c.to_digit(radix as u32) else { break };
        v = v * f64::from(radix) + f64::from(d);
        any = true;
    }
    if !any {
        return f64::NAN;
    }
    if neg { -v } else { v }
}

/// `ToInt32`.
pub fn to_int32(x: f64) -> i32 {
    to_uint32(x) as i32
}

/// `ToUint32`.
pub fn to_uint32(x: f64) -> u32 {
    if !x.is_finite() {
        return 0;
    }
    let m = x.trunc().rem_euclid(4_294_967_296.0);
    m as u32
}
