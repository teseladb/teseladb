//! JSON formatting of Rust values.
//!
//! [`ToJson`] renders a value as a JSON string ([`Formatter`] holds the shared helpers);
//! [`JsonWrite`] streams it to an [`OutputStream`](crate::engine::core::iostream::OutputStream) instead, element by element, so large
//! ranges are written without building the whole document in memory. Both are
//! implemented for integers, floats, booleans, strings, [`DateTime`], pairs, sequences and
//! maps (written as JSON objects), and for the JSON objects of
//! [`json_elements`](crate::engine::json::json_elements). Floats use the shortest decimal
//! form that reads back as the same value; strings are escaped as JSON requires.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{c_char, c_int, c_long};

use crate::engine::core::future::{ExceptionPtr, Future};
use crate::engine::core::iostream::OutputStreamState;
use crate::engine::core::loop_::do_for_each;
use crate::engine::core::sstring::{Sstring, to_sstring};
use crate::engine::json::json_elements::Jsonable;

/// A broken-down calendar time, laid out as the C `struct tm` (it is passed to `strftime`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DateTime {
    /// Seconds.
    pub tm_sec: c_int,
    /// Minutes.
    pub tm_min: c_int,
    /// Hours.
    pub tm_hour: c_int,
    /// Day of the month.
    pub tm_mday: c_int,
    /// Month (0-11).
    pub tm_mon: c_int,
    /// Year - 1900.
    pub tm_year: c_int,
    /// Day of the week.
    pub tm_wday: c_int,
    /// Day in the year.
    pub tm_yday: c_int,
    /// Daylight saving time flag.
    pub tm_isdst: c_int,
    /// Seconds east of UTC.
    pub tm_gmtoff: c_long,
    /// Time zone abbreviation.
    pub tm_zone: usize,
}

unsafe extern "C" {
    fn strftime(s: *mut c_char, max: usize, format: *const c_char, tm: *const DateTime) -> usize;
}

/// The nesting state of the formatter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Not in a container.
    None,
    /// Inside an array.
    Array,
    /// Inside an object.
    Map,
}

/// Formats values as JSON.
///
/// The per-type formatting lives in the [`ToJson`] and [`JsonWrite`] implementations;
/// this type holds the shared helpers for containers and ranges.
pub struct Formatter;

impl Formatter {
    /// The opening of a container.
    pub fn begin(s: State) -> &'static str {
        match s {
            State::Array => "[",
            State::Map => "{",
            State::None => "",
        }
    }

    /// The closing of a container.
    pub fn end(s: State) -> &'static str {
        match s {
            State::Array => "]",
            State::Map => "}",
            State::None => "",
        }
    }

    /// The JSON text of the elements of `i`, as an array or object according to `s`.
    pub fn to_json_range<'a, I, T>(s: State, i: I) -> Result<Sstring, ExceptionPtr>
    where
        I: Iterator<Item = &'a T>,
        T: ToJsonIn + 'a + ?Sized,
    {
        // A buffer that grows amortized, made a string once.
        let mut res: Vec<u8> = Vec::new();
        res.extend_from_slice(Self::begin(s).as_bytes());
        for (n, x) in i.enumerate() {
            if n != 0 {
                res.push(b',');
            }
            res.extend_from_slice(x.to_json_in(s)?.as_bytes());
        }
        res.extend_from_slice(Self::end(s).as_bytes());
        Ok(Sstring::from_bytes(&res))
    }

    /// Streams the elements of `i` to `stream`, as an array or object according to `s`.
    ///
    /// The returned future refers to the stream's state, not to the stream
    /// handle: moving the stream is fine, and dropping it before the future resolves
    /// aborts instead of freeing the state under the pending writes. No element is
    /// formatted ahead of the stream, and nothing is allocated besides the continuations.
    pub fn write_range<I, T>(stream: &mut OutputStreamState<u8>, s: State, i: I) -> Future<()>
    where
        I: Iterator<Item = T> + 'static,
        T: JsonWriteIn + 'static,
    {
        let elements = stream.hold();
        let end = stream.hold();
        let mut first = true;
        stream.write_str(Self::begin(s)).then(move |()| {
            do_for_each(i, move |m: T| {
                // SAFETY: `elements` keeps the state alive (see `StateRef`); the stream's
                // operations run one at a time on the shard and the previous one has
                // resolved, so no other reference to the state is live.
                let stream = unsafe { &mut *elements.ptr() };
                if first {
                    first = false;
                    return m.write_in(stream, s);
                }
                let next = stream.hold();
                // SAFETY: `next` keeps the state alive; the separator write has resolved.
                stream.write_str(",").then(move |()| m.write_in(unsafe { &mut *next.ptr() }, s))
            })
            // SAFETY: `end` keeps the state alive; the elements are all written.
            .then(move |()| unsafe { &mut *end.ptr() }.write_str(Self::end(s)))
        })
    }
}

