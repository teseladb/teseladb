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
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    /// The bytes not yet read.
    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.pos..]
    }

    /// Whether everything has been read.
    pub fn is_empty(&self) -> bool {
        self.pos == self.data.len()
    }

    /// The next `n` bytes.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.data.len()).ok_or_else(decode)?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    /// A `uint8`.
    pub fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.bytes(1)?[0])
    }

    /// A `uint16`.
    pub fn u16(&mut self) -> Result<u16, Error> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    /// A `uint24`.
    pub fn u24(&mut self) -> Result<u32, Error> {
        let b = self.bytes(3)?;
        Ok(u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }

    /// A `uint32`.
    pub fn u32(&mut self) -> Result<u32, Error> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// A vector with a 1-byte length.
    pub fn vec8(&mut self) -> Result<&'a [u8], Error> {
        let n = usize::from(self.u8()?);
        self.bytes(n)
    }

    /// A vector with a 2-byte length.
    pub fn vec16(&mut self) -> Result<&'a [u8], Error> {
        let n = usize::from(self.u16()?);
        self.bytes(n)
    }

    /// A vector with a 3-byte length.
    pub fn vec24(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u24()? as usize;
        self.bytes(n)
    }

    /// Fails unless everything has been read.
    pub fn finish(&self) -> Result<(), Error> {
        if self.is_empty() { Ok(()) } else { Err(decode()) }
    }
}

/// Appends a `uint16`.
pub fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Appends a `uint24`.
pub fn put_u24(out: &mut Vec<u8>, v: usize) {
    out.extend_from_slice(&(v as u32).to_be_bytes()[1..]);
}

/// Appends a vector with a 1-byte length.
pub fn put_vec8(out: &mut Vec<u8>, v: &[u8]) {
    out.push(v.len() as u8);
    out.extend_from_slice(v);
}

/// Appends a vector with a 2-byte length.
pub fn put_vec16(out: &mut Vec<u8>, v: &[u8]) {
    put_u16(out, v.len() as u16);
    out.extend_from_slice(v);
}

/// Appends a vector with a 3-byte length.
pub fn put_vec24(out: &mut Vec<u8>, v: &[u8]) {
    put_u24(out, v.len());
    out.extend_from_slice(v);
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn integers_and_vectors() {
        let mut out = Vec::new();
        put_vec8(&mut out, &[1]);
        put_vec16(&mut out, &[2, 3]);
        put_vec24(&mut out, &[4]);
        out.extend_from_slice(&7u32.to_be_bytes());
        let mut r = Reader::new(&out);
        assert_eq!(r.vec8().unwrap(), &[1]);
        assert_eq!(r.vec16().unwrap(), &[2, 3]);
        assert_eq!(r.vec24().unwrap(), &[4]);
        assert_eq!(r.u32().unwrap(), 7);
        assert!(r.finish().is_ok());
        assert_eq!(r.u8(), Err(decode()));
        let mut short = Reader::new(&[0, 5, 1]);
        assert_eq!(short.vec16(), Err(decode()));
        assert_eq!(Reader::new(&[1]).finish(), Err(decode()));
        assert_eq!(Reader::new(&[1, 2]).rest(), &[1, 2]);
    }
}
