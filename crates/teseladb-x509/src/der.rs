//! Distinguished Encoding Rules (ITU-T X.690): a strict reader that rejects every encoding DER
//! forbids (indefinite or non-minimal lengths, high tag numbers, non-minimal integers, booleans
//! other than 00 and FF, bit strings with set padding bits) and a writer of the same subset.

use crate::Error;

/// Universal and context-specific tags used by X.509.
pub mod tag {
    /// BOOLEAN.
    pub const BOOLEAN: u8 = 0x01;
    /// INTEGER.
    pub const INTEGER: u8 = 0x02;
    /// BIT STRING.
    pub const BIT_STRING: u8 = 0x03;
    /// OCTET STRING.
    pub const OCTET_STRING: u8 = 0x04;
    /// NULL.
    pub const NULL: u8 = 0x05;
    /// OBJECT IDENTIFIER.
    pub const OID: u8 = 0x06;
    /// ENUMERATED.
    pub const ENUMERATED: u8 = 0x0a;
    /// `UTF8String`.
    pub const UTF8_STRING: u8 = 0x0c;
    /// `PrintableString`.
    pub const PRINTABLE_STRING: u8 = 0x13;
    /// `TeletexString` (`T61String`).
    pub const TELETEX_STRING: u8 = 0x14;
    /// `IA5String`.
    pub const IA5_STRING: u8 = 0x16;
    /// `UTCTime`.
    pub const UTC_TIME: u8 = 0x17;
    /// `GeneralizedTime`.
    pub const GENERALIZED_TIME: u8 = 0x18;
    /// `VisibleString`.
    pub const VISIBLE_STRING: u8 = 0x1a;
    /// `UniversalString`.
    pub const UNIVERSAL_STRING: u8 = 0x1c;
    /// `BMPString`.
    pub const BMP_STRING: u8 = 0x1e;
    /// SEQUENCE (constructed).
    pub const SEQUENCE: u8 = 0x30;
    /// SET (constructed).
    pub const SET: u8 = 0x31;

    /// Context-specific constructed tag `[n]`.
    #[must_use]
    pub const fn context(n: u8) -> u8 {
        0xa0 | n
    }

    /// Context-specific primitive tag `[n]`.
    #[must_use]
    pub const fn context_primitive(n: u8) -> u8 {
        0x80 | n
    }
}

/// Longest length accepted: four length octets, more than any certificate needs.
const MAX_LENGTH_OCTETS: usize = 4;

/// One decoded element.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tlv<'a> {
    /// The identifier octet.
    pub tag: u8,
    /// The contents octets.
    pub value: &'a [u8],
    /// The whole encoding (identifier, length and contents).
    pub raw: &'a [u8],
}

/// A reader over a sequence of DER elements.
#[derive(Clone, Debug)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// A reader over `data`.
    #[must_use]
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, pos: 0 }
    }

    /// Whether every element has been read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pos == self.data.len()
    }

    /// The offset of the next element in the data of the reader.
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// The tag of the next element, if any.
    #[must_use]
    pub fn peek_tag(&self) -> Option<u8> {
        self.data.get(self.pos).copied()
    }

    /// Reads the next element of any tag.
    ///
    /// # Errors
    ///
    /// [`Error::Der`] when the input ends inside the element or its length is not minimal.
    pub fn read_any(&mut self) -> Result<Tlv<'a>, Error> {
        let start = self.pos;
        let rest = &self.data[start..];
        let (&tag, after) = rest.split_first().ok_or(Error::Der)?;
        // High tag numbers (low five bits all set) never appear in X.509.
        if tag & 0x1f == 0x1f {
            return Err(Error::Der);
        }
        let (&first, after) = after.split_first().ok_or(Error::Der)?;
        let (len, header) = if first < 0x80 {
            (usize::from(first), 2)
        } else {
            let n = usize::from(first & 0x7f);
            if n == 0 || n > MAX_LENGTH_OCTETS || after.len() < n {
                return Err(Error::Der);
            }
            let octets = &after[..n];
            if octets[0] == 0 {
                return Err(Error::Der);
            }
            let len = octets.iter().fold(0usize, |acc, &b| (acc << 8) | usize::from(b));
            if len < 0x80 {
                return Err(Error::Der);
            }
            (len, 2 + n)
        };
        let end = start
            .checked_add(header)
            .and_then(|h| h.checked_add(len))
            .filter(|&e| e <= self.data.len())
            .ok_or(Error::Der)?;
        self.pos = end;
        Ok(Tlv { tag, value: &self.data[start + header..end], raw: &self.data[start..end] })
    }

    /// Reads the next element, which must have tag `tag`, and returns its contents.
    ///
    /// # Errors
    ///
    /// [`Error::Der`] when the next element is missing, malformed or has another tag.
    pub fn read(&mut self, tag: u8) -> Result<&'a [u8], Error> {
        Ok(self.read_tlv(tag)?.value)
    }

    /// Reads the next element, which must have tag `tag`.
    ///
    /// # Errors
    ///
    /// [`Error::Der`] when the next element is missing, malformed or has another tag.
    pub fn read_tlv(&mut self, tag: u8) -> Result<Tlv<'a>, Error> {
        let tlv = self.read_any()?;
        if tlv.tag != tag {
            return Err(Error::Der);
        }
        Ok(tlv)
    }

    /// Reads the next element if it has tag `tag`.
    ///
    /// # Errors
    ///
    /// [`Error::Der`] when the next element has this tag but is malformed.
    pub fn read_optional(&mut self, tag: u8) -> Result<Option<&'a [u8]>, Error> {
        if self.peek_tag() == Some(tag) { self.read(tag).map(Some) } else { Ok(None) }
    }

    /// Fails if any element is left.
    ///
    /// # Errors
    ///
    /// [`Error::Der`] when bytes remain.
    pub fn finish(&self) -> Result<(), Error> {
        if self.is_empty() { Ok(()) } else { Err(Error::Der) }
    }
}