/// How an element is formatted inside a container (pairs
/// become `"key":value`, or `{"key":value}` inside an array; anything else falls back to
/// `ToJson`).
pub trait ToJsonIn {
    /// Formats `self` in state `s`.
    fn to_json_in(&self, s: State) -> Result<Sstring, ExceptionPtr>;
}

/// How an element is streamed inside a container (see [`ToJsonIn`]).
pub trait JsonWriteIn {
    /// Writes `self` in state `s`.
    ///
    /// The returned future may outlive the borrow of `stream`: it holds the stream's
    /// state, so dropping the stream before the future resolves aborts.
    fn write_in(self, stream: &mut OutputStreamState<u8>, s: State) -> Future<()>;
}

/// Rendering of a value as a JSON string.
pub trait ToJson {
    /// return a json formatted value
    fn to_json(&self) -> Result<Sstring, ExceptionPtr>;
}

/// Streaming of a value as JSON to an output stream.
pub trait JsonWrite {
    /// Writes the json formatted value to the stream.
    ///
    /// `self` is taken by value, so the value lives in the returned future for as long as
    /// the write needs it. The future holds the stream's state (see
    /// [`Formatter::write_range`]): dropping the stream before it resolves aborts.
    fn write_json(self, s: &mut OutputStreamState<u8>) -> Future<()>;
}

// fallback template
impl<T: ToJson + ?Sized> ToJsonIn for T {
    #[inline]
    fn to_json_in(&self, _: State) -> Result<Sstring, ExceptionPtr> {
        self.to_json()
    }
}

impl<T: JsonWrite> JsonWriteIn for T {
    #[inline]
    fn write_in(self, stream: &mut OutputStreamState<u8>, _: State) -> Future<()> {
        self.write_json(stream)
    }
}

/// A key-value pair (`is_pair_like`), as produced by iterating a map.
pub struct JsonPair<K, V>(pub K, pub V);

impl<K: ToJson, V: ToJson> ToJsonIn for JsonPair<K, V> {
    fn to_json_in(&self, s: State) -> Result<Sstring, ExceptionPtr> {
        if s == State::Array {
            let mut r = Sstring::from("{");
            r += &self.to_json_in(State::None)?;
            r += "}";
            return Ok(r);
        }
        let mut r = self.0.to_json()?;
        r += ":";
        r += &self.1.to_json()?;
        Ok(r)
    }
}

impl<K: ToJson + 'static, V: JsonWrite + 'static> JsonWriteIn for JsonPair<K, V> {
    fn write_in(self, stream: &mut OutputStreamState<u8>, s: State) -> Future<()> {
        // The continuations hold the stream's state (see `Formatter::write_range`).
        let next = stream.hold();
        if s == State::Array {
            let close = stream.hold();
            return stream.write_str("{").then(move |()| {
                // SAFETY: `next` keeps the state alive, and the opening write has resolved,
                // so no other reference to the state is live.
                self.write_in(unsafe { &mut *next.ptr() }, State::None)
                    // SAFETY: `close` keeps the state alive; the pair has been written.
                    .then(move |()| unsafe { &mut *close.ptr() }.write_str("}"))
            });
        }
        let JsonPair(key, value) = self;
        let mut k = match key.to_json() {
            Ok(k) => k,
            Err(e) => return crate::engine::core::future::make_exception_future(e),
        };
        k += ":";
        // SAFETY: `next` keeps the state alive; the key write has resolved.
        stream.write_sstring(&k).then(move |()| value.write_json(unsafe { &mut *next.ptr() }))
    }
}

