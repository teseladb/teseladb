//! Values written as RefDb writes them in error messages: `{a: 1, b: { c: "x" }}`.

use teseladb_bson::{RawDoc, RawValue};

/// A value as RefDb writes an element in its messages (`BSONElement::toString`):
/// `{ a: [ 1, "x" ] }`, `1.0`, `ObjectId('…')`, strings in quotes without escapes.
pub fn shell(v: RawValue<'_>) -> String {
    match v {
        RawValue::Document(d) => object(d),
        RawValue::Array(a) => spaced('[', a.iter().map(shell).collect(), ']'),
        RawValue::String(s) | RawValue::Symbol(s) => format!("\"{s}\""),
        RawValue::Int32(n) => n.to_string(),
        RawValue::Int64(n) => n.to_string(),
        RawValue::Double(x) => double(x),
        RawValue::Decimal128(d) => d.to_string(),
        RawValue::Boolean(b) => b.to_string(),
        RawValue::Null => "null".to_owned(),
        RawValue::Undefined => "undefined".to_owned(),
        RawValue::ObjectId(id) => format!("ObjectId('{}')", hex_lower(&id)),
        RawValue::DateTime(ms) => format!("new Date({ms})"),
        RawValue::Timestamp { time, increment } => format!("Timestamp({time}, {increment})"),
        RawValue::Binary { subtype: UUID_SUBTYPE, bytes } if bytes.len() == UUID_LEN => {
            let h = hex_lower(bytes);
            format!(
                "UUID(\"{}-{}-{}-{}-{}\")",
                &h[..8],
                &h[8..12],
                &h[12..16],
                &h[16..20],
                &h[20..]
            )
        }
        RawValue::Binary { subtype, bytes } => {
            let h: String = bytes.iter().map(|b| format!("{b:02X}")).collect();
            format!("BinData({subtype}, {h})")
        }
        RawValue::Regex { pattern, options } => format!("/{pattern}/{options}"),
        RawValue::JavaScript(code) => code.to_owned(),
        RawValue::JavaScriptWithScope { code, scope } => {
            format!("CodeWScope( {code}, {})", object(scope))
        }
        RawValue::DbPointer { namespace, id } => format!("DBRef('{namespace}',{})", hex_lower(&id)),
        RawValue::MinKey => "MinKey".to_owned(),
        RawValue::MaxKey => "MaxKey".to_owned(),
    }
}

/// Binary subtype of a UUID.
const UUID_SUBTYPE: u8 = 4;
/// Bytes of a UUID.
const UUID_LEN: usize = 16;

fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A document as RefDb writes an object element: `{ a: 1, b: "x" }`, `{}` when empty.
fn object(d: RawDoc<'_>) -> String {
    spaced('{', d.iter().map(|(k, v)| format!("{k}: {}", shell(v))).collect(), '}')
}

/// Items between `open` and `close` with a space inside each, or the bare pair when empty.
fn spaced(open: char, items: Vec<String>, close: char) -> String {
    if items.is_empty() {
        return format!("{open}{close}");
    }
    format!("{open} {} {close}", items.join(", "))
}

/// A document as RefDb's update messages write it around its elements: `{_id: 1, a: "x"}`.
pub(crate) fn shell_doc(d: RawDoc<'_>) -> String {
    let fields: Vec<String> = d.iter().map(|(k, v)| format!("{k}: {}", shell(v))).collect();
    format!("{{{}}}", fields.join(", "))
}

/// Significant digits of a double in a `BSONElement`'s text (C's `%.16g`).
const ELEMENT_DIGITS: usize = 16;
/// Significant digits of a double written to a C++ stream with its defaults (C's `%g`).
const STREAM_DIGITS: usize = 6;
/// Below `1e-4` C's `%g` switches to the exponent form.
const SMALLEST_FIXED_EXPONENT: i32 = -4;

/// A double as RefDb writes a value in messages: C's `%.16g`, with `.0` added when that
/// leaves a bare integer (`1.0`, `-0.0`, `1e+20`, `1e-05`, `nan`, `inf`).
pub fn double(x: f64) -> String {
    if x.is_nan() {
        // A `BSONElement` writes every NaN alike, whatever its sign.
        return "nan".to_owned();
    }
    let shown = g(x, ELEMENT_DIGITS);
    let bare = shown.bytes().all(|b| b.is_ascii_digit() || b == b'-');
    if bare { format!("{shown}.0") } else { shown }
}