/// Reads exactly one element with tag `tag` filling the whole of `data`.
///
/// # Errors
///
/// [`Error::Der`] when `data` is not exactly one element with `tag`.
pub fn read_single(data: &[u8], tag: u8) -> Result<Tlv<'_>, Error> {
    let mut r = Reader::new(data);
    let tlv = r.read_tlv(tag)?;
    r.finish()?;
    Ok(tlv)
}

/// Checks that INTEGER contents are minimal and returns them unchanged.
///
/// # Errors
///
/// [`Error::Der`] for an empty or non-minimal INTEGER.
pub fn integer(value: &[u8]) -> Result<&[u8], Error> {
    match value {
        [] => Err(Error::Der),
        [0x00, next, ..] if next & 0x80 == 0 => Err(Error::Der),
        [0xff, next, ..] if next & 0x80 != 0 => Err(Error::Der),
        _ => Ok(value),
    }
}

/// The magnitude of a non-negative INTEGER, big-endian and without the sign octet.
///
/// # Errors
///
/// [`Error::Der`] for a malformed or negative INTEGER.
pub fn unsigned(value: &[u8]) -> Result<&[u8], Error> {
    let v = integer(value)?;
    if v[0] & 0x80 != 0 {
        return Err(Error::Der);
    }
    Ok(if v.len() > 1 && v[0] == 0 { &v[1..] } else { v })
}

/// A non-negative INTEGER that fits in a `u64`.
///
/// # Errors
///
/// [`Error::Der`] for a malformed or negative INTEGER, or one above `u64::MAX`.
pub fn small_unsigned(value: &[u8]) -> Result<u64, Error> {
    let v = unsigned(value)?;
    if v.len() > 8 {
        return Err(Error::Der);
    }
    Ok(v.iter().fold(0u64, |acc, &b| (acc << 8) | u64::from(b)))
}

/// A DER BOOLEAN.
///
/// # Errors
///
/// [`Error::Der`] for anything but one byte 0x00 or 0xFF.
pub fn boolean(value: &[u8]) -> Result<bool, Error> {
    match value {
        [0x00] => Ok(false),
        [0xff] => Ok(true),
        _ => Err(Error::Der),
    }
}

/// A BIT STRING: its bytes and the number of unused bits of the last byte, which must be zero.
///
/// # Errors
///
/// [`Error::Der`] for an empty BIT STRING, more than 7 unused bits, or unused bits that are not zero.
pub fn bit_string(value: &[u8]) -> Result<(&[u8], u8), Error> {
    let (&unused, bytes) = value.split_first().ok_or(Error::Der)?;
    if unused > 7 || (bytes.is_empty() && unused != 0) {
        return Err(Error::Der);
    }
    if let Some(&last) = bytes.last()
        && last & ((1u8 << unused) - 1) != 0
    {
        return Err(Error::Der);
    }
    Ok((bytes, unused))
}

/// A BIT STRING that must hold whole bytes (keys and signatures).
///
/// # Errors
///
/// [`Error::Der`] for a BIT STRING with unused bits.
pub fn octet_aligned_bits(value: &[u8]) -> Result<&[u8], Error> {
    match bit_string(value)? {
        (bytes, 0) => Ok(bytes),
        _ => Err(Error::Der),
    }
}

