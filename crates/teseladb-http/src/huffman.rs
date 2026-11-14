//! The Huffman code of HPACK (RFC 7541, section 5.2 and appendix B): encoding and decoding of
//! header strings.

use crate::hpack::HpackError;

/// The Huffman code of each symbol, and its length in bits: the 256 octets and EOS (RFC 7541,
/// appendix B).
pub(crate) const HUFFMAN: [(u32, u8); 257] = [
    (0x1ff8, 13),
    (0x007f_ffd8, 23),
    (0x0fff_ffe2, 28),
    (0x0fff_ffe3, 28),
    (0x0fff_ffe4, 28),
    (0x0fff_ffe5, 28),
    (0x0fff_ffe6, 28),
    (0x0fff_ffe7, 28),
    (0x0fff_ffe8, 28),
    (0x00ff_ffea, 24),
    (0x3fff_fffc, 30),
    (0x0fff_ffe9, 28),
    (0x0fff_ffea, 28),
    (0x3fff_fffd, 30),
    (0x0fff_ffeb, 28),
    (0x0fff_ffec, 28),
    (0x0fff_ffed, 28),
    (0x0fff_ffee, 28),
    (0x0fff_ffef, 28),
    (0x0fff_fff0, 28),
    (0x0fff_fff1, 28),
    (0x0fff_fff2, 28),
    (0x3fff_fffe, 30),
    (0x0fff_fff3, 28),
    (0x0fff_fff4, 28),
    (0x0fff_fff5, 28),
    (0x0fff_fff6, 28),
    (0x0fff_fff7, 28),
    (0x0fff_fff8, 28),
    (0x0fff_fff9, 28),
    (0x0fff_fffa, 28),
    (0x0fff_fffb, 28),
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
    (0x0fff_fffc, 28),
    (0xfffe6, 20),
    (0x003f_ffd2, 22),
    (0xfffe7, 20),
    (0xfffe8, 20),
    (0x003f_ffd3, 22),
    (0x003f_ffd4, 22),
    (0x003f_ffd5, 22),
    (0x007f_ffd9, 23),
    (0x003f_ffd6, 22),
    (0x007f_ffda, 23),
    (0x007f_ffdb, 23),
    (0x007f_ffdc, 23),
    (0x007f_ffdd, 23),
    (0x007f_ffde, 23),
    (0x00ff_ffeb, 24),
    (0x007f_ffdf, 23),
    (0x00ff_ffec, 24),
    (0x00ff_ffed, 24),
    (0x003f_ffd7, 22),
    (0x007f_ffe0, 23),
    (0x00ff_ffee, 24),
    (0x007f_ffe1, 23),
    (0x007f_ffe2, 23),
    (0x007f_ffe3, 23),
    (0x007f_ffe4, 23),
    (0x001f_ffdc, 21),
    (0x003f_ffd8, 22),
    (0x007f_ffe5, 23),
    (0x003f_ffd9, 22),
    (0x007f_ffe6, 23),
    (0x007f_ffe7, 23),
    (0x00ff_ffef, 24),
    (0x003f_ffda, 22),
    (0x001f_ffdd, 21),
    (0xfffe9, 20),
    (0x003f_ffdb, 22),
    (0x003f_ffdc, 22),
    (0x007f_ffe8, 23),
    (0x007f_ffe9, 23),
    (0x001f_ffde, 21),
    (0x007f_ffea, 23),
    (0x003f_ffdd, 22),
    (0x003f_ffde, 22),
    (0x00ff_fff0, 24),
    (0x001f_ffdf, 21),
    (0x003f_ffdf, 22),
    (0x007f_ffeb, 23),
    (0x007f_ffec, 23),
    (0x001f_ffe0, 21),
    (0x001f_ffe1, 21),
    (0x003f_ffe0, 22),
    (0x001f_ffe2, 21),
    (0x007f_ffed, 23),
    (0x003f_ffe1, 22),
    (0x007f_ffee, 23),
    (0x007f_ffef, 23),
    (0xfffea, 20),
    (0x003f_ffe2, 22),
    (0x003f_ffe3, 22),
    (0x003f_ffe4, 22),
    (0x007f_fff0, 23),
    (0x003f_ffe5, 22),
    (0x003f_ffe6, 22),
    (0x007f_fff1, 23),
    (0x03ff_ffe0, 26),
    (0x03ff_ffe1, 26),
    (0xfffeb, 20),
    (0x7fff1, 19),
    (0x003f_ffe7, 22),
    (0x007f_fff2, 23),
    (0x003f_ffe8, 22),
    (0x01ff_ffec, 25),
    (0x03ff_ffe2, 26),
    (0x03ff_ffe3, 26),
    (0x03ff_ffe4, 26),
    (0x07ff_ffde, 27),
    (0x07ff_ffdf, 27),
    (0x03ff_ffe5, 26),
    (0x00ff_fff1, 24),
    (0x01ff_ffed, 25),
    (0x7fff2, 19),
    (0x001f_ffe3, 21),
    (0x03ff_ffe6, 26),
    (0x07ff_ffe0, 27),
    (0x07ff_ffe1, 27),
    (0x03ff_ffe7, 26),
    (0x07ff_ffe2, 27),
    (0x00ff_fff2, 24),
    (0x001f_ffe4, 21),
    (0x001f_ffe5, 21),
    (0x03ff_ffe8, 26),
    (0x03ff_ffe9, 26),
    (0x0fff_fffd, 28),
    (0x07ff_ffe3, 27),
    (0x07ff_ffe4, 27),
    (0x07ff_ffe5, 27),
    (0xfffec, 20),
    (0x00ff_fff3, 24),
    (0xfffed, 20),
    (0x001f_ffe6, 21),
    (0x003f_ffe9, 22),
    (0x001f_ffe7, 21),
    (0x001f_ffe8, 21),
    (0x007f_fff3, 23),
    (0x003f_ffea, 22),
    (0x003f_ffeb, 22),
    (0x01ff_ffee, 25),
    (0x01ff_ffef, 25),
    (0x00ff_fff4, 24),
    (0x00ff_fff5, 24),
    (0x03ff_ffea, 26),
    (0x007f_fff4, 23),
    (0x03ff_ffeb, 26),
    (0x07ff_ffe6, 27),
    (0x03ff_ffec, 26),
    (0x03ff_ffed, 26),
    (0x07ff_ffe7, 27),
    (0x07ff_ffe8, 27),
    (0x07ff_ffe9, 27),
    (0x07ff_ffea, 27),
    (0x07ff_ffeb, 27),
    (0x0fff_fffe, 28),
    (0x07ff_ffec, 27),
    (0x07ff_ffed, 27),
    (0x07ff_ffee, 27),
    (0x07ff_ffef, 27),
    (0x07ff_fff0, 27),
    (0x03ff_ffee, 26),
    (0x3fff_ffff, 30),
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
            out.push((acc >> bits).to_le_bytes()[0]);
        }
        acc &= (1u64 << bits) - 1;
    }
    if bits > 0 {
        let pad = 8 - bits;
        out.push(((acc << pad) | ((1u64 << pad) - 1)).to_le_bytes()[0]);
    }
    out
}

/// The symbols of `bytes`, coded: a code of EOS, or padding longer than seven bits or not made of
/// ones, is an error (RFC 7541, section 5.2).
///
/// # Errors
///
/// [`HpackError::Huffman`] for the code of EOS, a code that does not exist, or bad padding.
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
                // Every symbol but EOS (256) is a byte.
                out.push(sym.to_le_bytes()[0]);
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
