//! Numbers as ECMAScript writes and reads them (§6.1.6.1.20 Number::toString, §7.1.4.1.1
//! StringToNumber).

/// The shortest decimal digits of `x` (finite, positive) and its exponent `n` such that
/// `x = 0.d1d2… × 10^n`.
fn digits(x: f64) -> (String, i32) {
    // Rust's `{:e}` writes the shortest representation that reads back as `x`.
    let s = format!("{x:e}");
    let (mantissa, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
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
    let mut int = x.abs().trunc();
    let mut frac = x.abs() - int;
    let mut out = Vec::new();
    if int == 0.0 {
        out.push('0');
    }
    while int >= 1.0 {
        let d = (int % f64::from(radix)) as u32;
        out.push(char::from_digit(d, radix).unwrap_or('0'));
        int = (int / f64::from(radix)).trunc();
    }
    out.reverse();
    // The fraction's digits until they tell `x` from its neighbours: half the distance to the
    // next double, as SpiderMonkey's shortest form in a radix (measured on a `refrouter` 9.0.4 on
    // 02-11: `(0.1).toString(3)` has 34 digits).
    let next = f64::from_bits(x.abs().to_bits() + 1);
    let mut delta = (0.5 * (next - x.abs())).max(f64::from_bits(1));
    if frac >= delta {
        out.push('.');
        let r = f64::from(radix);
        let mut digits: Vec<u32> = Vec::new();
        loop {
            frac *= r;
            delta *= r;
            let d = frac.trunc() as u32;
            digits.push(d);
            frac -= f64::from(d);
            if (frac > 0.5 || (frac == 0.5 && d & 1 == 1)) && frac + delta > 1.0 {
                // Round up, carrying into the digits before.
                while let Some(last) = digits.last_mut() {
                    *last += 1;
                    if *last < radix {
                        break;
                    }
                    digits.pop();
                }
                break;
            }
            if frac < delta {
                break;
            }
        }
        out.extend(digits.into_iter().map(|d| char::from_digit(d, radix).unwrap_or('0')));
    }
    let s: String = out.into_iter().collect();
    if neg { format!("-{s}") } else { s }
}

/// `x.toFixed(digits)`.
pub fn to_fixed(x: f64, digits: usize) -> String {
    if !x.is_finite() || x.abs() >= 1e21 {
        return to_string(x);
    }
    let s = format!("{x:.digits$}");
    // `-0` has no sign; a small negative number keeps it (`-0.00`).
    if x == 0.0 && s.starts_with('-') {
        return s[1..].to_owned();
    }
    s
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
