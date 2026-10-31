//! Bit streams of Zstandard (RFC 8878, section 4.1): the forward stream of the table
//! descriptions, and the backward streams of Huffman literals and sequences, which the encoder
//! writes forward ending with a 1 bit and the decoder reads from that last bit downwards.

use crate::Error;

/// Reader of a backward bit stream.
#[derive(Debug)]
pub(crate) struct BackReader<'a> {
    src: &'a [u8],
    /// Bits left to read: bit `pos - 1` of the stream (as a little-endian integer) is next.
    /// Negative once the reader went past the start, which only zeros fill.
    pos: i64,
}

impl<'a> BackReader<'a> {
    /// A reader over `src`, whose last byte holds the end mark.
    pub(crate) fn new(src: &'a [u8]) -> Result<BackReader<'a>, Error> {
        let last = *src.last().ok_or(Error::Corrupt("empty bit stream"))?;
        if last == 0 {
            return Err(Error::Corrupt("bit stream without end mark"));
        }
        let pos = src.len() as i64 * 8 - i64::from(last.leading_zeros()) - 1;
        Ok(BackReader { src, pos })
    }

    /// The next `n` bits (at most 56) without consuming them; past the start they are zeros.
    pub(crate) fn peek(&self, n: u32) -> u64 {
        if n == 0 || self.pos <= 0 {
            return 0;
        }
        let lo = self.pos - i64::from(n);
        let start = lo.max(0) as usize;
        let end = self.pos as usize;
        let mut word = 0u64;
        for (k, &b) in self.src[start / 8..end.div_ceil(8)].iter().enumerate() {
            word |= u64::from(b) << (8 * k);
        }
        let width = (end - start) as u32;
        let v = (word >> (start % 8)) & ((1u64 << width) - 1);
        match lo < 0 {
            true => v << (-lo),
            false => v,
        }
    }

    /// Consumes `n` bits.
    pub(crate) fn consume(&mut self, n: u32) {
        self.pos -= i64::from(n);
    }

    /// Reads `n` bits (at most 56).
    pub(crate) fn read(&mut self, n: u32) -> u64 {
        let v = self.peek(n);
        self.consume(n);
        v
    }

    /// Bits left (negative past the start).
    pub(crate) fn left(&self) -> i64 {
        self.pos
    }
}

/// Reader of a forward bit stream (bits from the low end of each byte).
#[derive(Debug)]
pub(crate) struct ForwardReader<'a> {
    src: &'a [u8],
    bit: usize,
}

impl<'a> ForwardReader<'a> {
    pub(crate) fn new(src: &'a [u8]) -> ForwardReader<'a> {
        ForwardReader { src, bit: 0 }
    }

    /// The next `n` bits (at most 32) without consuming them.
    pub(crate) fn peek(&self, n: u32) -> Result<u32, Error> {
        let end = self.bit + n as usize;
        if end > self.src.len() * 8 {
            return Err(Error::Truncated);
        }
        let mut word = 0u64;
        for (k, &b) in self.src[self.bit / 8..end.div_ceil(8)].iter().enumerate() {
            word |= u64::from(b) << (8 * k);
        }
        Ok(((word >> (self.bit % 8)) & ((1u64 << n) - 1)) as u32)
    }

    pub(crate) fn consume(&mut self, n: u32) {
        self.bit += n as usize;
    }

    /// Whole bytes the reader touched.
    pub(crate) fn bytes_used(&self) -> usize {
        self.bit.div_ceil(8)
    }
}

/// Writer of a bit stream that a [`BackReader`] reads in reverse.
#[derive(Debug, Default)]
pub(crate) struct BitWriter {
    out: Vec<u8>,
    acc: u64,
    nbits: u32,
}

impl BitWriter {
    /// Appends the `n` low bits of `v` (at most 32).
    pub(crate) fn write(&mut self, v: u64, n: u32) {
        self.acc |= (v & ((1u64 << n) - 1)) << self.nbits;
        self.nbits += n;
        while self.nbits >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }

    /// Ends the stream with its mark and returns its bytes.
    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.write(1, 1);
        if self.nbits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn writer_and_back_reader_agree() {
        let mut w = BitWriter::default();
        let items = [(5u64, 3u32), (0, 0), (0x1234, 16), (1, 1), (0x7fff_ffff, 31), (3, 2)];
        for (v, n) in items {
            w.write(v, n);
        }
        let bytes = w.finish();
        let mut r = BackReader::new(&bytes).unwrap();
        for (v, n) in items.iter().rev() {
            assert_eq!(r.read(*n), *v);
        }
        assert_eq!(r.left(), 0);
        // Past the start, zeros fill the low bits.
        assert_eq!(r.read(4), 0);
        assert_eq!(r.left(), -4);
        assert_eq!(r.peek(3), 0);
    }

    #[test]
    fn partial_read_at_the_start_pads_with_zeros() {
        // One byte 0b0000_0111: mark at bit 2, two data bits `11`.
        let mut r = BackReader::new(&[0b111]).unwrap();
        assert_eq!(r.peek(4), 0b1100);
        r.consume(1);
        assert_eq!(r.read(1), 1);
        assert!(BackReader::new(&[]).is_err());
        assert!(BackReader::new(&[1, 0]).is_err());
    }

    #[test]
    fn forward_reader_bounds() {
        let mut r = ForwardReader::new(&[0xab, 0xcd]);
        assert_eq!(r.peek(4).unwrap(), 0xb);
        r.consume(4);
        assert_eq!(r.peek(12).unwrap(), 0xcda);
        assert_eq!(r.bytes_used(), 1);
        assert_eq!(r.peek(13), Err(Error::Truncated));
    }
}