/// Checks the contents of an OBJECT IDENTIFIER: non-empty, minimal sub-identifiers, last
/// octet complete.
///
/// # Errors
///
/// [`Error::Der`] for an empty identifier, a non-minimal sub-identifier or an incomplete last one.
pub fn oid(value: &[u8]) -> Result<&[u8], Error> {
    let mut start = true;
    for &b in value {
        if start && b == 0x80 {
            return Err(Error::Der);
        }
        start = b & 0x80 == 0;
    }
    if value.is_empty() || !start {
        return Err(Error::Der);
    }
    Ok(value)
}

/// Encodes one element.
#[must_use]
pub fn tlv(tag: u8, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len() + 6);
    out.push(tag);
    let len = value.len();
    if len < 0x80 {
        out.push(len.to_le_bytes()[0]);
    } else {
        let bytes = len.to_be_bytes();
        let skip = bytes.iter().position(|&b| b != 0).unwrap_or(bytes.len() - 1);
        // At most the eight bytes of a usize.
        out.push(0x80 | (bytes.len() - skip).to_le_bytes()[0]);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(value);
    out
}

/// A constructed element whose contents are the concatenation of `parts`.
#[must_use]
pub fn constructed(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    tlv(tag, &parts.concat())
}

/// A SEQUENCE of `parts`.
#[must_use]
pub fn sequence(parts: &[&[u8]]) -> Vec<u8> {
    constructed(tag::SEQUENCE, parts)
}

/// A non-negative INTEGER from its big-endian magnitude.
#[must_use]
pub fn unsigned_integer(magnitude: &[u8]) -> Vec<u8> {
    let start = magnitude.iter().position(|&b| b != 0).unwrap_or(magnitude.len());
    let m = &magnitude[start..];
    let mut v = Vec::with_capacity(m.len() + 1);
    if m.first().is_none_or(|&b| b & 0x80 != 0) {
        v.push(0);
    }
    v.extend_from_slice(m);
    tlv(tag::INTEGER, &v)
}

/// A non-negative INTEGER from a `u64`.
#[must_use]
pub fn small_integer(n: u64) -> Vec<u8> {
    unsigned_integer(&n.to_be_bytes())
}

/// A BOOLEAN.
#[must_use]
pub fn boolean_value(b: bool) -> Vec<u8> {
    tlv(tag::BOOLEAN, &[if b { 0xff } else { 0x00 }])
}

/// A BIT STRING of whole bytes.
#[must_use]
pub fn bits(bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(bytes.len() + 1);
    v.push(0);
    v.extend_from_slice(bytes);
    tlv(tag::BIT_STRING, &v)
}

/// A named-bit BIT STRING (`KeyUsage`): bit 0 is the most significant bit of the first byte,
/// with trailing zero bits removed as DER requires.
#[must_use]
pub fn named_bits(mask: u16) -> Vec<u8> {
    // Bit i of `mask` is named bit i.
    let mut bytes = [0u8; 2];
    for i in 0..16 {
        if mask & (1 << i) != 0 {
            bytes[i / 8] |= 0x80 >> (i % 8);
        }
    }
    let highest = (0..16usize).rev().find(|&i| mask & (1 << i) != 0);
    match highest {
        None => tlv(tag::BIT_STRING, &[0]),
        Some(h) => {
            let len = h / 8 + 1;
            let unused = (7 - h % 8).to_le_bytes()[0];
            let mut v = vec![unused];
            v.extend_from_slice(&bytes[..len]);
            tlv(tag::BIT_STRING, &v)
        }
    }
}

/// Reads a named-bit BIT STRING into a mask where bit i is named bit i.
///
/// # Errors
///
/// [`Error::Der`] for a malformed BIT STRING, trailing zero bits, or more than 16 named bits.
pub fn named_bits_mask(value: &[u8]) -> Result<u16, Error> {
    let (bytes, _) = bit_string(value)?;
    let mut mask = 0u16;
    for (i, &b) in bytes.iter().enumerate() {
        for j in 0..8 {
            if b & (0x80 >> j) != 0 {
                let bit = i * 8 + j;
                if bit >= 16 {
                    return Err(Error::Der);
                }
                mask |= 1 << bit;
            }
        }
    }
    Ok(mask)
}

/// An OCTET STRING.
#[must_use]
pub fn octets(bytes: &[u8]) -> Vec<u8> {
    tlv(tag::OCTET_STRING, bytes)
}

