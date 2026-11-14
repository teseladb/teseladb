//! Strings and numbers as Go's `encoding/json` writes them, for the JSON a service shares with Go programs byte for
//! byte: the body of a webhook that a signature covers, the fingerprint of a request.

/// The digits of the `\u` escapes.
const HEX: &[u8; 16] = b"0123456789abcdef";
/// The smallest `float64` above which Go writes a number with an exponent.
const EXPONENT_ABOVE: f64 = 1e21;
/// The largest `float64` below which Go writes a number with an exponent.
const EXPONENT_BELOW: f64 = 1e-6;

/// Writes `s` quoted with the escapes of `json.Marshal`, which also escapes `<`, `>`, `&`, U+2028 and U+2029.
pub fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' | '\0'..='\u{1f}' => {
                out.push('\\');
                out.push('u');
                let n = u32::from(c);
                for shift in [12, 8, 4, 0] {
                    let digit = usize::try_from((n >> shift) & 0xf).unwrap_or(0);
                    out.push(char::from(HEX[digit]));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Writes `f` as Go writes a `float64`: the shortest digits that read back, with an exponent (`1e+21`, `1e-7`)
/// outside 1e-6 to 1e21. Go refuses NaN and the infinities; they are written as `null`.
pub fn write_f64(f: f64, out: &mut String) {
    if !f.is_finite() {
        out.push_str("null");
        return;
    }
    let abs = f.abs();
    if abs != 0.0 && !(EXPONENT_BELOW..EXPONENT_ABOVE).contains(&abs) {
        let text = format!("{f:e}");
        let (mantissa, exponent) = text.split_once('e').unwrap_or((&text, "0"));
        out.push_str(mantissa);
        out.push('e');
        if !exponent.starts_with('-') {
            out.push('+');
        }
        out.push_str(exponent);
    } else {
        out.push_str(&f.to_string());
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    fn string(s: &str) -> String {
        let mut out = String::new();
        write_string(s, &mut out);
        out
    }

    fn float(f: f64) -> String {
        let mut out = String::new();
        write_f64(f, &mut out);
        out
    }

    /// The escape of a code point written as Go writes it.
    fn esc(hex: &str) -> String {
        format!("{}u{hex}", '\\')
    }

    #[test]
    fn write_string_when_a_text_has_what_go_escapes_escapes_it() {
        let text = format!("<a&b>{}{}\u{8}\u{c}\n\r\t\u{1}\"\\ \u{20ac}", '\u{2028}', '\u{2029}');
        let want = format!(
            "\"{}a{}b{}{}{}\\b\\f\\n\\r\\t{}\\\"\\\\ \u{20ac}\"",
            esc("003c"),
            esc("0026"),
            esc("003e"),
            esc("2028"),
            esc("2029"),
            esc("0001"),
        );
        assert_eq!(string(&text), want);
    }

    #[test]
    fn write_f64_when_a_number_is_written_gives_what_go_gives() {
        for (f, go) in [
            (1.5, "1.5"),
            (-0.0, "-0"),
            (1e21, "1e+21"),
            (1e-7, "1e-7"),
            (123_456_789_012_345_680.0, "123456789012345680"),
            (0.000_001, "0.000001"),
            (1e20, "100000000000000000000"),
            (1.5e300, "1.5e+300"),
            (-2.5e-10, "-2.5e-10"),
            (f64::NAN, "null"),
            (f64::INFINITY, "null"),
        ] {
            assert_eq!(float(f), go, "{f}");
        }
    }
}
