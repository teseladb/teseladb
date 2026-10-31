//! The project's little-endian binary codec: fixed-width integers, canonical LEB128 and
//! length-prefixed bytes.
//!
//! The decoder never panics on arbitrary input: every error is a [`DecodeError`].

use core::fmt;

/// Decoding error, with the offset where it was detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// Missing bytes: `needed` more were required from `offset`.
    UnexpectedEof {
        /// Read offset.
        offset: usize,
        /// Bytes that were missing.
        needed: usize,
    },
    /// Varint longer than 10 bytes, overflowing u64 or not canonical.
    BadVarint {
        /// Offset of the varint's first byte.
        offset: usize,
    },
    /// A value is not valid for the field (boolean other than 0/1, unknown tag…).
    InvalidValue {
        /// Offset of the value.
        offset: usize,
    },
    /// Invalid UTF-8 in a string.
    BadUtf8 {
        /// Offset of the start of the string.
        offset: usize,
    },
    /// Bytes left over at the end.
    TrailingBytes {
        /// Offset of the first leftover byte.
        offset: usize,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::UnexpectedEof { offset, needed } => {
                write!(f, "unexpected end at {offset}: {needed} bytes missing")
            }
            DecodeError::BadVarint { offset } => write!(f, "invalid varint at {offset}"),
            DecodeError::InvalidValue { offset } => write!(f, "invalid value at {offset}"),
            DecodeError::BadUtf8 { offset } => write!(f, "invalid UTF-8 at {offset}"),
            DecodeError::TrailingBytes { offset } => write!(f, "trailing bytes from {offset}"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Writer over a `Vec<u8>`.
#[derive(Debug, Default, Clone)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    /// Empty encoder.
    pub fn new() -> Encoder {
        Encoder::default()
    }

    /// Encoder with reserved capacity.
    pub fn with_capacity(n: usize) -> Encoder {
        Encoder { buf: Vec::with_capacity(n) }
    }

    /// Bytes written so far.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Number of bytes written.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Tells whether nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Returns the buffer.
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    /// `u8`.
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }

    /// Boolean as a `u8` 0/1.
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.u8(u8::from(v))
    }

    /// `u16` little-endian.
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// `u32` little-endian.
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// `u64` little-endian.
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// `u128` little-endian.
    pub fn u128(&mut self, v: u128) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// `i16` little-endian.
    pub fn i16(&mut self, v: i16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// `i64` little-endian.
    pub fn i64(&mut self, v: i64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }

    /// Unsigned integer in LEB128 (1–10 bytes, canonical form).
    pub fn var(&mut self, mut v: u64) -> &mut Self {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(byte);
                return self;
            }
            self.buf.push(byte | 0x80);
        }
    }

    /// Raw bytes without a length.
    pub fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(b);
        self
    }

    /// Bytes prefixed with their LEB128 length.
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.var(b.len() as u64);
        self.raw(b)
    }

    /// UTF-8 string prefixed with its LEB128 length.
    pub fn str(&mut self, s: &str) -> &mut Self {
        self.bytes(s.as_bytes())
    }
}

