//! Decoding a key back to its values (FR-009): the same structure the encoding writes, read
//! through the type bits to restore each value's exact type. A descending field is read with its
//! bytes inverted back. Keys of a collation cannot be decoded: the sort key is not the string.

use std::fmt;

use teseladb_bson::{Decimal128, Document, Value};

use crate::encode::Direction;
use crate::typebits::{DECIMAL, DECIMAL_RAW, DOUBLE, INT32, INT64, STRING, SYMBOL, TypeBits};

/// A key or type bits that do not decode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodeError(&'static str);

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the key does not decode: {}", self.0)
    }
}

impl std::error::Error for DecodeError {}

/// The values of a key, one per field in `dirs`.
pub fn decode(key: &[u8], bits: &TypeBits, dirs: &[Direction]) -> Result<Vec<Value>, DecodeError> {
    read(key, bits.as_bytes(), dirs, false)
}

/// The values of a key without its type bits, one per field in `dirs`: a number comes back as
/// an `int` when it is a whole number that fits, a `long` when it fits that, and a `double`
/// otherwise, and a string as a string (018 FR-006: a range's bounds in shard key values). A
/// value that cannot be read without its bits (a NaN, a decimal) fails.
pub fn decode_untyped(key: &[u8], dirs: &[Direction]) -> Result<Vec<Value>, DecodeError> {
    read(key, &[], dirs, true)
}

