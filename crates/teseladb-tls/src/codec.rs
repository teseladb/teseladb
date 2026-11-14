//! The TLS presentation language (RFC 8446, section 3): big-endian integers and vectors with
//! 1-, 2- or 3-byte length prefixes.

use crate::{Alert, Error};

/// A reader that fails with `decode_error` on any short or trailing input.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

fn decode() -> Error {
    Error::Local(Alert::DecodeError)
}

impl<'a> Reader<'a> {
    /// A reader over `data`.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    /// The bytes not yet read.
    #[must_use]
    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    /// Whether everything has been read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pos == self.data.len()
    }

    /// The next `n` bytes.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::DecodeError`] when fewer than `n` bytes remain.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len()).ok_or_else(decode)?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    /// A `uint8`.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::DecodeError`] when no byte remains.
    pub fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.bytes(1)?[0])
    }

    /// A `uint16`.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::DecodeError`] when fewer than 2 bytes remain.
    pub fn u16(&mut self) -> Result<u16, Error> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    /// A `uint24`.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::DecodeError`] when fewer than 3 bytes remain.
    pub fn u24(&mut self) -> Result<u32, Error> {
        let b = self.bytes(3)?;
        Ok(u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }

    /// A `uint32`.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::DecodeError`] when fewer than 4 bytes remain.
    pub fn u32(&mut self) -> Result<u32, Error> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A vector with a 1-byte length.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::DecodeError`] when the vector or its length does not fit what remains.
    pub fn vec8(&mut self) -> Result<&'a [u8], Error> {
        let n = usize::from(self.u8()?);
        self.bytes(n)
    }

    /// A vector with a 2-byte length.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::DecodeError`] when the vector or its length does not fit what remains.
    pub fn vec16(&mut self) -> Result<&'a [u8], Error> {
        let n = usize::from(self.u16()?);
        self.bytes(n)
    }

    /// A vector with a 3-byte length.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::DecodeError`] when the vector or its length does not fit what remains.
    pub fn vec24(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u24()? as usize;
        self.bytes(n)
    }

    /// Fails unless everything has been read.
    ///
    /// # Errors
    ///
    /// [`Error::Local`] with [`Alert::DecodeError`] when bytes remain.
    pub fn finish(&self) -> Result<(), Error> {
        if self.is_empty() { Ok(()) } else { Err(decode()) }
    }
}

/// Appends a `uint16`.
pub fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Appends a `uint24`.
///
/// # Panics
///
/// When `v` is 2²⁴ or more: a truncated length would make the message corrupt.
pub fn put_u24(out: &mut Vec<u8>, v: usize) {
    let v =
        u32::try_from(v).ok().filter(|&n| n < 1 << 24).expect("a TLS uint24 holds less than 2^24");
    out.extend_from_slice(&v.to_be_bytes()[1..]);
}

/// Appends a vector with a 1-byte length.
///
/// # Panics
///
/// When `v` is longer than 255 bytes: a truncated length would make the message corrupt.
pub fn put_vec8(out: &mut Vec<u8>, v: &[u8]) {
    out.push(u8::try_from(v.len()).expect("a TLS vector with a 1-byte length"));
    out.extend_from_slice(v);
}

/// Appends a vector with a 2-byte length.
///
/// # Panics
///
/// When `v` is longer than 65 535 bytes: a truncated length would make the message corrupt.
pub fn put_vec16(out: &mut Vec<u8>, v: &[u8]) {
    put_u16(out, u16::try_from(v.len()).expect("a TLS vector with a 2-byte length"));
    out.extend_from_slice(v);
}

/// Appends a vector with a 3-byte length.
///
/// # Panics
///
/// As [`put_u24`].
pub fn put_vec24(out: &mut Vec<u8>, v: &[u8]) {
    put_u24(out, v.len());
    out.extend_from_slice(v);
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn lengths_that_do_not_fit_their_field_are_refused() {
        let at_most = |f: fn(&mut Vec<u8>, &[u8]), n: usize| {
            let mut out = Vec::new();
            f(&mut out, &vec![0; n]);
            out.len()
        };
        assert_eq!(at_most(put_vec8, 255), 256);
        assert_eq!(at_most(put_vec16, 65_535), 65_537);
        for (f, n) in
            [(put_vec8 as fn(&mut Vec<u8>, &[u8]), 256), (put_vec16, 65_536), (put_vec24, 1 << 24)]
        {
            assert!(std::panic::catch_unwind(|| at_most(f, n)).is_err(), "{n}");
        }
        let mut out = Vec::new();
        put_u24(&mut out, (1 << 24) - 1);
        assert_eq!(out, [0xff; 3]);
    }

    #[test]
    fn integers_and_vectors() {
        let mut out = Vec::new();
        put_vec8(&mut out, &[1]);
        put_vec16(&mut out, &[2, 3]);
        put_vec24(&mut out, &[4]);
        out.extend_from_slice(&7u32.to_be_bytes());
        let mut r = Reader::new(&out);
        assert_eq!(r.vec8().expect("vec8 succeeds"), &[1]);
        assert_eq!(r.vec16().expect("vec16 succeeds"), &[2, 3]);
        assert_eq!(r.vec24().expect("vec24 succeeds"), &[4]);
        assert_eq!(r.u32().expect("u32 succeeds"), 7);
        assert!(r.finish().is_ok());
        assert_eq!(r.u8(), Err(decode()));
        let mut short = Reader::new(&[0, 5, 1]);
        assert_eq!(short.vec16(), Err(decode()));
        assert_eq!(Reader::new(&[1]).finish(), Err(decode()));
        assert_eq!(Reader::new(&[1, 2]).rest(), &[1, 2]);
    }
}