/// A double as RefDb writes a plain number into a message stream: C's `%g`
/// (`2`, `1.23457e+06`, `0.000123457`, `-nan`).
pub fn stream_double(x: f64) -> String {
    g(x, STREAM_DIGITS)
}

/// C's `%.{digits}g` as glibc writes it.
fn g(x: f64, digits: usize) -> String {
    let sign = if x.is_sign_negative() { "-" } else { "" };
    if x.is_nan() {
        return format!("{sign}nan");
    }
    if x.is_infinite() {
        return format!("{sign}inf");
    }
    // Rust's `{:e}` rounds correctly, as glibc does, so the exponent is the one `%g` sees.
    let sci = format!("{x:.prec$e}", prec = digits - 1);
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let fixed_digits = i32::try_from(digits).unwrap_or(i32::MAX);
    if exp < SMALLEST_FIXED_EXPONENT || exp >= fixed_digits {
        let exp_sign = if exp < 0 { '-' } else { '+' };
        return format!("{}e{exp_sign}{:02}", trim_zeros(mantissa), exp.unsigned_abs());
    }
    let decimals = usize::try_from(fixed_digits - 1 - exp).unwrap_or(0);
    trim_zeros(&format!("{x:.decimals$}")).to_owned()
}

/// A decimal number without the zeros (and the point) `%g` drops at its end.
fn trim_zeros(s: &str) -> &str {
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.') } else { s }
}

#[cfg(test)]
mod tests {
    use super::{double, shell, shell_doc, stream_double};
    use teseladb_bson::{Document, RawDoc, Value};

    /// The text of `v` as `shell` writes it.
    fn text(v: Value) -> String {
        let bytes = Document::new().with("v", v).to_bytes().unwrap();
        let doc = RawDoc::from_bytes(&bytes).unwrap();
        shell(doc.get("v").unwrap())
    }

