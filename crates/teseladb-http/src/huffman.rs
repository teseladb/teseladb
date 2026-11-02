//! The Huffman code of HPACK (RFC 7541, section 5.2 and appendix B): encoding and decoding of
//! header strings.

use crate::hpack::HpackError;

/// The Huffman code of each symbol, and its length in bits: the 256 octets and EOS (RFC 7541,
/// appendix B).
pub(crate) const HUFFMAN: [(u32, u8); 257] = [
    (0x1ff8, 13),
    (0x7fffd8, 23),
    (0xfffffe2, 28),
    (0xfffffe3, 28),
    (0xfffffe4, 28),
    (0xfffffe5, 28),
    (0xfffffe6, 28),
    (0xfffffe7, 28),
    (0xfffffe8, 28),
    (0xffffea, 24),
    (0x3ffffffc, 30),
    (0xfffffe9, 28),
    (0xfffffea, 28),
    (0x3ffffffd, 30),
    (0xfffffeb, 28),
    (0xfffffec, 28),
    (0xfffffed, 28),
    (0xfffffee, 28),
    (0xfffffef, 28),
    (0xffffff0, 28),
    (0xffffff1, 28),
    (0xffffff2, 28),
    (0x3ffffffe, 30),
    (0xffffff3, 28),
    (0xffffff4, 28),
    (0xffffff5, 28),
    (0xffffff6, 28),
    (0xffffff7, 28),
    (0xffffff8, 28),
    (0xffffff9, 28),
    (0xffffffa, 28),
    (0xffffffb, 28),
    (0x14, 6),
    (0x3f8, 10),
    (0x3f9, 10),
    (0xffa, 12),
    (0x1ff9, 13),
    (0x15, 6),
    (0xf8, 8),
    (0x7fa, 11),
    (0x3fa, 10),
    (0x3fb, 10),
    (0xf9, 8),
    (0x7fb, 11),
    (0xfa, 8),
    (0x16, 6),
    (0x17, 6),
    (0x18, 6),
    (0x0, 5),
    (0x1, 5),
    (0x2, 5),
    (0x19, 6),
    (0x1a, 6),
    (0x1b, 6),
    (0x1c, 6),
    (0x1d, 6),
    (0x1e, 6),
    (0x1f, 6),
    (0x5c, 7),
    (0xfb, 8),
    (0x7ffc, 15),
    (0x20, 6),
    (0xffb, 12),
    (0x3fc, 10),
    (0x1ffa, 13),
    (0x21, 6),
    (0x5d, 7),
    (0x5e, 7),
    (0x5f, 7),
    (0x60, 7),
    (0x61, 7),
    (0x62, 7),
    (0x63, 7),
    (0x64, 7),
    (0x65, 7),
    (0x66, 7),
    (0x67, 7),
    (0x68, 7),
    (0x69, 7),
    (0x6a, 7),
    (0x6b, 7),
    (0x6c, 7),
    (0x6d, 7),
    (0x6e, 7),
    (0x6f, 7),
    (0x70, 7),
    (0x71, 7),
    (0x72, 7),
    (0xfc, 8),
    (0x73, 7),
    (0xfd, 8),
    (0x1ffb, 13),
    (0x7fff0, 19),
    (0x1ffc, 13),
    (0x3ffc, 14),
    (0x22, 6),
    (0x7ffd, 15),
    (0x3, 5),
    (0x23, 6),
    (0x4, 5),
    (0x24, 6),
    (0x5, 5),
    (0x25, 6),
    (0x26, 6),
    (0x27, 6),
    (0x6, 5),
    (0x74, 7),
    (0x75, 7),
    (0x28, 6),
    (0x29, 6),
    (0x2a, 6),
    (0x7, 5),
    (0x2b, 6),
    (0x76, 7),
    (0x2c, 6),
    (0x8, 5),
    (0x9, 5),
    (0x2d, 6),
    (0x77, 7),
    (0x78, 7),
    (0x79, 7),
    (0x7a, 7),
    (0x7b, 7),
    (0x7ffe, 15),
    (0x7fc, 11),
    (0x3ffd, 14),
    (0x1ffd, 13),
    (0xffffffc, 28),
    (0xfffe6, 20),
    (0x3fffd2, 22),
    (0xfffe7, 20),
    (0xfffe8, 20),
    (0x3fffd3, 22),
    (0x3fffd4, 22),
    (0x3fffd5, 22),
    (0x7fffd9, 23),
    (0x3fffd6, 22),
    (0x7fffda, 23),
    (0x7fffdb, 23),
    (0x7fffdc, 23),
    (0x7fffdd, 23),
    (0x7fffde, 23),
    (0xffffeb, 24),
    (0x7fffdf, 23),
    (0xffffec, 24),
    (0xffffed, 24),
    (0x3fffd7, 22),
    (0x7fffe0, 23),
    (0xffffee, 24),
    (0x7fffe1, 23),
    (0x7fffe2, 23),
    (0x7fffe3, 23),
    (0x7fffe4, 23),
    (0x1fffdc, 21),
    (0x3fffd8, 22),
    (0x7fffe5, 23),
    (0x3fffd9, 22),
    (0x7fffe6, 23),
    (0x7fffe7, 23),
    (0xffffef, 24),
    (0x3fffda, 22),
    (0x1fffdd, 21),
    (0xfffe9, 20),
    (0x3fffdb, 22),
    (0x3fffdc, 22),
    (0x7fffe8, 23),
    (0x7fffe9, 23),
    (0x1fffde, 21),
    (0x7fffea, 23),
    (0x3fffdd, 22),
    (0x3fffde, 22),
    (0xfffff0, 24),
    (0x1fffdf, 21),
    (0x3fffdf, 22),
    (0x7fffeb, 23),
    (0x7fffec, 23),
    (0x1fffe0, 21),
    (0x1fffe1, 21),
    (0x3fffe0, 22),
    (0x1fffe2, 21),
    (0x7fffed, 23),
    (0x3fffe1, 22),
    (0x7fffee, 23),
    (0x7fffef, 23),
    (0xfffea, 20),
    (0x3fffe2, 22),
    (0x3fffe3, 22),
    (0x3fffe4, 22),
    (0x7ffff0, 23),
    (0x3fffe5, 22),
    (0x3fffe6, 22),
    (0x7ffff1, 23),
    (0x3ffffe0, 26),
    (0x3ffffe1, 26),
    (0xfffeb, 20),
    (0x7fff1, 19),
    (0x3fffe7, 22),
    (0x7ffff2, 23),
    (0x3fffe8, 22),
    (0x1ffffec, 25),
    (0x3ffffe2, 26),
    (0x3ffffe3, 26),
    (0x3ffffe4, 26),
    (0x7ffffde, 27),
    (0x7ffffdf, 27),
    (0x3ffffe5, 26),
    (0xfffff1, 24),
    (0x1ffffed, 25),
    (0x7fff2, 19),
    (0x1fffe3, 21),
    (0x3ffffe6, 26),
    (0x7ffffe0, 27),
    (0x7ffffe1, 27),
    (0x3ffffe7, 26),
    (0x7ffffe2, 27),
    (0xfffff2, 24),
    (0x1fffe4, 21),
    (0x1fffe5, 21),
    (0x3ffffe8, 26),
    (0x3ffffe9, 26),
    (0xffffffd, 28),
    (0x7ffffe3, 27),
    (0x7ffffe4, 27),
    (0x7ffffe5, 27),
    (0xfffec, 20),
    (0xfffff3, 24),
    (0xfffed, 20),
    (0x1fffe6, 21),
    (0x3fffe9, 22),
    (0x1fffe7, 21),
    (0x1fffe8, 21),
    (0x7ffff3, 23),
    (0x3fffea, 22),
    (0x3fffeb, 22),
    (0x1ffffee, 25),
    (0x1ffffef, 25),
    (0xfffff4, 24),
    (0xfffff5, 24),
    (0x3ffffea, 26),
    (0x7ffff4, 23),
    (0x3ffffeb, 26),
    (0x7ffffe6, 27),
    (0x3ffffec, 26),
    (0x3ffffed, 26),
    (0x7ffffe7, 27),
    (0x7ffffe8, 27),
    (0x7ffffe9, 27),
    (0x7ffffea, 27),
    (0x7ffffeb, 27),
    (0xffffffe, 28),
    (0x7ffffec, 27),
    (0x7ffffed, 27),
    (0x7ffffee, 27),
    (0x7ffffef, 27),
    (0x7fffff0, 27),
    (0x3ffffee, 26),
    (0x3fffffff, 30),
];