#[inline]
fn is_control_char(c: u8) -> bool {
    c <= 0x1F
}

fn needs_escaping(str: &[u8]) -> bool {
    str.iter().any(|&c| is_control_char(c) || c == b'"' || c == b'\\')
}

fn string_view_to_json(str: &[u8]) -> Sstring {
    if !needs_escaping(str) {
        // The string between quotes, in one allocation of the final size.
        let mut r = Sstring::filled(str.len() + 2, b'"');
        r.as_bytes_mut()[1..=str.len()].copy_from_slice(str);
        return r;
    }
    // A buffer that grows amortized, made a string once.
    let mut oss: Vec<u8> = Vec::with_capacity(str.len() + 2);
    oss.push(b'"');
    for &c in str {
        match c {
            b'"' => {
                oss.extend_from_slice(b"\\\"");
            }
            b'\\' => {
                oss.extend_from_slice(b"\\\\");
            }
            0x08 => {
                oss.extend_from_slice(b"\\b");
            }
            0x0C => {
                oss.extend_from_slice(b"\\f");
            }
            b'\n' => {
                oss.extend_from_slice(b"\\n");
            }
            b'\r' => {
                oss.extend_from_slice(b"\\r");
            }
            b'\t' => {
                oss.extend_from_slice(b"\\t");
            }
            _ => {
                if is_control_char(c) {
                    // `\u00XX`: four uppercase hexadecimal digits, zero-padded.
                    const HEX: &[u8; 16] = b"0123456789ABCDEF";
                    let esc = [
                        b'\\',
                        b'u',
                        b'0',
                        b'0',
                        HEX[usize::from(c >> 4)],
                        HEX[usize::from(c & 0xF)],
                    ];
                    oss.extend_from_slice(&esc);
                } else {
                    oss.push(c);
                }
            }
        }
    }
    oss.push(b'"');
    Sstring::from_bytes(&oss)
}

/// Formats the string as a quoted, escaped JSON string.
impl ToJson for Sstring {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        Ok(string_view_to_json(self.as_bytes()))
    }
}

/// Formats the string as a quoted, escaped JSON string.
impl ToJson for str {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        Ok(string_view_to_json(self.as_bytes()))
    }
}

impl ToJson for &str {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        Ok(string_view_to_json(self.as_bytes()))
    }
}

/// Formats the bytes as a quoted, escaped JSON string; NUL bytes are escaped like any
/// other control character.
impl ToJson for [u8] {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        Ok(string_view_to_json(self))
    }
}

macro_rules! integer_to_json {
    ($($t:ty),*) => {$(
        /// return a json formatted integer
        impl ToJson for $t {
            #[inline]
            fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
                Ok(to_sstring(self))
            }
        }
    )*};
}
integer_to_json!(i32, u32, i64, u64);

/// The decimal exponent from which an `f32` is written in exponent notation:
/// `min(16, digits10 + 1)` with 6 decimal digits of precision.
const FLOAT_EXP_UPPER: i32 = 7;
/// The decimal exponent from which an `f64` is written in exponent notation:
/// `min(16, digits10 + 1)` with 15 decimal digits of precision.
const DOUBLE_EXP_UPPER: i32 = 16;
/// Below this decimal exponent a float is written in exponent notation.
const EXP_LOWER: i32 = -4;
/// Room for the longest `{}` of a double: sign, 17 digits, point, up to 4 leading zeros and
/// `e-308`, or up to 15 trailing zeros in fixed notation.
const FLOAT_BUF: usize = 48;

