//! Base64 with the standard alphabet and `=` padding (RFC 4648), as Extended JSON writes binary
//! data. Reading is strict: a length that is not a multiple of 4, a character outside the
//! alphabet or padding anywhere but at the end is refused.

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `bytes` in base64.
#[must_use]
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The bytes of base64 `text`, if it is valid.
#[must_use]
pub fn decode(text: &str) -> Option<Vec<u8>> {
    let t = text.as_bytes();
    if !t.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(t.len() / 4 * 3);
    for (i, quad) in t.chunks(4).enumerate() {
        let last = i == t.len() / 4 - 1;
        let pad = quad.iter().rev().take_while(|&&c| c == b'=').count();
        if pad > 2 || (pad > 0 && !last) {
            return None;
        }
        let mut n = 0u32;
        for &c in &quad[..4 - pad] {
            let v = u32::try_from(ALPHABET.iter().position(|&a| a == c)?).ok()?;
            n = (n << 6) | v;
        }
        for _ in 0..pad {
            n <<= 6;
        }
        let [_, b0, b1, b2] = n.to_be_bytes();
        let bytes = [b0, b1, b2];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    Some(out)
}

#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
mod tests {
    use super::*;

    #[test]
    fn rfc_4648_vectors_and_refusals() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), coded);
            assert_eq!(decode(coded).expect("decode succeeds"), plain.as_bytes());
        }
        assert_eq!(decode("//8=").expect("decode succeeds"), [0xFF, 0xFF]);
        for bad in ["Zg=", "Z===", "Zg==Zg==", "Zm9!", "=Zm9"] {
            assert_eq!(decode(bad), None, "{bad}");
        }
    }
}