/// The symbol of end of string, which a string never contains (RFC 7541, section 5.2).
const EOS: usize = 256;

/// The length in bytes of `bytes` once coded.
#[must_use]
pub fn encoded_len(bytes: &[u8]) -> usize {
    let bits: usize = bytes.iter().map(|b| usize::from(HUFFMAN[usize::from(*b)].1)).sum();
    bits.div_ceil(8)
}

/// `bytes` coded, padded with the most significant bits of EOS (all ones).
#[must_use]
pub fn encode(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(encoded_len(bytes));
    let mut acc: u64 = 0;
    let mut bits: u32 = 0;
    for b in bytes {
        let (code, len) = HUFFMAN[usize::from(*b)];
        acc = (acc << len) | u64::from(code);
        bits += u32::from(len);
        while bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
        acc &= (1u64 << bits) - 1;
    }
    if bits > 0 {
        let pad = 8 - bits;
        out.push(((acc << pad) | ((1u64 << pad) - 1)) as u8);
    }
    out
}

/// The symbols of `bytes`, coded: a code of EOS, or padding longer than seven bits or not made of
/// ones, is an error (RFC 7541, section 5.2).
pub fn decode(bytes: &[u8]) -> Result<Vec<u8>, HpackError> {
    let table = by_length();
    let mut out = Vec::with_capacity(bytes.len() * 8 / 5);
    let mut code: u32 = 0;
    let mut len: u8 = 0;
    for byte in bytes {
        for shift in (0..8).rev() {
            code = (code << 1) | u32::from((byte >> shift) & 1);
            len += 1;
            if let Some(sym) = lookup(table, code, len) {
                if sym == EOS {
                    return Err(HpackError::Huffman);
                }
                out.push(sym as u8);
                code = 0;
                len = 0;
            } else if len >= 30 {
                return Err(HpackError::Huffman);
            }
        }
    }
    // What remains is padding: fewer than eight bits, all ones.
    if len > 7 || code != (1u32 << len) - 1 {
        return Err(HpackError::Huffman);
    }
    Ok(out)
}