    /// Every pair measured in RefDb 9.0.4's `Expected a number in: a: …` and
    /// `Cannot apply $inc …` messages.
    #[test]
    fn values_are_written_as_refdb_writes_an_element() {
        let d = |pairs: &[(&str, Value)]| {
            let mut doc = Document::new();
            for (k, v) in pairs {
                doc.push(*k, v.clone());
            }
            Value::Document(doc)
        };
        let uuid: Vec<u8> = (0..16).collect();
        for (v, shown) in [
            (Value::Array(vec![1.into(), "x".into()]), r#"[ 1, "x" ]"#),
            (d(&[("b", d(&[("c", 1.into())]))]), "{ b: { c: 1 } }"),
            (Value::from("q\"t\n"), "\"q\"t\n\""),
            (Value::Timestamp { time: 1, increment: 2 }, "Timestamp(1, 2)"),
            (
                Value::Binary { subtype: 4, bytes: uuid },
                r#"UUID("00010203-0405-0607-0809-0a0b0c0d0e0f")"#,
            ),
            (Value::Binary { subtype: 0, bytes: vec![1, 2, 0xff] }, "BinData(0, 0102FF)"),
            (Value::Binary { subtype: 0x80, bytes: Vec::new() }, "BinData(128, )"),
            (Value::Regex { pattern: "a.b".into(), options: "i".into() }, "/a.b/i"),
            (Value::MinKey, "MinKey"),
            (Value::MaxKey, "MaxKey"),
            (Value::Int64(7), "7"),
            (Value::Int32(3), "3"),
            (Value::Double(2.5), "2.5"),
            (Value::Decimal128("1.5".parse().unwrap()), "1.5"),
            (Value::Decimal128("1E+30".parse().unwrap()), "1E+30"),
            (Value::Decimal128("-Infinity".parse().unwrap()), "-Infinity"),
            (Value::Null, "null"),
            (Value::Undefined, "undefined"),
            (Value::DateTime(-5), "new Date(-5)"),
            (
                Value::ObjectId([1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]),
                "ObjectId('0102030405060708090a0b0c')",
            ),
            (Value::JavaScript("x+1".into()), "x+1"),
            (
                Value::JavaScriptWithScope {
                    code: "f()".into(),
                    scope: Document::new().with("x", 1),
                },
                "CodeWScope( f(), { x: 1 })",
            ),
            (Value::Array(vec!["a".into(), Value::Array(Vec::new())]), r#"[ "a", [] ]"#),
            (d(&[("a b", 1.into()), ("", 2.into())]), "{ a b: 1, : 2 }"),
            (d(&[]), "{}"),
            (Value::Boolean(false), "false"),
            (Value::Symbol("s".into()), "\"s\""),
            (
                Value::DbPointer { namespace: "db.c".into(), id: [0xab; 12] },
                "DBRef('db.c',abababababababababababab)",
            ),
        ] {
            assert_eq!(text(v.clone()), shown, "{v:?}");
        }
        // Around a document RefDb's update messages write `{_id: …}`; inside it, elements.
        let doc = Document::new().with("_id", Document::new().with("x", 1.5)).with("a", "x");
        let bytes = doc.to_bytes().unwrap();
        assert_eq!(shell_doc(RawDoc::from_bytes(&bytes).unwrap()), r#"{_id: { x: 1.5 }, a: "x"}"#);
    }

    /// Every pair measured in RefDb 9.0.4's messages (duplicate key, `$push` on a
    /// non-array, `maxTimeMS`, `killOp` and `create` with other options all agree).
    #[test]
    fn doubles_are_written_as_refdb_writes_them() {
        let nan_with_sign = f64::from_bits(0xfff8_0000_0000_0000);
        for (x, shown) in [
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (-1.0, "-1.0"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (1.0 / 3.0, "0.3333333333333333"),
            (2.0 / 3.0, "0.6666666666666666"),
            (0.1 + 0.2, "0.3"),
            (123_456.789, "123456.789"),
            (123_456_789.0, "123456789.0"),
            (1e14, "100000000000000.0"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (2f64.powi(53), "9007199254740992.0"),
            (1e17, "1e+17"),
            (1e20, "1e+20"),
            (-1e20, "-1e+20"),
            (1e21, "1e+21"),
            (1e100, "1e+100"),
            (2f64.powi(63), "9.223372036854776e+18"),
            (12_345_678_901_234_567.0, "1.234567890123457e+16"),
            (1.5e300, "1.5e+300"),
            (f64::MAX, "1.797693134862316e+308"),
            (1e-4, "0.0001"),
            (0.000_099_999_999_999_999_99, "9.999999999999999e-05"),
            (1e-5, "1e-05"),
            (0.000_123_4, "0.0001234"),
            (1.5e-7, "1.5e-07"),
            (123.456e-10, "1.23456e-08"),
            (1e-300, "1e-300"),
            (5e-324, "4.940656458412465e-324"),
            (1_234_567.0, "1234567.0"),
            (0.5, "0.5"),
            (100.25, "100.25"),
            (f64::NAN, "nan"),
            (nan_with_sign, "nan"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
        ] {
            assert_eq!(double(x), shown, "{x:?}");
        }
    }

    /// Measured in RefDb 9.0.4's `The limit field in delete objects must be 0 or 1. Got …`.
    #[test]
    fn stream_doubles_are_written_as_refdb_writes_them() {
        let nan_with_sign = f64::from_bits(0xfff8_0000_0000_0000);
        for (x, shown) in [
            (2.0, "2"),
            (-1.0, "-1"),
            (5.0, "5"),
            (1.5, "1.5"),
            (0.5, "0.5"),
            (0.1 + 0.2, "0.3"),
            (0.999_999_9, "1"),
            (123_456.0, "123456"),
            (1_234_567.0, "1.23457e+06"),
            (1e20, "1e+20"),
            (1e30, "1e+30"),
            (1e-5, "1e-05"),
            (0.000_123_456_7, "0.000123457"),
            (f64::NAN, "nan"),
            (nan_with_sign, "-nan"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
        ] {
            assert_eq!(stream_double(x), shown, "{x:?}");
        }
    }
}