/// An OBJECT IDENTIFIER from its contents.
#[must_use]
pub fn object_id(content: &[u8]) -> Vec<u8> {
    tlv(tag::OID, content)
}

/// NULL.
#[must_use]
pub fn null() -> Vec<u8> {
    tlv(tag::NULL, &[])
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn position_when_elements_are_read_moves_past_each() {
        let mut r = Reader::new(&[0x05, 0x00, 0x04, 0x01, 0xff]);
        assert_eq!(r.position(), 0);
        assert!(r.read(tag::NULL).is_ok());
        assert_eq!(r.position(), 2);
        assert!(r.read(tag::OCTET_STRING).is_ok());
        assert_eq!(r.position(), 5);
    }

    #[test]
    fn rejects_non_minimal_lengths_and_high_tags() {
        assert_eq!(Reader::new(&[0x04, 0x81, 0x05, 1, 2, 3, 4, 5]).read_any(), Err(Error::Der));
        assert_eq!(Reader::new(&[0x04, 0x82, 0x00, 0x80]).read_any(), Err(Error::Der));
        assert_eq!(Reader::new(&[0x04, 0x80]).read_any(), Err(Error::Der));
        assert_eq!(Reader::new(&[0x1f, 0x01, 0x00]).read_any(), Err(Error::Der));
        assert_eq!(Reader::new(&[0x04, 0x85, 1, 1, 1, 1, 1]).read_any(), Err(Error::Der));
        assert_eq!(Reader::new(&[0x04, 0x03, 1]).read_any(), Err(Error::Der));
        assert_eq!(Reader::new(&[0x04]).read_any(), Err(Error::Der));
        assert_eq!(Reader::new(&[]).read_any(), Err(Error::Der));
        let long = tlv(tag::OCTET_STRING, &[7u8; 300]);
        assert_eq!(&long[..4], &[0x04, 0x82, 0x01, 0x2c]);
        assert_eq!(Reader::new(&long).read(tag::OCTET_STRING).map(<[u8]>::len), Ok(300));
        assert_eq!(read_single(&[0x05, 0x00, 0x05, 0x00], tag::NULL), Err(Error::Der));
        assert_eq!(Reader::new(&[0x05, 0x00]).read(tag::INTEGER), Err(Error::Der));
        assert_eq!(Reader::new(&[0x05, 0x00]).read_optional(tag::INTEGER), Ok(None));
    }

    #[test]
    fn integers_booleans_bits_and_oids() {
        assert_eq!(integer(&[]), Err(Error::Der));
        assert_eq!(integer(&[0, 1]), Err(Error::Der));
        assert_eq!(integer(&[0xff, 0x80]), Err(Error::Der));
        assert_eq!(integer(&[0xff, 0x7f]), Ok(&[0xff, 0x7f][..]));
        assert_eq!(unsigned(&[0x80]), Err(Error::Der));
        assert_eq!(unsigned(&[0, 0x80]), Ok(&[0x80][..]));
        assert_eq!(small_unsigned(&[1; 9]), Err(Error::Der));
        assert_eq!(small_unsigned(&[1, 0]), Ok(256));
        assert_eq!(boolean(&[1]), Err(Error::Der));
        assert_eq!(boolean(&[0]), Ok(false));
        assert_eq!(bit_string(&[8, 0]), Err(Error::Der));
        assert_eq!(bit_string(&[1]), Err(Error::Der));
        assert_eq!(bit_string(&[1, 0x01]), Err(Error::Der));
        assert_eq!(octet_aligned_bits(&[1, 0x02]), Err(Error::Der));
        assert_eq!(oid(&[]), Err(Error::Der));
        assert_eq!(oid(&[0x80, 1]), Err(Error::Der));
        assert_eq!(oid(&[0x2a, 0x86]), Err(Error::Der));
        assert_eq!(unsigned_integer(&[0, 0, 0x80]), vec![2, 2, 0, 0x80]);
        assert_eq!(unsigned_integer(&[]), vec![2, 1, 0]);
        assert_eq!(small_integer(5), vec![2, 1, 5]);
        for mask in [0u16, 1, 0x20, 0x60, 0x100, 0x1ff, 0x8000] {
            let enc = named_bits(mask);
            let v = read_single(&enc, tag::BIT_STRING).expect("the test input is valid").value;
            assert_eq!(named_bits_mask(v), Ok(mask));
        }
        assert_eq!(named_bits_mask(&[0, 0, 0, 0x80]), Err(Error::Der));
    }
}