/// A fixed stack buffer written through `fmt::Write`, so formatting a number does not allocate.
struct StackBuf {
    buf: [u8; FLOAT_BUF],
    len: usize,
}

impl StackBuf {
    #[inline]
    const fn new() -> Self {
        Self { buf: [0; FLOAT_BUF], len: 0 }
    }

    #[inline]
    fn push(&mut self, bytes: &[u8]) {
        // The callers only push what fits in FLOAT_BUF (see its comment); a slice index panics
        // instead of writing out of bounds if that ever changed.
        self.buf[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }

    #[inline]
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl std::fmt::Write for StackBuf {
    #[inline]
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        if self.len + s.len() > FLOAT_BUF {
            return Err(std::fmt::Error);
        }
        self.push(s.as_bytes());
        Ok(())
    }
}

/// The JSON form of a finite `f32`/`f64`: the shortest decimal digits that read back as `v`
/// (taken from Rust's `{:e}`, which picks the shortest representation closest to the
/// exact value), written in fixed notation
/// when the decimal exponent `e` of the first digit satisfies `-4 <= e < exp_upper` and as
/// `d[.ddd]e±XX` otherwise, with a sign and at least two exponent digits (`1e+16`, `1e-05`).
/// Fixed notation adds no trailing `.0` (`3.0` gives `3`).
fn fmt_shortest<F: std::fmt::LowerExp>(v: F, exp_upper: i32) -> Sstring {
    use std::fmt::Write as _;
    let mut sci = StackBuf::new();
    // `{:e}` of a finite float is at most a sign, 17 digits, a point and `e-324`: it fits.
    let _ = write!(sci, "{:e}", v);
    let sci = sci.as_bytes();
    let (negative, body) = match sci.first() {
        Some(b'-') => (true, &sci[1..]),
        _ => (false, sci),
    };
    let e_pos = body.iter().position(|&c| c == b'e').unwrap_or(body.len());
    let (mantissa, exp_text) = (&body[..e_pos], body.get(e_pos + 1..).unwrap_or(&[]));
    let mut exp: i32 = 0;
    let (exp_negative, exp_digits) = match exp_text.first() {
        Some(b'-') => (true, &exp_text[1..]),
        _ => (false, exp_text),
    };
    for &d in exp_digits {
        exp = exp * 10 + i32::from(d - b'0');
    }
    if exp_negative {
        exp = -exp;
    }
    // The significand digits without the point: `mantissa` is `d` or `d.ddd`.
    let mut digits = [0u8; 20];
    let mut n = 0;
    for &c in mantissa {
        if c != b'.' {
            digits[n] = c;
            n += 1;
        }
    }
    let digits = &digits[..n];
    let n = n as i32;

    let mut out = StackBuf::new();
    if negative {
        out.push(b"-");
    }
    if exp < EXP_LOWER || exp >= exp_upper {
        out.push(&digits[..1]);
        if n > 1 {
            out.push(b".");
            out.push(&digits[1..]);
        }
        out.push(if exp < 0 { b"e-" } else { b"e+" });
        let abs = exp.unsigned_abs();
        if abs >= 100 {
            out.push(&[b'0' + (abs / 100) as u8]);
        }
        out.push(&[b'0' + (abs / 10 % 10) as u8, b'0' + (abs % 10) as u8]);
    } else if exp >= 0 {
        let int_len = exp + 1;
        if int_len >= n {
            out.push(digits);
            for _ in 0..int_len - n {
                out.push(b"0");
            }
        } else {
            out.push(&digits[..int_len as usize]);
            out.push(b".");
            out.push(&digits[int_len as usize..]);
        }
    } else {
        out.push(b"0.");
        for _ in 0..-exp - 1 {
            out.push(b"0");
        }
        out.push(digits);
    }
    Sstring::from_bytes(out.as_bytes())
}

/// Formats the number in its shortest round-trip form.
///
/// JSON cannot represent an infinite or NaN value: formatting one fails with an
/// out-of-range error (infinite) or an invalid-argument error (NaN).
impl ToJson for f32 {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        if self.is_infinite() {
            return Err(ExceptionPtr::new(crate::engine::core::future::OutOfRange::new(
                "Infinite float value is not supported".to_string(),
            )));
        } else if self.is_nan() {
            return Err(ExceptionPtr::new(crate::engine::core::future::InvalidArgument::new(
                "Invalid float value".to_string(),
            )));
        }
        Ok(fmt_shortest(*self, FLOAT_EXP_UPPER))
    }
}