/// The codes of each length, sorted, with their symbols.
type ByLength = [Vec<(u32, usize)>; 31];

fn by_length() -> &'static ByLength {
    static TABLE: std::sync::OnceLock<ByLength> = std::sync::OnceLock::new();
    TABLE.get_or_init(build)
}

fn build() -> ByLength {
    let mut table: ByLength = std::array::from_fn(|_| Vec::new());
    for (sym, (code, len)) in HUFFMAN.iter().enumerate() {
        table[usize::from(*len)].push((*code, sym));
    }
    for codes in &mut table {
        codes.sort_unstable();
    }
    table
}

fn lookup(table: &ByLength, code: u32, len: u8) -> Option<usize> {
    let codes = table.get(usize::from(len))?;
    codes.binary_search_by_key(&code, |(c, _)| *c).ok().map(|i| codes[i].1)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn the_examples_of_the_rfc_code_and_decode() {
        // RFC 7541, C.4.1 and C.6.1.
        let cases: [(&str, &[u8]); 3] = [
            (
                "www.example.com",
                &[0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff],
            ),
            ("no-cache", &[0xa8, 0xeb, 0x10, 0x64, 0x9c, 0xbf]),
            ("302", &[0x64, 0x02]),
        ];
        for (text, coded) in cases {
            assert_eq!(encode(text.as_bytes()), coded, "{text}");
            assert_eq!(encoded_len(text.as_bytes()), coded.len());
            assert_eq!(decode(coded).expect(text), text.as_bytes());
        }
    }

    #[test]
    fn every_octet_survives_a_round_trip() {
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&all)).expect("all octets"), all);
        assert_eq!(decode(&[]).expect("empty"), Vec::<u8>::new());
    }

    #[test]
    fn bad_padding_and_eos_are_refused() {
        // 'a' is 00011 (5 bits): padded with zeros instead of ones.
        assert_eq!(decode(&[0x18]), Err(HpackError::Huffman));
        // A whole byte of padding (eight ones) after a complete symbol.
        assert_eq!(decode(&[0x1f, 0xff]), Err(HpackError::Huffman));
        // EOS itself: thirty ones.
        assert_eq!(decode(&[0xff, 0xff, 0xff, 0xfc]), Err(HpackError::Huffman));
    }
}