/// Reader over a slice. Each read advances; on error the position does not change.
#[derive(Debug, Clone)]
pub struct Decoder<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    /// Decoder at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Decoder<'a> {
        Decoder { buf, pos: 0 }
    }

    /// Current position.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes left to read.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Error if unread bytes remain.
    pub fn finish(&self) -> Result<(), DecodeError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(DecodeError::TrailingBytes { offset: self.pos })
        }
    }

    /// Reads `n` raw bytes.
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.remaining() < n {
            return Err(DecodeError::UnexpectedEof { offset: self.pos, needed: n });
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.raw(N)?);
        Ok(a)
    }

    /// `u8`.
    pub fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.array::<1>()?[0])
    }

    /// Boolean (only 0 or 1).
    pub fn bool(&mut self) -> Result<bool, DecodeError> {
        let at = self.pos;
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => {
                self.pos = at;
                Err(DecodeError::InvalidValue { offset: at })
            }
        }
    }

    /// `u16` little-endian.
    pub fn u16(&mut self) -> Result<u16, DecodeError> {
        self.array().map(u16::from_le_bytes)
    }

    /// `u32` little-endian.
    pub fn u32(&mut self) -> Result<u32, DecodeError> {
        self.array().map(u32::from_le_bytes)
    }

    /// `u64` little-endian.
    pub fn u64(&mut self) -> Result<u64, DecodeError> {
        self.array().map(u64::from_le_bytes)
    }

    /// `u128` little-endian.
    pub fn u128(&mut self) -> Result<u128, DecodeError> {
        self.array().map(u128::from_le_bytes)
    }

    /// `i16` little-endian.
    pub fn i16(&mut self) -> Result<i16, DecodeError> {
        self.array().map(i16::from_le_bytes)
    }

    /// `i64` little-endian.
    pub fn i64(&mut self) -> Result<i64, DecodeError> {
        self.array().map(i64::from_le_bytes)
    }

    /// Unsigned LEB128, at most 10 bytes, no padding bytes and no u64 overflow.
    pub fn var(&mut self) -> Result<u64, DecodeError> {
        let start = self.pos;
        let mut v: u64 = 0;
        for i in 0..10 {
            let Some(&byte) = self.buf.get(start + i) else {
                return Err(DecodeError::UnexpectedEof { offset: start + i, needed: 1 });
            };
            let part = u64::from(byte & 0x7f);
            // The tenth byte can only contribute bit 63.
            if i == 9 && part > 1 {
                return Err(DecodeError::BadVarint { offset: start });
            }
            v |= part << (7 * i);
            if byte & 0x80 == 0 {
                // Canonical: the last byte cannot be zero unless it is the only one.
                if byte == 0 && i > 0 {
                    return Err(DecodeError::BadVarint { offset: start });
                }
                self.pos = start + i + 1;
                return Ok(v);
            }
        }
        Err(DecodeError::BadVarint { offset: start })
    }

    /// LEB128 length converted to `usize`, checking that that many bytes are available.
    fn len_prefix(&mut self) -> Result<usize, DecodeError> {
        let at = self.pos;
        let n = self.var()?;
        match usize::try_from(n) {
            Ok(n) if n <= self.remaining() => Ok(n),
            _ => {
                let needed = usize::try_from(n).unwrap_or(usize::MAX);
                self.pos = at;
                Err(DecodeError::UnexpectedEof { offset: at, needed })
            }
        }
    }

    /// Bytes prefixed with their length.
    pub fn bytes(&mut self) -> Result<&'a [u8], DecodeError> {
        let n = self.len_prefix()?;
        self.raw(n)
    }

    /// UTF-8 string prefixed with its length.
    pub fn str(&mut self) -> Result<&'a str, DecodeError> {
        let at = self.pos;
        let b = self.bytes()?;
        core::str::from_utf8(b).map_err(|_| {
            self.pos = at;
            DecodeError::BadUtf8 { offset: at }
        })
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_types() {
        let mut e = Encoder::with_capacity(64);
        assert!(e.is_empty());
        e.u8(7)
            .bool(true)
            .bool(false)
            .u16(0xbeef)
            .u32(0xdead_beef)
            .u64(u64::MAX)
            .u128(1 << 100)
            .i16(-2)
            .i64(i64::MIN)
            .var(0)
            .var(300)
            .var(u64::MAX)
            .bytes(b"abc")
            .str("naïve café")
            .raw(&[9, 9]);
        assert!(!e.is_empty());
        let len = e.len();
        let v = e.clone().finish();
        assert_eq!(e.as_slice(), &v[..]);
        assert_eq!(v.len(), len);
        let mut d = Decoder::new(&v);
        assert_eq!(d.u8().unwrap(), 7);
        assert!(d.bool().unwrap());
        assert!(!d.bool().unwrap());
        assert_eq!(d.u16().unwrap(), 0xbeef);
        assert_eq!(d.u32().unwrap(), 0xdead_beef);
        assert_eq!(d.u64().unwrap(), u64::MAX);
        assert_eq!(d.u128().unwrap(), 1 << 100);
        assert_eq!(d.i16().unwrap(), -2);
        assert_eq!(d.i64().unwrap(), i64::MIN);
        assert_eq!(d.var().unwrap(), 0);
        assert_eq!(d.var().unwrap(), 300);
        assert_eq!(d.var().unwrap(), u64::MAX);
        assert_eq!(d.bytes().unwrap(), b"abc");
        assert_eq!(d.str().unwrap(), "naïve café");
        assert_eq!(d.raw(2).unwrap(), &[9, 9]);
        assert_eq!(d.position(), v.len());
        d.finish().unwrap();
    }

    #[test]
    fn varint_lengths() {
        for (v, n) in [(0u64, 1usize), (127, 1), (128, 2), (16383, 2), (16384, 3), (u64::MAX, 10)] {
            let mut e = Encoder::new();
            e.var(v);
            assert_eq!(e.len(), n, "length of {v}");
        }
    }

    #[test]
    fn rejects_non_canonical_and_overflowing_varints() {
        // 0 with padding.
        assert_eq!(Decoder::new(&[0x80, 0x00]).var(), Err(DecodeError::BadVarint { offset: 0 }));
        // 11 continuation bytes.
        assert_eq!(Decoder::new(&[0xff; 11]).var(), Err(DecodeError::BadVarint { offset: 0 }));
        // Ten bytes with continuation even though the last one only contributes bit 63.
        let mut cont = [0xffu8; 10];
        cont[9] = 0x81;
        assert_eq!(Decoder::new(&cont).var(), Err(DecodeError::BadVarint { offset: 0 }));
        // Tenth byte with more than one bit.
        let mut over = [0xffu8; 10];
        over[9] = 0x02;
        assert_eq!(Decoder::new(&over).var(), Err(DecodeError::BadVarint { offset: 0 }));
        // Truncated.
        assert_eq!(
            Decoder::new(&[0x80]).var(),
            Err(DecodeError::UnexpectedEof { offset: 1, needed: 1 })
        );
    }

    #[test]
    fn errors_do_not_advance_and_never_panic() {
        let mut d = Decoder::new(&[2, 0xff]);
        assert_eq!(d.bool(), Err(DecodeError::InvalidValue { offset: 0 }));
        assert_eq!(d.position(), 0);
        assert_eq!(d.u32(), Err(DecodeError::UnexpectedEof { offset: 0, needed: 4 }));
        assert_eq!(d.finish(), Err(DecodeError::TrailingBytes { offset: 0 }));

        // Length beyond what is available.
        let mut d = Decoder::new(&[5, b'a']);
        assert_eq!(d.bytes(), Err(DecodeError::UnexpectedEof { offset: 0, needed: 5 }));
        assert_eq!(d.position(), 0);

        // Invalid UTF-8.
        let mut d = Decoder::new(&[1, 0xff]);
        assert_eq!(d.str(), Err(DecodeError::BadUtf8 { offset: 0 }));
        assert_eq!(d.position(), 0);

        // Huge length (does not fit in the input).
        let mut e = Encoder::new();
        e.var(u64::MAX);
        let v = e.finish();
        assert!(matches!(Decoder::new(&v).bytes(), Err(DecodeError::UnexpectedEof { .. })));

        // Arbitrary inputs: no read panics.
        let mut rng = crate::Rng::new(1);
        for _ in 0..2000 {
            let n = (rng.next_u64() % 24) as usize;
            let buf: Vec<u8> = (0..n).map(|_| rng.next_u64() as u8).collect();
            let mut d = Decoder::new(&buf);
            let _ = d.var();
            let _ = d.str();
            let _ = d.bytes();
            let _ = d.u64();
        }
    }

    #[test]
    fn display_messages() {
        let cases = [
            DecodeError::UnexpectedEof { offset: 1, needed: 2 },
            DecodeError::BadVarint { offset: 3 },
            DecodeError::InvalidValue { offset: 4 },
            DecodeError::BadUtf8 { offset: 5 },
            DecodeError::TrailingBytes { offset: 6 },
        ];
        for c in cases {
            assert!(!c.to_string().is_empty());
        }
    }
}
