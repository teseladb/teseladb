//! RFC 8259 parser with limits.

use std::collections::HashSet;

use crate::error::{Error, ErrorKind};
use crate::value::{Number, Object, Value};

/// Parser limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Maximum nesting depth (arrays and objects).
    pub max_depth: usize,
    /// Maximum text size in bytes.
    pub max_size: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { max_depth: 128, max_size: 64 << 20 }
    }
}

/// Number of keys from which an object uses a set to detect duplicates.
const DUP_SET_FROM: usize = 16;

/// Parses a JSON value with the default limits.
///
/// # Errors
///
/// The [`Error`] of the first fault, with its position: see [`ErrorKind`].
pub fn parse(input: &[u8]) -> Result<Value, Error> {
    parse_with(input, Limits::default())
}

/// Parses a JSON value with custom limits.
///
/// # Errors
///
/// The [`Error`] of the first fault, with its position: see [`ErrorKind`].
pub fn parse_with(input: &[u8], limits: Limits) -> Result<Value, Error> {
    if input.len() > limits.max_size {
        return Err(Error::at(ErrorKind::Size, input, limits.max_size));
    }
    if let Err(e) = std::str::from_utf8(input) {
        return Err(Error::at(ErrorKind::Utf8, input, e.valid_up_to()));
    }
    let mut p = Parser { s: input, pos: 0, limits };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.pos != input.len() {
        return Err(p.err(ErrorKind::Trailing));
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a [u8],
    pos: usize,
    limits: Limits,
}

impl Parser<'_> {
    fn err(&self, kind: ErrorKind) -> Error {
        Error::at(kind, self.s, self.pos)
    }

    /// Error at the current position: `Eof` if the text ran out, otherwise `kind`.
    fn err_here(&self, kind: ErrorKind) -> Error {
        self.err(if self.pos >= self.s.len() { ErrorKind::Eof } else { kind })
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.pos).copied()
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, b: u8) -> Result<(), Error> {
        if self.peek() == Some(b) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.err_here(ErrorKind::Unexpected))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Value, Error> {
        match self.peek() {
            Some(b'{') => self.object(depth + 1),
            Some(b'[') => self.array(depth + 1),
            Some(b'"') => self.string().map(Value::String),
            Some(b't') => self.literal(b"true", Value::Bool(true)),
            Some(b'f') => self.literal(b"false", Value::Bool(false)),
            Some(b'n') => self.literal(b"null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number().map(Value::Number),
            _ => Err(self.err_here(ErrorKind::Unexpected)),
        }
    }

    fn literal(&mut self, word: &[u8], v: Value) -> Result<Value, Error> {
        for &b in word {
            self.expect(b)?;
        }
        Ok(v)
    }

    fn enter(&self, depth: usize) -> Result<(), Error> {
        if depth > self.limits.max_depth { Err(self.err(ErrorKind::Depth)) } else { Ok(()) }
    }

    fn array(&mut self, depth: usize) -> Result<Value, Error> {
        self.enter(depth)?;
        self.pos += 1;
        let mut out = Vec::new();
        self.ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(out));
        }
        loop {
            self.ws();
            out.push(self.value(depth)?);
            self.ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(out));
                }
                _ => return Err(self.err_here(ErrorKind::Unexpected)),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Value, Error> {
        self.enter(depth)?;
        self.pos += 1;
        let mut out = Object::new();
        let mut seen: Option<HashSet<String>> = None;
        self.ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(out));
        }
        loop {
            self.ws();
            let key_at = self.pos;
            if self.peek() != Some(b'"') {
                return Err(self.err_here(ErrorKind::Unexpected));
            }
            let key = self.string()?;
            let dup = match &mut seen {
                Some(set) => !set.insert(key.clone()),
                None => out.get(&key).is_some(),
            };
            if dup {
                return Err(Error::at(ErrorKind::DuplicateKey, self.s, key_at));
            }
            self.ws();
            self.expect(b':')?;
            self.ws();
            let v = self.value(depth)?;
            out.push_unchecked(key, v);
            if seen.is_none() && out.len() >= DUP_SET_FROM {
                seen = Some(out.iter().map(|(k, _)| k.to_owned()).collect());
            }
            self.ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(out));
                }
                _ => return Err(self.err_here(ErrorKind::Unexpected)),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, Error> {
        let mut v = 0u32;
        for _ in 0..4 {
            let d = match self.peek() {
                Some(c @ b'0'..=b'9') => c - b'0',
                Some(c @ b'a'..=b'f') => c - b'a' + 10,
                Some(c @ b'A'..=b'F') => c - b'A' + 10,
                _ => return Err(self.err_here(ErrorKind::Escape)),
            };
            v = v * 16 + u32::from(d);
            self.pos += 1;
        }
        Ok(v)
    }

    fn string(&mut self) -> Result<String, Error> {
        self.pos += 1; // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c == b'"' || c == b'\\' || c < 0x20 {
                    break;
                }
                self.pos += 1;
            }
            out.extend_from_slice(&self.s[start..self.pos]);
            match self.peek() {
                Some(b'"') => {
                    self.pos += 1;
                    // Runs of valid UTF-8 separated by ASCII and already decoded escapes.
                    return Ok(String::from_utf8(out).expect("validated UTF-8"));
                }
                Some(b'\\') => {
                    let esc_at = self.pos;
                    self.pos += 1;
                    let c = self.peek();
                    self.pos += 1;
                    let ch = match c {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'b') => '\u{8}',
                        Some(b'f') => '\u{c}',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'u') => self.unicode(esc_at)?,
                        None => {
                            self.pos -= 1;
                            return Err(self.err(ErrorKind::Eof));
                        }
                        Some(_) => return Err(Error::at(ErrorKind::Escape, self.s, esc_at)),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                Some(_) => return Err(self.err(ErrorKind::Control)),
                None => return Err(self.err(ErrorKind::Eof)),
            }
        }
    }

    /// `\uXXXX` already consumed up to the `u`, with its pair if it is a high surrogate.
    fn unicode(&mut self, esc_at: usize) -> Result<char, Error> {
        let hi = self.hex4()?;
        let code = match hi {
            0xD800..=0xDBFF => {
                if self.s.get(self.pos..self.pos + 2) != Some(b"\\u") {
                    return Err(Error::at(ErrorKind::Escape, self.s, esc_at));
                }
                self.pos += 2;
                let lo = self.hex4()?;
                if !(0xDC00..=0xDFFF).contains(&lo) {
                    return Err(Error::at(ErrorKind::Escape, self.s, esc_at));
                }
                0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
            }
            0xDC00..=0xDFFF => return Err(Error::at(ErrorKind::Escape, self.s, esc_at)),
            c => c,
        };
        Ok(char::from_u32(code).expect("outside the surrogates"))
    }

    fn digits(&mut self) -> usize {
        let start = self.pos;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.pos += 1;
        }
        self.pos - start
    }

    fn number(&mut self) -> Result<Number, Error> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.err(ErrorKind::Number));
                }
            }
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.err_here(ErrorKind::Number)),
        }
        let mut float = false;
        if self.peek() == Some(b'.') {
            self.pos += 1;
            if self.digits() == 0 {
                return Err(self.err_here(ErrorKind::Number));
            }
            float = true;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if self.digits() == 0 {
                return Err(self.err_here(ErrorKind::Number));
            }
            float = true;
        }
        let text = std::str::from_utf8(&self.s[start..self.pos]).expect("ASCII");
        if float {
            // Outside the range of `f64` (e.g. 1e400) it cannot be represented.
            return match text.parse::<f64>() {
                Ok(v) if v.is_finite() => Ok(Number::Float(v)),
                _ => Err(Error::at(ErrorKind::Number, self.s, start)),
            };
        }
        Ok(text
            .parse::<i64>()
            .map(Number::Int)
            .or_else(|_| text.parse::<u64>().map(Number::UInt))
            .unwrap_or_else(|_| Number::Big(text.to_owned())))
    }
}