/// Formats the number in its shortest round-trip form.
///
/// JSON cannot represent an infinite or NaN value: formatting one fails with an
/// out-of-range error (infinite) or an invalid-argument error (NaN).
impl ToJson for f64 {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        if self.is_infinite() {
            return Err(ExceptionPtr::new(crate::engine::core::future::OutOfRange::new(
                "Infinite double value is not supported".to_string(),
            )));
        } else if self.is_nan() {
            return Err(ExceptionPtr::new(crate::engine::core::future::InvalidArgument::new(
                "Invalid double value".to_string(),
            )));
        }
        Ok(fmt_shortest(*self, DOUBLE_EXP_UPPER))
    }
}

/// Formats the value as `true` or `false`.
impl ToJson for bool {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        Ok(Sstring::from(if *self { "true" } else { "false" }))
    }
}

/// Formats the date and time as a quoted RFC 3339 string (`%FT%TZ`), assuming it is in
/// UTC.
impl ToJson for DateTime {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        // use RFC3339/RFC8601 "internet format"
        // which is stipulated as mandatory for swagger
        // dates
        // Note that this assumes dates are in UTC timezone
        const TIME_FORMAT: &std::ffi::CStr = c"%FT%TZ";
        let mut buff = [0u8; 50];
        // SAFETY: buff holds 50 bytes; strftime writes at most that many.
        let n =
            unsafe { strftime(buff.as_mut_ptr() as *mut c_char, 50, TIME_FORMAT.as_ptr(), self) };
        let mut res = Sstring::from("\"");
        res.append_bytes(&buff[..n]);
        res += "\"";
        Ok(res)
    }
}

/// Formats the object with its own [`Jsonable::to_json`].
impl ToJson for dyn Jsonable {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        Jsonable::to_json(self)
    }
}

/// Formats the slice as a JSON array of its formatted elements.
impl<T: ToJsonIn> ToJson for [T] {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        Formatter::to_json_range(State::Array, self.iter())
    }
}

impl<T: ToJsonIn> ToJson for Vec<T> {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        Formatter::to_json_range(State::Array, self.iter())
    }
}

fn map_to_json<'a, K: ToJson + 'a, V: ToJson + 'a>(
    it: impl Iterator<Item = (&'a K, &'a V)>,
) -> Result<Sstring, ExceptionPtr> {
    // A buffer that grows amortized, made a string once.
    let mut res: Vec<u8> = Vec::new();
    res.extend_from_slice(Formatter::begin(State::Map).as_bytes());
    for (n, (k, v)) in it.enumerate() {
        if n != 0 {
            res.push(b',');
        }
        res.extend_from_slice(k.to_json()?.as_bytes());
        res.push(b':');
        res.extend_from_slice(v.to_json()?.as_bytes());
    }
    res.extend_from_slice(Formatter::end(State::Map).as_bytes());
    Ok(Sstring::from_bytes(&res))
}

impl<K: ToJson, V: ToJson> ToJson for BTreeMap<K, V> {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        map_to_json(self.iter())
    }
}

impl<K: ToJson, V: ToJson, S> ToJson for HashMap<K, V, S> {
    fn to_json(&self) -> Result<Sstring, ExceptionPtr> {
        map_to_json(self.iter())
    }
}