fn read(
    key: &[u8],
    bits: &[u8],
    dirs: &[Direction],
    untyped: bool,
) -> Result<Vec<Value>, DecodeError> {
    let mut r = Reader { key, pos: 0, invert: 0, bits, bit: 0, untyped };
    let values = dirs
        .iter()
        .map(|dir| {
            r.invert = if *dir == Direction::Descending { 0xFF } else { 0 };
            let class = r.byte()?;
            r.value(class)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if r.pos != key.len() || r.bit != r.bits.len() {
        return Err(DecodeError("bytes left over"));
    }
    Ok(values)
}

struct Reader<'a> {
    key: &'a [u8],
    pos: usize,
    /// `0xFF` while reading a descending field.
    invert: u8,
    bits: &'a [u8],
    bit: usize,
    /// Reading without type bits ([`decode_untyped`]).
    untyped: bool,
}

/// The tag [`Reader::tag`] gives without type bits.
const UNTYPED: u8 = 0xFF;

impl Reader<'_> {
    fn byte(&mut self) -> Result<u8, DecodeError> {
        let b = *self.key.get(self.pos).ok_or(DecodeError("key cut short"))?;
        self.pos += 1;
        Ok(b ^ self.invert)
    }

    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut out = [0; N];
        for b in &mut out {
            *b = self.byte()?;
        }
        Ok(out)
    }

    fn tag(&mut self) -> Result<u8, DecodeError> {
        if self.untyped {
            return Ok(UNTYPED);
        }
        let t = *self.bits.get(self.bit).ok_or(DecodeError("type bits cut short"))?;
        self.bit += 1;
        Ok(t)
    }

    fn bits_n<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let s = self.bits.get(self.bit..self.bit + N).ok_or(DecodeError("type bits cut short"))?;
        self.bit += N;
        Ok(s.try_into().expect("N bytes"))
    }

    /// The value of class byte `class`.
    fn value(&mut self, class: u8) -> Result<Value, DecodeError> {
        Ok(match class.checked_sub(0x10).ok_or(DecodeError("not a class byte"))? {
            0 => Value::MinKey,
            1 => Value::Undefined,
            2 => Value::Null,
            3 => self.number()?,
            4 => {
                let s = self.text()?;
                match self.tag()? {
                    STRING | UNTYPED => Value::String(s),
                    SYMBOL => Value::Symbol(s),
                    _ => return Err(DecodeError("a string's type bits")),
                }
            }
            5 => Value::Document(self.document()?),
            6 => {
                let mut items = Vec::new();
                loop {
                    match self.byte()? {
                        0 => break,
                        c => items.push(self.value(c)?),
                    }
                }
                Value::Array(items)
            }
            7 => {
                let stored = u32::from_be_bytes(self.bytes()?) as usize;
                let subtype = self.byte()?;
                let inner = if subtype == teseladb_bson::types::BINARY_OLD { 4 } else { 0 };
                let len = stored
                    .checked_sub(inner)
                    .ok_or(DecodeError("old binary shorter than its length"))?;
                let bytes = (0..len).map(|_| self.byte()).collect::<Result<_, _>>()?;
                Value::Binary { subtype, bytes }
            }
            8 => Value::ObjectId(self.bytes()?),
            9 => Value::Boolean(self.byte()? == 1),
            10 => Value::DateTime((u64::from_be_bytes(self.bytes()?) ^ (1 << 63)) as i64),
            11 => {
                let time = u32::from_be_bytes(self.bytes()?);
                Value::Timestamp { time, increment: u32::from_be_bytes(self.bytes()?) }
            }
            12 => Value::Regex { pattern: self.text()?, options: self.text()? },
            13 => {
                let len = u32::from_be_bytes(self.bytes()?) as usize;
                let ns = (0..len).map(|_| self.byte()).collect::<Result<Vec<_>, _>>()?;
                let namespace = String::from_utf8(ns).map_err(|_| DecodeError("not UTF-8"))?;
                Value::DbPointer { namespace, id: self.bytes()? }
            }
            14 => Value::JavaScript(self.text()?),
            15 => Value::JavaScriptWithScope { code: self.text()?, scope: self.document()? },
            16 => Value::MaxKey,
            _ => return Err(DecodeError("not a class byte")),
        })
    }

    fn document(&mut self) -> Result<Document, DecodeError> {
        let mut d = Document::new();
        loop {
            match self.byte()? {
                0 => return Ok(d),
                class => {
                    let name = self.text()?;
                    let v = self.value(class)?;
                    d.push(name, v);
                }
            }
        }
    }

    /// Text up to `00 00`, with `00 FF` back to `00`.
    fn text(&mut self) -> Result<String, DecodeError> {
        let mut out = Vec::new();
        loop {
            match self.byte()? {
                0 => match self.byte()? {
                    0 => break,
                    0xFF => out.push(0),
                    _ => return Err(DecodeError("a lone 00 in text")),
                },
                b => out.push(b),
            }
        }
        String::from_utf8(out).map_err(|_| DecodeError("not UTF-8"))
    }

    fn number(&mut self) -> Result<Value, DecodeError> {
        // Sub-class, then for finite values the exponent and the digits (inverted if negative).
        let sub = self.byte()?;
        let finite = match sub {
            0x03 | 0x05 => {
                let negative = sub == 0x03;
                let flip = if negative { 0xFF } else { 0 };
                let [hi, lo] = self.bytes::<2>()?;
                let exponent = i32::from(u16::from_be_bytes([hi ^ flip, lo ^ flip])) - 0x8000;
                let mut digits = Vec::new();
                loop {
                    match self.byte()? ^ flip {
                        0 => break,
                        b @ 1..=100 => digits.extend([(b - 1) / 10, (b - 1) % 10]),
                        _ => return Err(DecodeError("not a digit pair")),
                    }
                }
                while digits.last() == Some(&0) {
                    digits.pop();
                }
                Some((negative, digits, exponent))
            }
            0x01 | 0x02 | 0x04 | 0x06 => None,
            _ => return Err(DecodeError("not a number")),
        };
        let tag = match self.tag()? {
            UNTYPED => match (sub, &finite) {
                (0x04, _) => INT32,
                (_, Some((negative, digits, exponent))) => {
                    match integer(*negative, digits, *exponent) {
                        Ok(n) if i32::try_from(n).is_ok() => INT32,
                        Ok(_) => INT64,
                        Err(_) => DOUBLE,
                    }
                }
                _ => DOUBLE,
            },
            t => t,
        };
        let text = |negative: bool, digits: &[u8], exponent: i32| {
            let d: String = digits.iter().map(|&x| char::from(b'0' + x)).collect();
            format!("{}0.{d}E{exponent}", if negative { "-" } else { "" })
        };
        Ok(match (tag, sub, finite) {
            (INT32 | INT64, 0x04, _) => {
                if tag == INT32 {
                    Value::Int32(0)
                } else {
                    Value::Int64(0)
                }
            }
            (INT32 | INT64, _, Some((negative, digits, exponent))) => {
                let n = integer(negative, &digits, exponent)?;
                if tag == INT32 {
                    Value::Int32(
                        i32::try_from(n).map_err(|_| DecodeError("an int32 out of range"))?,
                    )
                } else {
                    Value::Int64(n)
                }
            }
            (DOUBLE, 0x01, _) => Value::Double(f64::from_bits(u64::from_be_bytes(self.bits_n()?))),
            (DOUBLE, 0x02, _) => Value::Double(f64::NEG_INFINITY),
            (DOUBLE, 0x06, _) => Value::Double(f64::INFINITY),
            (DOUBLE, 0x04, _) => Value::Double(if self.bits_n::<1>()? == [1] { -0.0 } else { 0.0 }),
            (DOUBLE, _, Some((negative, digits, exponent))) => {
                // The digits are the double's exact value: parsing them gives it back.
                Value::Double(
                    text(negative, &digits, exponent)
                        .parse()
                        .map_err(|_| DecodeError("a double"))?,
                )
            }
            (DECIMAL, _, finite) => {
                let original = i32::from(i16::from_be_bytes(self.bits_n()?));
                let d = match finite {
                    None => {
                        let negative = self.bits_n::<1>()? == [1];
                        Decimal128::from_parts(negative, 0, original)
                    }
                    Some((negative, digits, exponent)) => {
                        let coefficient =
                            scaled(&digits, exponent - digits.len() as i32 - original)?;
                        Decimal128::from_parts(negative, coefficient, original)
                    }
                };
                Value::Decimal128(d.ok_or(DecodeError("a decimal out of range"))?)
            }
            (DECIMAL_RAW, _, _) => Value::Decimal128(Decimal128::from_le_bytes(self.bits_n()?)),
            _ => return Err(DecodeError("a number's type bits")),
        })
    }
}