macro_rules! scalar_write {
    ($($t:ty),*) => {$(
        /// return a json formatted value written to the stream
        impl JsonWrite for $t {
            #[inline]
            fn write_json(self, s: &mut OutputStreamState<u8>) -> Future<()> {
                match ToJson::to_json(&self) {
                    Ok(j) => s.write_sstring(&j),
                    Err(e) => crate::engine::core::future::make_exception_future(e),
                }
            }
        }
    )*};
}
scalar_write!(Sstring, &str, i32, i64, u64, f32, f64, bool, DateTime);

/// Writes the vector to the stream as a JSON array, one element at a time.
///
/// The map implementations below write a JSON object instead, one key-value pair per
/// member. The returned future resolves when the write completes.
impl<T: JsonWriteIn + 'static> JsonWrite for Vec<T> {
    fn write_json(self, s: &mut OutputStreamState<u8>) -> Future<()> {
        Formatter::write_range(s, State::Array, self.into_iter())
    }
}

impl<K: ToJson + 'static, V: JsonWrite + 'static> JsonWrite for BTreeMap<K, V> {
    fn write_json(self, s: &mut OutputStreamState<u8>) -> Future<()> {
        Formatter::write_range(s, State::Map, self.into_iter().map(|(k, v)| JsonPair(k, v)))
    }
}

/// A hash map is written as a JSON object in the map's iteration order, which the hash
/// table decides.
impl<K: ToJson + 'static, V: JsonWrite + 'static, S: 'static> JsonWrite for HashMap<K, V, S> {
    fn write_json(self, s: &mut OutputStreamState<u8>) -> Future<()> {
        Formatter::write_range(s, State::Map, self.into_iter().map(|(k, v)| JsonPair(k, v)))
    }
}

/// Writes the object to the stream with its own [`Jsonable::write`], which takes the box
/// and keeps the object alive until the write resolves.
impl JsonWrite for Box<dyn Jsonable> {
    fn write_json(self, s: &mut OutputStreamState<u8>) -> Future<()> {
        self.write(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j<T: ToJson>(v: T) -> String {
        String::from_utf8(v.to_json().ok().map(|s| s.as_bytes().to_vec()).unwrap_or_default())
            .unwrap_or_default()
    }

    /// Integers, floats and booleans render in their plain JSON forms.
    #[test]
    fn simple_values() {
        assert_eq!(j(3i32), "3");
        assert_eq!(j(3.0f64), "3");
        assert_eq!(j(3.5f64), "3.5");
        assert_eq!(j(true), "true");
        assert_eq!(j(false), "false");
    }

    /// Floats render in the shortest form, switching to exponent notation at the limits.
    #[test]
    fn floats_shortest_form() {
        assert_eq!(j(1e16f64), "1e+16");
        assert_eq!(j(1e15f64), "1000000000000000");
        assert_eq!(j(1e-5f64), "1e-05");
        assert_eq!(j(1e-4f64), "0.0001");
        assert_eq!(j(1.5e20f64), "1.5e+20");
        assert_eq!(j(-2.5e-7f64), "-2.5e-07");
        assert_eq!(j(1e100f64), "1e+100");
        assert_eq!(j(5e-324f64), "5e-324");
        assert_eq!(j(0.1f64), "0.1");
        assert_eq!(j(0.0f64), "0");
        assert_eq!(j(-0.0f64), "-0");
        assert_eq!(j(123.456f64), "123.456");
        assert_eq!(j(1.7976931348623157e308f64), "1.7976931348623157e+308");
        assert_eq!(j(0.1f32), "0.1");
        assert_eq!(j(1e7f32), "1e+07");
        assert_eq!(j(1234567f32), "1234567");
        assert_eq!(j(3.4028235e38f32), "3.4028235e+38");
    }

    #[test]
    fn non_finite_fail() {
        assert!(f64::INFINITY.to_json().is_err());
        assert!(f64::NAN.to_json().is_err());
        assert!(f32::NEG_INFINITY.to_json().is_err());
    }
}