/// The integer `±0.digits × 10^exponent`.
fn integer(negative: bool, digits: &[u8], exponent: i32) -> Result<i64, DecodeError> {
    let shift = exponent - digits.len() as i32;
    let magnitude = scaled(digits, shift)?;
    let n = if negative { -(magnitude as i128) } else { magnitude as i128 };
    i64::try_from(n).map_err(|_| DecodeError("an integer out of range"))
}

/// `digits × 10^shift` as an integer (`shift` not negative).
fn scaled(digits: &[u8], shift: i32) -> Result<u128, DecodeError> {
    let shift = u32::try_from(shift).map_err(|_| DecodeError("not an integer"))?;
    let mut n: u128 = 0;
    for &d in digits {
        n = n
            .checked_mul(10)
            .and_then(|n| n.checked_add(u128::from(d)))
            .ok_or(DecodeError("too many digits"))?;
    }
    n.checked_mul(10u128.checked_pow(shift).ok_or(DecodeError("too many digits"))?)
        .ok_or(DecodeError("too many digits"))
}

/// How many bytes of `key` its first values take, one per field in `dirs`: where the primary key
/// of an index entry starts (009, the duplicates of an index with `prepareUnique`). It reads the
/// structure alone, so it also measures strings written in a collation's order.
pub fn values_len(key: &[u8], dirs: &[Direction]) -> Result<usize, DecodeError> {
    let mut s = Skipper { key, pos: 0, invert: 0 };
    for dir in dirs {
        s.invert = if *dir == Direction::Descending { 0xFF } else { 0 };
        let class = s.byte()?;
        s.value(class)?;
    }
    Ok(s.pos)
}

/// Walks the bytes of encoded values without their type bits.
struct Skipper<'a> {
    key: &'a [u8],
    pos: usize,
    /// `0xFF` while reading a descending field.
    invert: u8,
}

impl Skipper<'_> {
    fn byte(&mut self) -> Result<u8, DecodeError> {
        let b = *self.key.get(self.pos).ok_or(DecodeError("key cut short"))?;
        self.pos += 1;
        Ok(b ^ self.invert)
    }

    fn skip(&mut self, n: usize) -> Result<(), DecodeError> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.key.len());
        self.pos = end.ok_or(DecodeError("key cut short"))?;
        Ok(())
    }

    fn length(&mut self) -> Result<usize, DecodeError> {
        let mut b = [0; 4];
        for x in &mut b {
            *x = self.byte()?;
        }
        Ok(u32::from_be_bytes(b) as usize)
    }

    fn value(&mut self, class: u8) -> Result<(), DecodeError> {
        match class.checked_sub(0x10).ok_or(DecodeError("not a class byte"))? {
            0..=2 | 16 => Ok(()),
            3 => self.number(),
            4 | 14 => self.text(),
            5 => self.document(),
            6 => loop {
                match self.byte()? {
                    0 => return Ok(()),
                    c => self.value(c)?,
                }
            },
            7 => {
                let stored = self.length()?;
                let subtype = self.byte()?;
                let inner = if subtype == teseladb_bson::types::BINARY_OLD { 4 } else { 0 };
                let len = stored
                    .checked_sub(inner)
                    .ok_or(DecodeError("old binary shorter than its length"))?;
                self.skip(len)
            }
            8 => self.skip(12),
            9 => self.skip(1),
            10 | 11 => self.skip(8),
            12 => {
                self.text()?;
                self.text()
            }
            13 => {
                let len = self.length()?;
                self.skip(len + 12)
            }
            15 => {
                self.text()?;
                self.document()
            }
            _ => Err(DecodeError("not a class byte")),
        }
    }

    fn document(&mut self) -> Result<(), DecodeError> {
        loop {
            match self.byte()? {
                0 => return Ok(()),
                class => {
                    self.text()?;
                    self.value(class)?;
                }
            }
        }
    }

    fn text(&mut self) -> Result<(), DecodeError> {
        loop {
            if self.byte()? == 0 {
                match self.byte()? {
                    0 => return Ok(()),
                    0xFF => {}
                    _ => return Err(DecodeError("a lone 00 in text")),
                }
            }
        }
    }

    fn number(&mut self) -> Result<(), DecodeError> {
        match self.byte()? {
            sub @ (0x03 | 0x05) => {
                let flip = if sub == 0x03 { 0xFF } else { 0 };
                self.skip(2)?;
                while self.byte()? ^ flip != 0 {}
                Ok(())
            }
            0x01 | 0x02 | 0x04 | 0x06 => Ok(()),
            _ => Err(DecodeError("not a number")),
        }
    }
}
